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
    let title = crate::roster::display_name(session, &row.sender).await;
    let body: String = row.body.chars().take(200).collect();
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
