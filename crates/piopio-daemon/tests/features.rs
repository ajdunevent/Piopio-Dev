//! What the server and contacts support.

mod common;

use common::{TestDaemon, test_server, text, wait_for};

/// The account lists the server's features, PEP and the upload service.
#[tokio::test]
async fn server_features() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let nora = TestDaemon::start().await.unwrap();
    let (_, account) = nora.add_local("nora").await;
    let features = wait_for(20, "the server's features", || async {
        let f = account.server_features().await.unwrap();
        (!f.is_empty()).then_some(f)
    })
    .await;
    for wanted in [
        "urn:xmpp:blocking",
        "urn:xmpp:carbons:2",
        "urn:xmpp:mam:2",
        "pep",
        "urn:xmpp:http:upload:0",
    ] {
        assert!(
            features.iter().any(|f| f == wanted),
            "{wanted} in {features:?}"
        );
    }
}

/// A contact who shares presence: their app's features show on the
/// conversation, and stay known after they go offline.
#[tokio::test]
async fn contact_features() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let nora = TestDaemon::start().await.unwrap();
    let owen = TestDaemon::start().await.unwrap();
    let (nora_path, nora_account) = nora.add_local("nora").await;
    let (owen_path, owen_account) = owen.add_local("owen").await;
    let chat = nora
        .conversation(nora_account.start_chat("owen@localhost").await.unwrap())
        .await;
    let nora_roster = nora.roster(nora_path).await;
    let owen_roster = owen.roster(owen_path).await;
    nora_roster
        .add_contact("owen@localhost", "", &[])
        .await
        .unwrap();
    wait_for(20, "Owen to see Nora's request", || async {
        let contacts = owen_roster.get_contacts().await.unwrap();
        contacts
            .iter()
            .any(|c| text(c, "jid") == "nora@localhost")
            .then_some(())
    })
    .await;
    owen_roster
        .approve_subscription("nora@localhost")
        .await
        .unwrap();
    wait_for(20, "Owen's features", || async {
        let f = chat.peer_features().await.unwrap();
        f.iter()
            .any(|f| f == "urn:xmpp:jingle:apps:rtp:audio")
            .then_some(())
    })
    .await;

    owen_account.set_enabled(false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let f = chat.peer_features().await.unwrap();
    assert!(
        f.iter().any(|f| f == "urn:xmpp:reactions:0"),
        "still known while offline: {f:?}"
    );
}
