//! One XMPP account: its D-Bus object and the task that keeps it connected.
//!
//! tokio-xmpp's `StanzaStream` handles resource binding, Stream Management and
//! resumption. The daemon supplies its own connector so that it, not the
//! library, decides what happens on failure: an authentication failure stops
//! retrying and is reported to the UI instead of looping forever.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_xmpp::Stanza;
use tokio_xmpp::connect::{DnsConfig, StartTlsServerConnector};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::ns;
use tokio_xmpp::parsers::stanza_error::{DefinedCondition, ErrorType, StanzaError};
use tokio_xmpp::stanzastream::{Connection, Event, StanzaStream, StreamEvent};
use tokio_xmpp::xmlstream::Timeouts;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::OwnedObjectPath;

use piopio_dbus::ACCOUNT_API_VERSION;

use crate::login::authenticate;

/// Connection state as shown on the Account interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Disconnected => "disconnected",
            State::Connecting => "connecting",
            State::Connected => "connected",
            State::Error => "error",
        }
    }
}

/// `net.dunevent.Piopio.Account`, served at `/net/dunevent/Piopio/account/<id>`.
pub struct AccountObject {
    jid: BareJid,
    enabled: bool,
    state: State,
    error: String,
}

#[zbus::interface(name = "net.dunevent.Piopio.Account")]
impl AccountObject {
    #[zbus(property)]
    fn jid(&self) -> String {
        self.jid.to_string()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.enabled
    }

    /// One of disconnected, connecting, connected or error.
    #[zbus(property)]
    fn state(&self) -> String {
        self.state.as_str().to_owned()
    }

    /// Why the account is in the error state, or why the last attempt failed.
    #[zbus(property)]
    fn error(&self) -> String {
        self.error.clone()
    }

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        ACCOUNT_API_VERSION
    }
}

/// What the connector and the stream report back to the account task.
#[derive(Debug)]
enum Status {
    Connecting,
    /// A connection attempt failed; the connector retries after a delay.
    Retrying(String),
    /// The server rejected the credentials; no retry until told to.
    AuthFailed(String),
}

/// The daemon's handle on a running account.
pub struct Account {
    pub path: OwnedObjectPath,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Account {
    /// Export the account on the bus and start connecting.
    pub async fn start(
        connection: &zbus::Connection,
        path: OwnedObjectPath,
        jid: BareJid,
        password: String,
    ) -> zbus::Result<Self> {
        let object = AccountObject {
            jid: jid.clone(),
            enabled: true,
            state: State::Connecting,
            error: String::new(),
        };
        let server = connection.object_server();
        server.at(&path, object).await?;
        let iface = server.interface::<_, AccountObject>(&path).await?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(iface, jid, password, shutdown_rx));
        Ok(Self {
            path,
            shutdown: Some(shutdown_tx),
            task,
        })
    }

    /// Close the stream cleanly and wait for the task to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .is_err()
        {
            tracing::warn!(path = %self.path, "account did not stop in time; aborting it");
            self.task.abort();
        }
    }
}

async fn run(
    iface: InterfaceRef<AccountObject>,
    jid: BareJid,
    password: String,
    mut shutdown: oneshot::Receiver<()>,
) {
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let retry = Arc::new(Notify::new());
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let connector = make_connector(jid.clone(), password, status_tx, retry, cancel_rx);
    let mut stream = StanzaStream::new(connector, 16);
    let mut connected = false;

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(status) = status_rx.recv() => match status {
                Status::Connecting => set_state(&iface, State::Connecting, None).await,
                Status::Retrying(why) => {
                    tracing::warn!(%jid, "connection failed: {why}");
                    set_state(&iface, State::Connecting, Some(why)).await;
                }
                Status::AuthFailed(why) => {
                    tracing::warn!(%jid, "authentication failed: {why}");
                    set_state(&iface, State::Error, Some(why)).await;
                }
            },
            event = stream.next() => match event {
                Some(Event::Stream(StreamEvent::Reset { bound_jid, .. })) => {
                    tracing::info!(%bound_jid, "online");
                    connected = true;
                    set_state(&iface, State::Connected, Some(String::new())).await;
                }
                Some(Event::Stream(StreamEvent::Resumed)) => {
                    tracing::info!(%jid, "stream resumed");
                    connected = true;
                    set_state(&iface, State::Connected, Some(String::new())).await;
                }
                Some(Event::Stream(StreamEvent::Suspended)) => {
                    tracing::info!(%jid, "connection lost, reconnecting");
                    connected = false;
                    set_state(&iface, State::Connecting, None).await;
                }
                Some(Event::Stanza(stanza)) => handle_stanza(&stream, stanza).await,
                None => break,
            },
        }
    }

    // Stop any connection attempt in progress. A live stream is closed politely;
    // one still waiting for a connection is simply dropped, which ends its worker.
    let _ = cancel_tx.send(true);
    if connected {
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.close()).await;
    } else {
        drop(stream);
    }
    set_state(&iface, State::Disconnected, None).await;
}

