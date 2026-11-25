//! When new messages notify: always, except in the conversation the user
//! is reading (the window focused, that chat on screen). A stand-in for the
//! notification portal records what the daemon shows.
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test notifications

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestDaemon, test_server, text, wait_for};
use zbus::zvariant::OwnedValue;

#[derive(Clone, Default)]
struct Portal {
    shown: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.portal.Notification")]
impl Portal {
    fn add_notification(&self, _id: String, notification: HashMap<String, OwnedValue>) {
        let body = notification
            .get("body")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_default();
        self.shown.lock().unwrap().push(body);
    }

    fn remove_notification(&self, _id: String) {}

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

impl Portal {
    fn has(&self, body: &str) -> bool {
        self.shown.lock().unwrap().iter().any(|b| b == body)
    }
}

#[tokio::test]
async fn notifies_unless_reading_that_conversation() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    // Accounts only this test uses: a fresh account's first sync of a big
    // archive would take live messages for history (which doesn't notify).
    let tess = TestDaemon::start().await.unwrap();
    let uma = TestDaemon::start().await.unwrap();
    let portal = Portal::default();
    tess.connection
        .object_server()
        .at("/org/freedesktop/portal/desktop", portal.clone())
        .await
        .unwrap();
    tess.connection
        .request_name("org.freedesktop.portal.Desktop")
        .await
        .unwrap();
    let (tess_path, _) = tess.add_local("tess").await;
    let (_, uma_account) = uma.add_local("uma").await;
    let chat = uma
        .conversation(uma_account.start_chat("tess@localhost").await.unwrap())
        .await;
    chat.set_encryption_wanted(false).await.unwrap();

    let send = |body: &'static str| {
        let chat = chat.clone();
        async move { chat.send(body, HashMap::new()).await.unwrap() }
    };
    let arrived = |body: &'static str| {
        let (tess, tess_path) = (&tess, tess_path.clone());
        async move {
            wait_for(20, "the message to arrive", || async {
                let c = tess.conversation_with(&tess_path, "uma@localhost").await?;
                let m = tess.conversation(c).await.get_messages("", 20).await.ok()?;
                m.iter().any(|m| text(m, "body") == body).then_some(())
            })
            .await;
            // Notifying follows storing; give it a moment.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };

    // No window: a notification.
    send("one").await;
    wait_for(20, "a notification with no window", || async {
        portal.has("one").then_some(())
    })
    .await;

    // A window reading this conversation: none.
    tess.connection
        .request_name(piopio_dbus::APP_ID)
        .await
        .unwrap();
    let conversation = tess
        .conversation_with(&tess_path, "uma@localhost")
        .await
        .unwrap();
    let daemon = tess.daemon().await;
    daemon.set_viewing(conversation.as_str()).await.unwrap();
    send("two").await;
    arrived("two").await;
    assert!(
        !portal.has("two"),
        "notified while reading the conversation"
    );

    // The window in the background (or on another conversation): one.
    daemon.set_viewing("").await.unwrap();
    send("three").await;
    wait_for(
        20,
        "a notification with the window in the background",
        || async { portal.has("three").then_some(()) },
    )
    .await;

    // Reading it again, then closing the window: one.
    daemon.set_viewing(conversation.as_str()).await.unwrap();
    tess.connection
        .release_name(piopio_dbus::APP_ID)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    send("four").await;
    wait_for(20, "a notification after the window closed", || async {
        portal.has("four").then_some(())
    })
    .await;
}

/// The status shown in GNOME's Background Apps (Background portal).
#[derive(Clone, Default)]
struct Background {
    status: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.portal.Background")]
impl Background {
    fn set_status(&self, options: HashMap<String, OwnedValue>) {
        let message = options
            .get("message")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_default();
        self.status.lock().unwrap().push(message);
    }
}

impl Background {
    fn last(&self) -> Option<String> {
        self.status.lock().unwrap().last().cloned()
    }
}

#[tokio::test]
async fn background_apps_status() {
    if test_server().is_none() {
        eprintln!("PIOPIO_TEST_SERVER not set; skipping");
        return;
    }
    let dir = std::sync::Arc::new(tempfile::tempdir().unwrap());
    let vera = TestDaemon::start_with_env(dir, &[("PIOPIO_TEST_BACKGROUND_STATUS", "1")])
        .await
        .unwrap();
    let portal = Background::default();
    vera.connection
        .object_server()
        .at("/org/freedesktop/portal/desktop", portal.clone())
        .await
        .unwrap();
    vera.connection
        .request_name("org.freedesktop.portal.Desktop")
        .await
        .unwrap();
    wait_for(20, "the status with no accounts", || async {
        (portal.last().as_deref() == Some("No accounts")).then_some(())
    })
    .await;
    let (vera_path, _) = vera.add_local("vera").await;
    wait_for(20, "the status once connected", || async {
        (portal.last().as_deref() == Some("Connected")).then_some(())
    })
    .await;

    let wade = TestDaemon::start().await.unwrap();
    let (_, wade_account) = wade.add_local("wade").await;
    let chat = wade
        .conversation(wade_account.start_chat("vera@localhost").await.unwrap())
        .await;
    chat.set_encryption_wanted(false).await.unwrap();
    chat.send("Are you there?", HashMap::new()).await.unwrap();
    wait_for(20, "the unread message in the status", || async {
        (portal.last().as_deref() == Some("1 unread message")).then_some(())
    })
    .await;
    // Read: back to connected.
    let c = vera
        .conversation_with(&vera_path, "wade@localhost")
        .await
        .unwrap();
    vera.conversation(c).await.mark_read().await.unwrap();
    wait_for(20, "the status after reading", || async {
        (portal.last().as_deref() == Some("Connected")).then_some(())
    })
    .await;
}
