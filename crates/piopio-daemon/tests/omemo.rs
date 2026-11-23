//! OMEMO between Piopio daemons on the local test server: encryption by
//! default, Blind Trust Before Verification, and refusing untrusted devices.
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test omemo

mod common;

use std::collections::HashMap;

use common::{TestDaemon, message, test_server, text, wait_for};

#[tokio::test]
async fn encryption_and_trust() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let erin = TestDaemon::start().await.unwrap();
    let frank = TestDaemon::start().await.unwrap();
    let (erin_path, erin_account) = erin.add_local("erin").await;
    let (frank_path, _) = frank.add_local("frank").await;
    let erin_device = erin.wait_published(&erin_path).await;
    frank.wait_published(&frank_path).await;

    // Erin opens a chat: it is encrypted before anything is sent.
    let erin_chat = erin
        .conversation(erin_account.start_chat("frank@localhost").await.unwrap())
        .await;
    wait_for(20, "the conversation to use OMEMO", || async {
        (erin_chat.encryption().await.unwrap() == "omemo").then_some(())
    })
    .await;
    let sent = erin_chat
        .send("Only for you", HashMap::new())
        .await
        .unwrap();

    let frank_chat = wait_for(20, "Frank's conversation", || async {
        frank.conversations(&frank_path).await.into_iter().next()
    })
    .await;
    let frank_chat = frank.conversation(frank_chat).await;
    let got = wait_for(20, "Frank to receive it", || async {
        frank_chat
            .get_messages("", 10)
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(text(&got, "body"), "Only for you");
    assert_eq!(text(&got, "encryption"), "omemo");
    let mine = erin_chat.get_messages("", 10).await.unwrap();
    let mine = message(&mine, &sent).unwrap();
    assert_eq!(text(mine, "encryption"), "omemo");

    // Frank answers over the session Erin started.
    frank_chat.send("Understood", HashMap::new()).await.unwrap();
    wait_for(20, "Erin to read the answer", || async {
        let m = erin_chat.get_messages("", 10).await.unwrap();
        m.iter()
            .any(|m| text(m, "body") == "Understood" && text(m, "encryption") == "omemo")
            .then_some(())
    })
    .await;

    // Frank trusted Erin's device blindly; its fingerprint matches hers.
    let frank_omemo = frank.omemo(frank_path.clone()).await;
    let device = frank
        .wait_device(&frank_path, "erin@localhost", erin_device)
        .await;
    assert_eq!(text(&device, "trust"), "blind");
    let erin_fingerprint = erin
        .omemo(erin_path.clone())
        .await
        .fingerprint()
        .await
        .unwrap();
    assert_eq!(text(&device, "fingerprint"), erin_fingerprint);
    assert_eq!(erin_fingerprint.len(), 64 + 7);

    // Frank verifies it. From then on Erin's new devices are not trusted
    // until he decides: a message from her new tablet shows as untrusted.
    frank_omemo
        .set_trust("erin@localhost", erin_device, "verified")
        .await
        .unwrap();
    let tablet = TestDaemon::start().await.unwrap();
    let (tablet_path, tablet_account) = tablet.add_local("erin").await;
    let tablet_device = tablet.wait_published(&tablet_path).await;
    let tablet_chat = tablet
        .conversation(tablet_account.start_chat("frank@localhost").await.unwrap())
        .await;
    tablet_chat
        .send("From my tablet", HashMap::new())
        .await
        .unwrap();
    wait_for(20, "Frank to get the tablet's message", || async {
        let m = frank_chat.get_messages("", 10).await.unwrap();
        m.iter()
            .any(|m| {
                text(m, "body") == "From my tablet" && text(m, "encryption") == "omemo-untrusted"
            })
            .then_some(())
    })
    .await;
    let new_device = frank
        .wait_device(&frank_path, "erin@localhost", tablet_device)
        .await;
    assert_eq!(text(&new_device, "trust"), "undecided");

    // Frank distrusts Erin's first device too: with no trusted device left,
    // his message is not sent at all rather than sent in the clear.
    frank_omemo
        .set_trust("erin@localhost", erin_device, "untrusted")
        .await
        .unwrap();
    let refused = frank_chat
        .send("Nobody can read this", HashMap::new())
        .await
        .unwrap();
    wait_for(20, "the message to fail", || async {
        let m = frank_chat.get_messages("", 10).await.unwrap();
        message(&m, &refused)
            .filter(|m| text(m, "state") == "failed")
            .map(|_| ())
    })
    .await;
}

/// Removing an account takes its device out of the published list.
#[tokio::test]
async fn removed_account_retires_its_device() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    // Uses its own JIDs' device lists only through the server, so pick a
    // user no other test adds devices for concurrently: frank's list is read
    // by erin's test, but only for erin's devices.
    let one = TestDaemon::start().await.unwrap();
    let two = TestDaemon::start().await.unwrap();
    let (one_path, _) = one.add_local("bob").await;
    let (two_path, _) = two.add_local("bob").await;
    let one_device = one.wait_published(&one_path).await;
    two.wait_published(&two_path).await;
    two.wait_device(&two_path, "bob@localhost", one_device)
        .await;

    one.daemon()
        .await
        .remove_account(&one_path.as_ref())
        .await
        .unwrap();
    let omemo = two.omemo(two_path.clone()).await;
    wait_for(20, "the removed device to become inactive", || async {
        omemo
            .get_devices("bob@localhost")
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.get("device-id").and_then(|v| u32::try_from(v).ok()) == Some(one_device))
            .filter(|d| d.get("active").and_then(|v| bool::try_from(v).ok()) == Some(false))
            .map(|_| ())
    })
    .await;
}
