//! Encryption: a conversation's page in Conversation Details (whether to
//! encrypt, and the contact's devices with their trust), and each
//! account's Encryption page in Accounts (this device's fingerprint and
//! your other devices).
//!
//! Verifying a device: compare its fingerprint with the one the other
//! device shows (in person or over a call), then switch it to trusted.

use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use piopio_dbus::{ConversationProxy, Dict, OmemoProxy};

use crate::client::{self, run};
use crate::window::{AccountInfo, Window};

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
        ("blind", _) => "Trusted, not verified",
        ("untrusted", _) => "Not trusted: messages aren’t encrypted for it",
        _ => "New: not trusted until you verify or trust it",
    }
}

/// A fingerprint in two lines, as people compare them.
fn fingerprint_markup(fingerprint: &str) -> String {
    let groups = fingerprint.split(' ').count();
    fingerprint_lines(fingerprint, groups.div_ceil(2).max(1))
}

/// A fingerprint with `per_line` groups on each line: two keeps it narrow
/// enough for a phone.
fn fingerprint_lines(fingerprint: &str, per_line: usize) -> String {
    let groups: Vec<&str> = fingerprint.split(' ').collect();
    let lines: Vec<String> = groups.chunks(per_line).map(|c| c.join(" ")).collect();
    glib::markup_escape_text(&lines.join("\n")).to_string()
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
        .description("To verify a device, choose Verify… and compare the fingerprint with the one your contact’s app shows, in person or over a call")
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
        fill(&theirs, &omemo, &peer, false).await;
    });
    navigation
}

/// An account's Encryption page, opened from its row in Accounts: this
/// device's fingerprint and the account's other devices. Each account has
/// its own keys.
pub fn account_page(window: &Rc<Window>, account: &AccountInfo) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    let own = adw::PreferencesGroup::builder()
        .title("This Device")
        .description("Your contacts see this fingerprint for you; they can compare it with this one to verify it")
        .build();
    let mine = adw::PreferencesGroup::builder()
        .title("Your Other Devices")
        .description("To verify one, choose Verify… and compare the fingerprint with the one that device shows for itself")
        .build();
    page.add(&own);
    page.add(&mine);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let nav = adw::NavigationPage::builder()
        .title("Encryption")
        .child(&view)
        .build();
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
        fill(&mine, &omemo, &jid, true).await;
    });
    nav
}

/// Lists `jid`'s devices in `group`, each with Verify… and a menu to
/// trust it without verifying or not trust it, and keeps the list current
/// (a device's key arrives, or its trust changes).
/// `yours`: the devices are the account's own.
async fn fill(group: &adw::PreferencesGroup, omemo: &OmemoProxy<'static>, jid: &str, yours: bool) {
    let rows: Rc<std::cell::RefCell<Vec<adw::ActionRow>>> = Rc::default();
    refill(group, omemo, jid, yours, &rows).await;
    let changes = {
        let omemo = omemo.clone();
        client::forward(move || async move { omemo.receive_devices_changed().await.ok() })
    };
    let (group, omemo, jid) = (group.downgrade(), omemo.clone(), jid.to_owned());
    glib::spawn_future_local(async move {
        while let Ok(signal) = changes.recv().await {
            let Some(group) = group.upgrade() else { break };
            if signal.args().is_ok_and(|a| a.jid() == &jid) {
                refill(&group, &omemo, &jid, yours, &rows).await;
            }
        }
    });
}

async fn refill(
    group: &adw::PreferencesGroup,
    omemo: &OmemoProxy<'static>,
    jid: &str,
    yours: bool,
    rows: &Rc<std::cell::RefCell<Vec<adw::ActionRow>>>,
) {
    let (o, j) = (omemo.clone(), jid.to_owned());
    let devices = run(async move { o.get_devices(&j).await })
        .await
        .unwrap_or_default();
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    if devices.is_empty() {
        let row = adw::ActionRow::builder()
            .title(if yours {
                "No other devices"
            } else {
                "No devices with OMEMO"
            })
            .css_classes(["dim-label"])
            .build();
        group.add(&row);
        rows.borrow_mut().push(row);
        return;
    }
    for d in devices {
        let row = device_row(group, omemo, jid, yours, &d);
        group.add(&row);
        rows.borrow_mut().push(row);
    }
}

