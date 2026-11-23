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

/// Client side of `net.dunevent.Piopio.Daemon`.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Daemon",
    default_service = "net.dunevent.Piopio.Daemon",
    default_path = "/net/dunevent/Piopio"
)]
pub trait Daemon {
    /// Disconnect everything and exit.
    fn quit(&self) -> zbus::Result<()>;

    /// Version of this interface.
    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}
