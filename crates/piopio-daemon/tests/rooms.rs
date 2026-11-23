//! Group chats between daemons on the local test server: joining,
//! talking, history from the room's archive, bookmarks across devices,
//! reactions and corrections in a room, and leaving.
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test rooms

mod common;

use std::collections::HashMap;

use common::{TestDaemon, test_server, text, wait_for};
use zbus::proxy::CacheProperties;

async fn room(
    d: &TestDaemon,
    path: zbus::zvariant::OwnedObjectPath,
) -> piopio_dbus::RoomProxy<'static> {
    piopio_dbus::RoomProxy::builder(&d.connection)
        .path(path)
        .unwrap()
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap()
}

async fn wait_joined(r: &piopio_dbus::RoomProxy<'_>) {
    let state = wait_for(20, "joining the room", || async {
        let s = r.state().await.unwrap();
        (s == "joined" || s == "error").then_some(s)
    })
    .await;
    assert_eq!(state, "joined", "{}", r.error().await.unwrap());
}

#[tokio::test]
async fn group_chat() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let jid = "lunch@conference.localhost";
    let erin = TestDaemon::start().await.unwrap();
    let frank = TestDaemon::start().await.unwrap();
    let (_, erin_account) = erin.add_local("erin").await;
    let (frank_path, frank_account) = frank.add_local("frank").await;

    let erin_room_path = erin_account
        .join_room(jid, "Erin", HashMap::new())
        .await
        .unwrap();
    let erin_room = room(&erin, erin_room_path.clone()).await;
    wait_joined(&erin_room).await;
    let erin_chat = erin.conversation(erin_room_path).await;
    assert_eq!(erin_chat.kind().await.unwrap(), "room");
    assert_eq!(erin_chat.title().await.unwrap(), "lunch");
    assert_eq!(erin_chat.encryption().await.unwrap(), "none");
    erin_chat
        .send("Before Frank arrives", HashMap::new())
        .await
        .unwrap();
    wait_for(20, "the room to reflect Erin's message", || async {
        let m = erin_chat.get_messages("", 10).await.unwrap();
        m.iter()
            .any(|m| text(m, "state") == "received" && !text(m, "server-id").is_empty())
            .then_some(())
    })
    .await;

    // Frank joins with the account's name and gets the history.
    let frank_room_path = frank_account
        .join_room(jid, "", HashMap::new())
        .await
        .unwrap();
    let frank_room = room(&frank, frank_room_path.clone()).await;
    wait_joined(&frank_room).await;
    assert_eq!(frank_room.nick().await.unwrap(), "frank");
    let frank_chat = frank.conversation(frank_room_path.clone()).await;
    let early = wait_for(20, "the room's history", || async {
        frank_chat
            .get_messages("", 10)
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(text(&early, "body"), "Before Frank arrives");
    assert_eq!(text(&early, "sender"), format!("{jid}/Erin"));
    assert_eq!(
        frank_chat.unread().await.unwrap(),
        0,
        "history is not unread"
    );

    // Both see each other.
    wait_for(20, "Erin to see Frank", || async {
        let o = erin_room.get_occupants().await.unwrap();
        o.iter().any(|o| text(o, "nick") == "frank").then_some(())
    })
    .await;

    // Live messages, a reaction and a correction.
    frank_chat.send("Hi Erin!", HashMap::new()).await.unwrap();
    let got = wait_for(20, "Erin to get Frank's message", || async {
        let m = erin_chat.get_messages("", 10).await.unwrap();
        m.into_iter().find(|m| text(m, "body") == "Hi Erin!")
    })
    .await;
    assert_eq!(erin_chat.unread().await.unwrap(), 1);
    erin_chat.react(&text(&got, "id"), &["👋"]).await.unwrap();
    wait_for(20, "Frank to see the reaction", || async {
        let m = frank_chat.get_messages("", 10).await.unwrap();
        let mine = m.into_iter().find(|m| text(m, "body") == "Hi Erin!")?;
        let r = mine
            .get("reactions")
            .and_then(|v| HashMap::<String, Vec<String>>::try_from(v.try_clone().ok()?).ok())?;
        (r.get(&format!("{jid}/Erin")) == Some(&vec!["👋".to_owned()])).then_some(())
    })
    .await;
    let mine = frank_chat
        .get_messages("", 10)
        .await
        .unwrap()
        .into_iter()
        .find(|m| text(m, "body") == "Hi Erin!")
        .unwrap();
    frank_chat
        .correct(&text(&mine, "id"), "Hi Erin! 👋")
        .await
        .unwrap();
    wait_for(20, "Erin to see the correction", || async {
        let m = erin_chat.get_messages("", 10).await.unwrap();
        m.iter()
            .any(|m| text(m, "body") == "Hi Erin! 👋")
            .then_some(())
    })
    .await;

    // Frank's second device joins by itself, from his bookmarks.
    let tablet = TestDaemon::start().await.unwrap();
    let (tablet_path, _) = tablet.add_local("frank").await;
    let tablet_room = wait_for(20, "the bookmarked room on the tablet", || async {
        tablet.conversations(&tablet_path).await.into_iter().next()
    })
    .await;
    let tablet_room = room(&tablet, tablet_room).await;
    wait_joined(&tablet_room).await;

    // Frank leaves on his first device: the room stays, with its history.
    frank_room.leave().await.unwrap();
    wait_for(20, "Frank to be out", || async {
        (frank_room.state().await.unwrap() == "left").then_some(())
    })
    .await;
    assert!(
        frank_chat
            .send("Am I still here?", HashMap::new())
            .await
            .is_err()
    );
    assert!(!frank_chat.get_messages("", 10).await.unwrap().is_empty());
    let _ = frank_path;
}
