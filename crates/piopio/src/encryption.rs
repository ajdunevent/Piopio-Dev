//! Encryption: a conversation's page in Conversation Details (whether to
//! encrypt, and the contact's devices with their trust), and the
//! Encryption dialog from the main menu (this device's fingerprint and
//! your other devices, for each account).
//!
//! Verifying a device: compare its fingerprint with the one the other
//! device shows (in person or over a call), then switch it to trusted.

use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use piopio_dbus::{ConversationProxy, Dict, OmemoProxy};

use crate::client::{self, run};
use crate::window::Window;

fn text(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

/// The account's object path, from one of its conversations' paths.
fn account_path(conversation: &str) -> String {
    conversation
        .rsplit_once("/conversation/")
        .map(|(account, _)| account.to_owned())
        .unwrap_or_default()
}

fn trust_text(trust: &str, active: bool) -> &'static str {
    match (trust, active) {
        (_, false) => "No longer in use",
        ("verified", _) => "Verified",
        ("blind", _) => "Trusted automatically, not verified",
        ("untrusted", _) => "Not trusted: messages are not encrypted for it",
        _ => "New device: not trusted until you decide",
    }
}

/// A fingerprint in two lines, as people compare them.
fn fingerprint_markup(fingerprint: &str) -> String {
    let groups: Vec<&str> = fingerprint.split(' ').collect();
    let half = groups.len().div_ceil(2);
    glib::markup_escape_text(&format!(
        "{}\n{}",
        groups[..half].join(" "),
        groups[half..].join(" ")
    ))
    .to_string()
}

/// The page, filled in as the devices load.
pub fn page(window: &Rc<Window>, conversation: ConversationProxy<'static>) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let view = adw::ToolbarView::builder()
        .top_bar_style(adw::ToolbarStyle::Raised)
        .content(&page)
        .build();
    view.add_top_bar(&adw::HeaderBar::new());
    let navigation = adw::NavigationPage::new(&view, "Encryption");

    let settings = adw::PreferencesGroup::new();
    let wanted = adw::SwitchRow::builder().title("Encrypt Messages").build();
    settings.add(&wanted);
    page.add(&settings);

    let theirs = adw::PreferencesGroup::builder()
        .title("Contact’s Devices")
        .description("To verify a device, compare its fingerprint with the one your contact’s app shows (in person or over a call), then switch it on")
        .build();
    page.add(&theirs);

    let path = account_path(conversation.inner().path().as_str());
    let weak = Rc::downgrade(window);
    glib::spawn_future_local(async move {
        let Ok(bus) = client::bus().await else { return };
        let conv = conversation.clone();
        let loaded = run(async move {
            let omemo = OmemoProxy::builder(&bus)
                .path(path)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            let peer = conv.peer().await?;
            let wanted = conv.encryption_wanted().await?;
            let encryption = conv.encryption().await?;
            Ok::<_, zbus::Error>((omemo, peer, wanted, encryption))
        })
        .await;
        let (omemo, peer, is_wanted, encryption) = match loaded {
            Ok(v) => v,
            Err(e) => {
                if let Some(w) = weak.upgrade() {
                    w.toast(&client::explain(&e));
                }
                return;
            }
        };
        wanted.set_active(is_wanted);
        wanted.set_subtitle(match (is_wanted, encryption.as_str()) {
            (true, "omemo") => "Messages are end-to-end encrypted with OMEMO",
            (true, "omemo2") => "Messages are end-to-end encrypted with OMEMO 2",
            (true, _) => "This contact’s apps don’t support OMEMO, so messages are not encrypted",
            (false, _) => "Messages are not encrypted",
        });
        let conv = conversation.clone();
        wanted.connect_active_notify(move |row| {
            let (conv, on) = (conv.clone(), row.is_active());
            glib::spawn_future_local(async move {
                let _ = run(async move { conv.set_encryption_wanted(on).await }).await;
            });
        });

        theirs.set_title(&glib::markup_escape_text(&format!("{peer}’s Devices")));
        fill(&theirs, &omemo, &peer).await;
    });
    navigation
}

