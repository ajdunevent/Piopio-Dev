//! The live side of one account: its stanza stream, IQ tracking, and what
//! happens each time it comes online.
//!
//! The session task owns the `StanzaStream`. Everything else (D-Bus handlers,
//! the catch-up task) talks to it through a [`Session`] handle.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_xmpp::Stanza;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::ns;
use tokio_xmpp::parsers::stanza_error::{DefinedCondition, ErrorType, StanzaError};
use tokio_xmpp::parsers::stream_features::StreamFeatures;
use tokio_xmpp::stanzastream::{Event, StanzaStream, StanzaToken, StreamEvent};
use zbus::object_server::InterfaceRef;
use zbus::zvariant::OwnedObjectPath;

use crate::account::{AccountObject, State, set_state};
use crate::connect::{Server, Status, connector};
use crate::store::Store;
use crate::{messages, roster};

const IQ_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an IQ request did not produce a result.
#[derive(Debug)]
pub enum IqError {
    /// The entity answered with an error.
    Error(Box<StanzaError>),
    /// No answer in time, or the stream was reset before one came.
    NoReply,
}

impl std::fmt::Display for IqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IqError::Error(e) => {
                let text = e.texts.values().next().map(String::as_str).unwrap_or("");
                write!(f, "{:?} {text}", e.defined_condition)
            }
            IqError::NoReply => f.write_str("no reply"),
        }
    }
}

type IqReply = Result<Option<Element>, IqError>;

enum Command {
    Send(Box<Stanza>, Option<oneshot::Sender<StanzaToken>>),
    Iq(Box<Iq>, oneshot::Sender<IqReply>),
}

/// Everything an account's parts share. Cheap to clone through `Arc`.
pub struct Shared {
    pub account_id: i64,
    pub jid: BareJid,
    pub path: OwnedObjectPath,
    pub store: Store,
    pub bus: zbus::Connection,
    /// The running connection's command queue; None while disabled.
    commands: std::sync::Mutex<Option<mpsc::Sender<Command>>>,
    /// The JID the server bound us to, once online.
    pub bound: Mutex<Option<Jid>>,
    pub online: watch::Sender<bool>,
    /// Set once the archive catch-up after login has finished; until then,
    /// live archive ids must not move the catch-up position forward.
    pub caught_up: AtomicBool,
    pub roster: roster::RosterState,
    pub conversations: messages::Conversations,
    /// The presence we announce: (show, status). Show is "available", "away",
    /// "chat", "dnd" or "xa".
    pub presence: Mutex<(String, String)>,
    pub omemo: crate::omemo::OmemoState,
    pub rooms: crate::rooms::Rooms,
    pub uploads: crate::files::Uploads,
}

/// Handle used by everything outside the session task.
pub type Session = Arc<Shared>;

impl Shared {
    fn commands(&self) -> Option<mpsc::Sender<Command>> {
        self.commands.lock().expect("lock").clone()
    }

    /// Queues a stanza. Returns a token that follows it through the queue,
    /// or None if the account is disabled.
    pub async fn send(&self, stanza: impl Into<Stanza>) -> Option<StanzaToken> {
        let (tx, rx) = oneshot::channel();
        self.commands()?
            .send(Command::Send(Box::new(stanza.into()), Some(tx)))
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Sends an IQ get or set and waits for the matching result.
    pub async fn iq(&self, iq: Iq) -> IqReply {
        let (tx, rx) = oneshot::channel();
        let Some(commands) = self.commands() else {
            return Err(IqError::NoReply);
        };
        if commands.send(Command::Iq(Box::new(iq), tx)).await.is_err() {
            return Err(IqError::NoReply);
        }
        match tokio::time::timeout(IQ_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            _ => Err(IqError::NoReply),
        }
    }

    /// True when `from` is our own account (absent, our bare JID, or one of our resources).
    pub fn is_own(&self, from: Option<&Jid>) -> bool {
        from.is_none_or(|j| j.to_bare() == self.jid)
    }

    pub fn is_online(&self) -> bool {
        *self.online.borrow()
    }
}

/// Creates the account's shared state. Nothing connects until [`connect`].
pub fn new_shared(
    account_id: i64,
    jid: BareJid,
    path: OwnedObjectPath,
    store: Store,
    bus: zbus::Connection,
) -> Session {
    Arc::new(Shared {
        account_id,
        jid,
        path,
        store,
        bus,
        commands: std::sync::Mutex::new(None),
        bound: Mutex::new(None),
        online: watch::Sender::new(false),
        caught_up: AtomicBool::new(false),
        roster: roster::RosterState::default(),
        conversations: messages::Conversations::default(),
        presence: Mutex::new(("available".into(), String::new())),
        omemo: crate::omemo::OmemoState::default(),
        rooms: crate::rooms::Rooms::default(),
        uploads: crate::files::Uploads::default(),
    })
}

/// A running connection for one account.
pub struct Connection {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Connection {
    /// Closes the stream cleanly and waits for the task to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .is_err()
        {
            tracing::warn!("connection did not stop in time; aborting it");
            self.task.abort();
        }
    }
}

/// Starts connecting the account and keeps it connected until stopped.
pub fn connect(
    session: &Session,
    iface: InterfaceRef<AccountObject>,
    password: String,
    server: Server,
) -> Connection {
    let (commands_tx, commands_rx) = mpsc::channel(64);
    *session.commands.lock().expect("lock") = Some(commands_tx);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run(
        session.clone(),
        iface,
        password,
        server,
        commands_rx,
        shutdown_rx,
    ));
    Connection {
        shutdown: Some(shutdown_tx),
        task,
    }
}