fn device_row(
    group: &adw::PreferencesGroup,
    omemo: &OmemoProxy<'static>,
    jid: &str,
    yours: bool,
    d: &Dict,
) -> adw::ActionRow {
    let id = d
        .get("device-id")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    let active = d
        .get("active")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(true);
    let trust = text(d, "trust");
    let fingerprint = text(d, "fingerprint");
    let mut subtitle = if fingerprint.is_empty() && active {
        "Getting its fingerprint…".to_owned()
    } else {
        trust_text(&trust, active).to_owned()
    };
    if text(d, "protocol") == "omemo2" {
        subtitle.push_str(" · OMEMO 2 only");
    }
    let row = adw::ActionRow::builder()
        .title(if fingerprint.is_empty() {
            format!("Device {id}")
        } else {
            fingerprint_markup(&fingerprint)
        })
        .subtitle(subtitle)
        .css_classes(["fingerprint"])
        .build();
    let icon = match trust.as_str() {
        "verified" => "device-verified-symbolic",
        "blind" => "channel-secure-symbolic",
        _ => "dialog-warning-symbolic",
    };
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    if fingerprint.is_empty() || !active {
        return row;
    }
    let set = {
        let (o, j) = (omemo.clone(), jid.to_owned());
        move |trust: &'static str| {
            let (o, j) = (o.clone(), j.clone());
            glib::spawn_future_local(async move {
                // DevicesChanged redraws the list.
                let _ = run(async move { o.set_trust(&j, id, trust).await }).await;
            });
        }
    };
    if trust != "verified" {
        let verify = gtk::Button::builder()
            .label("Verify…")
            .valign(gtk::Align::Center)
            .build();
        let (set, fp, group) = (set.clone(), fingerprint.clone(), group.downgrade());
        let omemo = omemo.clone();
        verify.connect_clicked(move |_| {
            let (set, fp, omemo) = (set.clone(), fp.clone(), omemo.clone());
            let Some(group) = group.upgrade() else { return };
            glib::spawn_future_local(async move {
                let o = omemo.clone();
                let own = run(async move { o.fingerprint().await })
                    .await
                    .unwrap_or_default();
                verify_dialog(&group, &fp, &own, yours, set);
            });
        });
        row.add_suffix(&verify);
    }
    // The other choices, in a menu.
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let popover = gtk::Popover::builder().child(&menu).build();
    let more = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Trust")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .popover(&popover)
        .build();
    let choice = |label: &str, value: &'static str| {
        let b = gtk::Button::builder()
            .label(label)
            .css_classes(["flat"])
            .build();
        let (set, p) = (set.clone(), popover.clone());
        b.connect_clicked(move |_| {
            p.popdown();
            set(value);
        });
        menu.append(&b);
    };
    if trust != "blind" {
        choice(
            if trust == "verified" {
                "Remove Verification"
            } else {
                "Trust Without Verifying"
            },
            "blind",
        );
    }
    if trust != "untrusted" {
        choice("Don’t Trust", "untrusted");
    }
    row.add_suffix(&more);
    row
}

/// Shows the fingerprint to compare, and records the answer.
fn verify_dialog(
    parent: &impl IsA<gtk::Widget>,
    theirs: &str,
    own: &str,
    yours: bool,
    set: impl Fn(&'static str) + 'static,
) {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    body.append(
        &gtk::Label::builder()
            .label(fingerprint_lines(theirs, 2))
            .use_markup(true)
            .justify(gtk::Justification::Center)
            .selectable(true)
            .focusable(false)
            .css_classes(["fingerprint", "title-4"])
            .build(),
    );
    if !own.is_empty() {
        body.append(
            &gtk::Label::builder()
                .label(format!(
                    "{}\n{}",
                    if yours {
                        "This device’s fingerprint, to check from that one:"
                    } else {
                        "Your fingerprint, for them to check:"
                    },
                    fingerprint_lines(own, 2)
                ))
                .use_markup(true)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["fingerprint", "dim-label", "caption"])
                .build(),
        );
    }
    let (heading, text) = if yours {
        (
            "Verify Your Device",
            "On that device, open its encryption settings (in Piopio: Accounts, then the lock) and check it shows exactly this fingerprint for itself.",
        )
    } else {
        (
            "Verify Device",
            "Ask your contact to read out the fingerprint their app shows for this device, in person or over a call. Only if it’s exactly this one is the device really theirs.",
        )
    };
    let ask = adw::AlertDialog::new(Some(heading), Some(text));
    ask.set_extra_child(Some(&body));
    ask.add_responses(&[
        ("cancel", "_Cancel"),
        ("differ", "They _Don’t Match"),
        ("match", "They _Match"),
    ]);
    ask.set_response_appearance("differ", adw::ResponseAppearance::Destructive);
    ask.set_response_appearance("match", adw::ResponseAppearance::Suggested);
    ask.connect_response(None, move |_, response| match response {
        "match" => set("verified"),
        "differ" => set("untrusted"),
        _ => {}
    });
    ask.present(Some(parent));
}
