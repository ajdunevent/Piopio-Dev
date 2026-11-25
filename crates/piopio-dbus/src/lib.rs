//! Names, object paths and client proxies shared by the Piopio daemon and UI.
//!
//! The daemon serves the interfaces; the UI talks to them through the proxies
//! defined here. The API grows milestone by milestone, following the project
//! spec. Every interface carries an `ApiVersion` property, and a breaking change
//! creates a new interface name ending in `.v2`.

/// Application ID of the UI. Also its D-Bus name, icon name and desktop file name.
pub const APP_ID: &str = "net.dunevent.Piopio";

/// Well-known bus name owned by the daemon.
pub const DAEMON_BUS_NAME: &str = "net.dunevent.Piopio.Daemon";

/// Root object: carries the Daemon and Files interfaces and the ObjectManager.
pub const ROOT_PATH: &str = "/net/dunevent/Piopio";

/// Name of the Daemon interface on [`ROOT_PATH`].
pub const DAEMON_INTERFACE: &str = "net.dunevent.Piopio.Daemon";

/// Version of the Daemon interface implemented by this build.
pub const DAEMON_API_VERSION: u32 = 1;

/// Version of the Account interface implemented by this build.
pub const ACCOUNT_API_VERSION: u32 = 1;

/// Version of the Roster interface implemented by this build.
pub const ROSTER_API_VERSION: u32 = 1;

/// Version of the Conversation interface implemented by this build.
pub const CONVERSATION_API_VERSION: u32 = 1;

/// Version of the Omemo interface implemented by this build.
pub const OMEMO_API_VERSION: u32 = 1;

/// Accounts live at `ACCOUNT_PATH_PREFIX/<id>`.
pub const ACCOUNT_PATH_PREFIX: &str = "/net/dunevent/Piopio/account";

