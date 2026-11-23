//! `net.dunevent.Piopio.Daemon` on the root object.

use std::collections::HashMap;
use std::sync::Arc;

use piopio_dbus::DAEMON_API_VERSION;
use tokio::sync::{Mutex, watch};
use tokio_xmpp::jid::BareJid;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use crate::access;
use crate::account::{Account, server_from_options};
use crate::connect::Server;
use crate::store::Store;

/// Every account the daemon is running.
pub struct Accounts {
    store: Store,
    by_jid: HashMap<BareJid, Account>,
}

impl Accounts {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            by_jid: HashMap::new(),
        }
    }

    /// Starts every stored account (enabled ones connect).
    pub async fn load(&mut self, bus: &zbus::Connection) -> anyhow::Result<()> {
        for row in self.store.accounts().await? {
            let server: Server = serde_json::from_str(&row.options).unwrap_or_default();
            let jid: BareJid = row.jid.parse()?;
            match Account::start(bus, self.store.clone(), row, server).await {
                Ok(account) => {
                    self.by_jid.insert(jid, account);
                }
                Err(e) => tracing::error!(%jid, "could not start the account: {e:#}"),
            }
        }
        Ok(())
    }

    /// Disconnects every account cleanly.
    pub async fn stop_all(&mut self) {
        for (_, account) in self.by_jid.drain() {
            account.stop().await;
        }
    }
}

pub struct Daemon {
    accounts: Arc<Mutex<Accounts>>,
    quit: watch::Sender<bool>,
}

impl Daemon {
    pub fn new(accounts: Arc<Mutex<Accounts>>, quit: watch::Sender<bool>) -> Self {
        Self { accounts, quit }
    }
}

fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

#[zbus::interface(name = "net.dunevent.Piopio.Daemon")]
impl Daemon {
    /// Adds an account, stores it, and starts connecting. Returns its path.
    ///
    /// Options: `host` (s) and `port` (q or u) to skip DNS SRV lookup.
    async fn add_account(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
        jid: &str,
        password: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        access::check(&header)?;
        let jid: BareJid = jid
            .parse()
            .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("not a valid JID: {e}")))?;
        if jid.node().is_none() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "the JID needs a user part, like user@example.org".into(),
            ));
        }
        let server = server_from_options(&options)?;

        let mut accounts = self.accounts.lock().await;
        if accounts.by_jid.contains_key(&jid) {
            return Err(zbus::fdo::Error::Failed(format!("{jid} is already added")));
        }
        let store = accounts.store.clone();
        let id = store
            .add_account(
                jid.to_string(),
                password.to_owned(),
                serde_json::to_string(&server).map_err(failed)?,
            )
            .await
            .map_err(failed)?;
        let row = store
            .accounts()
            .await
            .map_err(failed)?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| failed("the new account vanished"))?;
        let account = Account::start(connection, store, row, server)
            .await
            .map_err(failed)?;
        let path = account.path.clone();
        accounts.by_jid.insert(jid, account);
        Ok(path)
    }

    /// Disconnects an account and deletes it with all its local data.
    async fn remove_account(
        &self,
        #[zbus(header)] header: Header<'_>,
        account: ObjectPath<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let mut accounts = self.accounts.lock().await;
        let jid = accounts
            .by_jid
            .iter()
            .find(|(_, a)| a.path.as_ref() == account)
            .map(|(jid, _)| jid.clone())
            .ok_or_else(|| zbus::fdo::Error::UnknownObject(account.to_string()))?;
        let removed = accounts.by_jid.remove(&jid).expect("found above");
        let id = removed.id;
        // Contacts should stop encrypting for a device that is going away.
        crate::omemo::retire(&removed.session).await;
        removed.stop().await;
        accounts.store.remove_account(id).await.map_err(failed)?;
        Ok(())
    }

    /// Lists the accounts' object paths.
    async fn list_accounts(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<Vec<OwnedObjectPath>> {
        access::check(&header)?;
        let accounts = self.accounts.lock().await;
        let mut paths: Vec<(i64, OwnedObjectPath)> = accounts
            .by_jid
            .values()
            .map(|a| (a.id, a.path.clone()))
            .collect();
        paths.sort_by_key(|(id, _)| *id);
        Ok(paths.into_iter().map(|(_, p)| p).collect())
    }

    /// Disconnects everything and exits.
    async fn quit(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let _ = self.quit.send(true);
        Ok(())
    }

    /// Whether the daemon starts when the user logs in (with no window).
    /// Always false while RunInBackground is. Writable.
    #[zbus(property)]
    fn start_at_login(&self) -> bool {
        crate::settings::get().is_some_and(|s| s.start_at_login())
    }

    #[zbus(property)]
    async fn set_start_at_login(
        &mut self,
        on: bool,
        #[zbus(connection)] connection: &zbus::Connection,
    ) {
        if let Some(s) = crate::settings::get() {
            s.set_start_at_login(on).await;
            crate::background::apply(connection.clone(), s.start_at_login()).await;
        }
    }

    /// Whether the daemon keeps running, receiving messages and showing
    /// notifications, after the window closes. Writable.
    #[zbus(property)]
    fn run_in_background(&self) -> bool {
        crate::settings::get().is_none_or(|s| s.run_in_background())
    }

    #[zbus(property)]
    async fn set_run_in_background(
        &mut self,
        on: bool,
        #[zbus(connection)] connection: &zbus::Connection,
    ) {
        if let Some(s) = crate::settings::get() {
            s.set_run_in_background(on).await;
            crate::background::apply(connection.clone(), s.start_at_login()).await;
        }
    }

    /// The default notification sound: "default" (the desktop's),
    /// "silent" or "custom" (set with SetNotificationSound). Writable with
    /// "default" or "silent".
    #[zbus(property)]
    fn notification_sound(&self) -> String {
        crate::settings::get()
            .map(|s| s.sound())
            .unwrap_or_else(|| "default".into())
    }

    #[zbus(property)]
    async fn set_notification_sound(&mut self, value: String) -> zbus::fdo::Result<()> {
        if !matches!(value.as_str(), "default" | "silent") {
            return Err(zbus::fdo::Error::InvalidArgs(
                "default or silent; a custom sound is set with SetCustomSound".into(),
            ));
        }
        if let Some(s) = crate::settings::get() {
            s.set_sound(&value).await;
        }
        Ok(())
    }

    /// Plays this sound file (at most 2 MiB) for notifications by default.
    async fn set_custom_sound(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        file: zbus::zvariant::OwnedFd,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        crate::settings::save_sound("default".into(), file.into())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
        if let Some(s) = crate::settings::get() {
            s.set_sound("custom").await;
        }
        let _ = self.notification_sound_changed(&emitter).await;
        Ok(())
    }

    /// Version of this interface. A breaking change creates `net.dunevent.Piopio.Daemon.v2`.
    #[zbus(property)]
    fn api_version(&self) -> u32 {
        DAEMON_API_VERSION
    }
}
