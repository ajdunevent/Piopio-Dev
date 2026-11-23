//! End-to-end tests against a throwaway Prosody on this machine.
//!
//! Run them through the server wrapper, which starts Prosody and sets
//! PIOPIO_TEST_SERVER; without it these tests are skipped:
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test local_server

mod common;

use std::collections::HashMap;

use common::{TestDaemon, test_server, text, wait_for, wait_until_connected};

/// Two people become contacts, chat, and see receipts and read markers.
#[tokio::test]
async fn contacts_chat_receipts_and_markers() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let alice = TestDaemon::start().await.unwrap();
    let bob = TestDaemon::start().await.unwrap();
    let (alice_path, _) = alice.add_local("alice").await;
    let (bob_path, _) = bob.add_local("bob").await;
    let alice_roster = alice.roster(alice_path.clone()).await;
    let bob_roster = bob.roster(bob_path.clone()).await;

    // Alice adds Bob; Bob sees the request and approves (and asks back).
    alice_roster
        .add_contact("bob@localhost", "Bob", &[])
        .await
        .unwrap();
    wait_for(20, "Bob to see Alice's request", || async {
        let contacts = bob_roster.get_contacts().await.unwrap();
        contacts
            .iter()
            .any(|c| {
                text(c, "jid") == "alice@localhost"
                    && c.get("pending-in")
                        .is_some_and(|v| bool::try_from(v).unwrap_or(false))
            })
            .then_some(())
    })
    .await;
    bob_roster
        .approve_subscription("alice@localhost")
        .await
        .unwrap();
    wait_for(20, "Alice to see Bob's request back", || async {
        let contacts = alice_roster.get_contacts().await.unwrap();
        contacts
            .iter()
            .any(|c| {
                text(c, "jid") == "bob@localhost"
                    && c.get("pending-in")
                        .is_some_and(|v| bool::try_from(v).unwrap_or(false))
            })
            .then_some(())
    })
    .await;
    alice_roster
        .approve_subscription("bob@localhost")
        .await
        .unwrap();
    for (roster, peer) in [
        (&alice_roster, "bob@localhost"),
        (&bob_roster, "alice@localhost"),
    ] {
        wait_for(20, "a mutual subscription", || async {
            let contacts = roster.get_contacts().await.unwrap();
            contacts
                .iter()
                .find(|c| text(c, "jid") == peer)
                .filter(|c| text(c, "subscription") == "both" && text(c, "presence") != "offline")
                .map(|_| ())
        })
        .await;
    }
    let bob_entry = alice_roster
        .get_contacts()
        .await
        .unwrap()
        .into_iter()
        .find(|c| text(c, "jid") == "bob@localhost")
        .unwrap();
    assert_eq!(text(&bob_entry, "name"), "Bob");

    let alice_device = alice.wait_published(&alice_path).await;
    let bob_device = bob.wait_published(&bob_path).await;
    alice
        .wait_device(&alice_path, "bob@localhost", bob_device)
        .await;
    bob.wait_device(&bob_path, "alice@localhost", alice_device)
        .await;

    // Alice writes; Bob receives; Alice sees the receipt. Both use OMEMO,
    // so the message is encrypted without anyone asking for it.
    let alice_account = alice.account(alice_path.clone()).await;
    let chat_path = alice_account.start_chat("bob@localhost").await.unwrap();
    let alice_chat = alice.conversation(chat_path).await;
    assert_eq!(alice_chat.title().await.unwrap(), "Bob");
    let sent_id = alice_chat.send("Hello Bob", HashMap::new()).await.unwrap();

    let bob_chat_path = wait_for(20, "Bob's conversation with Alice", || async {
        bob.conversation_with(&bob_path, "alice@localhost").await
    })
    .await;
    let bob_chat = bob.conversation(bob_chat_path).await;
    assert_eq!(bob_chat.peer().await.unwrap(), "alice@localhost");
    let received = wait_for(20, "Bob to receive the message", || async {
        // Older history from other tests may come first.
        bob_chat
            .get_messages("", 50)
            .await
            .unwrap()
            .into_iter()
            .find(|m| text(m, "body") == "Hello Bob")
    })
    .await;
    assert_eq!(text(&received, "body"), "Hello Bob");
    assert_eq!(text(&received, "encryption"), "omemo2");
    assert!(!bool::try_from(received.get("outgoing").unwrap()).unwrap());
    assert_eq!(bob_chat.unread().await.unwrap(), 1);

    let state_of = |id: String| {
        let chat = &alice_chat;
        async move {
            chat.get_messages("", 50)
                .await
                .unwrap()
                .into_iter()
                .find(|m| text(m, "id") == id)
                .map(|m| text(&m, "state"))
                .unwrap_or_default()
        }
    };
    wait_for(20, "the delivery receipt", || async {
        (state_of(sent_id.clone()).await == "received").then_some(())
    })
    .await;

    // Bob reads it; Alice sees the displayed marker.
    bob_chat.mark_read().await.unwrap();
    assert_eq!(bob_chat.unread().await.unwrap(), 0);
    wait_for(20, "the displayed marker", || async {
        (state_of(sent_id.clone()).await == "displayed").then_some(())
    })
    .await;

    // Bob answers; Alice's conversation shows both, oldest first.
    bob_chat.send("Hi Alice", HashMap::new()).await.unwrap();
    wait_for(20, "Alice to receive the answer", || async {
        let messages = alice_chat.get_messages("", 50).await.unwrap();
        messages
            .iter()
            .any(|m| text(m, "body") == "Hi Alice")
            .then_some(())
    })
    .await;
    let messages = alice_chat.get_messages("", 50).await.unwrap();
    let messages = &messages[messages.len() - 2..];
    assert_eq!(text(&messages[0], "body"), "Hello Bob");
    assert_eq!(text(&messages[1], "body"), "Hi Alice");
    for m in messages {
        assert_eq!(text(m, "encryption"), "omemo2", "{m:?}");
    }
    assert_eq!(alice_chat.encryption().await.unwrap(), "omemo2");
    assert_eq!(alice_chat.unread().await.unwrap(), 1);

    // Typing notifications: Bob sees Alice typing, and stop when she sends.
    alice_chat.set_chat_state("composing").await.unwrap();
    wait_for(20, "Bob to see Alice typing", || async {
        (bob_chat.peer_state().await.unwrap() == "composing").then_some(())
    })
    .await;
    alice_chat.send("Typed it", HashMap::new()).await.unwrap();
    wait_for(20, "the typing to end", || async {
        bob_chat
            .peer_state()
            .await
            .unwrap()
            .is_empty()
            .then_some(())
    })
    .await;
}