/// Builds the connector `StanzaStream` calls whenever it needs a new stream.
fn make_connector(
    jid: BareJid,
    password: String,
    status: mpsc::UnboundedSender<Status>,
    retry: Arc<Notify>,
    cancel: watch::Receiver<bool>,
) -> Box<dyn FnMut(Option<String>, oneshot::Sender<Connection>) + Send + 'static> {
    Box::new(move |_preferred_location, mut slot| {
        let (jid, password, status, retry, mut cancel) = (
            jid.clone(),
            password.clone(),
            status.clone(),
            retry.clone(),
            cancel.clone(),
        );
        tokio::spawn(async move {
            const MAX_DELAY: Duration = Duration::from_secs(60);
            let mut delay = Duration::from_secs(1);
            loop {
                let _ = status.send(Status::Connecting);
                // TODO: also try direct TLS (XEP-0368), and SASL2, Bind 2 and FAST.
                let server = StartTlsServerConnector::from(DnsConfig::srv_default_client(
                    jid.domain().as_str(),
                ));
                let full = Jid::from(jid.clone());
                let attempt = tokio::select! {
                    _ = cancelled(&mut cancel) => return park(slot).await,
                    _ = slot.closed() => return,
                    attempt = authenticate(server, &full, &password, Timeouts::default()) => attempt,
                };
                let wait = match attempt {
                    Ok((features, stream)) => {
                        let _ = slot.send(Connection {
                            stream: stream.box_stream(),
                            features,
                            identity: full,
                        });
                        return;
                    }
                    Err(e @ tokio_xmpp::Error::Auth(_)) => {
                        tracing::debug!(%jid, "login failed: {e:?}");
                        let _ = status.send(Status::AuthFailed(describe(&e, &jid)));
                        // Keep the slot: dropping it would fail the whole stream.
                        // Wait until new credentials or a re-enable ask for another try.
                        delay = Duration::from_secs(1);
                        None
                    }
                    Err(e) => {
                        tracing::debug!(%jid, "connection attempt failed: {e:?}");
                        let _ = status.send(Status::Retrying(describe(&e, &jid)));
                        let wait = delay;
                        delay = (delay * 2).min(MAX_DELAY);
                        Some(wait)
                    }
                };
                tokio::select! {
                    _ = cancelled(&mut cancel) => return park(slot).await,
                    _ = slot.closed() => return,
                    _ = retry.notified(), if wait.is_none() => {}
                    _ = tokio::time::sleep(wait.unwrap_or_default()), if wait.is_some() => {}
                }
            }
        });
    })
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    let _ = cancel.wait_for(|c| *c).await;
}

/// Holds on to the connection slot of an account that is shutting down.
///
/// tokio-xmpp 6 panics if a connector drops its slot, and a `StanzaStream`
/// still waiting for its first connection never notices that its frontend is
/// gone. Until that is fixed upstream, an account removed while offline leaves
/// this small task and the stream's worker parked; the daemon exiting frees them.
async fn park(slot: oneshot::Sender<Connection>) {
    let _slot = slot;
    std::future::pending::<()>().await;
}

/// Turns a connection error into a sentence the UI can show.
fn describe(error: &tokio_xmpp::Error, jid: &BareJid) -> String {
    use tokio_xmpp::Error as E;
    use tokio_xmpp::error::AuthError;
    use tokio_xmpp::parsers::sasl::DefinedCondition as Sasl;

    let domain = jid.domain();
    match error {
        E::DnsProto(_) | E::DnsNet(_) | E::Idna => format!("Couldn't find the server for {domain}"),
        E::Io(e) => format!("Network error: {e}"),
        E::Connection(e) => format!("Couldn't connect securely to {domain}: {e}"),
        E::Disconnected => "The server closed the connection".to_owned(),
        E::Protocol(e) => format!("The server sent something unexpected: {e}"),
        E::Auth(AuthError::Fail(Sasl::NotAuthorized)) => "Wrong address or password".to_owned(),
        E::Auth(AuthError::Fail(Sasl::AccountDisabled)) => {
            "This account is disabled on the server".to_owned()
        }
        E::Auth(AuthError::Fail(Sasl::CredentialsExpired)) => "The password has expired".to_owned(),
        E::Auth(AuthError::NoMechanism) => {
            "The server offers no login method Piopio supports".to_owned()
        }
        E::Auth(e) => format!("The server refused the login: {e}"),
        other => other.to_string(),
    }
}

/// Answers what the server or other entities ask of us. Pings get a result;
/// every other request gets an error, as RFC 6120 requires.
async fn handle_stanza(stream: &StanzaStream, stanza: Stanza) {
    match stanza {
        Stanza::Iq(Iq::Get {
            from, id, payload, ..
        }) if payload.is("ping", ns::PING) => {
            let reply = Iq::Result {
                from: None,
                to: from,
                id,
                payload: None,
            };
            stream.send(Box::new(reply.into())).await;
        }
        Stanza::Iq(Iq::Get { from, id, .. }) | Stanza::Iq(Iq::Set { from, id, .. }) => {
            let reply = Iq::Error {
                from: None,
                to: from,
                id,
                error: StanzaError::new(
                    ErrorType::Cancel,
                    DefinedCondition::ServiceUnavailable,
                    "en",
                    "",
                ),
                payload: None,
            };
            stream.send(Box::new(reply.into())).await;
        }
        other => tracing::trace!(?other, "stanza not handled yet"),
    }
}

/// Updates the D-Bus properties and emits PropertiesChanged for what changed.
/// `error: None` leaves the Error property as it is.
async fn set_state(iface: &InterfaceRef<AccountObject>, state: State, error: Option<String>) {
    let emitter = iface.signal_emitter();
    let mut object = iface.get_mut().await;
    if object.state != state {
        object.state = state;
        let _ = object.state_changed(emitter).await;
    }
    if let Some(error) = error
        && object.error != error
    {
        object.error = error;
        let _ = object.error_changed(emitter).await;
    }
}
