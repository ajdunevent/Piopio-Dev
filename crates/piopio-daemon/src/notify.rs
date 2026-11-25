//! Desktop notifications for new messages, through the Notification portal.
//!
//! Clicking one activates the UI (`net.dunevent.Piopio`) with the action
//! `app.open-conversation` and the conversation's object path. The UI is
//! D-Bus activatable, so this works whether or not it is running. A
//! conversation's notification is withdrawn once it has been read, here or
//! on another device.
//!
//! A conversation may override Do Not Disturb: GNOME shows urgent
//! notifications regardless, and on Phosh feedbackd is asked for an
//! "important" event, which overrides its Quiet and Silent profiles.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, Value};

use crate::session::Session;
use crate::store::MessageRow;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const NOTIFICATION: &str = "org.freedesktop.portal.Notification";

/// What a conversation's notifications do.
pub struct Style {
    pub muted: bool,
    /// A group chat that notifies only when you're mentioned.
    pub mentions_only: bool,
    pub override_quiet: bool,
}

/// `mentioned`: whether a group message mentions us (always true for
/// one-to-one messages).
pub async fn message(
    session: &Session,
    conversation: &OwnedObjectPath,
    row: &MessageRow,
    mentioned: bool,
) {
    // Only the conversation being read needs none: with the window in the
    // background, or on another conversation, messages still notify.
    if crate::activity::viewing(conversation.as_str()) {
        return;
    }
    let Some(style) = style(session, conversation).await else {
        return;
    };
    if style.muted || (style.mentions_only && !mentioned) {
        return;
    }
    // Do not disturb (our own status) means it, unless this conversation
    // may ring through.
    if session.presence.lock().await.0 == "dnd" && !style.override_quiet {
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
    } else if crate::files::is_file_link(&row.body) || row.attachment.is_some() {
        "Sent a file".to_owned()
    } else {
        row.body.chars().take(200).collect()
    };
    let mut notification: HashMap<&str, Value<'_>> = HashMap::new();
    notification.insert("title", Value::from(title));
    notification.insert("body", Value::from(body));
    // Urgent notifications show even during GNOME's Do Not Disturb.
    notification.insert(
        "priority",
        Value::from(if style.override_quiet {
            "urgent"
        } else {
            "high"
        }),
    );
    notification.insert("default-action", Value::from("app.open-conversation"));
    notification.insert(
        "default-action-target",
        Value::from(conversation.as_str().to_owned()),
    );
    notification.insert("category", Value::from("im.received"));
    let id = conversation.as_str().replace('/', "-");
    show(session, &id, notification).await;
    if style.override_quiet {
        override_quiet(session).await;
    }
}

/// Takes back a conversation's notification (it has been read).
pub async fn withdraw(session: &Session, conversation: &OwnedObjectPath) {
    let id = conversation.as_str().replace('/', "-");
    let _ = session
        .bus
        .call_method(
            Some(PORTAL),
            PORTAL_PATH,
            Some(NOTIFICATION),
            "RemoveNotification",
            &(id.as_str(),),
        )
        .await;
}

/// On Phosh: asks feedbackd to sound even in its Quiet or Silent profile.
/// In the Full profile Phosh already gives the notification's feedback.
async fn override_quiet(session: &Session) {
    let Ok(feedback) = zbus::Proxy::new(
        &session.bus,
        "org.sigxcpu.Feedback",
        "/org/sigxcpu/Feedback",
        "org.sigxcpu.Feedback",
    )
    .await
    else {
        return;
    };
    let profile: String = match feedback.get_property("Profile").await {
        Ok(p) => p,
        Err(_) => return, // no feedbackd here
    };
    if profile == "full" {
        return;
    }
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("important", Value::from(true));
    hints.insert("profile", Value::from("full"));
    let result: zbus::Result<u32> = feedback
        .call(
            "TriggerFeedback",
            &(piopio_dbus::APP_ID, "message-new-instant", hints, -1i32),
        )
        .await;
    if let Err(e) = result {
        tracing::debug!("feedbackd: {e}");
    }
}

async fn show(session: &Session, id: &str, notification: HashMap<&str, Value<'_>>) {
    let id = id.replace('/', "-");
    let result = session
        .bus
        .call_method(
            Some(PORTAL),
            PORTAL_PATH,
            Some(NOTIFICATION),
            "AddNotification",
            &(id.as_str(), notification),
        )
        .await;
    if let Err(e) = result {
        tracing::debug!("notification not shown: {e}");
    }
}

async fn style(session: &Session, conversation: &OwnedObjectPath) -> Option<Style> {
    let iface = session
        .bus
        .object_server()
        .interface::<_, crate::messages::ConversationObject>(conversation)
        .await
        .ok()?;
    Some(iface.get().await.notification_style())
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
