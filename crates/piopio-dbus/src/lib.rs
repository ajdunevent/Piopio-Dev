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

/// Version of the Roster interface implemented by this build.
pub const ROSTER_API_VERSION: u32 = 1;

/// Version of the Conversation interface implemented by this build.
pub const CONVERSATION_API_VERSION: u32 = 1;

/// Version of the Omemo interface implemented by this build.
pub const OMEMO_API_VERSION: u32 = 1;

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

    /// Disconnect an account and delete it with all its local data.
    fn remove_account(&self, account: &zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    /// The accounts' object paths, oldest first.
    fn list_accounts(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;

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

    #[zbus(property)]
    fn set_enabled(&self, enabled: bool) -> zbus::Result<()>;

    /// Show (available, away, chat, dnd or xa) and status text.
    #[zbus(property)]
    fn presence(&self) -> zbus::Result<(String, String)>;

    #[zbus(property)]
    fn set_presence(&self, presence: (&str, &str)) -> zbus::Result<()>;

    /// One of disconnected, connecting, connected or error.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    /// Why the account is in the error state, or why the last attempt failed.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;

    /// Opens (or creates) the one-to-one conversation with `jid`.
    fn start_chat(&self, jid: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Changes the password on the server, then uses it.
    fn change_password(&self, new: &str) -> zbus::Result<()>;
}

/// A dictionary as passed over D-Bus (`a{sv}`).
pub type Dict = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;

/// Client side of `net.dunevent.Piopio.Roster`, on each account object.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Roster",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Roster {
    /// jid, name, groups, subscription, ask, presence, status, pending-in.
    fn get_contacts(&self) -> zbus::Result<Vec<Dict>>;
    fn add_contact(&self, jid: &str, name: &str, groups: &[&str]) -> zbus::Result<()>;
    fn remove_contact(&self, jid: &str) -> zbus::Result<()>;
    fn rename_contact(&self, jid: &str, name: &str) -> zbus::Result<()>;
    fn approve_subscription(&self, jid: &str) -> zbus::Result<()>;
    fn deny_subscription(&self, jid: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn contact_changed(&self, jid: &str, changes: Dict) -> zbus::Result<()>;

    #[zbus(signal)]
    fn subscription_request(&self, jid: &str) -> zbus::Result<()>;
}

/// Client side of `net.dunevent.Piopio.Conversation`.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Conversation",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Conversation {
    #[zbus(property)]
    fn peer(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn kind(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn unread(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn last_activity(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn pinned(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn muted(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn encryption(&self) -> zbus::Result<String>;

    /// One page of history, newest last, older than `before_id` ("" = newest).
    fn get_messages(&self, before_id: &str, limit: u32) -> zbus::Result<Vec<Dict>>;

    /// Sends a message; returns its id.
    fn send(
        &self,
        body: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<String>;

    /// Clears Unread and tells the contact we have read up to here.
    fn mark_read(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn message_added(&self, message: Dict) -> zbus::Result<()>;

    #[zbus(signal)]
    fn message_changed(&self, id: &str, changes: Dict) -> zbus::Result<()>;
}

/// Client side of `net.dunevent.Piopio.Omemo`, on each account object.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Omemo",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Omemo {
    /// This device's OMEMO device id.
    #[zbus(property)]
    fn device_id(&self) -> zbus::Result<u32>;

    /// This device's fingerprint, as contacts see it.
    #[zbus(property)]
    fn fingerprint(&self) -> zbus::Result<String>;

    /// True once this device and its keys are published for this login.
    #[zbus(property)]
    fn published(&self) -> zbus::Result<bool>;

    /// The devices of a JID (the account's own JID for its other devices).
    /// Keys: device-id (u), active (b), trust (s: blind, verified,
    /// untrusted or undecided) and fingerprint (s, empty until known).
    fn get_devices(&self, jid: &str) -> zbus::Result<Vec<Dict>>;

    /// Sets a device's trust.
    fn set_trust(&self, jid: &str, device_id: u32, trust: &str) -> zbus::Result<()>;

    /// The devices of a JID, or their trust, changed.
    #[zbus(signal)]
    fn devices_changed(&self, jid: &str) -> zbus::Result<()>;

    /// Version of this interface.
    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}
