//! Whether a Piopio window is open, and what follows from that.
//!
//! The UI owns the bus name `net.dunevent.Piopio` (its GApplication ID) for
//! exactly as long as its window exists, so the daemon watches that name
//! instead of asking the UI to report in. With no window, servers are told
//! we're inactive (XEP-0352, see `csi`).
//!
//! The window does report which conversation is in front of the user
//! (focused, its chat on screen): new messages there need no notification;
//! everything else, window or not, gets one.

use std::sync::{Mutex, OnceLock};

use futures_util::StreamExt;
use tokio::sync::watch;

static UI_PRESENT: OnceLock<watch::Receiver<bool>> = OnceLock::new();

/// The conversation (object path) the user is reading, if any.
static VIEWING: Mutex<String> = Mutex::new(String::new());

/// The window says which conversation is in front of the user ("" for
/// none: unfocused, the list shown on a narrow window, nothing open).
pub fn set_viewing(conversation: String) {
    *VIEWING.lock().expect("lock") = conversation;
}

/// Whether the user is reading `conversation` right now.
pub fn viewing(conversation: &str) -> bool {
    ui_present() && *VIEWING.lock().expect("lock") == conversation
}

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
                if !present {
                    set_viewing(String::new());
                }
                tx.send_replace(present);
            }
        }
    });
    Ok(())
}

/// Follows whether a window is open.
pub fn subscribe() -> Option<watch::Receiver<bool>> {
    UI_PRESENT.get().cloned()
}

/// True while a Piopio window is open.
pub fn ui_present() -> bool {
    UI_PRESENT.get().is_some_and(|rx| *rx.borrow())
}