async fn run(
    shared: Session,
    iface: InterfaceRef<AccountObject>,
    password: String,
    server: Server,
    mut commands: mpsc::Receiver<Command>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let connector = connector(shared.jid.clone(), password, server, status_tx, cancel_rx);
    let mut stream = StanzaStream::new(connector, 16);
    set_state(&iface, State::Connecting, Some(String::new())).await;

    let mut pending: HashMap<String, (Option<Jid>, oneshot::Sender<IqReply>)> = HashMap::new();
    let mut next_id: u64 = 0;
    let mut connected = false;

    // Incoming stanzas are handled on their own task, in order. Handlers send
    // replies through this loop, so handling them here would deadlock.
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<Stanza>();
    let dispatcher = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(stanza) = incoming_rx.recv().await {
                handle_stanza(&shared, stanza).await;
            }
        })
    };

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(status) = status_rx.recv() => match status {
                Status::Connecting => set_state(&iface, State::Connecting, None).await,
                Status::Retrying(why) => {
                    tracing::warn!(jid = %shared.jid, "connection failed: {why}");
                    set_state(&iface, State::Connecting, Some(why)).await;
                }
                Status::AuthFailed(why) => {
                    tracing::warn!(jid = %shared.jid, "authentication failed: {why}");
                    set_state(&iface, State::Error, Some(why)).await;
                }
            },
            Some(command) = commands.recv() => match command {
                Command::Send(stanza, reply) => {
                    let token = stream.send(stanza).await;
                    if let Some(reply) = reply {
                        let _ = reply.send(token);
                    }
                }
                Command::Iq(iq, reply) => {
                    let mut iq = *iq;
                    next_id += 1;
                    let id = format!("piopio-{next_id}");
                    *iq.id_mut() = id.clone();
                    pending.retain(|_, (_, tx)| !tx.is_closed());
                    pending.insert(id, (iq.to().cloned(), reply));
                    stream.send(Box::new(iq.into())).await;
                }
            },
            event = stream.next() => match event {
                Some(Event::Stream(StreamEvent::Reset { bound_jid, features })) => {
                    tracing::info!(%bound_jid, "online");
                    connected = true;
                    // State is lost: requests in flight will never be answered.
                    for (_, (_, tx)) in pending.drain() {
                        let _ = tx.send(Err(IqError::NoReply));
                    }
                    *shared.bound.lock().await = Some(bound_jid);
                    set_state(&iface, State::Connected, Some(String::new())).await;
                    shared.online.send_replace(true);
                    tokio::spawn(came_online(shared.clone(), features));
                }
                Some(Event::Stream(StreamEvent::Resumed)) => {
                    tracing::info!(jid = %shared.jid, "stream resumed");
                    connected = true;
                    set_state(&iface, State::Connected, Some(String::new())).await;
                    shared.online.send_replace(true);
                }
                Some(Event::Stream(StreamEvent::Suspended)) => {
                    tracing::info!(jid = %shared.jid, "connection lost, reconnecting");
                    connected = false;
                    shared.online.send_replace(false);
                    set_state(&iface, State::Connecting, None).await;
                }
                Some(Event::Stanza(Stanza::Iq(Iq::Result { from, id, payload, .. }))) => {
                    if let Some((_, tx)) = take_pending(&shared, &mut pending, &id, from.as_ref()) {
                        let _ = tx.send(Ok(payload));
                    }
                }
                Some(Event::Stanza(Stanza::Iq(Iq::Error { from, id, error, .. }))) => {
                    if let Some((_, tx)) = take_pending(&shared, &mut pending, &id, from.as_ref()) {
                        let _ = tx.send(Err(IqError::Error(Box::new(error))));
                    }
                }
                Some(Event::Stanza(stanza)) => {
                    let _ = incoming_tx.send(stanza);
                }
                None => break,
            },
        }
    }

    *shared.commands.lock().expect("lock") = None;
    drop(incoming_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), dispatcher).await;
    shared.online.send_replace(false);
    for (_, (_, tx)) in pending.drain() {
        let _ = tx.send(Err(IqError::NoReply));
    }
    // Stop any connection attempt in progress. A live stream is closed politely;
    // one still waiting for a connection is simply dropped.
    let _ = cancel_tx.send(true);
    if connected {
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.close()).await;
    } else {
        drop(stream);
    }
    set_state(&iface, State::Disconnected, None).await;
}

