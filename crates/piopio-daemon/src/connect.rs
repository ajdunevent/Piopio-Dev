//! Opening connections: where to connect, and the connector `StanzaStream`
//! calls whenever it needs a fresh stream.
//!
//! tokio-xmpp's `StanzaStream` handles resource binding, Stream Management and
//! resumption. The daemon supplies its own connector so that it, not the
//! library, decides what happens on failure: an authentication failure stops
//! retrying and is reported to the UI instead of looping forever.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_xmpp::connect::{
    DnsConfig, ServerConnector, StartTlsServerConnector, TcpServerConnector,
};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::stanzastream::Connection;
use tokio_xmpp::xmlstream::Timeouts;

use crate::login::authenticate;

/// Where an account connects. Stored with the account as JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Server {
    /// Look up the server with DNS SRV records (the normal case).
    #[default]
    Srv,
    /// A fixed host and port, with STARTTLS.
    Host { host: String, port: u16 },
    /// Unencrypted TCP to a fixed address. Only for the test suite, and only
    /// accepted when PIOPIO_ALLOW_PLAINTEXT=1.
    TestPlaintext { address: String },
}

/// What the connector reports back to the session.
#[derive(Debug)]
pub enum Status {
    Connecting,
    /// A connection attempt failed; the connector retries after a delay.
    Retrying(String),
    /// The server rejected the credentials. No retry until the password
    /// changes or the account is re-enabled, which starts a new connection.
    AuthFailed(String),
}

pub type Connector = Box<dyn FnMut(Option<String>, oneshot::Sender<Connection>) + Send + 'static>;

/// Builds the connector `StanzaStream` calls whenever it needs a new stream.
pub fn connector(
    jid: BareJid,
    password: String,
    server: Server,
    status: mpsc::UnboundedSender<Status>,
    cancel: watch::Receiver<bool>,
) -> Connector {
    Box::new(move |_preferred_location, mut slot| {
        let (jid, password, server, status, mut cancel) = (
            jid.clone(),
            password.clone(),
            server.clone(),
            status.clone(),
            cancel.clone(),
        );
        tokio::spawn(async move {
            const MAX_DELAY: Duration = Duration::from_secs(60);
            let mut delay = Duration::from_secs(1);
            loop {
                let _ = status.send(Status::Connecting);
                let full = Jid::from(jid.clone());
                let attempt = tokio::select! {
                    _ = cancelled(&mut cancel) => return park(slot).await,
                    _ = slot.closed() => return,
                    attempt = attempt(&server, &full, &password) => attempt,
                };
                let wait = match attempt {
                    Ok(connection) => {
                        let _ = slot.send(connection);
                        return;
                    }
                    Err(e @ tokio_xmpp::Error::Auth(_)) => {
                        tracing::debug!(%jid, "login failed: {e:?}");
                        let _ = status.send(Status::AuthFailed(describe(&e, &jid)));
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
                    _ = std::future::pending::<()>(), if wait.is_none() => {}
                    _ = tokio::time::sleep(wait.unwrap_or_default()), if wait.is_some() => {}
                }
            }
        });
    })
}

async fn attempt(
    server: &Server,
    jid: &Jid,
    password: &str,
) -> Result<Connection, tokio_xmpp::Error> {
    // TODO: also try direct TLS (XEP-0368), and SASL2, Bind 2 and FAST.
    match server {
        Server::Srv => {
            let dns = DnsConfig::srv_default_client(jid.domain().as_str());
            login(StartTlsServerConnector::from(dns), jid, password).await
        }
        Server::Host { host, port } => {
            let dns = DnsConfig::no_srv(host, *port);
            login(StartTlsServerConnector::from(dns), jid, password).await
        }
        Server::TestPlaintext { address } => {
            let dns = DnsConfig::addr(address);
            login(TcpServerConnector::from(dns), jid, password).await
        }
    }
}

async fn login<C: ServerConnector>(
    server: C,
    jid: &Jid,
    password: &str,
) -> Result<Connection, tokio_xmpp::Error> {
    let (features, stream) = authenticate(server, jid, password, Timeouts::default()).await?;
    Ok(Connection {
        stream: stream.box_stream(),
        features,
        identity: jid.clone(),
    })
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    let _ = cancel.wait_for(|c| *c).await;
}

/// Holds on to the connection slot of a connection that is being stopped.
///
/// tokio-xmpp 6 panics if a connector drops its slot, and a `StanzaStream`
/// still waiting for a connection never notices that its frontend is gone.
/// Until that is fixed upstream, a connection stopped while offline leaves
/// this small task and the stream's worker parked; the daemon exiting frees them.
async fn park(slot: oneshot::Sender<Connection>) {
    let _slot = slot;
    std::future::pending::<()>().await;
}

/// Turns a connection error into a sentence the UI can show.
pub fn describe(error: &tokio_xmpp::Error, jid: &BareJid) -> String {
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
