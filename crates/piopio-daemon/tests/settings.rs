//! App settings and conversations' notification settings.

mod common;

use std::io::Write;

use common::{TestDaemon, test_server};

#[tokio::test]
async fn app_settings() {
    let daemon = TestDaemon::start().await.unwrap();
    let proxy = daemon.daemon().await;
    assert!(proxy.run_in_background().await.unwrap());
    assert!(proxy.start_at_login().await.unwrap());
    assert_eq!(proxy.notification_sound().await.unwrap(), "default");

    // Starting at login needs running in the background.
    proxy.set_run_in_background(false).await.unwrap();
    assert!(!proxy.start_at_login().await.unwrap());
    proxy.set_run_in_background(true).await.unwrap();
    assert!(proxy.start_at_login().await.unwrap());

    proxy.set_notification_sound("silent").await.unwrap();
    assert_eq!(proxy.notification_sound().await.unwrap(), "silent");
    assert!(proxy.set_notification_sound("loud").await.is_err());

    let mut sound = tempfile::tempfile().unwrap();
    sound.write_all(b"RIFF....WAVEfmt ").unwrap();
    proxy
        .set_custom_sound(zbus::zvariant::Fd::from(&sound))
        .await
        .unwrap();
    assert_eq!(proxy.notification_sound().await.unwrap(), "custom");
}

#[tokio::test]
async fn conversation_notification_settings() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let daemon = TestDaemon::start().await.unwrap();
    let (_, account) = daemon.add_local("nora").await;
    let chat = daemon
        .conversation(account.start_chat("owen@localhost").await.unwrap())
        .await;
    assert_eq!(chat.notification_sound().await.unwrap(), "default");
    assert!(!chat.override_quiet().await.unwrap());
    chat.set_override_quiet(true).await.unwrap();
    assert!(chat.override_quiet().await.unwrap());
    let mut sound = tempfile::tempfile().unwrap();
    sound.write_all(b"OggS").unwrap();
    chat.set_custom_sound(zbus::zvariant::Fd::from(&sound))
        .await
        .unwrap();
    assert_eq!(chat.notification_sound().await.unwrap(), "custom");
    chat.set_notification_sound("silent").await.unwrap();
    assert_eq!(chat.notification_sound().await.unwrap(), "silent");
}