/// The Encryption dialog: for each account, this device's fingerprint
/// and the account's other devices with their trust.
pub fn dialog(window: &Rc<Window>) {
    // Sized up front: the devices arrive after it opens.
    let dialog = adw::PreferencesDialog::builder()
        .title("Encryption")
        .search_enabled(false)
        .content_height(560)
        .build();
    let page = adw::PreferencesPage::new();
    dialog.add(&page);
    let accounts = window.accounts.borrow().clone();
    let several = accounts.len() > 1;
    for account in accounts {
        let own = adw::PreferencesGroup::builder()
            .title(if several {
                glib::markup_escape_text(&format!("This Device ({})", account.jid)).to_string()
            } else {
                "This Device".to_owned()
            })
            .description("Your contacts see this fingerprint for you; they can compare it with this one to verify it")
            .build();
        let mine = adw::PreferencesGroup::builder()
            .title("Your Other Devices")
            .description("To verify one, compare its fingerprint with the one it shows for itself, then switch it on")
            .build();
        page.add(&own);
        page.add(&mine);
        let (path, jid, w) = (
            account.path.clone(),
            account.jid.clone(),
            Rc::downgrade(window),
        );
        glib::spawn_future_local(async move {
            let Ok(bus) = client::bus().await else { return };
            let loaded = run(async move {
                let omemo = OmemoProxy::builder(&bus)
                    .path(path)?
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await?;
                let fingerprint = omemo.fingerprint().await?;
                Ok::<_, zbus::Error>((omemo, fingerprint))
            })
            .await;
            let (omemo, fingerprint) = match loaded {
                Ok(v) => v,
                Err(e) => {
                    if let Some(w) = w.upgrade() {
                        w.toast(&client::explain(&e));
                    }
                    return;
                }
            };
            own.add(
                &adw::ActionRow::builder()
                    .title(fingerprint_markup(&fingerprint))
                    .css_classes(["fingerprint"])
                    .build(),
            );
            fill(&mine, &omemo, &jid).await;
        });
    }
    dialog.present(Some(&window.window));
}

/// Adds a row per device of `jid` to `group`, with a switch for trust.
async fn fill(group: &adw::PreferencesGroup, omemo: &OmemoProxy<'static>, jid: &str) {
    let (o, j) = (omemo.clone(), jid.to_owned());
    let devices = run(async move { o.get_devices(&j).await })
        .await
        .unwrap_or_default();
    if devices.is_empty() {
        group.add(
            &adw::ActionRow::builder()
                .title("No devices with OMEMO")
                .css_classes(["dim-label"])
                .build(),
        );
        return;
    }
    for d in devices {
        let id = d
            .get("device-id")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let active = d
            .get("active")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(true);
        let trust = text(&d, "trust");
        let fingerprint = text(&d, "fingerprint");
        let row = adw::ActionRow::builder()
            .title(if fingerprint.is_empty() {
                format!("Device {id}")
            } else {
                fingerprint_markup(&fingerprint)
            })
            .subtitle(if text(&d, "protocol") == "omemo2" {
                format!("{} · OMEMO 2 only", trust_text(&trust, active))
            } else {
                trust_text(&trust, active).to_owned()
            })
            .css_classes(["fingerprint"])
            .build();
        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Trusted")
            .active(trust == "verified" || trust == "blind")
            .sensitive(!fingerprint.is_empty())
            .build();
        row.add_suffix(&switch);
        let (o, j, r) = (omemo.clone(), jid.to_owned(), row.clone());
        switch.connect_state_set(move |_, on| {
            let (o, j, r) = (o.clone(), j.clone(), r.clone());
            let trust = if on { "verified" } else { "untrusted" };
            glib::spawn_future_local(async move {
                if run(async move { o.set_trust(&j, id, trust).await })
                    .await
                    .is_ok()
                {
                    r.set_subtitle(trust_text(trust, active));
                }
            });
            glib::Propagation::Proceed
        });
        group.add(&row);
    }
}
