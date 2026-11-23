//! Dialogs: adding an account, starting a chat, managing accounts, and
//! answering subscription requests.

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use piopio_dbus::{AccountProxy, DaemonProxy, Dict};
use zbus::zvariant::OwnedObjectPath;

use crate::client::{self, run};
use crate::window::{AccountInfo, Window};

fn text(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

/// Asks for an address and password, logs in, and only keeps the account if
/// the login works.
pub fn add_account(window: &Rc<Window>) {
    let address = adw::EntryRow::builder()
        .title("Address")
        .input_purpose(gtk::InputPurpose::Email)
        .build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    let group = adw::PreferencesGroup::builder()
        .description("Your XMPP address looks like an email address, for example name@example.org.")
        .build();
    group.add(&address);
    group.add(&password);
    let status = gtk::Label::builder()
        .wrap(true)
        .css_classes(["error"])
        .visible(false)
        .build();
    let button = gtk::Button::builder()
        .label("Log In")
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .sensitive(false)
        .build();
    let spinner = crate::spinner();
    spinner.set_visible(false);
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&group);
    body.append(&status);
    body.append(&spinner);
    body.append(&button);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&adw::Clamp::builder().child(&body).build()));
    let dialog = adw::Dialog::builder()
        .title("Add Account")
        .content_width(420)
        .child(&view)
        .build();

    let check = {
        let (address, password, button) = (address.clone(), password.clone(), button.clone());
        move || {
            let a = address.text();
            button.set_sensitive(
                a.contains('@') && !a.starts_with('@') && !password.text().is_empty(),
            );
        }
    };
    address.connect_changed({
        let check = check.clone();
        move |_| check()
    });
    password.connect_changed(move |_| check());
    password.connect_entry_activated({
        let button = button.clone();
        move |_| {
            if button.is_sensitive() {
                button.emit_clicked();
            }
        }
    });

    let weak = Rc::downgrade(window);
    button.connect_clicked({
        let dialog = dialog.clone();
        move |button| {
            let Some(window) = weak.upgrade() else { return };
            let jid = address.text().trim().to_string();
            let pw = password.text().to_string();
            button.set_sensitive(false);
            spinner.set_visible(true);
            status.set_visible(false);
            let (button, spinner, status, dialog) = (
                button.clone(),
                spinner.clone(),
                status.clone(),
                dialog.clone(),
            );
            glib::spawn_future_local(async move {
                let result = log_in(jid, pw).await;
                spinner.set_visible(false);
                button.set_sensitive(true);
                match result {
                    Ok(path) => {
                        window.add_account_info(path).await;
                        dialog.close();
                        window.toast("Account added");
                    }
                    Err(why) => {
                        status.set_label(&why);
                        status.set_visible(true);
                    }
                }
            });
        }
    });
    dialog.present(Some(&window.window));
}