/// Runs after every login that lost state (not after a resumption).
async fn came_online(shared: Session, features: StreamFeatures) {
    shared.caught_up.store(false, Ordering::SeqCst);
    roster::fetch(&shared).await;
    messages::enable_carbons(&shared).await;
    send_presence(&shared).await;
    let _ = features;
    shared.uploads.forget().await;
    crate::omemo::online(&shared).await;
    crate::rooms::online(&shared).await;
    messages::catch_up(&shared).await;
}

/// Announces our current presence to the server (and so to our contacts).
pub async fn send_presence(shared: &Shared) {
    use tokio_xmpp::parsers::presence::{Presence, Show};
    let (show, status) = shared.presence.lock().await.clone();
    let mut presence = Presence::available();
    presence.show = match show.as_str() {
        "away" => Some(Show::Away),
        "chat" => Some(Show::Chat),
        "dnd" => Some(Show::Dnd),
        "xa" => Some(Show::Xa),
        _ => None,
    };
    if !status.is_empty() {
        presence.set_status("", status);
    }
    presence.add_payload(crate::disco::caps());
    shared.send(presence).await;
}

async fn handle_stanza(shared: &Session, stanza: Stanza) {
    match stanza {
        // Answers to our requests are matched in the connection loop.
        Stanza::Iq(Iq::Result { .. } | Iq::Error { .. }) => {}
        Stanza::Iq(Iq::Get {
            from, id, payload, ..
        }) => {
            let reply = if payload.is("ping", ns::PING) {
                result(from, id)
            } else if let Some(reply) = crate::disco::answer(shared, &from, &id, &payload) {
                reply
            } else {
                error(from, id, DefinedCondition::ServiceUnavailable)
            };
            shared.send(reply).await;
        }
        Stanza::Iq(Iq::Set {
            from, id, payload, ..
        }) => {
            let reply = if payload.is("query", ns::ROSTER) {
                if shared.is_own(from.as_ref()) {
                    roster::push(shared, payload).await;
                    result(from, id)
                } else {
                    // Roster pushes from anyone else are forged.
                    error(from, id, DefinedCondition::Forbidden)
                }
            } else {
                error(from, id, DefinedCondition::ServiceUnavailable)
            };
            shared.send(reply).await;
        }
        Stanza::Message(message) => messages::incoming(shared, message).await,
        Stanza::Presence(presence) => {
            if !crate::rooms::presence(shared, &presence).await {
                roster::presence(shared, presence).await
            }
        }
    }
}

/// Finds the request an IQ answer belongs to. The answer must come from the
/// entity the request went to; for requests to our own account, the server or
/// our bare JID may answer.
fn take_pending(
    shared: &Shared,
    pending: &mut HashMap<String, (Option<Jid>, oneshot::Sender<IqReply>)>,
    id: &str,
    from: Option<&Jid>,
) -> Option<(Option<Jid>, oneshot::Sender<IqReply>)> {
    let (to, _) = pending.get(id)?;
    let matches = match (to, from) {
        (None, None) => true,
        (None, Some(from)) => {
            from.to_bare() == shared.jid || from.as_str() == shared.jid.domain().as_str()
        }
        (Some(to), Some(from)) => {
            to == from || (to.to_bare() == shared.jid && from.to_bare() == shared.jid)
        }
        (Some(to), None) => to.to_bare() == shared.jid,
    };
    if !matches {
        tracing::warn!(?from, id, "IQ answer from the wrong entity ignored");
        return None;
    }
    pending.remove(id)
}

pub fn result(to: Option<Jid>, id: String) -> Iq {
    Iq::Result {
        from: None,
        to,
        id,
        payload: None,
    }
}

pub fn error(to: Option<Jid>, id: String, condition: DefinedCondition) -> Iq {
    Iq::Error {
        from: None,
        to,
        id,
        error: StanzaError::new(ErrorType::Cancel, condition, "en", ""),
        payload: None,
    }
}

/// A random id for stanzas we originate (message ids double as origin ids).
pub fn new_id() -> String {
    let mut raw = [0u8; 12];
    getrandom::fill(&mut raw).expect("randomness");
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
