//! Desktop notifications for new messages, through the Notification portal.
//!
//! Clicking one activates the UI (`net.dunevent.Piopio`) with the action
//! `app.open-conversation` and the conversation's object path. The UI is
//! D-Bus activatable, so this works whether or not it is running.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, Value};

use crate::session::Session;
use crate::store::MessageRow;

pub async fn message(session: &Session, conversation: &OwnedObjectPath, row: &MessageRow) {
    if crate::activity::ui_present() {
        return; // the window shows it
    }
    if muted(session, conversation).await {
        return;
    }
    // Do not disturb means it.
    if session.presence.lock().await.0 == "dnd" {
        return;
    }
    let title = match row.sender.split_once('/') {
        // A group chat: "nick in room".
        Some((room, nick)) => {
            let room = room.split('@').next().unwrap_or(room);
            format!("{nick} in {room}")
        }
        None => crate::roster::display_name(session, &row.sender).await,
    };
    let body: String = if row.encryption == "omemo-failed" {
        "Encrypted message".to_owned()
    } else if crate::files::is_file_link(&row.body) {
        "Sent a file".to_owned()
    } else {
        row.body.chars().take(200).collect()
    };
    let mut notification: HashMap<&str, Value<'_>> = HashMap::new();
    notification.insert("title", Value::from(title));
    notification.insert("body", Value::from(body));
    notification.insert("priority", Value::from("high"));
    notification.insert("default-action", Value::from("app.open-conversation"));
    notification.insert(
        "default-action-target",
        Value::from(conversation.as_str().to_owned()),
    );
    notification.insert("category", Value::from("im.received"));
    let id = conversation.as_str().replace('/', "-");
    show(session, &id, notification).await;
}

async fn show(session: &Session, id: &str, notification: HashMap<&str, Value<'_>>) {
    let id = id.replace('/', "-");
    let result = session
        .bus
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Notification"),
            "AddNotification",
            &(id.as_str(), notification),
        )
        .await;
    if let Err(e) = result {
        tracing::debug!("notification not shown: {e}");
    }
}

async fn muted(session: &Session, conversation: &OwnedObjectPath) -> bool {
    let Ok(iface) = session
        .bus
        .object_server()
        .interface::<_, crate::messages::ConversationObject>(conversation)
        .await
    else {
        return false;
    };
    iface.get().await.muted_setting() == "always"
}

/// An incoming call: a notification that opens the call, even with no
/// window open (the window shows its own ringing).
pub async fn incoming_call(
    session: &Session,
    from: &tokio_xmpp::jid::BareJid,
    video: bool,
    call: &OwnedObjectPath,
) {
    if crate::activity::ui_present() {
        return;
    }
    let name = crate::roster::display_name(session, from.as_str()).await;
    let mut notification: HashMap<&str, Value<'_>> = HashMap::new();
    notification.insert("title", Value::from(name));
    notification.insert(
        "body",
        Value::from(if video { "Video call" } else { "Call" }),
    );
    notification.insert("priority", Value::from("urgent"));
    notification.insert("category", Value::from("call.incoming"));
    notification.insert("default-action", Value::from("app.open-call"));
    notification.insert(
        "default-action-target",
        Value::from(call.as_str().to_owned()),
    );
    show(session, &format!("call-{}", call.as_str()), notification).await;
}