/// Adds the account and waits for the first login; removes it again if the
/// login fails, so a typo never leaves a broken account behind.
async fn log_in(jid: String, password: String) -> Result<OwnedObjectPath, String> {
    let bus = client::bus().await.map_err(|e| client::explain(&e))?;
    run(async move {
        let daemon = DaemonProxy::new(&bus)
            .await
            .map_err(|e| client::explain(&e))?;
        let path = daemon
            .add_account(&jid, &password, HashMap::new())
            .await
            .map_err(|e| client::explain(&e))?;
        let account = AccountProxy::builder(&bus)
            .path(path.clone())
            .map_err(|e| e.to_string())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| client::explain(&e))?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let state = account.state().await.unwrap_or_default();
            let error = account.error().await.unwrap_or_default();
            if state == "connected" {
                return Ok(path);
            }
            let give_up = state == "error" || tokio::time::Instant::now() > deadline;
            if give_up {
                let _ = daemon.remove_account(&path.as_ref()).await;
                return Err(if error.is_empty() {
                    "The server did not answer in time.".to_owned()
                } else {
                    error
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
}

/// Joins a group chat by its address.
pub fn join_room(window: &Rc<Window>) {
    let accounts: Vec<Rc<AccountInfo>> = window.accounts.borrow().clone();
    if accounts.is_empty() {
        add_account(window);
        return;
    }
    let account_names =
        gtk::StringList::new(&accounts.iter().map(|a| a.jid.as_str()).collect::<Vec<_>>());
    let account_row = adw::ComboRow::builder()
        .title("Account")
        .model(&account_names)
        .visible(accounts.len() > 1)
        .build();
    let address = adw::EntryRow::builder()
        .title("Address (room@conference.example.org)")
        .input_purpose(gtk::InputPurpose::Email)
        .build();
    let nick = adw::EntryRow::builder().title("Nickname").build();
    let password = adw::PasswordEntryRow::builder()
        .title("Password (if the room has one)")
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&account_row);
    group.add(&address);
    group.add(&nick);
    group.add(&password);
    let private = adw::SwitchRow::builder()
        .title("Private and Encrypted")
        .subtitle("If the room is new: only people you invite can join, and messages use OMEMO")
        .active(true)
        .build();
    group.add(&private);
    let join = gtk::Button::builder()
        .label("Join")
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .sensitive(false)
        .build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&group);
    body.append(&join);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&adw::Clamp::builder().child(&body).build()));
    let dialog = adw::Dialog::builder()
        .title("Join Group Chat")
        .content_width(420)
        .child(&view)
        .build();

    address.connect_changed({
        let join = join.clone();
        move |a| join.set_sensitive(a.text().contains('@'))
    });
    for entry in [&address, &nick] {
        entry.connect_entry_activated({
            let join = join.clone();
            move |_| {
                if join.is_sensitive() {
                    join.emit_clicked();
                }
            }
        });
    }
    let (w, d) = (window.clone(), dialog.clone());
    join.connect_clicked(move |button| {
        let account = accounts[account_row.selected() as usize % accounts.len()].clone();
        let jid = address.text().trim().to_string();
        let nick = nick.text().trim().to_string();
        let password = password.text().to_string();
        let private = private.is_active();
        let (w, d, button) = (w.clone(), d.clone(), button.clone());
        button.set_sensitive(false);
        glib::spawn_future_local(async move {
            let proxy = account.proxy.clone();
            let result = run(async move {
                let mut options = std::collections::HashMap::new();
                if !password.is_empty() {
                    options.insert("password", zbus::zvariant::Value::from(password));
                }
                options.insert("private", zbus::zvariant::Value::from(private));
                proxy.join_room(&jid, &nick, options).await
            })
            .await;
            button.set_sensitive(true);
            match result {
                Ok(path) => {
                    d.close();
                    w.add_conversation(path.clone()).await;
                    w.open_conversation(path.as_str());
                }
                Err(e) => w.toast(&client::explain(&e)),
            }
        });
    });
    dialog.present(Some(&window.window));
}

