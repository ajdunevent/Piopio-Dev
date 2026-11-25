//! Conversation Details: who the conversation is with, its encryption, and
//! its own settings (pinned, notifications, typing notifications and read
//! receipts, which override the account's defaults), with Block and Delete.

use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use piopio_dbus::{ConversationProxy, Dict, RoomProxy};

use crate::chat::ChatPage;
use crate::client::{self, run};
use crate::window::{AccountInfo, Window, set_avatar_image};

fn text(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

/// What the dialog shows, read before it opens.
struct Loaded {
    peer: String,
    title: String,
    picture: String,
    room: bool,
    pinned: bool,
    muted: String,
    encryption: String,
    encryption_wanted: bool,
    chat_states: String,
    receipts: String,
    blocked: bool,
    override_quiet: bool,
    /// The account's defaults for typing notifications and read receipts.
    default_chat_states: bool,
    default_receipts: bool,
    /// Whether the server supports blocking (also while unknown).
    can_block: bool,
    /// The roster entry, if the peer is a contact.
    contact: Option<Dict>,
    /// Group chats: subject, whether private, how many people.
    subject: String,
    private: bool,
    occupants: usize,
}

async fn load(
    conversation: ConversationProxy<'static>,
    account: Rc<AccountInfo>,
) -> Option<Loaded> {
    let (proxy, roster) = (account.proxy.clone(), account.roster.clone());
    let path = conversation.inner().path().to_owned();
    run(async move {
        let c = &conversation;
        let peer = c.peer().await?;
        let room = c.kind().await? == "room";
        let contact = if room {
            None
        } else {
            roster
                .get_contacts()
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|d| text(d, "jid") == peer)
        };
        let (subject, private, occupants) = if room {
            let bus = client::bus().await?;
            let r = RoomProxy::builder(&bus).path(path)?.build().await?;
            (
                r.subject().await.unwrap_or_default(),
                r.private().await.unwrap_or(false),
                r.get_occupants().await.map(|o| o.len()).unwrap_or(0),
            )
        } else {
            (String::new(), false, 0)
        };
        Ok::<_, zbus::Error>(Loaded {
            title: c.title().await?,
            picture: c.avatar().await.unwrap_or_default(),
            pinned: c.pinned().await?,
            muted: c.muted().await?,
            encryption: c.encryption().await.unwrap_or_default(),
            encryption_wanted: c.encryption_wanted().await.unwrap_or(true),
            chat_states: c
                .send_chat_states()
                .await
                .unwrap_or_else(|_| "default".into()),
            receipts: c.send_receipts().await.unwrap_or_else(|_| "default".into()),
            blocked: c.blocked().await.unwrap_or(false),
            override_quiet: c.override_quiet().await.unwrap_or(false),
            default_chat_states: proxy.send_chat_states().await.unwrap_or(true),
            default_receipts: proxy.send_receipts().await.unwrap_or(true),
            can_block: {
                let f = proxy.server_features().await.unwrap_or_default();
                f.is_empty() || f.iter().any(|f| f == "urn:xmpp:blocking")
            },
            peer,
            room,
            contact,
            subject,
            private,
            occupants,
        })
    })
    .await
    .ok()
}

pub fn show(window: &Rc<Window>, chat: &Rc<ChatPage>) {
    let account = window
        .accounts
        .borrow()
        .iter()
        .find(|a| a.path.as_str() == chat.account_path())
        .cloned();
    let Some(account) = account else { return };
    let (window, chat) = (window.clone(), chat.clone());
    glib::spawn_future_local(async move {
        let Some(loaded) = load(chat.proxy().clone(), account.clone()).await else {
            window.toast("Couldn’t load the conversation’s details");
            return;
        };
        let dialog = build(&window, &chat, &account, loaded);
        dialog.present(Some(&window.window));
        // Nothing focused at first, so a phone's keyboard stays down.
        dialog.set_focus(None::<&gtk::Widget>);
    });
}

