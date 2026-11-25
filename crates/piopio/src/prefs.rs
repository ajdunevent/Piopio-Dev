//! Preferences: running in the background and at login, each account's
//! typing notifications and read receipts (which conversations can override
//! in their details), and blocked contacts.

use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use futures_util::StreamExt;
use piopio_dbus::DaemonProxy;

use crate::client::{self, run};
use crate::window::Window;

/// A toast in the dialog `widget` is in (the window's would be hidden
/// behind it).
pub fn toast_near(widget: &impl IsA<gtk::Widget>, text: &str) {
    if let Some(dialog) = widget
        .ancestor(adw::PreferencesDialog::static_type())
        .and_downcast::<adw::PreferencesDialog>()
    {
        dialog.add_toast(adw::Toast::new(text));
    }
}

pub fn show(window: &Rc<Window>) {
    let window = window.clone();
    glib::spawn_future_local(async move {
        let Ok(bus) = client::bus().await else { return };
        let loaded = run(async move {
            let daemon = DaemonProxy::new(&bus).await?;
            let background = daemon.run_in_background().await?;
            let login = daemon.start_at_login().await?;
            let warn = daemon.warn_on_quit().await.unwrap_or(true);
            Ok::<_, zbus::Error>((daemon, background, login, warn))
        })
        .await;
        match loaded {
            Ok((daemon, background, login, warn)) => {
                build(&window, daemon, background, login, warn).present(Some(&window.window));
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

    // Who's blocked, to unblock them: blocking also removes them from the
    // contacts and the conversation list, so this is where they are.
    let blocked = adw::PreferencesGroup::builder()
        .title("Blocked Contacts")
        .description("They can’t send you messages or see when you’re online.")
        .build();
    let shown: Rc<std::cell::RefCell<Vec<adw::ActionRow>>> = Rc::default();
    for account in window.accounts.borrow().iter().cloned() {
        let (group, shown, w) = (blocked.clone(), shown.clone(), Rc::downgrade(window));
        let changes = {
            let proxy = account.proxy.clone();
            client::forward(move || async move {
                let later = proxy.receive_blocked_changed().await;
                let first = proxy.blocked().await.unwrap_or_default();
                Some(
                    futures_util::stream::once(async move { first })
                        .chain(later.filter_map(|c| async move { c.get().await.ok() })),
                )
            })
        };
        let several = window.accounts.borrow().len() > 1;
        glib::spawn_future_local(async move {
            while let Ok(list) = changes.recv().await {
                // This account's rows are the ones named after it.
                let mut rows = shown.borrow_mut();
                rows.retain(|r| {
                    if r.widget_name() == account.jid.as_str() {
                        group.remove(r);
                        false
                    } else {
                        true
                    }
                });
                for jid in list {
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&jid).as_str())
                        .name(account.jid.as_str())
                        .build();
                    if several {
                        row.set_subtitle(&glib::markup_escape_text(&account.jid));
                    }
                    let unblock = gtk::Button::builder()
                        .label("Unblock")
                        .valign(gtk::Align::Center)
                        .build();
                    let (proxy, w) = (account.proxy.clone(), w.clone());
                    unblock.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let (proxy, jid, w, b) = (proxy.clone(), jid.clone(), w.clone(), b.clone());
                        glib::spawn_future_local(async move {
                            if let Err(e) = run(async move { proxy.unblock(&jid).await }).await {
                                b.set_sensitive(true);
                                if w.upgrade().is_some() {
                                    toast_near(&b, &client::explain(&e));
                                }
                            }
                        });
                    });
                    row.add_suffix(&unblock);
                    group.add(&row);
                    rows.push(row);
                }
                if rows.is_empty() {
                    group.set_description(Some("No one is blocked."));
                } else {
                    group.set_description(Some(
                        "They can’t send you messages or see when you’re online.",
                    ));
                }
            }
        });
    }

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
    // Last, below everything else.
    page.add(&blocked);
    dialog
}