/// Starts a chat with a contact or any address.
pub fn new_chat(window: &Rc<Window>) {
    let accounts: Vec<Rc<AccountInfo>> = window.accounts.borrow().clone();
    if accounts.is_empty() {
        add_account(window);
        return;
    }
    let account_names =
        gtk::StringList::new(&accounts.iter().map(|a| a.jid.as_str()).collect::<Vec<_>>());
    let account_row = adw::ComboRow::builder()
        .title("From")
        .model(&account_names)
        .visible(accounts.len() > 1)
        .build();
    let address = adw::EntryRow::builder()
        .title("Address")
        .input_purpose(gtk::InputPurpose::Email)
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&account_row);
    group.add(&address);
    let start = gtk::Button::builder()
        .label("Start Chat")
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .sensitive(false)
        .build();
    let contacts = adw::PreferencesGroup::builder().title("Contacts").build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&group);
    body.append(&start);
    body.append(&contacts);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&adw::Clamp::builder().child(&body).build())
            .build(),
    ));
    let dialog = adw::Dialog::builder()
        .title("New Chat")
        .content_width(420)
        .child(&view)
        .build();

    address.connect_changed({
        let start = start.clone();
        move |a| start.set_sensitive(a.text().contains('@'))
    });
    address.connect_entry_activated({
        let start = start.clone();
        move |_| {
            if start.is_sensitive() {
                start.emit_clicked();
            }
        }
    });

    let open_chat = {
        let (window, dialog, accounts) = (window.clone(), dialog.clone(), accounts.clone());
        let account_row = account_row.clone();
        move |jid: String, add: bool, name: String| {
            let account = accounts[account_row.selected() as usize % accounts.len()].clone();
            let (window, dialog) = (window.clone(), dialog.clone());
            glib::spawn_future_local(async move {
                let (proxy, roster) = (account.proxy.clone(), account.roster.clone());
                let target = jid.clone();
                let result = run(async move {
                    // Someone new becomes a contact, which asks to see each
                    // other's status; existing contacts are left as they are.
                    let known = roster
                        .get_contacts()
                        .await
                        .unwrap_or_default()
                        .iter()
                        .any(|c| text(c, "jid") == target);
                    if add && !known {
                        let _ = roster.add_contact(&target, &name, &[]).await;
                    }
                    proxy.start_chat(&target).await
                })
                .await;
                match result {
                    Ok(path) => {
                        dialog.close();
                        window.add_conversation(path.clone()).await;
                        window.open_conversation(path.as_str());
                    }
                    Err(e) => window.toast(&client::explain(&e)),
                }
            });
        }
    };
    start.connect_clicked({
        let (open_chat, address) = (open_chat.clone(), address.clone());
        move |_| open_chat(address.text().trim().to_string(), true, String::new())
    });

    // List the selected account's contacts.
    let shown_rows: Rc<std::cell::RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let fill = {
        let (contacts, accounts, account_row) =
            (contacts.clone(), accounts.clone(), account_row.clone());
        let open_chat = open_chat.clone();
        move || {
            let account = accounts[account_row.selected() as usize % accounts.len()].clone();
            let (contacts, open_chat, shown_rows) =
                (contacts.clone(), open_chat.clone(), shown_rows.clone());
            glib::spawn_future_local(async move {
                let roster = account.roster.clone();
                let list = run(async move { roster.get_contacts().await })
                    .await
                    .unwrap_or_default();
                for row in shown_rows.borrow_mut().drain(..) {
                    contacts.remove(&row);
                }
                let mut list: Vec<Dict> = list
                    .into_iter()
                    .filter(|c| text(c, "subscription") != "none" || !text(c, "name").is_empty())
                    .collect();
                list.sort_by_key(|c| {
                    let name = text(c, "name");
                    if name.is_empty() {
                        text(c, "jid")
                    } else {
                        name
                    }
                    .to_lowercase()
                });
                contacts.set_visible(!list.is_empty());
                for c in list {
                    let jid = text(&c, "jid");
                    let name = text(&c, "name");
                    let shown = if name.is_empty() {
                        jid.clone()
                    } else {
                        name.clone()
                    };
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&shown).as_str())
                        .subtitle(
                            glib::markup_escape_text(if name.is_empty() { "" } else { &jid })
                                .as_str(),
                        )
                        .activatable(true)
                        .build();
                    row.add_prefix(&adw::Avatar::new(32, Some(&shown), true));
                    let presence = text(&c, "presence");
                    if presence != "offline" && !presence.is_empty() {
                        row.add_suffix(&gtk::Image::from_icon_name("user-available-symbolic"));
                    }
                    let open_chat = open_chat.clone();
                    row.connect_activated(move |_| open_chat(jid.clone(), false, String::new()));
                    contacts.add(&row);
                    shown_rows.borrow_mut().push(row);
                }
            });
        }
    };
    account_row.connect_selected_notify({
        let fill = fill.clone();
        move |_| fill()
    });
    fill();
    dialog.present(Some(&window.window));
}

