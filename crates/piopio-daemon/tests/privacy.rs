//! Blocking, and the typing and read-receipt settings.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{TestDaemon, test_server, text, wait_for};

/// Blocking someone hides their messages; unblocking lets them through.
#[tokio::test]
async fn blocking() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let kim = TestDaemon::start().await.unwrap();
    let leo = TestDaemon::start().await.unwrap();
    let (_, kim_account) = kim.add_local("kim").await;
    let (_, leo_account) = leo.add_local("leo").await;
    let kim_chat = kim
        .conversation(kim_account.start_chat("leo@localhost").await.unwrap())
        .await;
    let leo_chat = leo
        .conversation(leo_account.start_chat("kim@localhost").await.unwrap())
        .await;
    assert!(!kim_chat.blocked().await.unwrap());

    kim_account.block("leo@localhost").await.unwrap();
    wait_for(10, "Blocked", || async {
        kim_chat.blocked().await.unwrap().then_some(())
    })
    .await;
    leo_chat
        .send("Can you hear me?", HashMap::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let seen = kim_chat.get_messages("", 10).await.unwrap();
    assert!(
        !seen.iter().any(|m| text(m, "body") == "Can you hear me?"),
        "a blocked contact's message arrived"
    );

    kim_account.unblock("leo@localhost").await.unwrap();
    wait_for(10, "not Blocked", || async {
        (!kim_chat.blocked().await.unwrap()).then_some(())
    })
    .await;
    leo_chat
        .send("How about now?", HashMap::new())
        .await
        .unwrap();
    wait_for(20, "the message after unblocking", || async {
        let m = kim_chat.get_messages("", 10).await.unwrap();
        m.iter()
            .any(|m| text(m, "body") == "How about now?")
            .then_some(())
    })
    .await;
}

/// A conversation follows the account's default unless it says otherwise.
#[tokio::test]
async fn typing_and_receipt_settings() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let mia = TestDaemon::start().await.unwrap();
    let (_, account) = mia.add_local("mia").await;
    let chat = mia
        .conversation(account.start_chat("kim@localhost").await.unwrap())
        .await;
    assert!(account.send_chat_states().await.unwrap());
    assert_eq!(chat.send_chat_states().await.unwrap(), "default");
    assert!(chat.chat_states_enabled().await.unwrap());

    account.set_send_chat_states(false).await.unwrap();
    account.set_send_receipts(false).await.unwrap();
    assert!(!chat.chat_states_enabled().await.unwrap());
    assert!(!chat.receipts_enabled().await.unwrap());

    chat.set_send_chat_states("on").await.unwrap();
    assert!(chat.chat_states_enabled().await.unwrap());
    assert!(!chat.receipts_enabled().await.unwrap());
    assert!(chat.set_send_receipts("sometimes").await.is_err());
}
