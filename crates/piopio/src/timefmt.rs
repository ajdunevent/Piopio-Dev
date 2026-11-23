//! Times as people read them: following the desktop's 12 or 24 hour
//! setting (GNOME's clock-format, read through the Settings portal inside
//! Flatpak), and shorter the further back they are.

use std::cell::{Cell, RefCell};

use adw::glib;
use adw::prelude::*;
use futures_util::StreamExt;

use crate::client;

thread_local! {
    static TWELVE_HOUR: Cell<bool> = const { Cell::new(false) };
    static LISTENERS: RefCell<Vec<Box<dyn Fn()>>> = RefCell::default();
}

const NAMESPACE: &str = "org.gnome.desktop.interface";
const KEY: &str = "clock-format";

/// Calls `f` whenever the clock format changes (and a minute passes, so
/// "today" and "this week" stay true).
pub fn on_change(f: impl Fn() + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

fn changed() {
    LISTENERS.with(|l| {
        for f in l.borrow().iter() {
            f();
        }
    });
}

/// Learns the clock format and follows changes to it. Call once.
pub fn init() {
    // Outside Flatpak the setting can be read directly. Inside, GSettings
    // only sees the sandbox's own copy, so ask the portal.
    let sandboxed = std::path::Path::new("/.flatpak-info").exists();
    if let Some(schema) = gtk_settings_schema().filter(|_| !sandboxed) {
        let settings =
            adw::gio::Settings::new_full(&schema, None::<&adw::gio::SettingsBackend>, None);
        TWELVE_HOUR.with(|t| t.set(settings.string(KEY) == "12h"));
        settings.connect_changed(Some(KEY), |s, _| {
            TWELVE_HOUR.with(|t| t.set(s.string(KEY) == "12h"));
            changed();
        });
        // Kept alive for its signal.
        std::mem::forget(settings);
    } else {
        follow_portal();
    }
    glib::timeout_add_seconds_local(60, || {
        changed();
        glib::ControlFlow::Continue
    });
}

fn gtk_settings_schema() -> Option<adw::gio::SettingsSchema> {
    adw::gio::SettingsSchemaSource::default()?
        .lookup(NAMESPACE, true)
        .filter(|s| s.has_key(KEY))
}

/// Reads clock-format from the Settings portal and listens for changes.
fn follow_portal() {
    let values = client::forward(|| async {
        let bus = client::bus().await.ok()?;
        let proxy = zbus::Proxy::new(
            &bus,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
        )
        .await
        .ok()?;
        let first: Option<String> = proxy
            .call::<_, _, zbus::zvariant::OwnedValue>("ReadOne", &(NAMESPACE, KEY))
            .await
            .ok()
            .and_then(|v| String::try_from(v).ok());
        let later = proxy.receive_signal("SettingChanged").await.ok()?;
        Some(
            futures_util::stream::iter(first).chain(later.filter_map(|m| async move {
                let (ns, key, value): (String, String, zbus::zvariant::OwnedValue) =
                    m.body().deserialize().ok()?;
                (ns == NAMESPACE && key == KEY)
                    .then(|| String::try_from(value).ok())
                    .flatten()
            })),
        )
    });
    glib::spawn_future_local(async move {
        while let Ok(format) = values.recv().await {
            TWELVE_HOUR.with(|t| t.set(format == "12h"));
            changed();
        }
    });
}

fn local(ms: i64) -> Option<(glib::DateTime, glib::DateTime)> {
    let when = glib::DateTime::from_unix_local(ms.div_euclid(1000)).ok()?;
    let now = glib::DateTime::now_local().ok()?;
    Some((when, now))
}

/// The time of day, as the desktop shows it.
fn clock(when: &glib::DateTime) -> String {
    let twelve = TWELVE_HOUR.with(Cell::get);
    let format = if twelve { "%l:%M %p" } else { "%H:%M" };
    when.format(format)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// Days between two local dates (0 = same day).
fn days_between(when: &glib::DateTime, now: &glib::DateTime) -> i64 {
    let date = |d: &glib::DateTime| {
        let (y, m, day) = d.ymd();
        glib::DateTime::from_local(y, m, day, 12, 0, 0.0).ok()
    };
    match (date(when), date(now)) {
        (Some(a), Some(b)) => b.difference(&a).as_days(),
        _ => i64::MAX,
    }
}

/// For the conversation list: the time today, the weekday within the last
/// week, the date before that.
pub fn short(ms: i64) -> String {
    // Never active (a contact with no history yet).
    if ms <= 0 {
        return String::new();
    }
    let Some((when, now)) = local(ms) else {
        return String::new();
    };
    let days = days_between(&when, &now);
    let text = match days {
        0 => return clock(&when),
        1..=6 => when.format("%a"),
        _ => when.format("%x"),
    };
    text.map(|s| s.to_string()).unwrap_or_default()
}

/// For a message: the time, with the day when it isn't today.
pub fn message(ms: i64) -> String {
    let Some((when, now)) = local(ms) else {
        return String::new();
    };
    let day = match days_between(&when, &now) {
        0 => return clock(&when),
        1..=6 => when.format("%a"),
        _ => when.format("%x"),
    };
    format!(
        "{} {}",
        day.map(|s| s.to_string()).unwrap_or_default(),
        clock(&when)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(dt: &glib::DateTime) -> i64 {
        dt.to_unix() * 1000
    }

    #[test]
    fn today_follows_the_clock_format() {
        let now = glib::DateTime::now_local().unwrap();
        let noonish =
            glib::DateTime::from_local(now.year(), now.month(), now.day_of_month(), 15, 5, 0.0)
                .unwrap();
        TWELVE_HOUR.with(|t| t.set(false));
        assert_eq!(short(ms(&noonish)), "15:05");
        TWELVE_HOUR.with(|t| t.set(true));
        let twelve = short(ms(&noonish));
        assert!(twelve.starts_with("3:05"), "{twelve}");
        TWELVE_HOUR.with(|t| t.set(false));
    }

    #[test]
    fn older_days_get_shorter() {
        let now = glib::DateTime::now_local().unwrap();
        let three_days = now.add_days(-3).unwrap();
        assert_eq!(short(ms(&three_days)), three_days.format("%a").unwrap());
        let last_month = now.add_days(-40).unwrap();
        assert_eq!(short(ms(&last_month)), last_month.format("%x").unwrap());
        assert!(message(ms(&three_days)).starts_with(three_days.format("%a").unwrap().as_str()));
    }
}
