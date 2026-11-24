//! Preferences: running in the background and at login, the notification
//! sound, and each account's typing notifications and read receipts (which
//! conversations can override in their details).

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use piopio_dbus::DaemonProxy;

use crate::client::{self, run};
use crate::window::Window;

/// Where a sound row's choice goes: the app's default or a conversation's.
#[derive(Clone)]
pub enum SoundTarget {
    App(DaemonProxy<'static>),
    Conversation(piopio_dbus::ConversationProxy<'static>),
}

impl SoundTarget {
    async fn set(&self, value: &'static str) -> zbus::Result<()> {
        match self {
            SoundTarget::App(p) => p.set_notification_sound(value).await,
            SoundTarget::Conversation(p) => p.set_notification_sound(value).await,
        }
    }

    async fn set_custom(&self, file: std::fs::File) -> zbus::Result<()> {
        let fd = zbus::zvariant::Fd::from(&file);
        match self {
            SoundTarget::App(p) => p.set_custom_sound(fd).await,
            SoundTarget::Conversation(p) => p.set_custom_sound(fd).await,
        }
    }
}

/// A row choosing a notification sound: the first choice ("Desktop Default"
/// for the app, "Default" for a conversation), None, or a file of one's own.
pub fn sound_row(
    window: &Rc<Window>,
    first: &str,
    current: &str,
    target: SoundTarget,
) -> adw::ComboRow {
    let row = adw::ComboRow::builder()
        .title("Notification Sound")
        .model(&gtk::StringList::new(&[first, "None", "Custom…"]))
        .build();
    let index = match current {
        "silent" => 1,
        "custom" => 2,
        _ => 0,
    };
    row.set_selected(index);
    let change = gtk::Button::builder()
        .label("Change…")
        .valign(gtk::Align::Center)
        .visible(index == 2)
        .build();
    row.add_suffix(&change);
    let previous = Rc::new(Cell::new(index));

    let pick = {
        let (window, row, target, previous, change) = (
            window.clone(),
            row.clone(),
            target.clone(),
            previous.clone(),
            change.clone(),
        );
        Rc::new(move || {
            let filter = gtk::FileFilter::new();
            filter.add_mime_type("audio/*");
            filter.set_name(Some("Sounds"));
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let chooser = gtk::FileDialog::builder()
                .title("Choose a Sound")
                .filters(&filters)
                .build();
            let (w, row, target, previous, change) = (
                window.clone(),
                row.clone(),
                target.clone(),
                previous.clone(),
                change.clone(),
            );
            chooser.open(
                Some(&window.window),
                gtk::gio::Cancellable::NONE,
                move |result| {
                    let Some(path) = result.ok().and_then(|f| f.path()) else {
                        // Cancelled: back to what it was.
                        row.set_selected(previous.get());
                        return;
                    };
                    let (w, row, target, previous, change) = (
                        w.clone(),
                        row.clone(),
                        target.clone(),
                        previous.clone(),
                        change.clone(),
                    );
                    glib::spawn_future_local(async move {
                        let set = run(async move {
                            let file = std::fs::File::open(&path).map_err(|e| {
                                zbus::Error::Failure(format!("can't read the sound: {e}"))
                            })?;
                            target.set_custom(file).await
                        })
                        .await;
                        match set {
                            Ok(()) => {
                                previous.set(2);
                                change.set_visible(true);
                            }
                            Err(e) => {
                                row.set_selected(previous.get());
                                w.toast(&client::explain(&e));
                            }
                        }
                    });
                },
            );
        })
    };
    {
        let pick = pick.clone();
        change.connect_clicked(move |_| pick());
    }
    let w = Rc::downgrade(window);
    row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected == previous.get() {
            return;
        }
        if selected == 2 {
            pick();
            return;
        }
        previous.set(selected);
        change.set_visible(false);
        let target = target.clone();
        let value = if selected == 1 { "silent" } else { "default" };
        let w = w.clone();
        glib::spawn_future_local(async move {
            if let Err(e) = run(async move { target.set(value).await }).await
                && let Some(w) = w.upgrade()
            {
                w.toast(&client::explain(&e));
            }
        });
    });
    row
}

pub fn show(window: &Rc<Window>) {
    let window = window.clone();
    glib::spawn_future_local(async move {
        let Ok(bus) = client::bus().await else { return };
        let loaded = run(async move {
            let daemon = DaemonProxy::new(&bus).await?;
            let background = daemon.run_in_background().await?;
            let login = daemon.start_at_login().await?;
            let sound = daemon.notification_sound().await?;
            let warn = daemon.warn_on_quit().await.unwrap_or(true);
            Ok::<_, zbus::Error>((daemon, background, login, sound, warn))
        })
        .await;
        match loaded {
            Ok((daemon, background, login, sound, warn)) => {
                build(&window, daemon, background, login, &sound, warn)
                    .present(Some(&window.window));
            }
            Err(e) => window.toast(&client::explain(&e)),
        }
    });
}

