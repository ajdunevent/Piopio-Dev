//! Whether a Piopio window is open, and what follows from that.
//!
//! The UI owns the bus name `net.dunevent.Piopio` (its GApplication ID) for
//! exactly as long as its window exists, so the daemon watches that name
//! instead of asking the UI to report in. With no window, new messages produce
//! desktop notifications.
//!
//! Telling servers we are inactive (XEP-0352) belongs here too, but CSI is a
//! nonza and tokio-xmpp 6's `StanzaStream` can only send stanzas; it follows
//! once the library can.

use std::sync::OnceLock;

use futures_util::StreamExt;
use tokio::sync::watch;

static UI_PRESENT: OnceLock<watch::Receiver<bool>> = OnceLock::new();

/// Starts watching for the UI's bus name.
pub async fn watch_ui(bus: &zbus::Connection) -> zbus::Result<()> {
    let dbus = zbus::fdo::DBusProxy::new(bus).await?;
    let name: zbus::names::BusName<'static> = piopio_dbus::APP_ID.try_into().expect("valid name");
    let present = dbus.name_has_owner(name.clone()).await.unwrap_or(false);
    let (tx, rx) = watch::channel(present);
    let _ = UI_PRESENT.set(rx);
    let mut changes = dbus
        .receive_name_owner_changed_with_args(&[(0, piopio_dbus::APP_ID)])
        .await?;
    tokio::spawn(async move {
        while let Some(change) = changes.next().await {
            if let Ok(args) = change.args() {
                let present = args.new_owner().is_some();
                tracing::debug!(present, "UI presence changed");
                tx.send_replace(present);
            }
        }
    });
    Ok(())
}

/// True while a Piopio window is open.
pub fn ui_present() -> bool {
    UI_PRESENT.get().is_some_and(|rx| *rx.borrow())
}
