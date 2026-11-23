//! App-wide settings, kept in the database: whether the daemon starts at
//! login and keeps running with no window, and the default notification
//! sound. Custom sounds are copied into the data directory.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::store::Store;

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();

pub struct Settings {
    store: Store,
    start_at_login: AtomicBool,
    run_in_background: AtomicBool,
    /// "default", "silent" or "custom" (the file from [`sound_file`]).
    sound: RwLock<String>,
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
        sound: RwLock::new(
            store
                .setting("notification-sound")
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "default".into()),
        ),
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

    pub fn sound(&self) -> String {
        self.sound.read().expect("lock").clone()
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

    pub async fn set_sound(&self, sound: &str) {
        *self.sound.write().expect("lock") = sound.to_owned();
        let _ = self
            .store
            .set_setting("notification-sound", sound.to_owned())
            .await;
    }
}

/// Where a custom notification sound is kept: "default" for the app-wide
/// one, or a conversation's id.
pub fn sound_file(owner: &str) -> PathBuf {
    crate::store::data_dir().join("sounds").join(owner)
}

/// Saves a custom sound read from `fd` (at most 2 MiB).
pub async fn save_sound(owner: String, fd: std::os::fd::OwnedFd) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::{Read, Seek};
        let mut data = Vec::new();
        let mut file = std::fs::File::from(fd);
        // From the start, wherever the sender left it (pipes can't seek).
        let _ = file.rewind();
        file.take(2 * 1024 * 1024 + 1).read_to_end(&mut data)?;
        anyhow::ensure!(
            data.len() <= 2 * 1024 * 1024,
            "the sound is larger than 2 MiB"
        );
        anyhow::ensure!(!data.is_empty(), "the sound file is empty");
        let path = sound_file(&owner);
        std::fs::create_dir_all(path.parent().expect("has a parent"))?;
        std::fs::write(path, data)?;
        Ok(())
    })
    .await?
}