/// Lists accounts with their state; each can be turned off or removed.
pub fn accounts(window: &Rc<Window>) {
    let group = adw::PreferencesGroup::builder()
        .title("Accounts")
        .header_suffix(
            &gtk::Button::builder()
                .icon_name("list-add-symbolic")
                .tooltip_text("Add Account")
                .css_classes(["flat"])
                .action_name("app.add-account")
                .build(),
        )
        .build();
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let dialog = adw::PreferencesDialog::builder().title("Accounts").build();
    dialog.add(&page);

    for account in window.accounts.borrow().iter().cloned() {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&account.jid).as_str())
            .build();
        let enabled = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Connected")
            .build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove Account")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&enabled);
        row.add_suffix(&remove);
        group.add(&row);

        // The account's picture; click to change it.
        let picture = adw::Avatar::new(40, Some(&account.jid), true);
        let choose = gtk::Button::builder()
            .child(&picture)
            .tooltip_text("Change Picture")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        row.add_prefix(&choose);
        {
            let (proxy, picture, choose) = (account.proxy.clone(), picture.clone(), choose.clone());
            glib::spawn_future_local(async move {
                let (path, features) = run(async move {
                    (
                        proxy.avatar().await.unwrap_or_default(),
                        proxy.server_features().await.unwrap_or_default(),
                    )
                })
                .await;
                crate::window::set_avatar_image(&picture, &path);
                // Pictures are stored with PEP; without it they can't be set.
                if !features.is_empty() && !features.iter().any(|f| f == "pep") {
                    choose.set_sensitive(false);
                    choose.set_tooltip_text(Some("Your server can’t store a picture"));
                }
            });
        }
        {
            let (proxy, picture, w) = (account.proxy.clone(), picture.clone(), window.clone());
            choose.connect_clicked(move |_| {
                let filter = gtk::FileFilter::new();
                // The formats the daemon reads (add_pixbuf_formats is
                // deprecated since GTK 4.20).
                for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                    filter.add_mime_type(mime);
                }
                filter.set_name(Some("Pictures"));
                let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let chooser = gtk::FileDialog::builder()
                    .title("Choose a Picture")
                    .filters(&filters)
                    .build();
                let (proxy, picture, w2) = (proxy.clone(), picture.clone(), w.clone());
                chooser.open(
                    Some(&w.window),
                    gtk::gio::Cancellable::NONE,
                    move |result| {
                        let Some(path) = result.ok().and_then(|f| f.path()) else {
                            return;
                        };
                        let (proxy, picture, w) = (proxy.clone(), picture.clone(), w2.clone());
                        glib::spawn_future_local(async move {
                            let set = run(async move {
                                let f = std::fs::File::open(&path).map_err(|e| {
                                    zbus::Error::Failure(format!("can't read the picture: {e}"))
                                })?;
                                proxy.set_avatar(zbus::zvariant::Fd::from(&f)).await?;
                                proxy.avatar().await
                            })
                            .await;
                            match set {
                                Ok(path) => crate::window::set_avatar_image(&picture, &path),
                                Err(e) => w.toast(&client::explain(&e)),
                            }
                        });
                    },
                );
            });
        }

        let proxy = account.proxy.clone();
        let (row2, enabled2) = (row.clone(), enabled.clone());
        glib::spawn_future_local(async move {
            let status = run(async move {
                (
                    proxy.enabled().await.unwrap_or(false),
                    proxy.state().await.unwrap_or_default(),
                    proxy.error().await.unwrap_or_default(),
                )
            })
            .await;
            enabled2.set_active(status.0);
            let subtitle = match (status.1.as_str(), status.2.is_empty()) {
                ("connected", _) => "Connected".to_owned(),
                ("error", _) => status.2,
                ("disconnected", _) => "Off".to_owned(),
                (_, true) => "Connecting…".to_owned(),
                (_, false) => format!("Connecting… ({})", status.2),
            };
            row2.set_subtitle(&glib::markup_escape_text(&subtitle));
        });

        let proxy = account.proxy.clone();
        enabled.connect_state_set(move |_, on| {
            let proxy = proxy.clone();
            glib::spawn_future_local(async move {
                let _ = run(async move { proxy.set_enabled(on).await }).await;
            });
            glib::Propagation::Proceed
        });

        let (weak, dialog2, group) = (Rc::downgrade(window), dialog.clone(), group.clone());
        remove.connect_clicked(move |_| {
            let Some(window) = weak.upgrade() else { return };
            let confirm = adw::AlertDialog::new(
                Some("Remove Account?"),
                Some(&format!(
                    "{} will be removed from this device with all its chats. The account itself stays on its server.",
                    account.jid
                )),
            );
            confirm.add_responses(&[("cancel", "_Cancel"), ("remove", "_Remove")]);
            confirm.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            let (account, group, row, window2) = (account.clone(), group.clone(), row.clone(), window.clone());
            confirm.connect_response(None, move |_, response| {
                if response != "remove" {
                    return;
                }
                let (account, group, row, window) = (account.clone(), group.clone(), row.clone(), window2.clone());
                glib::spawn_future_local(async move {
                    let path = account.path.clone();
                    let result = match client::bus().await {
                        Ok(bus) => run(async move {
                            DaemonProxy::new(&bus).await?.remove_account(&path.as_ref()).await
                        })
                        .await,
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok(()) => group.remove(&row),
                        Err(e) => window.toast(&client::explain(&e)),
                    }
                });
            });
            confirm.present(Some(&dialog2));
        });
    }

    dialog.present(Some(&window.window));
}

