//! Names, object paths and client proxies shared by the Piopio daemon and UI.
//!
//! The daemon serves the interfaces; the UI talks to them through the proxies
//! defined here. The API grows milestone by milestone, following the project
//! spec. Every interface carries an `ApiVersion` property, and a breaking change
//! creates a new interface name ending in `.v2`.

/// Application ID of the UI. Also its D-Bus name, icon name and desktop file name.
pub const APP_ID: &str = "net.dunevent.Piopio";

/// Well-known bus name owned by the daemon.
pub const DAEMON_BUS_NAME: &str = "net.dunevent.Piopio.Daemon";

/// Root object: carries the Daemon and Files interfaces and the ObjectManager.
pub const ROOT_PATH: &str = "/net/dunevent/Piopio";

/// Name of the Daemon interface on [`ROOT_PATH`].
pub const DAEMON_INTERFACE: &str = "net.dunevent.Piopio.Daemon";

/// Version of the Daemon interface implemented by this build.
pub const DAEMON_API_VERSION: u32 = 1;

/// Version of the Account interface implemented by this build.
pub const ACCOUNT_API_VERSION: u32 = 1;

/// Accounts live at `ACCOUNT_PATH_PREFIX/<id>`.
pub const ACCOUNT_PATH_PREFIX: &str = "/net/dunevent/Piopio/account";

/// Client side of `net.dunevent.Piopio.Daemon`.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Daemon",
    default_service = "net.dunevent.Piopio.Daemon",
    default_path = "/net/dunevent/Piopio"
)]
pub trait Daemon {
    /// Add an account and start connecting. Returns the account's object path.
    fn add_account(
        &self,
        jid: &str,
        password: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Disconnect an account and forget it.
    fn remove_account(&self, account: &zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    /// Disconnect everything and exit.
    fn quit(&self) -> zbus::Result<()>;

    /// Version of this interface.
    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}

/// Client side of `net.dunevent.Piopio.Account`, one object per account.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Account",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Account {
    #[zbus(property)]
    fn jid(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn enabled(&self) -> zbus::Result<bool>;

    /// One of disconnected, connecting, connected or error.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    /// Why the account is in the error state, or why the last attempt failed.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}