/// A second device sees sent messages through Carbons, a new device gets
/// history from the archive, and a restarted daemon keeps everything.
#[tokio::test]
async fn carbons_archive_and_restart() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let phone = TestDaemon::start().await.unwrap();
    let laptop = TestDaemon::start().await.unwrap();
    let dave = TestDaemon::start().await.unwrap();
    let (phone_path, phone_account) = phone.add_local("carol").await;
    let (laptop_path, _) = laptop.add_local("carol").await;
    let (dave_path, _) = dave.add_local("dave").await;
    phone.wait_published(&phone_path).await;
    let laptop_device = laptop.wait_published(&laptop_path).await;
    dave.wait_published(&dave_path).await;
    // The phone must know the laptop to encrypt for it too.
    phone
        .wait_device(&phone_path, "carol@localhost", laptop_device)
        .await;

    // Carol writes from her phone; her laptop sees it as a carbon.
    let chat = phone
        .conversation(phone_account.start_chat("dave@localhost").await.unwrap())
        .await;
    chat.send("Sent from my phone", HashMap::new())
        .await
        .unwrap();
    let laptop_chat = wait_for(20, "the carbon on the laptop", || async {
        laptop
            .conversation_with(&laptop_path, "dave@localhost")
            .await
    })
    .await;
    let laptop_chat = laptop.conversation(laptop_chat).await;
    let copy = wait_for(20, "the carbon's message", || async {
        laptop_chat
            .get_messages("", 10)
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(text(&copy, "body"), "Sent from my phone");
    assert!(bool::try_from(copy.get("outgoing").unwrap()).unwrap());
    assert_eq!(laptop_chat.peer().await.unwrap(), "dave@localhost");

    // Dave answers; both of Carol's devices get it, once each.
    let dave_chat = wait_for(20, "Dave's conversation", || async {
        dave.conversation_with(&dave_path, "carol@localhost").await
    })
    .await;
    let dave_chat = dave.conversation(dave_chat).await;
    dave_chat.send("Got it", HashMap::new()).await.unwrap();
    for c in [&chat, &laptop_chat] {
        wait_for(20, "the answer on both devices", || async {
            let m = c.get_messages("", 10).await.unwrap();
            (m.len() == 2 && text(&m[1], "body") == "Got it").then_some(())
        })
        .await;
    }

    // A brand-new device gets the conversation from the archive.
    let tablet = TestDaemon::start().await.unwrap();
    let (tablet_path, _) = tablet.add_local("carol").await;
    let tablet_chat = wait_for(20, "the archived conversation", || async {
        tablet
            .conversation_with(&tablet_path, "dave@localhost")
            .await
    })
    .await;
    let tablet_chat = tablet.conversation(tablet_chat).await;
    let history = wait_for(20, "archived history", || async {
        let m = tablet_chat.get_messages("", 10).await.unwrap();
        (m.len() >= 2).then_some(m)
    })
    .await;
    // They were encrypted before this device existed, so it cannot read them.
    for m in &history {
        assert_eq!(text(m, "encryption"), "omemo-failed", "{m:?}");
        assert_eq!(text(m, "body"), "");
    }
    assert_eq!(
        tablet_chat.unread().await.unwrap(),
        0,
        "first sync is not unread"
    );

    // The phone's daemon restarts: account, conversation and history survive,
    // and what arrived meanwhile comes from the archive.
    let data = phone.data.clone();
    phone.daemon().await.quit().await.unwrap();
    drop(phone);
    dave_chat
        .send("While you were away", HashMap::new())
        .await
        .unwrap();
    let phone = TestDaemon::start_with(data).await.unwrap();
    let accounts = phone.daemon().await.list_accounts().await.unwrap();
    assert_eq!(accounts.len(), 1);
    wait_until_connected(&phone.account(accounts[0].clone()).await).await;
    let chats = phone.conversations(&accounts[0]).await;
    assert_eq!(chats.len(), 1);
    let chat = phone.conversation(chats[0].clone()).await;
    wait_for(20, "the message that arrived while offline", || async {
        // Archive timestamps have whole-second precision, so this one may
        // sort just before "Got it" when both fall in the same second.
        let m = chat.get_messages("", 10).await.unwrap();
        (m.len() == 3
            && m.iter().any(|m| {
                text(m, "body") == "While you were away" && text(m, "encryption") == "omemo2"
            }))
        .then_some(())
    })
    .await;
    let _ = phone_path;
}
