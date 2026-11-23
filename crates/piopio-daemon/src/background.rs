//! Running in the background (Background portal).
//!
//! Inside Flatpak, the daemon asks once to run in the background and to start
//! when the user logs in. The portal then writes an autostart entry that
//! launches `piopio-daemon` directly, with no window. Outside Flatpak there is
//! no portal to ask, and nothing to do.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

pub async fn request(bus: zbus::Connection) {
    if !std::path::Path::new("/.flatpak-info").exists() {
        return;
    }
    if let Err(e) = ask(&bus).await {
        tracing::info!("background permission not granted: {e}");
    }
}

async fn ask(bus: &zbus::Connection) -> anyhow::Result<()> {
    let token = format!("piopio{}", std::process::id());
    let sender = bus
        .unique_name()
        .map(|n| n.trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default();
    let handle = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

    // Subscribe before calling, so the answer can't slip past.
    let request = zbus::Proxy::new(
        bus,
        "org.freedesktop.portal.Desktop",
        handle.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut responses = request.receive_signal("Response").await?;

    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert(
        "reason",
        Value::from("Piopio receives messages and shows notifications while its window is closed."),
    );
    options.insert("autostart", Value::from(true));
    options.insert("commandline", Value::from(vec!["piopio-daemon"]));
    options.insert("dbus-activatable", Value::from(false));
    bus.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.Background"),
        "RequestBackground",
        &("", options),
    )
    .await?;

    if let Some(message) = responses.next().await {
        let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
        let background = results
            .get("background")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        let autostart = results
            .get("autostart")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        tracing::info!(code, background, autostart, "background portal answered");
    }
    Ok(())
}
