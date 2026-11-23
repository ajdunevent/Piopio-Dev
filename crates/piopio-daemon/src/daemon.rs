//! `net.dunevent.Piopio.Daemon` on the root object.

use std::collections::HashMap;
use std::sync::Arc;

use piopio_dbus::{ACCOUNT_PATH_PREFIX, DAEMON_API_VERSION};
use tokio::sync::{Mutex, watch};
use tokio_xmpp::jid::BareJid;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use crate::access;
use crate::account::Account;

/// Every account the daemon is running, by bare JID.
#[derive(Default)]
pub struct Accounts {
    by_jid: HashMap<BareJid, Account>,
    next_id: u64,
}

impl Accounts {
    /// Disconnect every account cleanly.
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

#[zbus::interface(name = "net.dunevent.Piopio.Daemon")]
impl Daemon {
    /// Add an account and start connecting. Returns the account's object path.
    ///
    /// Credentials are only held in memory for now; storing them through the
    /// Secret portal comes with the encrypted database.
    async fn add_account(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
        jid: &str,
        password: &str,
        _options: HashMap<&str, Value<'_>>,
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

        let mut accounts = self.accounts.lock().await;
        if accounts.by_jid.contains_key(&jid) {
            return Err(zbus::fdo::Error::Failed(format!("{jid} is already added")));
        }
        accounts.next_id += 1;
        let path = OwnedObjectPath::try_from(format!("{ACCOUNT_PATH_PREFIX}/{}", accounts.next_id))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        let account = Account::start(connection, path.clone(), jid.clone(), password.to_owned())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        accounts.by_jid.insert(jid, account);
        Ok(path)
    }

    /// Disconnect an account and forget it.
    async fn remove_account(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
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
        let path = removed.path.clone();
        removed.stop().await;
        connection
            .object_server()
            .remove::<crate::account::AccountObject, _>(&path)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    /// Disconnect everything and exit.
    async fn quit(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let _ = self.quit.send(true);
        Ok(())
    }

    /// Version of this interface. A breaking change creates `net.dunevent.Piopio.Daemon.v2`.
    #[zbus(property)]
    fn api_version(&self) -> u32 {
        DAEMON_API_VERSION
    }
}
