//! Starts the real daemon binary on a session bus and talks to it like the UI would.
//!
//! Each test starts its own dbus-daemon (skipped if it isn't installed). None of
//! these tests needs a real XMPP server.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{TestDaemon, wait_for};
use piopio_dbus::{ACCOUNT_API_VERSION, DAEMON_API_VERSION};

#[tokio::test]
async fn daemon_answers_and_quits() {
    let Some(mut test) = TestDaemon::start().await else {
        return;
    };
    let daemon = test.daemon().await;
    assert_eq!(daemon.api_version().await.unwrap(), DAEMON_API_VERSION);

    daemon.quit().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), test.child.wait())
        .await
        .expect("daemon exits after Quit")
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn add_account_rejects_bad_jids() {
    let Some(test) = TestDaemon::start().await else {
        return;
    };
    let daemon = test.daemon().await;
    for jid in ["not a jid@@", "example.org"] {
        let err = daemon
            .add_account(jid, "pw", HashMap::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, zbus::Error::MethodError(ref name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"),
            "{jid}: {err:?}"
        );
    }
}

#[tokio::test]
async fn unreachable_server_is_reported_and_account_can_be_removed() {
    let Some(test) = TestDaemon::start().await else {
        return;
    };
    let daemon = test.daemon().await;

    // .invalid never resolves, so the connection attempt fails quickly.
    let path = daemon
        .add_account("someone@example.invalid", "pw", HashMap::new())
        .await
        .unwrap();
    let account = test.account(path.clone()).await;
    assert_eq!(account.jid().await.unwrap(), "someone@example.invalid");
    assert_eq!(account.api_version().await.unwrap(), ACCOUNT_API_VERSION);
    assert!(account.enabled().await.unwrap());

    let error = wait_for(20, "a connection error", || async {
        let e = account.error().await.unwrap();
        (!e.is_empty()).then_some(e)
    })
    .await;
    eprintln!("reported error: {error}");
    assert_eq!(
        account.state().await.unwrap(),
        "connecting",
        "it keeps retrying"
    );

    let dup = daemon
        .add_account("someone@example.invalid", "pw", HashMap::new())
        .await;
    assert!(dup.is_err(), "the same JID can't be added twice");

    daemon.remove_account(&path.as_ref()).await.unwrap();
    assert!(account.state().await.is_err(), "the object is gone");
}
