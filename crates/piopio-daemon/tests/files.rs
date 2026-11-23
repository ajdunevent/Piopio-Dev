//! Sharing files through the test server's upload service: encrypted
//! (aesgcm://) between OMEMO users, with a thumbnail made by the daemon,
//! and in the clear (with OOB data) in a group chat.
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test files

mod common;

use std::collections::HashMap;
use std::io::Write;

use common::{TestDaemon, message, test_server, text, wait_for};

fn picture() -> std::fs::File {
    let mut png = Vec::new();
    image::RgbImage::from_fn(800, 600, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    })
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .unwrap();
    let mut f = tempfile::tempfile().unwrap();
    f.write_all(&png).unwrap();
    use std::io::Seek;
    f.rewind().unwrap();
    f
}

#[tokio::test]
async fn encrypted_picture_and_room_file() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let alice = TestDaemon::start().await.unwrap();
    let bob = TestDaemon::start().await.unwrap();
    let (alice_path, alice_account) = alice.add_local("alice").await;
    let (bob_path, bob_account) = bob.add_local("bob").await;
    // Not contacts: Alice fetches Bob's devices when she first sends.
    alice.wait_published(&alice_path).await;
    bob.wait_published(&bob_path).await;

    let alice_chat = alice
        .conversation(alice_account.start_chat("bob@localhost").await.unwrap())
        .await;
    let file = picture();
    let sent = alice_chat
        .send_file(
            zbus::zvariant::Fd::from(&file),
            "Kererū.png",
            HashMap::new(),
        )
        .await
        .unwrap();
    let mine = wait_for(30, "the upload", || async {
        let m = alice_chat.get_messages("", 50).await.unwrap();
        message(&m, &sent)
            .filter(|m| text(m, "state") == "sent" || text(m, "state") == "received")
            .cloned()
    })
    .await;
    assert_eq!(text(&mine, "file-state"), "done");
    assert!(text(&mine, "body").starts_with("aesgcm://"), "{mine:?}");
    assert_eq!(text(&mine, "encryption"), "omemo");
    assert!(!text(&mine, "file-thumbnail").is_empty());

    let bob_chat = wait_for(20, "Bob's conversation", || async {
        bob.conversation_with(&bob_path, "alice@localhost").await
    })
    .await;
    let bob_chat = bob.conversation(bob_chat).await;
    let got = wait_for(30, "Bob to download the picture", || async {
        let m = bob_chat.get_messages("", 50).await.unwrap();
        m.into_iter()
            .find(|m| text(m, "file-name") == "Kererū.png" && text(m, "file-state") == "done")
    })
    .await;
    assert_eq!(text(&got, "file-mime"), "image/png");
    let original = std::fs::read(text(&mine, "file-path")).unwrap();
    assert_eq!(std::fs::read(text(&got, "file-path")).unwrap(), original);
    let thumb = image::open(text(&got, "file-thumbnail")).unwrap();
    assert!(thumb.width() <= 480 && thumb.height() <= 480);

    // In a group chat the file goes in the clear, with OOB data.
    let room = "photos@conference.localhost";
    let alice_room = alice
        .conversation(
            alice_account
                .join_room(room, "", HashMap::new())
                .await
                .unwrap(),
        )
        .await;
    let bob_room = bob
        .conversation(
            bob_account
                .join_room(room, "", HashMap::new())
                .await
                .unwrap(),
        )
        .await;
    let joined = |d: &TestDaemon, p: zbus::zvariant::OwnedObjectPath| {
        let c = d.connection.clone();
        async move {
            let r = piopio_dbus::RoomProxy::builder(&c)
                .path(p)
                .unwrap()
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await
                .unwrap();
            let state = wait_for(20, "joining", || async {
                let s = r.state().await.unwrap();
                (s == "joined" || s == "error").then_some(s)
            })
            .await;
            assert_eq!(state, "joined", "{}", r.error().await.unwrap());
        }
    };
    joined(&alice, alice_room.inner().path().to_owned().into()).await;
    joined(&bob, bob_room.inner().path().to_owned().into()).await;
    let mut notes = tempfile::tempfile().unwrap();
    notes.write_all(b"bring binoculars").unwrap();
    use std::io::Seek;
    notes.rewind().unwrap();
    bob_room
        .send_file(
            zbus::zvariant::Fd::from(&notes),
            "notes.txt",
            HashMap::new(),
        )
        .await
        .unwrap();
    let remote = wait_for(30, "Alice to get the link", || async {
        let m = alice_room.get_messages("", 50).await.unwrap();
        m.into_iter().find(|m| text(m, "file-name") == "notes.txt")
    })
    .await;
    assert!(text(&remote, "body").starts_with("http://127.0.0.1:15280/"));
    assert_eq!(
        text(&remote, "file-state"),
        "remote",
        "only pictures come by themselves"
    );
    alice_room
        .download_file(&text(&remote, "id"))
        .await
        .unwrap();
    let done = wait_for(30, "the download", || async {
        let m = alice_room.get_messages("", 50).await.unwrap();
        m.into_iter()
            .find(|m| text(m, "file-name") == "notes.txt" && text(m, "file-state") == "done")
    })
    .await;
    assert_eq!(
        std::fs::read(text(&done, "file-path")).unwrap(),
        b"bring binoculars"
    );
}

/// Setting our picture; someone who opens a chat with us sees it.
#[tokio::test]
async fn avatars() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let carol = TestDaemon::start().await.unwrap();
    let dave = TestDaemon::start().await.unwrap();
    let (_, carol_account) = carol.add_local("carol").await;
    let (dave_path, dave_account) = dave.add_local("dave").await;
    carol_account
        .set_avatar(zbus::zvariant::Fd::from(&picture()))
        .await
        .unwrap();
    let own = carol_account.avatar().await.unwrap();
    let image = image::open(&own).unwrap();
    assert!(image.width() <= 192 && image.height() <= 192);

    let chat = dave
        .conversation(dave_account.start_chat("carol@localhost").await.unwrap())
        .await;
    let seen = wait_for(20, "Carol's picture", || async {
        let p = chat.avatar().await.unwrap();
        (!p.is_empty()).then_some(p)
    })
    .await;
    assert_eq!(std::fs::read(seen).unwrap(), std::fs::read(own).unwrap());
    let _ = dave_path;
}