/// "Default (On)", "On", "Off" for an override of an account default.
fn override_row(title: &str, subtitle: &str, value: &str, default_on: bool) -> adw::ComboRow {
    let default = if default_on {
        "Default (On)"
    } else {
        "Default (Off)"
    };
    let row = adw::ComboRow::builder()
        .title(title)
        .subtitle(subtitle)
        .model(&gtk::StringList::new(&[default, "On", "Off"]))
        .build();
    row.set_selected(match value {
        "on" => 1,
        "off" => 2,
        _ => 0,
    });
    row
}

fn override_value(selected: u32) -> &'static str {
    match selected {
        1 => "on",
        2 => "off",
        _ => "default",
    }
}

/// Runs a property change, showing a toast if it fails.
fn apply<F>(window: &Rc<Window>, change: F)
where
    F: std::future::Future<Output = zbus::Result<()>> + Send + 'static,
{
    let w = Rc::downgrade(window);
    glib::spawn_future_local(async move {
        if let Err(e) = run(change).await
            && let Some(w) = w.upgrade()
        {
            w.toast(&client::explain(&e));
        }
    });
}

fn build(
    window: &Rc<Window>,
    chat: &Rc<ChatPage>,
    account: &Rc<AccountInfo>,
    d: Loaded,
) -> adw::PreferencesDialog {
    let dialog = adw::PreferencesDialog::builder()
        .title("Conversation Details")
        .search_enabled(false)
        .build();
    let page = adw::PreferencesPage::new();
    dialog.add(&page);
    let proxy = chat.proxy().clone();

    // Who: picture, name and address.
    let avatar = adw::Avatar::new(96, Some(&d.title), true);
    set_avatar_image(&avatar, &d.picture);
    let name = gtk::Label::builder()
        .label(&d.title)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["title-1"])
        .build();
    // Only when the name isn't already the address.
    let address = gtk::Label::builder()
        .label(&d.peer)
        .selectable(true)
        .focusable(false)
        .wrap(true)
        .visible(d.title != d.peer)
        .css_classes(["dim-label"])
        .build();
    let who = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    who.append(&avatar);
    who.append(&name);
    who.append(&address);
    let top = adw::PreferencesGroup::new();
    top.add(&who);
    page.add(&top);

    if d.room {
        let about = adw::PreferencesGroup::builder().title("Group Chat").build();
        if !d.subject.is_empty() {
            about.add(
                &adw::ActionRow::builder()
                    .title("Subject")
                    .subtitle(glib::markup_escape_text(&d.subject).as_str())
                    .subtitle_selectable(true)
                    .build(),
            );
        }
        about.add(
            &adw::ActionRow::builder()
                .title("People")
                .subtitle(d.occupants.to_string())
                .build(),
        );
        about.add(
            &adw::ActionRow::builder()
                .title("Kind")
                .subtitle(if d.private {
                    "Private: members only, everyone sees each other’s address"
                } else {
                    "Public"
                })
                .build(),
        );
        page.add(&about);
    } else {
        page.add(&contact_group(window, account, &d));
    }

    // Encryption, with the devices one level down.
    let security = adw::PreferencesGroup::builder().title("Encryption").build();
    let status = match (d.encryption.as_str(), d.encryption_wanted) {
        ("omemo2", _) => "OMEMO 2",
        ("omemo", _) => "OMEMO",
        (_, false) => "Off",
        (_, true) if d.room => "Not available: only private group chats can be encrypted",
        (_, true) => "Not available: their apps don’t support OMEMO",
    };
    let encryption = adw::ActionRow::builder()
        .title("End-to-End Encryption")
        .subtitle(status)
        .activatable(!d.room)
        .build();
    let icon = gtk::Image::from_icon_name(if d.encryption == "none" {
        "channel-insecure-symbolic"
    } else {
        "channel-secure-symbolic"
    });
    encryption.add_prefix(&icon);
    if !d.room {
        encryption.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let (w, dl, c) = (Rc::downgrade(window), dialog.downgrade(), chat.clone());
        encryption.connect_activated(move |_| {
            if let (Some(w), Some(dialog)) = (w.upgrade(), dl.upgrade()) {
                dialog.push_subpage(&crate::encryption::page(&w, c.proxy().clone()));
            }
        });
    }
    security.add(&encryption);
    page.add(&security);

    // This conversation's own settings.
    let settings = adw::PreferencesGroup::builder()
        .title("Conversation")
        .build();
    let notifications = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();
    let pin = adw::SwitchRow::builder()
        .title("Pin to Top")
        .active(d.pinned)
        .build();
    {
        let (w, p) = (window.clone(), proxy.clone());
        pin.connect_active_notify(move |row| {
            let (p, on) = (p.clone(), row.is_active());
            apply(&w, async move { p.set_pinned(on).await });
        });
    }
    settings.add(&pin);
    if d.room {
        let notify = adw::ComboRow::builder()
            .title("Notifications")
            .model(&gtk::StringList::new(&[
                "All Messages",
                "Only Mentions",
                "Off",
            ]))
            .build();
        notify.set_selected(match d.muted.as_str() {
            "mentions" => 1,
            "always" => 2,
            _ => 0,
        });
        let (w, p) = (window.clone(), proxy.clone());
        notify.connect_selected_notify(move |row| {
            let value = ["never", "mentions", "always"][row.selected().min(2) as usize];
            let p = p.clone();
            apply(&w, async move { p.set_muted(value).await });
        });
        notifications.add(&notify);
    } else {
        let mute = adw::SwitchRow::builder()
            .title("Mute Notifications")
            .active(d.muted == "always")
            .build();
        let (w, p) = (window.clone(), proxy.clone());
        mute.connect_active_notify(move |row| {
            let value = if row.is_active() { "always" } else { "never" };
            let p = p.clone();
            apply(&w, async move { p.set_muted(value).await });
        });
        notifications.add(&mute);

        let typing = override_row(
            "Send Typing Notifications",
            "Let them see when you’re writing",
            &d.chat_states,
            d.default_chat_states,
        );
        let (w, p) = (window.clone(), proxy.clone());
        typing.connect_selected_notify(move |row| {
            let (p, value) = (p.clone(), override_value(row.selected()));
            apply(&w, async move { p.set_send_chat_states(value).await });
        });
        settings.add(&typing);

        let receipts = override_row(
            "Send Read Receipts",
            "Let them see when you’ve read their messages",
            &d.receipts,
            d.default_receipts,
        );
        let (w, p) = (window.clone(), proxy.clone());
        receipts.connect_selected_notify(move |row| {
            let (p, value) = (p.clone(), override_value(row.selected()));
            apply(&w, async move { p.set_send_receipts(value).await });
        });
        settings.add(&receipts);
    }
    page.add(&settings);

    // All of this conversation's notifications, not only calls.
    let urgent = adw::SwitchRow::builder()
        .title("Override Do Not Disturb")
        .subtitle("Includes Quiet and Silent modes")
        .active(d.override_quiet)
        .build();
    {
        let (w, p) = (window.clone(), proxy.clone());
        urgent.connect_active_notify(move |row| {
            let (p, on) = (p.clone(), row.is_active());
            apply(&w, async move { p.set_override_quiet(on).await });
        });
    }
    notifications.add(&urgent);
    page.add(&notifications);

    // Block and delete.
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    // Blocking needs the server's support (XEP-0191); unblocking stays
    // possible either way.
    if !d.room && (d.can_block || d.blocked) {
        let block = gtk::Button::builder()
            .label(if d.blocked { "_Unblock" } else { "_Block…" })
            .use_underline(true)
            .css_classes(["pill"])
            .build();
        let (c, dl, blocked, peer) = (chat.clone(), dialog.downgrade(), d.blocked, d.peer.clone());
        let account = account.clone();
        block.connect_clicked(move |_| {
            let Some(dialog) = dl.upgrade() else { return };
            if blocked {
                c.set_blocked(false);
                dialog.close();
                return;
            }
            let ask = adw::AlertDialog::new(
                Some("Block Contact?"),
                Some(&format!(
                    "{peer} can no longer send you messages or see when you’re online, on any of your devices. They’re also removed from your contacts and this conversation is deleted. You can unblock them in Preferences."
                )),
            );
            ask.add_responses(&[("cancel", "_Cancel"), ("block", "_Block")]);
            ask.set_response_appearance("block", adw::ResponseAppearance::Destructive);
            let (c, dl, account) = (c.clone(), dialog.downgrade(), account.clone());
            let peer = peer.clone();
            ask.connect_response(None, move |_, response| {
                if response != "block" {
                    return;
                }
                if let Some(d) = dl.upgrade() {
                    d.close();
                }
                // Blocked, then gone from the contacts and the list.
                let (proxy, roster, conversation, peer) = (
                    account.proxy.clone(),
                    account.roster.clone(),
                    c.proxy().clone(),
                    peer.clone(),
                );
                let w = c.window();
                glib::spawn_future_local(async move {
                    let done = run(async move {
                        proxy.block(&peer).await?;
                        let _ = roster.remove_contact(&peer).await;
                        conversation.delete().await
                    })
                    .await;
                    if let (Err(e), Some(w)) = (done, w.and_then(|w| w.upgrade())) {
                        w.toast(&client::explain(&e));
                    }
                });
            });
            ask.present(Some(&dialog));
        });
        actions.append(&block);
    }
    let delete = gtk::Button::builder()
        .label(if d.room {
            "_Leave and Delete…"
        } else {
            "_Delete Conversation…"
        })
        .use_underline(true)
        .css_classes(["pill", "destructive-action"])
        .build();
    {
        let (c, dl) = (chat.clone(), dialog.downgrade());
        delete.connect_clicked(move |_| {
            if let Some(d) = dl.upgrade() {
                d.close();
            }
            c.confirm_delete();
        });
    }
    actions.append(&delete);
    let end = adw::PreferencesGroup::new();
    end.add(&actions);
    page.add(&end);
    dialog
}

