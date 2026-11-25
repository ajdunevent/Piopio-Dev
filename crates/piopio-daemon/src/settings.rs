//! App-wide settings, kept in the database: whether the daemon starts at
//! login, keeps running with no window, and warns before quitting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::store::Store;

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

pub struct Settings {
    store: Store,
    start_at_login: AtomicBool,
    run_in_background: AtomicBool,
    warn_on_quit: AtomicBool,
}

/// The settings, once [`load`] has run.
pub fn get() -> Option<&'static Arc<Settings>> {
    SETTINGS.get()
}

pub async fn load(store: Store) -> Arc<Settings> {
    let flag = |v: Option<String>| v.is_none_or(|v| v == "true");
    let settings = Arc::new(Settings {
        start_at_login: AtomicBool::new(flag(store.setting("start-at-login").await.ok().flatten())),
        run_in_background: AtomicBool::new(flag(
            store.setting("run-in-background").await.ok().flatten(),
        )),
        warn_on_quit: AtomicBool::new(flag(store.setting("warn-on-quit").await.ok().flatten())),
        store,
    });
    let _ = SETTINGS.set(settings.clone());
    settings
}

impl Settings {
    pub fn start_at_login(&self) -> bool {
        // Starting at login only makes sense when running in the background.
        self.start_at_login.load(Ordering::SeqCst) && self.run_in_background()
    }

    pub fn run_in_background(&self) -> bool {
        self.run_in_background.load(Ordering::SeqCst)
    }

    pub fn warn_on_quit(&self) -> bool {
        self.warn_on_quit.load(Ordering::SeqCst)
    }

    pub async fn set_warn_on_quit(&self, on: bool) {
        self.warn_on_quit.store(on, Ordering::SeqCst);
        let _ = self.store.set_setting("warn-on-quit", on.to_string()).await;
    }

    pub async fn set_start_at_login(&self, on: bool) {
        self.start_at_login.store(on, Ordering::SeqCst);
        let _ = self
            .store
            .set_setting("start-at-login", on.to_string())
            .await;
    }

    pub async fn set_run_in_background(&self, on: bool) {
        self.run_in_background.store(on, Ordering::SeqCst);
        let _ = self
            .store
            .set_setting("run-in-background", on.to_string())
            .await;
    }
}
