//! Piopio UI: shows what the daemon knows and sends the user's actions to it.
//!
//! The UI holds no protocol state and no connections of its own. Closing the
//! window ends this process and frees its memory; the daemon keeps running.

mod chat;
mod client;
mod dialogs;
mod encryption;
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

const CSS: &str = r#"
.chat-history { background: none; padding: 6px 12px; }
.chat-history > row { padding: 2px 0; background: none; }
.bubble { border-radius: 14px; padding: 6px 10px; margin: 0 6px; }
.bubble.incoming { background-color: alpha(currentColor, 0.08); margin-right: 48px; }
.bubble.outgoing {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  margin-left: 48px;
}
.bubble.outgoing .dim-label, .bubble.outgoing .message-state { color: inherit; opacity: 0.75; }
.unread-badge {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  border-radius: 999px; padding: 0 7px; min-width: 10px;
  font-size: smaller; font-weight: bold;
}
row.unread .title { font-weight: bold; }
.undecryptable { font-style: italic; opacity: 0.7; }
.bubble .quote {
  border-left: 3px solid alpha(currentColor, 0.5);
  padding-left: 6px; margin-bottom: 2px; opacity: 0.85;
}
.reactions { margin: 0 6px; }
button.reaction { padding: 0 8px; min-height: 24px; border-radius: 999px; font-size: smaller; }
.fingerprint { font-family: monospace; }
"#;

fn main() -> glib::ExitCode {
    // The D-Bus runtime starts before GTK so its thread is ready for the first call.
    let _ = client::runtime();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    let window: Rc<RefCell<Option<Rc<window::Window>>>> = Rc::default();

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        #[cfg(feature = "gnome-51")]
        provider.load_from_string(CSS);
        #[cfg(not(feature = "gnome-51"))]
        #[allow(deprecated)]
        provider.load_from_data(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

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
    app.set_accels_for_action("app.new-chat", &["<Control>n"]);
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

use adw::{gdk, gio, glib, gtk};