/// The contact's name (which can be changed here) and whether they're in
/// the contact list.
fn contact_group(
    window: &Rc<Window>,
    account: &Rc<AccountInfo>,
    d: &Loaded,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Contact").build();
    let current = d
        .contact
        .as_ref()
        .map(|c| text(c, "name"))
        .unwrap_or_default();
    let name = adw::EntryRow::builder()
        .title("Display Name")
        .text(&current)
        .show_apply_button(true)
        .build();
    {
        let (w, roster, peer) = (window.clone(), account.roster.clone(), d.peer.clone());
        name.connect_apply(move |row| {
            let (roster, peer, text) = (roster.clone(), peer.clone(), row.text().to_string());
            apply(&w, async move {
                roster.rename_contact(&peer, text.trim()).await
            });
        });
    }
    group.add(&name);

    // Only for someone who isn't a contact yet: adding them shares
    // online status both ways and keeps them on your other devices.
    if d.contact.is_none() {
        let listed = adw::ActionRow::builder()
            .title("Not in Your Contacts")
            .build();
        let add = gtk::Button::builder()
            .label("Add")
            .valign(gtk::Align::Center)
            .build();
        let (w, roster, peer, row) = (
            window.clone(),
            account.roster.clone(),
            d.peer.clone(),
            listed.clone(),
        );
        add.connect_clicked(move |b| {
            b.set_sensitive(false);
            row.set_title("Added to Your Contacts");
            let (roster, peer) = (roster.clone(), peer.clone());
            apply(&w, async move { roster.add_contact(&peer, "", &[]).await });
        });
        listed.add_suffix(&add);
        group.add(&listed);
    }
    group
}
