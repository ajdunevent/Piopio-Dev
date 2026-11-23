//! Piopio UI: displays what the daemon knows and sends the user's actions to it.
//!
//! The UI holds no protocol state. Closing the window ends this process; the
//! daemon keeps running in the background.

use adw::prelude::*;
use piopio_dbus::APP_ID;

fn main() -> adw::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    let status = adw::StatusPage::builder()
        .icon_name(APP_ID)
        .title("Piopio")
        .description("Nothing to show yet.")
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));

    adw::ApplicationWindow::builder()
        .application(app)
        .title("Piopio")
        .default_width(420)
        .default_height(640)
        .content(&toolbar)
        .build()
        .present();
}
