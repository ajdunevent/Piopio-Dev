//! Piopio UI: shows what the daemon knows and sends the user's actions to it.
//!
//! The UI holds no protocol state and no connections of its own. Closing the
//! window ends this process and frees its memory; the daemon keeps running.

mod animation;
mod calls;
mod chat;
mod client;
mod details;
mod dialogs;
mod encryption;
mod prefs;
mod styling;
mod timefmt;
mod window;

/// A loading spinner: libadwaita's on GNOME 51, GTK's on older systems.
pub fn spinner() -> gtk::Widget {
    #[cfg(feature = "gnome-51")]
    return adw::Spinner::new().upcast();
    #[cfg(not(feature = "gnome-51"))]
    #[allow(deprecated)]
    return gtk::Spinner::builder().spinning(true).build().upcast();
}

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use piopio_dbus::APP_ID;

fn main() -> glib::ExitCode {
    // The D-Bus runtime starts before GTK so its thread is ready for the first call.
    let _ = client::runtime();
    // Styles and icons; libadwaita loads style.css from here by itself.
    gio::resources_register_include!("piopio.gresource").expect("the UI's resources");
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    let window: Rc<RefCell<Option<Rc<window::Window>>>> = Rc::default();

    // Every entry point creates the window first if it doesn't exist yet,
    // including actions invoked by a notification while no window is open.
    let ensure = {
        let window = window.clone();
        move |app: &adw::Application| -> Rc<window::Window> {
            if let Some(w) = window.borrow().as_ref() {
                return w.clone();
            }
            let w = window::Window::new(app);
            let slot = window.clone();
            w.window.connect_destroy(move |_| {
                slot.borrow_mut().take();
            });
            *window.borrow_mut() = Some(w.clone());
            w
        }
    };

    app.connect_startup(|_| crate::timefmt::init());

    app.connect_activate({
        let ensure = ensure.clone();
        move |app| ensure(app).window.present()
    });
    // xmpp: links open a chat with that address.
    app.connect_open({
        let ensure = ensure.clone();
        move |app, files, _| {
            let w = ensure(app);
            w.window.present();
            if let Some(file) = files.first() {
                let uri = file.uri().to_string();
                let jid = uri
                    .trim_start_matches("xmpp:")
                    .split(['?', '/'])
                    .next()
                    .unwrap_or("")
                    .to_owned();
                if jid.contains('@') {
                    open_address(&w, jid);
                }
            }
        }
    });

    type Handler = Rc<dyn Fn(&adw::Application, Option<&glib::Variant>)>;
    let action = |name: &str, f: Handler, param: Option<&glib::VariantTy>| {
        let a = gio::SimpleAction::new(name, param);
        let app2 = app.clone();
        a.connect_activate(move |_, p| f(&app2, p));
        app.add_action(&a);
    };
    for (name, which) in [
        ("new-chat", 0),
        ("add-account", 1),
        ("accounts", 2),
        ("about", 3),
        ("join-room", 4),
        ("preferences", 5),
        ("quit-fully", 6),
    ] {
        let ensure = ensure.clone();
        action(
            name,
            Rc::new(move |app, _| {
                let w = ensure(app);
                w.window.present();
                match which {
                    0 => dialogs::new_chat(&w),
                    1 => dialogs::add_account(&w),
                    2 => dialogs::accounts(&w),
                    4 => dialogs::join_room(&w),
                    5 => crate::prefs::show(&w),
                    6 => dialogs::quit_fully(&w),
                    _ => dialogs::about(&w),
                }
            }),
            None,
        );
    }
    {
        let ensure = ensure.clone();
        action(
            "open-conversation",
            Rc::new(move |app, p| {
                let w = ensure(app);
                w.window.present();
                if let Some(path) = p.and_then(|p| p.get::<String>()) {
                    w.open_conversation(&path);
                }
            }),
            Some(glib::VariantTy::STRING),
        );
    }
    {
        // From a call notification.
        let ensure = ensure.clone();
        action(
            "open-call",
            Rc::new(move |app, p| {
                let w = ensure(app);
                w.window.present();
                if let Some(path) = p
                    .and_then(|p| p.get::<String>())
                    .and_then(|p| zbus::zvariant::OwnedObjectPath::try_from(p).ok())
                {
                    let w = w.clone();
                    glib::spawn_future_local(async move { calls::open(&w, path).await });
                }
            }),
            Some(glib::VariantTy::STRING),
        );
    }
    app.set_accels_for_action("app.new-chat", &["<Control>n"]);
    app.set_accels_for_action("app.join-room", &["<Control>j"]);
    app.set_accels_for_action("app.preferences", &["<Control>comma"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate({
        let app = app.clone();
        move |_, _| app.quit()
    });
    app.add_action(&quit);

    app.run()
}

/// Opens (or starts) a chat with `jid` on the first account.
fn open_address(w: &Rc<window::Window>, jid: String) {
    let w = w.clone();
    glib::spawn_future_local(async move {
        // Accounts load asynchronously right after the window appears.
        for _ in 0..50 {
            if !w.accounts.borrow().is_empty() {
                break;
            }
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
        }
        let Some(account) = w.accounts.borrow().first().cloned() else {
            w.toast("Add an account first");
            return;
        };
        let proxy = account.proxy.clone();
        match client::run(async move { proxy.start_chat(&jid).await }).await {
            Ok(path) => {
                w.add_conversation(path.clone()).await;
                w.open_conversation(path.as_str());
            }
            Err(e) => w.toast(&client::explain(&e)),
        }
    });
}

use adw::{gio, glib, gtk};
