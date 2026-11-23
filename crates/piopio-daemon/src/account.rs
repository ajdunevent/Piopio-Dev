//! One XMPP account: its D-Bus objects and the supervisor that connects,
//! disconnects and reconnects it as the user asks.

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};
use tokio_xmpp::jid::BareJid;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::{OwnedObjectPath, Value};

use piopio_dbus::ACCOUNT_API_VERSION;

use crate::access;
use crate::connect::Server;
use crate::roster::RosterObject;
use crate::session::{self, Session};
use crate::store::{AccountRow, Store};

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

enum Control {
    SetEnabled(bool),
    NewPassword(String),
    Shutdown(oneshot::Sender<()>),
}

/// `net.dunevent.Piopio.Account`, served at `/net/dunevent/Piopio/account/<id>`.
pub struct AccountObject {
    session: Session,
    enabled: bool,
    state: State,
    error: String,
    control: mpsc::UnboundedSender<Control>,
}

#[zbus::interface(name = "net.dunevent.Piopio.Account")]
impl AccountObject {
    #[zbus(property)]
    fn jid(&self) -> String {
        self.session.jid.to_string()
    }

    /// Whether the account should be connected. Writable.
    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.enabled
    }

    #[zbus(property)]
    async fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        let _ = self.control.send(Control::SetEnabled(enabled));
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

    /// Show (available, away, chat, dnd or xa) and status text. Writable.
    #[zbus(property)]
    async fn presence(&self) -> (String, String) {
        self.session.presence.lock().await.clone()
    }

    #[zbus(property)]
    async fn set_presence(&mut self, presence: (String, String)) -> zbus::fdo::Result<()> {
        if !matches!(
            presence.0.as_str(),
            "available" | "away" | "chat" | "dnd" | "xa"
        ) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "show is available, away, chat, dnd or xa".into(),
            ));
        }
        *self.session.presence.lock().await = presence;
        if self.session.is_online() {
            session::send_presence(&self.session).await;
        }
        Ok(())
    }

    #[zbus(property)]
    fn nickname(&self) -> String {
        String::new()
    }

    /// A cache handle for Files.GetAvatar; empty until avatars arrive.
    #[zbus(property)]
    fn avatar(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        ACCOUNT_API_VERSION
    }

    /// Opens (or creates) the one-to-one conversation with `jid`.
    async fn start_chat(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        access::check(&header)?;
        let peer: BareJid = jid
            .parse()
            .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("not a valid JID: {e}")))?;
        let (_, path) = self
            .session
            .conversations
            .get(&self.session, &peer)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        // Learn whether the peer uses OMEMO, so Encryption is right before
        // the first message.
        crate::omemo::refresh(&self.session, peer);
        Ok(path)
    }

    /// Joins a group chat and bookmarks it. Returns the conversation's path
    /// right away; the Room interface on it follows the joining. An empty
    /// nick uses the account's name. Options: "password" (s).
    async fn join_room(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
        nick: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        access::check(&header)?;
        let room: BareJid = jid
            .parse()
            .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("not a valid JID: {e}")))?;
        if room.node().is_none() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "a room's address looks like room@conference.example.org".into(),
            ));
        }
        let password = options
            .get("password")
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or("")
            .to_owned();
        crate::rooms::join(&self.session, &room, Some(nick.to_owned()), password)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Changes the account's password on the server (XEP-0077), then uses it.
    async fn change_password(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        new: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        if new.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "the password is empty".into(),
            ));
        }
        let user = self
            .session
            .jid
            .node()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let ns = "jabber:iq:register";
        let query = Element::builder("query", ns)
            .append(Element::builder("username", ns).append(user))
            .append(Element::builder("password", ns).append(new.to_owned()))
            .build();
        let iq = Iq::Set {
            from: None,
            to: Some(self.session.jid.domain().to_owned().into()),
            id: String::new(),
            payload: query,
        };
        self.session
            .iq(iq)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("the server refused: {e}")))?;
        self.session
            .store
            .set_password(self.session.account_id, new.to_owned())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let _ = self.control.send(Control::NewPassword(new.to_owned()));
        Ok(())
    }
}

