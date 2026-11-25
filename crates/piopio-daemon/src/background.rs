//! Running in the background (Background portal).
//!
//! Inside Flatpak, the daemon asks to run in the background and, when Start
//! at Login is on, to start when the user logs in. The portal then writes
//! (or removes) an autostart entry that launches `piopio-daemon` directly,
//! with no window. Outside Flatpak there is no portal to ask; starting at
//! login is up to how Piopio was installed.
//!
//! The daemon also keeps a one-line status there (portal version 2), which
//! GNOME shows in its Background Apps list: connecting, can't connect, or
//! the number of unread messages.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

/// Asks the portal again with the current Start at Login setting.
pub async fn apply(bus: zbus::Connection, autostart: bool) {
    if !std::path::Path::new("/.flatpak-info").exists() {
        return;
    }
    if let Err(e) = ask(&bus, autostart).await {
        tracing::info!("background permission not granted: {e}");
    }
}

async fn ask(bus: &zbus::Connection, autostart: bool) -> anyhow::Result<()> {
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
    options.insert("autostart", Value::from(autostart));
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

/// The status line, from each account's (enabled, state) and the unread
/// count. Under 96 characters, as the portal asks.
fn status_line(accounts: &[(bool, crate::account::State)], unread: u64) -> String {
    use crate::account::State;
    let enabled: Vec<State> = accounts
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, s)| *s)
        .collect();
    let line = if accounts.is_empty() {
        "No accounts".to_owned()
    } else if enabled.is_empty() {
        "Accounts turned off".to_owned()
    } else {
        let connected = enabled.iter().filter(|s| **s == State::Connected).count();
        let failing = enabled.iter().filter(|s| **s == State::Error).count();
        let unread = match unread {
            0 => None,
            1 => Some("1 unread message".to_owned()),
            n => Some(format!("{n} unread messages")),
        };
        let trouble = match (connected, failing) {
            (0, 0) => Some("Connecting…".to_owned()),
            (0, _) => Some("Can’t connect".to_owned()),
            (_, 0) => None,
            (_, 1) => Some("1 account can’t connect".to_owned()),
            (_, n) => Some(format!("{n} accounts can’t connect")),
        };
        match (unread, trouble) {
            (None, None) => "Connected".to_owned(),
            (Some(u), None) => u,
            (None, Some(t)) => t,
            (Some(u), Some(t)) => format!("{u} · {t}"),
        }
    };
    line.chars().take(95).collect()
}

/// Keeps the Background Apps status current: checked every few seconds,
/// sent only when it changes. Inside Flatpak only.
pub async fn keep_status(
    bus: zbus::Connection,
    store: crate::store::Store,
    accounts: std::sync::Arc<tokio::sync::Mutex<crate::daemon::Accounts>>,
) {
    // Tests stand in a portal of their own.
    let testing = std::env::var("PIOPIO_TEST_BACKGROUND_STATUS").as_deref() == Ok("1");
    if !testing && !std::path::Path::new("/.flatpak-info").exists() {
        return;
    }
    let mut shown = String::new();
    loop {
        let paths = accounts.lock().await.paths();
        let mut states = Vec::new();
        for path in paths {
            if let Ok(iface) = bus
                .object_server()
                .interface::<_, crate::account::AccountObject>(&path)
                .await
            {
                states.push(iface.get().await.status());
            }
        }
        let unread = store.total_unread().await.unwrap_or(0);
        let line = status_line(&states, unread);
        if line != shown {
            let mut options: HashMap<&str, Value<'_>> = HashMap::new();
            options.insert("message", Value::from(line.as_str()));
            match bus
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.Background"),
                    "SetStatus",
                    &(options,),
                )
                .await
            {
                Ok(_) => shown = line,
                // An older portal without SetStatus: stop asking.
                Err(zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
                {
                    tracing::debug!("the background portal has no SetStatus");
                    return;
                }
                // Not there yet, or busy: next time.
                Err(e) => tracing::debug!("background status not set: {e}"),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::status_line;
    use crate::account::State;

    #[test]
    fn status_lines() {
        assert_eq!(status_line(&[], 0), "No accounts");
        assert_eq!(
            status_line(&[(false, State::Disconnected)], 0),
            "Accounts turned off"
        );
        assert_eq!(status_line(&[(true, State::Connecting)], 0), "Connecting…");
        assert_eq!(
            status_line(&[(true, State::Error)], 4),
            "4 unread messages · Can’t connect"
        );
        assert_eq!(status_line(&[(true, State::Connected)], 0), "Connected");
        assert_eq!(
            status_line(&[(true, State::Connected)], 1),
            "1 unread message"
        );
        assert_eq!(
            status_line(&[(true, State::Connected), (true, State::Error)], 2),
            "2 unread messages · 1 account can’t connect"
        );
    }
}