/// Client side of `net.dunevent.Piopio.Daemon`.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Daemon",
    default_service = "net.dunevent.Piopio.Daemon",
    default_path = "/net/dunevent/Piopio"
)]
pub trait Daemon {
    /// Add an account and start connecting. Returns the account's object path.
    fn add_account(
        &self,
        jid: &str,
        password: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Disconnect an account and delete it with all its local data.
    fn remove_account(&self, account: &zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    /// The accounts' object paths, oldest first.
    fn list_accounts(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;

    /// Disconnect everything and exit.
    fn quit(&self) -> zbus::Result<()>;

    /// Start at login (always false while RunInBackground is).
    #[zbus(property)]
    fn start_at_login(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_start_at_login(&self, on: bool) -> zbus::Result<()>;

    /// Keep running, with notifications, after the window closes.
    #[zbus(property)]
    fn run_in_background(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_run_in_background(&self, on: bool) -> zbus::Result<()>;

    /// Warn before quitting fully.
    #[zbus(property)]
    fn warn_on_quit(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_warn_on_quit(&self, on: bool) -> zbus::Result<()>;

    /// The conversation the window shows a focused user ("" for none).
    fn set_viewing(&self, conversation: &str) -> zbus::Result<()>;

    /// Version of this interface.
    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}

/// Client side of `net.dunevent.Piopio.Account`, one object per account.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Account",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Account {
    #[zbus(property)]
    fn jid(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn set_enabled(&self, enabled: bool) -> zbus::Result<()>;

    /// Show (available, away, chat, dnd or xa) and status text.
    #[zbus(property)]
    fn presence(&self) -> zbus::Result<(String, String)>;

    #[zbus(property)]
    fn set_presence(&self, presence: (&str, &str)) -> zbus::Result<()>;

    /// One of disconnected, connecting, connected or error.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    /// Why the account is in the error state, or why the last attempt failed.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;

    /// Opens (or creates) the one-to-one conversation with `jid`.
    fn start_chat(&self, jid: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Joins a group chat and bookmarks it; returns its conversation's path.
    /// An empty nick uses the account's name. Options: "password" (s).
    fn join_room(
        &self,
        jid: &str,
        nick: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Changes the password on the server, then uses it.
    fn change_password(&self, new: &str) -> zbus::Result<()>;

    /// Calls someone; returns the call's path.
    fn start_call(&self, jid: &str, video: bool) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Someone invited us to a group chat.
    #[zbus(signal)]
    fn room_invitation(
        &self,
        room: &str,
        from: &str,
        reason: &str,
        password: &str,
    ) -> zbus::Result<()>;

    /// Sets the account's picture from an image file; empty removes it.
    fn set_avatar(&self, file: zbus::zvariant::Fd<'_>) -> zbus::Result<()>;

    /// The account's own picture (a small PNG file), or "".
    #[zbus(property)]
    fn avatar(&self) -> zbus::Result<String>;

    /// Default for conversations: tell contacts when we're typing.
    #[zbus(property)]
    fn send_chat_states(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_send_chat_states(&self, on: bool) -> zbus::Result<()>;

    /// Default for conversations: tell contacts when we've read their messages.
    #[zbus(property)]
    fn send_receipts(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_send_receipts(&self, on: bool) -> zbus::Result<()>;

    /// What the server supports, plus "pep" and "urn:xmpp:http:upload:0"
    /// for its PEP and upload services. Empty until known.
    #[zbus(property)]
    fn server_features(&self) -> zbus::Result<Vec<String>>;

    /// Everyone on the account's blocklist.
    #[zbus(property)]
    fn blocked(&self) -> zbus::Result<Vec<String>>;

    /// Blocks `jid` on the server (XEP-0191), for every device.
    fn block(&self, jid: &str) -> zbus::Result<()>;
    fn unblock(&self, jid: &str) -> zbus::Result<()>;
}

/// A dictionary as passed over D-Bus (`a{sv}`).
pub type Dict = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;

/// Client side of `net.dunevent.Piopio.Roster`, on each account object.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Roster",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Roster {
    /// jid, name, groups, subscription, ask, presence, status, pending-in.
    fn get_contacts(&self) -> zbus::Result<Vec<Dict>>;
    fn add_contact(&self, jid: &str, name: &str, groups: &[&str]) -> zbus::Result<()>;
    fn remove_contact(&self, jid: &str) -> zbus::Result<()>;
    fn rename_contact(&self, jid: &str, name: &str) -> zbus::Result<()>;
    fn approve_subscription(&self, jid: &str) -> zbus::Result<()>;
    fn deny_subscription(&self, jid: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn contact_changed(&self, jid: &str, changes: Dict) -> zbus::Result<()>;

    #[zbus(signal)]
    fn subscription_request(&self, jid: &str) -> zbus::Result<()>;
}

/// Client side of `net.dunevent.Piopio.Conversation`.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Conversation",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Conversation {
    #[zbus(property)]
    fn peer(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn kind(&self) -> zbus::Result<String>;
    /// Whether the peer is typing: composing, paused or "".
    #[zbus(property)]
    fn peer_state(&self) -> zbus::Result<String>;

    /// Tells the peer whether we are typing: composing, paused or active.
    fn set_chat_state(&self, state: &str) -> zbus::Result<()>;

    /// The peer's picture (a small PNG file), or "".
    #[zbus(property)]
    fn avatar(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn unread(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn last_activity(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn pinned(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn muted(&self) -> zbus::Result<String>;
    /// The encryption new messages get: "omemo" or "none".
    #[zbus(property)]
    fn encryption(&self) -> zbus::Result<String>;
    /// Whether to encrypt when the peer supports it (on by default).
    #[zbus(property)]
    fn encryption_wanted(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_encryption_wanted(&self, wanted: bool) -> zbus::Result<()>;

    /// One page of history, newest last, older than `before_id` ("" = newest).
    fn get_messages(&self, before_id: &str, limit: u32) -> zbus::Result<Vec<Dict>>;

    /// Sends a message; returns its id. Options: "reply-to" (s), the id
    /// of the message this one answers.
    fn send(
        &self,
        body: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<String>;

    /// Clears Unread and tells the contact we have read up to here.
    fn mark_read(&self) -> zbus::Result<()>;

    /// Shares a file, read from an open file descriptor; returns the
    /// message's id. The upload follows as MessageChanged (file-state).
    fn send_file(
        &self,
        file: zbus::zvariant::Fd<'_>,
        name: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<String>;

    /// Downloads a message's file; file-state and file-path follow.
    fn download_file(&self, id: &str) -> zbus::Result<()>;

    /// Deletes the conversation and its history from this computer.
    fn delete(&self) -> zbus::Result<()>;

    /// Pins the conversation to the top of the list.
    #[zbus(property)]
    fn set_pinned(&self, pinned: bool) -> zbus::Result<()>;

    /// never, mentions or always.
    #[zbus(property)]
    fn set_muted(&self, muted: &str) -> zbus::Result<()>;

    /// "default" (the account's setting), "on" or "off".
    #[zbus(property)]
    fn send_chat_states(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_send_chat_states(&self, value: &str) -> zbus::Result<()>;
    /// Whether the peer sees that we're typing, after the account's default.
    #[zbus(property)]
    fn chat_states_enabled(&self) -> zbus::Result<bool>;

    /// "default", "on" or "off".
    #[zbus(property)]
    fn send_receipts(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_send_receipts(&self, value: &str) -> zbus::Result<()>;
    /// Whether the peer sees that we've read their messages.
    #[zbus(property)]
    fn receipts_enabled(&self) -> zbus::Result<bool>;

    /// What the peer's apps support; empty when unknown.
    #[zbus(property)]
    fn peer_features(&self) -> zbus::Result<Vec<String>>;

    /// Override Do Not Disturb and quiet modes.
    #[zbus(property)]
    fn override_quiet(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_override_quiet(&self, on: bool) -> zbus::Result<()>;

    /// Whether the peer is on our blocklist.
    #[zbus(property)]
    fn blocked(&self) -> zbus::Result<bool>;

    /// Corrects one of our messages.
    fn correct(&self, id: &str, body: &str) -> zbus::Result<()>;

    /// Sets our reactions to a message; an empty list removes them.
    fn react(&self, id: &str, emojis: &[&str]) -> zbus::Result<()>;

    /// Retracts one of our messages.
    fn retract(&self, id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn message_added(&self, message: Dict) -> zbus::Result<()>;

    #[zbus(signal)]
    fn message_changed(&self, id: &str, changes: Dict) -> zbus::Result<()>;
}

/// Version of the Room interface implemented by this build.
pub const ROOM_API_VERSION: u32 = 1;

/// Client side of `net.dunevent.Piopio.Room`, on group chat conversations.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Room",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Room {
    /// Our nickname in the room.
    #[zbus(property)]
    fn nick(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn subject(&self) -> zbus::Result<String>;
    /// joining, joined, error or left.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    /// Why joining failed, when State is error.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;

    /// Who is in the room. Keys: nick, jid, role, affiliation, show.
    fn get_occupants(&self) -> zbus::Result<Vec<Dict>>;

    /// Joins again, optionally with another nickname or a password.
    fn join(&self, nick: &str, password: &str) -> zbus::Result<()>;

    /// Leaves the room; its history stays.
    fn leave(&self) -> zbus::Result<()>;

    /// Invites someone, making them a member of a members-only room.
    fn invite(&self, jid: &str, reason: &str) -> zbus::Result<()>;

    /// Members-only and non-anonymous, so messages are encrypted.
    #[zbus(property)]
    fn private(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn occupants_changed(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}

/// Client side of `net.dunevent.Piopio.Omemo`, on each account object.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Omemo",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Omemo {
    /// This device's OMEMO device id.
    #[zbus(property)]
    fn device_id(&self) -> zbus::Result<u32>;

    /// This device's fingerprint, as contacts see it.
    #[zbus(property)]
    fn fingerprint(&self) -> zbus::Result<String>;

    /// True once this device and its keys are published for this login.
    #[zbus(property)]
    fn published(&self) -> zbus::Result<bool>;

    /// The devices of a JID (the account's own JID for its other devices).
    /// Keys: device-id (u), active (b), trust (s: blind, verified,
    /// untrusted or undecided) and fingerprint (s, empty until known).
    fn get_devices(&self, jid: &str) -> zbus::Result<Vec<Dict>>;

    /// Sets a device's trust.
    fn set_trust(&self, jid: &str, device_id: u32, trust: &str) -> zbus::Result<()>;

    /// The devices of a JID, or their trust, changed.
    #[zbus(signal)]
    fn devices_changed(&self, jid: &str) -> zbus::Result<()>;

    /// Version of this interface.
    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}

/// Version of the Call interface implemented by this build.
pub const CALL_API_VERSION: u32 = 1;

/// Client side of `net.dunevent.Piopio.Call`, at `<account>/call/<n>`.
/// The UI runs the media and passes SDP and ICE candidates through it.
#[zbus::proxy(
    interface = "net.dunevent.Piopio.Call",
    default_service = "net.dunevent.Piopio.Daemon"
)]
pub trait Call {
    #[zbus(property)]
    fn peer(&self) -> zbus::Result<String>;
    /// incoming or outgoing.
    #[zbus(property)]
    fn direction(&self) -> zbus::Result<String>;
    /// ringing, calling, connecting, active or ended.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn end_reason(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn video(&self) -> zbus::Result<bool>;

    /// Answers an incoming call; the offer follows as RemoteDescription.
    fn accept(&self) -> zbus::Result<()>;
    /// Declines, cancels or hangs up.
    fn hangup(&self) -> zbus::Result<()>;
    /// Our offer (outgoing) or answer (incoming), as SDP.
    fn set_local_description(&self, kind: &str, description: &str) -> zbus::Result<()>;
    /// One of our ICE candidates.
    fn add_local_candidate(&self, mline: u32, candidate: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn remote_description(&self, kind: &str, description: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn remote_candidate(&self, mline: u32, candidate: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn api_version(&self) -> zbus::Result<u32>;
}
