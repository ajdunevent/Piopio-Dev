//! The first sync of a new account.

mod common;

use std::collections::HashMap;

use common::{TestDaemon, test_server, text, wait_for};

/// A new device of an account already in use: more than one page of
/// history from the archive arrives, and none of it counts as unread.
#[tokio::test]
async fn first_sync_goes_back_more_than_a_page() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    // Quinn's phone is online, so nothing waits as an offline message.
    let phone = TestDaemon::start().await.unwrap();
    let (phone_path, _) = phone.add_local("quinn").await;
    let pat = TestDaemon::start().await.unwrap();
    let (_, pat_account) = pat.add_local("pat").await;
    let chat = pat
        .conversation(pat_account.start_chat("quinn@localhost").await.unwrap())
        .await;
    for i in 0..150 {
        chat.send(&format!("message {i}"), HashMap::new())
            .await
            .unwrap();
    }
    wait_for(30, "everything sent", || async {
        let m = chat.get_messages("", 200).await.unwrap();
        m.iter()
            .all(|m| text(m, "state") != "pending")
            .then_some(())
    })
    .await;

    let on_phone = wait_for(30, "the phone's conversation", || async {
        phone.conversation_with(&phone_path, "pat@localhost").await
    })
    .await;
    let on_phone = phone.conversation(on_phone).await;
    wait_for(30, "the phone to get everything", || async {
        (on_phone.get_messages("", 200).await.unwrap().len() >= 150).then_some(())
    })
    .await;

    // A new laptop signs in to Quinn's account.
    let quinn = TestDaemon::start().await.unwrap();
    let (quinn_path, _) = quinn.add_local("quinn").await;
    let theirs = wait_for(30, "Quinn's conversation with Pat", || async {
        quinn.conversation_with(&quinn_path, "pat@localhost").await
    })
    .await;
    let theirs = quinn.conversation(theirs).await;
    wait_for(30, "all 150 messages", || async {
        let m = theirs.get_messages("", 200).await.unwrap();
        // Encrypted for the phone only, so the laptop can't read them; it
        // still has every one.
        (m.len() >= 150).then_some(())
    })
    .await;
    assert_eq!(theirs.unread().await.unwrap(), 0, "history isn't unread");
}
