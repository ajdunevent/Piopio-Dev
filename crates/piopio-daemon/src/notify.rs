//! Desktop notifications for new messages, through the Notification portal.
//!
//! Clicking one activates the UI (`net.dunevent.Piopio`) with the action
//! `app.open-conversation` and the conversation's object path. The UI is
//! D-Bus activatable, so this works whether or not it is running. A
//! conversation's notification is withdrawn once it has been read, here or
//! on another device.
//!
//! Sounds: each conversation plays the app's default, none, or its own
//! file (portal version 2 and later). A conversation may also ring through
//! Do Not Disturb: GNOME shows urgent notifications regardless, and on
//! Phosh feedbackd is asked for an "important" event, which may override
//! its Quiet and Silent profiles.

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use tokio::sync::OnceCell;
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::session::Session;
use crate::store::MessageRow;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const NOTIFICATION: &str = "org.freedesktop.portal.Notification";

/// What a conversation's notifications do.
pub struct Style {
    pub muted: bool,
    /// "default", "silent" or "custom".
    pub sound: String,
    pub override_quiet: bool,
    pub conversation_id: i64,
}

pub async fn message(session: &Session, conversation: &OwnedObjectPath, row: &MessageRow) {
    if crate::activity::ui_present() {
        return; // the window shows it
    }
    let Some(style) = style(session, conversation).await else {
        return;
    };
    if style.muted {
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
    let (sound, file) = effective_sound(&style);
    if portal_version(&session.bus).await >= 2 {
        match (sound.as_str(), &file) {
            ("custom", Some(path)) => {
                if let Ok(fd) = sealed_copy(path) {
                    notification.insert(
                        "sound",
                        Value::from(("file-descriptor", Value::from(zbus::zvariant::Fd::from(fd)))),
                    );
                }
            }
            ("silent", _) => {
                notification.insert("sound", Value::from("silent"));
            }
            _ => {
                notification.insert("sound", Value::from("default"));
            }
        }
    }
    let id = conversation.as_str().replace('/', "-");
    show(session, &id, notification).await;
    if style.override_quiet && sound != "silent" {
        ring_through_quiet(session, file).await;
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

/// Plays a notification sound by showing a test notification with it and
/// taking it back a few seconds later: `sound` is "default", "silent" or
/// "custom" with its file. Sounds need portal version 2.
pub async fn test(
    bus: &zbus::Connection,
    sound: &str,
    file: Option<std::path::PathBuf>,
) -> zbus::fdo::Result<()> {
    // The desktop's own sound needs nothing special; a chosen one (or
    // none) needs the version 2 portal.
    let chooses = portal_version(bus).await >= 2;
    if sound == "custom" && !chooses {
        return Err(zbus::fdo::Error::NotSupported(
            "this desktop can only play its own notification sound".into(),
        ));
    }
    let mut notification: HashMap<&str, Value<'_>> = HashMap::new();
    notification.insert("title", Value::from("Notification Sound"));
    notification.insert("body", Value::from("This is how new messages will sound."));
    notification.insert("priority", Value::from("high"));
    match (sound, file) {
        ("custom", Some(path)) => {
            let fd = sealed_copy(&path)
                .map_err(|e| zbus::fdo::Error::Failed(format!("can't read the sound: {e}")))?;
            notification.insert(
                "sound",
                Value::from(("file-descriptor", Value::from(zbus::zvariant::Fd::from(fd)))),
            );
        }
        ("silent", _) => {
            notification.insert("sound", Value::from("silent"));
        }
        _ if chooses => {
            notification.insert("sound", Value::from("default"));
        }
        _ => {}
    }
    bus.call_method(
        Some(PORTAL),
        PORTAL_PATH,
        Some(NOTIFICATION),
        "AddNotification",
        &("sound-test", notification),
    )
    .await
    .map_err(|e| {
        tracing::info!("test notification: {e}");
        zbus::fdo::Error::Failed("The desktop couldn’t show a notification to play it".into())
    })?;
    let bus = bus.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        let _ = bus
            .call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some(NOTIFICATION),
                "RemoveNotification",
                &("sound-test",),
            )
            .await;
    });
    Ok(())
}

/// The sound a conversation's notifications play: "default", "silent"
/// or "custom", with the custom sound's file.
pub fn sound_of(style: &Style) -> (String, Option<std::path::PathBuf>) {
    effective_sound(style)
}

/// The sound to play ("default", "silent" or "custom") and its file.
fn effective_sound(style: &Style) -> (String, Option<std::path::PathBuf>) {
    let own = match style.sound.as_str() {
        "default" => None,
        other => Some(other.to_owned()),
    };
    match own {
        Some(s) if s == "custom" => (
            s,
            Some(crate::settings::sound_file(
                &style.conversation_id.to_string(),
            )),
        ),
        Some(s) => (s, None),
        None => {
            let global = crate::settings::get()
                .map(|s| s.sound())
                .unwrap_or_else(|| "default".into());
            let file = (global == "custom").then(|| crate::settings::sound_file("default"));
            (global, file)
        }
    }
}

/// The Notification portal's version (sounds came with version 2).
async fn portal_version(bus: &zbus::Connection) -> u32 {
    static VERSION: OnceCell<u32> = OnceCell::const_new();
    *VERSION
        .get_or_init(|| async {
            let Ok(proxy) = zbus::Proxy::new(bus, PORTAL, PORTAL_PATH, NOTIFICATION).await else {
                return 1;
            };
            proxy.get_property::<u32>("version").await.unwrap_or(1)
        })
        .await
}

/// A sealed in-memory copy of a sound file, as the portal requires.
fn sealed_copy(path: &std::path::Path) -> std::io::Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, SealFlags};
    let data = std::fs::read(path)?;
    let fd = rustix::fs::memfd_create(
        "piopio-sound",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    let mut file = std::fs::File::from(fd);
    std::io::Write::write_all(&mut file, &data)?;
    let fd = OwnedFd::from(file);
    rustix::fs::fcntl_add_seals(
        &fd,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )?;
    Ok(fd)
}

/// On Phosh: asks feedbackd to sound even in its Quiet or Silent profile.
/// In the Full profile Phosh already gives the notification's feedback.
async fn ring_through_quiet(session: &Session, sound: Option<std::path::PathBuf>) {
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
    if let Some(path) = sound.as_ref().and_then(|p| p.to_str()) {
        hints.insert("sound-file", Value::from(path.to_owned()));
    }
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
