//! `net.dunevent.Piopio.Daemon` on the root object.

use piopio_dbus::DAEMON_API_VERSION;
use tokio::sync::watch;
use zbus::message::Header;

use crate::access;

pub struct Daemon {
    quit: watch::Sender<bool>,
}

impl Daemon {
    pub fn new(quit: watch::Sender<bool>) -> Self {
        Self { quit }
    }
}

#[zbus::interface(name = "net.dunevent.Piopio.Daemon")]
impl Daemon {
    /// Disconnect everything and exit.
    async fn quit(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        // Accounts and connections arrive in milestone 1; nothing to disconnect yet.
        let _ = self.quit.send(true);
        Ok(())
    }

    /// Version of this interface. A breaking change creates `net.dunevent.Piopio.Daemon.v2`.
    #[zbus(property)]
    fn api_version(&self) -> u32 {
        DAEMON_API_VERSION
    }
}
