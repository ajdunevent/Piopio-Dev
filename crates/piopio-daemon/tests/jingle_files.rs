//! Sending a file straight to a contact's device over Jingle (XEP-0234)
//! with in-band bytestreams, as on a server without an upload service.
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test jingle_files

mod common;

use std::collections::HashMap;
use std::io::{Seek, Write};

use common::{TestDaemon, message, test_server, text, wait_for};

#[tokio::test]
async fn file_over_jingle_without_upload_service() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let no_upload = [("PIOPIO_TEST_NO_UPLOAD", "1")];
    let dir = || std::sync::Arc::new(tempfile::tempdir().unwrap());
    let quinn = TestDaemon::start_with_env(dir(), &no_upload).await.unwrap();
    let rae = TestDaemon::start_with_env(dir(), &no_upload).await.unwrap();
    let (quinn_path, quinn_account) = quinn.add_local("quinn").await;
    let (rae_path, _) = rae.add_local("rae").await;

    // Contacts: Rae takes files from Quinn, and Quinn sees Rae's device.
    let quinn_roster = quinn.roster(quinn_path.clone()).await;
    let rae_roster = rae.roster(rae_path.clone()).await;
    quinn_roster
        .add_contact("rae@localhost", "", &[])
        .await
        .unwrap();
    wait_for(20, "Rae to see Quinn's request", || async {
        let contacts = rae_roster.get_contacts().await.unwrap();
        contacts
            .iter()
            .any(|c| text(c, "jid") == "quinn@localhost")
            .then_some(())
    })
    .await;
    rae_roster
        .approve_subscription("quinn@localhost")
        .await
        .unwrap();

    let chat = quinn
        .conversation(quinn_account.start_chat("rae@localhost").await.unwrap())
        .await;
    // An unencrypted conversation: files only go directly in the clear.
    chat.set_encryption_wanted(false).await.unwrap();
    wait_for(20, "Rae's device to take files", || async {
        let f = chat.peer_features().await.unwrap();
        f.iter()
            .any(|f| f == "urn:xmpp:jingle:apps:file-transfer:5")
            .then_some(())
    })
    .await;

    // A few blocks' worth, not a multiple of the block size.
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&data).unwrap();
    file.rewind().unwrap();
    let sent = chat
        .send_file(zbus::zvariant::Fd::from(&file), "notes.bin", HashMap::new())
        .await
        .unwrap();

    let rae_chat = wait_for(20, "Rae's conversation", || async {
        rae.conversation_with(&rae_path, "quinn@localhost").await
    })
    .await;
    let rae_chat = rae.conversation(rae_chat).await;
    let got = wait_for(60, "Rae to receive the file", || async {
        let m = rae_chat.get_messages("", 20).await.unwrap();
        m.into_iter()
            .find(|m| text(m, "file-name") == "notes.bin" && text(m, "file-state") == "done")
    })
    .await;
    assert_eq!(std::fs::read(text(&got, "file-path")).unwrap(), data);
    assert_eq!(text(&got, "encryption"), "none");

    let mine = wait_for(30, "Quinn's side to finish", || async {
        let m = chat.get_messages("", 20).await.unwrap();
        message(&m, &sent)
            .filter(|m| text(m, "file-state") == "done" && text(m, "state") == "received")
            .cloned()
    })
    .await;
    assert_eq!(text(&mine, "file-name"), "notes.bin");
}

/// Without an upload service, a file for an encrypted conversation isn't
/// sent at all rather than sent directly in the clear.
#[tokio::test]
async fn no_direct_file_in_an_encrypted_conversation() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let no_upload = [("PIOPIO_TEST_NO_UPLOAD", "1")];
    let dir = || std::sync::Arc::new(tempfile::tempdir().unwrap());
    let pat = TestDaemon::start_with_env(dir(), &no_upload).await.unwrap();
    let sam = TestDaemon::start_with_env(dir(), &no_upload).await.unwrap();
    let (pat_path, pat_account) = pat.add_local("pat").await;
    let (sam_path, _) = sam.add_local("sam").await;
    pat.wait_published(&pat_path).await;
    sam.wait_published(&sam_path).await;
    let pat_roster = pat.roster(pat_path.clone()).await;
    let sam_roster = sam.roster(sam_path.clone()).await;
    pat_roster
        .add_contact("sam@localhost", "", &[])
        .await
        .unwrap();
    wait_for(20, "Sam to see Pat's request", || async {
        let contacts = sam_roster.get_contacts().await.unwrap();
        contacts
            .iter()
            .any(|c| text(c, "jid") == "pat@localhost")
            .then_some(())
    })
    .await;
    sam_roster
        .approve_subscription("pat@localhost")
        .await
        .unwrap();

    let chat = pat
        .conversation(pat_account.start_chat("sam@localhost").await.unwrap())
        .await;
    wait_for(20, "the conversation to be encrypted", || async {
        (chat.encryption().await.unwrap() != "none").then_some(())
    })
    .await;
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"secret plans").unwrap();
    file.rewind().unwrap();
    let sent = chat
        .send_file(zbus::zvariant::Fd::from(&file), "plans.txt", HashMap::new())
        .await
        .unwrap();
    wait_for(20, "the file to be refused", || async {
        let m = chat.get_messages("", 20).await.unwrap();
        message(&m, &sent)
            .filter(|m| text(m, "state") == "failed" && text(m, "file-state") == "failed")
            .map(|_| ())
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    if let Some(c) = sam.conversation_with(&sam_path, "pat@localhost").await {
        let m = sam
            .conversation(c)
            .await
            .get_messages("", 20)
            .await
            .unwrap();
        assert!(m.iter().all(|m| text(m, "file-name") != "plans.txt"));
    }
}
