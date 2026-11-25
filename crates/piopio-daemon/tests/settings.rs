//! App settings and conversations' notification settings.

mod common;

use common::{TestDaemon, test_server};

#[tokio::test]
async fn app_settings() {
    let daemon = TestDaemon::start().await.unwrap();
    let proxy = daemon.daemon().await;
    assert!(proxy.run_in_background().await.unwrap());
    assert!(proxy.start_at_login().await.unwrap());
    assert!(proxy.warn_on_quit().await.unwrap());
    proxy.set_warn_on_quit(false).await.unwrap();
    assert!(!proxy.warn_on_quit().await.unwrap());

    // Starting at login needs running in the background.
    proxy.set_run_in_background(false).await.unwrap();
    assert!(!proxy.start_at_login().await.unwrap());
    proxy.set_run_in_background(true).await.unwrap();
    assert!(proxy.start_at_login().await.unwrap());
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
    assert!(!chat.override_quiet().await.unwrap());
    chat.set_override_quiet(true).await.unwrap();
    assert!(chat.override_quiet().await.unwrap());
}
