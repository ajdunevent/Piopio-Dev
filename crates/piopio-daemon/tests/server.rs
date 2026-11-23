//! Logs in to a real XMPP server with the test account.
//!
//! Ignored by default. CI runs it with `cargo test -- --ignored` when the
//! PIOPIO_TEST_JID and PIOPIO_TEST_PASSWORD secrets are set. Locally:
//!
//!   PIOPIO_TEST_JID=... PIOPIO_TEST_PASSWORD=... \
//!     cargo test -p piopio-daemon --test server -- --ignored

mod common;

use std::collections::HashMap;

use common::{TestDaemon, wait_for};

fn credentials() -> (String, String) {
    let jid = std::env::var("PIOPIO_TEST_JID").expect("PIOPIO_TEST_JID is set");
    let password = std::env::var("PIOPIO_TEST_PASSWORD").expect("PIOPIO_TEST_PASSWORD is set");
    (jid, password)
}

#[tokio::test]
#[ignore = "needs the test account on a real server"]
async fn logs_in_to_the_test_server() {
    let (jid, password) = credentials();
    let Some(test) = TestDaemon::start().await else {
        panic!("integration tests need dbus-daemon");
    };
    let daemon = test.daemon().await;

    let path = daemon
        .add_account(&jid, &password, HashMap::new())
        .await
        .unwrap();
    let account = test.account(path.clone()).await;

    let state = wait_for(60, "the account to connect or fail", || async {
        let state = account.state().await.unwrap();
        (state == "connected" || state == "error").then_some(state)
    })
    .await;
    let error = account.error().await.unwrap();
    assert_eq!(state, "connected", "login failed: {error}");
    assert!(
        error.is_empty(),
        "Error should be cleared once connected: {error}"
    );
    // OMEMO: the device and bundle publish on a real server too. Removing
    // the account takes the device out of the list again, so test runs
    // don't pile up dead devices on the test account.
    test.wait_published(&path).await;

    daemon.remove_account(&path.as_ref()).await.unwrap();
}