fn build(
    window: &Rc<Window>,
    daemon: DaemonProxy<'static>,
    background: bool,
    login: bool,
    sound: &str,
    warn: bool,
) -> adw::PreferencesDialog {
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .search_enabled(false)
        .build();
    let page = adw::PreferencesPage::new();
    dialog.add(&page);

    let general = adw::PreferencesGroup::builder().title("General").build();
    let run_background = adw::SwitchRow::builder()
        .title("Run in Background")
        .subtitle("Receive messages and notifications after the window closes")
        .active(background)
        .build();
    let start_at_login = adw::SwitchRow::builder()
        .title("Start at Login")
        .subtitle("Connect when you log in, without opening a window")
        .active(login)
        .sensitive(background)
        .build();
    let warn_on_quit = adw::SwitchRow::builder()
        .title("Show Warning When Quitting")
        .subtitle("Quit stops messages from arriving until Piopio starts again")
        .active(warn)
        .build();
    general.add(&run_background);
    general.add(&start_at_login);
    general.add(&warn_on_quit);
    page.add(&general);
    {
        let (d, w) = (daemon.clone(), window.clone());
        warn_on_quit.connect_active_notify(move |row| {
            let (d, on, w) = (d.clone(), row.is_active(), w.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = run(async move { d.set_warn_on_quit(on).await }).await {
                    w.toast(&client::explain(&e));
                }
            });
        });
    }
    {
        let (d, login_row, w) = (daemon.clone(), start_at_login.clone(), window.clone());
        run_background.connect_active_notify(move |row| {
            let (d, on) = (d.clone(), row.is_active());
            login_row.set_sensitive(on);
            let (login_row, w) = (login_row.clone(), w.clone());
            glib::spawn_future_local(async move {
                let result = run(async move {
                    d.set_run_in_background(on).await?;
                    d.start_at_login().await
                })
                .await;
                match result {
                    Ok(login) => login_row.set_active(login),
                    Err(e) => w.toast(&client::explain(&e)),
                }
            });
        });
    }
    {
        let (d, w) = (daemon.clone(), window.clone());
        start_at_login.connect_active_notify(move |row| {
            if !row.is_sensitive() {
                return; // following Run in Background, not the user
            }
            let (d, on, w) = (d.clone(), row.is_active(), w.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = run(async move { d.set_start_at_login(on).await }).await {
                    w.toast(&client::explain(&e));
                }
            });
        });
    }

    let notifications = adw::PreferencesGroup::builder()
        .title("Notifications")
        .description("Conversations can choose their own sound in their details.")
        .build();
    notifications.add(&sound_row(
        window,
        "Desktop Default",
        sound,
        SoundTarget::App(daemon),
    ));
    page.add(&notifications);

    // Each account's defaults for typing notifications and read receipts.
    let accounts = window.accounts.borrow().clone();
    let several = accounts.len() > 1;
    for account in accounts {
        let privacy = adw::PreferencesGroup::builder()
            .title("Privacy")
            .description(if several {
                format!(
                    "{}. Conversations can change these in their details.",
                    glib::markup_escape_text(&account.jid)
                )
            } else {
                "Conversations can change these in their details.".to_owned()
            })
            .build();
        let typing = adw::SwitchRow::builder()
            .title("Send Typing Notifications")
            .subtitle("Let contacts see when you’re writing")
            .build();
        let receipts = adw::SwitchRow::builder()
            .title("Send Read Receipts")
            .subtitle("Let contacts see when you’ve read their messages")
            .build();
        privacy.add(&typing);
        privacy.add(&receipts);
        page.add(&privacy);
        let (proxy, t, r) = (account.proxy.clone(), typing.clone(), receipts.clone());
        glib::spawn_future_local(async move {
            let (typing_on, receipts_on) = run(async move {
                (
                    proxy.send_chat_states().await.unwrap_or(true),
                    proxy.send_receipts().await.unwrap_or(true),
                )
            })
            .await;
            t.set_active(typing_on);
            r.set_active(receipts_on);
            let proxy = account.proxy.clone();
            t.connect_active_notify(move |row| {
                let (proxy, on) = (proxy.clone(), row.is_active());
                glib::spawn_future_local(async move {
                    let _ = run(async move { proxy.set_send_chat_states(on).await }).await;
                });
            });
            let proxy = account.proxy.clone();
            r.connect_active_notify(move |row| {
                let (proxy, on) = (proxy.clone(), row.is_active());
                glib::spawn_future_local(async move {
                    let _ = run(async move { proxy.set_send_receipts(on).await }).await;
                });
            });
        });
    }
    dialog
}
