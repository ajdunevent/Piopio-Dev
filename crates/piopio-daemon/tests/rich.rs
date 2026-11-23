//! Replies, corrections, reactions and retractions between two daemons,
//! over OMEMO (which both use by default).
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test rich

mod common;

use std::collections::HashMap;

use common::{TestDaemon, message, test_server, text, wait_for};
use zbus::zvariant::Value;

fn reactions(m: &piopio_dbus::Dict) -> HashMap<String, Vec<String>> {
    m.get("reactions")
        .and_then(|v| HashMap::<String, Vec<String>>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn flag(m: &piopio_dbus::Dict, key: &str) -> bool {
    m.get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

#[tokio::test]
async fn reply_correct_react_retract() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let alice = TestDaemon::start().await.unwrap();
    let bob = TestDaemon::start().await.unwrap();
    let (alice_path, alice_account) = alice.add_local("alice").await;
    let (bob_path, _) = bob.add_local("bob").await;
    let alice_device = alice.wait_published(&alice_path).await;
    let bob_device = bob.wait_published(&bob_path).await;
    // Each must know the other's new device (from PEP notifications), or
    // the first messages are encrypted only for their older devices.
    alice
        .wait_device(&alice_path, "bob@localhost", bob_device)
        .await;
    bob.wait_device(&bob_path, "alice@localhost", alice_device)
        .await;

    let alice_chat = alice
        .conversation(alice_account.start_chat("bob@localhost").await.unwrap())
        .await;
    let hello = alice_chat.send("Hello", HashMap::new()).await.unwrap();
    let bob_chat = wait_for(20, "Bob's conversation", || async {
        bob.conversation_with(&bob_path, "alice@localhost").await
    })
    .await;
    let bob_chat = bob.conversation(bob_chat).await;
    // Bob's first sync may bring older history from other tests: find
    // this message by the id it travelled with.
    let origin = alice_chat.get_messages("", 50).await.unwrap();
    let origin = text(message(&origin, &hello).unwrap(), "origin-id");
    let bobs_copy = wait_for(20, "Bob to receive Hello", || async {
        let m = bob_chat.get_messages("", 50).await.unwrap();
        m.into_iter().find(|m| text(m, "origin-id") == origin)
    })
    .await;
    assert_eq!(text(&bobs_copy, "body"), "Hello");
    assert_eq!(text(&bobs_copy, "encryption"), "omemo2");
    let bobs_id = text(&bobs_copy, "id");

    // Bob replies; Alice sees the reply without the quote, and what it answers.
    let options = HashMap::from([("reply-to", Value::from(bobs_id.as_str()))]);
    bob_chat.send("Hi back", options).await.unwrap();
    let reply = wait_for(20, "Alice to get the reply", || async {
        let m = alice_chat.get_messages("", 50).await.unwrap();
        m.into_iter().find(|m| text(m, "body") == "Hi back")
    })
    .await;
    let alices_hello = alice_chat.get_messages("", 50).await.unwrap();
    let alices_hello = message(&alices_hello, &hello).unwrap().clone();
    assert_eq!(text(&reply, "reply-to"), text(&alices_hello, "origin-id"));
    assert_eq!(text(&reply, "reply-to-sender"), "alice@localhost");
    assert_eq!(text(&reply, "reply-preview"), "Hello");

    // Alice corrects her message: Bob's copy changes, no new message appears.
    alice_chat.correct(&hello, "Hello there").await.unwrap();
    wait_for(20, "Bob to see the correction", || async {
        let m = bob_chat.get_messages("", 50).await.unwrap();
        message(&m, &bobs_id)
            .filter(|m| text(m, "body") == "Hello there" && flag(m, "edited"))
            .map(|_| ())
    })
    .await;
    let bodies: Vec<String> = bob_chat
        .get_messages("", 50)
        .await
        .unwrap()
        .iter()
        .map(|m| text(m, "body"))
        .collect();
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b.as_str() == "Hello there")
            .count(),
        1
    );

    // Bob reacts; Alice sees it on her message.
    bob_chat.react(&bobs_id, &["👍", "🎉"]).await.unwrap();
    wait_for(20, "Alice to see the reactions", || async {
        let m = alice_chat.get_messages("", 50).await.unwrap();
        let r = reactions(message(&m, &hello)?);
        (r.get("bob@localhost") == Some(&vec!["👍".to_owned(), "🎉".to_owned()])).then_some(())
    })
    .await;
    // Bob can't correct or retract Alice's message.
    assert!(bob_chat.correct(&bobs_id, "Forged").await.is_err());
    assert!(bob_chat.retract(&bobs_id).await.is_err());

    // Alice retracts it: gone on both sides, reactions too.
    alice_chat.retract(&hello).await.unwrap();
    wait_for(20, "Bob to see the retraction", || async {
        let m = bob_chat.get_messages("", 50).await.unwrap();
        message(&m, &bobs_id)
            .filter(|m| flag(m, "retracted") && text(m, "body").is_empty())
            .map(|_| ())
    })
    .await;
    let mine = alice_chat.get_messages("", 50).await.unwrap();
    let mine = message(&mine, &hello).unwrap();
    assert!(flag(mine, "retracted"));
    assert!(reactions(mine).is_empty());
}