/// Someone wants to see our presence: ask whether to allow it.
pub fn subscription_request(window: &Rc<Window>, account: &Rc<AccountInfo>, jid: String) {
    let ask = adw::AlertDialog::new(
        Some("Contact Request"),
        Some(&format!(
            "{jid} wants to add you as a contact and see when you're online."
        )),
    );
    ask.add_responses(&[("deny", "_Decline"), ("approve", "_Accept")]);
    ask.set_response_appearance("approve", adw::ResponseAppearance::Suggested);
    ask.set_default_response(Some("approve"));
    let roster = account.roster.clone();
    ask.connect_response(None, move |_, response| {
        let (roster, jid) = (roster.clone(), jid.clone());
        let approve = response == "approve";
        glib::spawn_future_local(async move {
            let _ = run(async move {
                if approve {
                    roster.approve_subscription(&jid).await
                } else {
                    roster.deny_subscription(&jid).await
                }
            })
            .await;
        });
    });
    ask.present(Some(&window.window));
}

/// Quit fully: the window and the background service, after a warning.
pub fn quit_fully(window: &Rc<Window>) {
    let ask = adw::AlertDialog::new(
        Some("Quit Piopio?"),
        Some(
            "This will fully exit and you won’t be able to receive new messages until Piopio is started again.",
        ),
    );
    ask.add_responses(&[("cancel", "_Cancel"), ("quit", "_Quit")]);
    ask.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
    ask.set_default_response(Some("cancel"));
    let w = Rc::downgrade(window);
    ask.connect_response(None, move |_, response| {
        if response != "quit" {
            return;
        }
        let Some(w) = w.upgrade() else { return };
        glib::spawn_future_local(async move {
            if let Ok(bus) = client::bus().await {
                let _ = run(async move { DaemonProxy::new(&bus).await?.quit().await }).await;
            }
            if let Some(app) = w.window.application() {
                app.quit();
            }
        });
    });
    ask.present(Some(&window.window));
}

pub fn about(window: &Rc<Window>) {
    let about = adw::AboutDialog::builder()
        .application_name("Piopio")
        .application_icon(piopio_dbus::APP_ID)
        .developer_name("AJ Dunevent")
        .version(env!("CARGO_PKG_VERSION"))
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/ajdunevent/Piopio-Dev")
        .comments("Chat over XMPP with end-to-end encryption.\n\nNamed for the North Island piopio, whose quick, whistling call gave it its Māori name.")
        .build();
    about.present(Some(&window.window));
}