/// Updates the D-Bus properties and emits PropertiesChanged for what changed.
/// `error: None` leaves the Error property as it is.
pub async fn set_state(iface: &InterfaceRef<AccountObject>, state: State, error: Option<String>) {
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

/// The daemon's handle on an account.
pub struct Account {
    pub id: i64,
    pub path: OwnedObjectPath,
    pub session: Session,
    control: mpsc::UnboundedSender<Control>,
    task: tokio::task::JoinHandle<()>,
}

impl Account {
    /// Exports the account's objects and starts connecting if it is enabled.
    pub async fn start(
        bus: &zbus::Connection,
        store: Store,
        row: AccountRow,
        server: Server,
    ) -> anyhow::Result<Self> {
        let jid: BareJid = row.jid.parse()?;
        let path =
            OwnedObjectPath::try_from(format!("{}/{}", piopio_dbus::ACCOUNT_PATH_PREFIX, row.id))?;
        let session = session::new_shared(row.id, jid, path.clone(), store, bus.clone());

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let object = AccountObject {
            session: session.clone(),
            enabled: row.enabled,
            state: State::Disconnected,
            error: String::new(),
            control: control_tx.clone(),
        };
        let objects = bus.object_server();
        objects.at(&path, object).await?;
        objects
            .at(
                &path,
                RosterObject {
                    session: session.clone(),
                },
            )
            .await?;
        objects
            .at(
                &path,
                crate::omemo::OmemoObject {
                    session: session.clone(),
                },
            )
            .await?;
        let iface = objects.interface::<_, AccountObject>(&path).await?;
        session.conversations.load(&session).await?;

        let task = tokio::spawn(supervise(
            session.clone(),
            iface,
            row.password,
            server,
            row.enabled,
            control_rx,
        ));
        Ok(Self {
            id: row.id,
            path,
            session,
            control: control_tx,
            task,
        })
    }

    /// Disconnects and unexports the account's objects.
    pub async fn stop(self) {
        let (tx, rx) = oneshot::channel();
        if self.control.send(Control::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        let _ = self.task.await;
        let objects = self.session.bus.object_server();
        self.session.conversations.unexport_all(&self.session).await;
        let _ = objects.remove::<RosterObject, _>(&self.path).await;
        let _ = objects
            .remove::<crate::omemo::OmemoObject, _>(&self.path)
            .await;
        let _ = objects.remove::<AccountObject, _>(&self.path).await;
    }
}

async fn supervise(
    session: Session,
    iface: InterfaceRef<AccountObject>,
    mut password: String,
    server: Server,
    enabled: bool,
    mut control: mpsc::UnboundedReceiver<Control>,
) {
    let mut connection = enabled
        .then(|| session::connect(&session, iface.clone(), password.clone(), server.clone()));
    while let Some(command) = control.recv().await {
        match command {
            Control::SetEnabled(enabled) => {
                let _ = session
                    .store
                    .set_account_enabled(session.account_id, enabled)
                    .await;
                let _ = iface
                    .get()
                    .await
                    .enabled_changed(iface.signal_emitter())
                    .await;
                if enabled && connection.is_none() {
                    connection = Some(session::connect(
                        &session,
                        iface.clone(),
                        password.clone(),
                        server.clone(),
                    ));
                } else if !enabled && let Some(c) = connection.take() {
                    c.stop().await;
                }
            }
            Control::NewPassword(new) => {
                password = new;
                if let Some(c) = connection.take() {
                    c.stop().await;
                    connection = Some(session::connect(
                        &session,
                        iface.clone(),
                        password.clone(),
                        server.clone(),
                    ));
                }
            }
            Control::Shutdown(done) => {
                if let Some(c) = connection.take() {
                    c.stop().await;
                }
                let _ = done.send(());
                break;
            }
        }
    }
}

/// Reads the AddAccount options into a server choice.
pub fn server_from_options(options: &HashMap<&str, Value<'_>>) -> zbus::fdo::Result<Server> {
    let get_str = |key: &str| -> Option<String> {
        match options.get(key) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        }
    };
    if let Some(address) = get_str("test-plaintext-address") {
        if std::env::var("PIOPIO_ALLOW_PLAINTEXT").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::AccessDenied(
                "unencrypted connections are only for the test suite".into(),
            ));
        }
        return Ok(Server::TestPlaintext { address });
    }
    if let Some(host) = get_str("host") {
        let port = match options.get("port") {
            None => 5222,
            Some(Value::U16(p)) => *p,
            Some(Value::U32(p)) => u16::try_from(*p).unwrap_or(5222),
            Some(Value::I32(p)) => u16::try_from(*p).unwrap_or(5222),
            Some(_) => {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "port must be a number".into(),
                ));
            }
        };
        return Ok(Server::Host { host, port });
    }
    Ok(Server::Srv)
}
