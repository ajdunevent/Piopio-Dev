//! Group chats (XEP-0045), their history from the room's archive
//! (XEP-0313) and the bookmarks that say which ones to join (XEP-0402).
//!
//! A room is a conversation of kind "room" whose peer is the room's JID.
//! Besides `net.dunevent.Piopio.Conversation` it carries
//! `net.dunevent.Piopio.Room`: our nickname, the subject, whether we are in,
//! and who else is.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::mam;
use tokio_xmpp::parsers::muc::Muc;
use tokio_xmpp::parsers::muc::muc::History;
use tokio_xmpp::parsers::muc::user::{MucUser, Status};
use tokio_xmpp::parsers::ns;
use tokio_xmpp::parsers::presence::{Presence, Type as PresenceType};
use tokio_xmpp::parsers::rsm::SetQuery;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Str};

use crate::access;
use crate::pep::n;
use crate::session::{Session, Shared, new_id};
use crate::store::RoomRow;

pub const BOOKMARKS: &str = "urn:xmpp:bookmarks:1";
const ROOM_API_VERSION: u32 = 1;
/// Messages fetched from a room's archive when we first join it.
const FIRST_JOIN_MESSAGES: usize = 50;
const MAX_CATCH_UP_PAGES: usize = 20;

#[derive(Clone, Debug, Default)]
pub struct Occupant {
    /// The occupant's real JID, when the room shows it to us.
    pub jid: String,
    pub role: String,
    pub affiliation: String,
    pub show: String,
}

/// A room as it is right now (not stored).
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub conversation: i64,
    pub path: Option<OwnedObjectPath>,
    pub nick: String,
    /// joining, joined, error or left.
    pub state: String,
    pub error: String,
    pub subject: String,
    pub occupants: BTreeMap<String, Occupant>,
    /// Whether joining was already retried (see presence errors).
    pub retried: bool,
    /// Members-only and non-anonymous: messages can be OMEMO encrypted.
    pub private: bool,
    /// Everyone affiliated with a private room (they get our keys).
    pub members: Vec<BareJid>,
    /// If we create this room, make it private (members only, real JIDs
    /// visible to members), as needed for OMEMO.
    pub create_private: bool,
}

/// The account's rooms, by room JID.
#[derive(Default)]
pub struct Rooms {
    live: Mutex<HashMap<String, Live>>,
}

impl Rooms {
    pub async fn get(&self, room: &str) -> Option<Live> {
        self.live.lock().await.get(room).cloned()
    }

    /// Our nickname in `room`, if it is one of our rooms.
    pub async fn nick(&self, room: &str) -> Option<String> {
        self.live.lock().await.get(room).map(|l| l.nick.clone())
    }

    /// A new XMPP session: nobody is in any room until joined again.
    pub async fn new_session(&self) {
        for live in self.live.lock().await.values_mut() {
            live.occupants.clear();
            if live.state == "joined" {
                live.state = "joining".into();
            }
        }
    }

    pub async fn is_room(&self, room: &str) -> bool {
        self.live.lock().await.contains_key(room)
    }

    async fn update<R>(&self, room: &str, f: impl FnOnce(&mut Live) -> R) -> Option<R> {
        self.live.lock().await.get_mut(room).map(f)
    }
}

fn str_value(s: impl Into<String>) -> OwnedValue {
    OwnedValue::from(Str::from(s.into()))
}

fn lower(debug: impl std::fmt::Debug) -> String {
    format!("{debug:?}").to_lowercase()
}

/// Exports a stored room on the bus (at startup, or when first joined).
pub async fn export(session: &Session, conversation: i64, path: OwnedObjectPath, peer: &str) {
    let room = session.store.room(conversation).await.ok().flatten();
    let nick = room
        .as_ref()
        .map(|r| r.nick.clone())
        .unwrap_or_else(|| default_nick(session));
    let state = if room.as_ref().is_some_and(|r| r.autojoin) {
        "joining"
    } else {
        "left"
    };
    session.rooms.live.lock().await.insert(
        peer.to_owned(),
        Live {
            conversation,
            path: Some(path.clone()),
            nick,
            state: state.into(),
            ..Default::default()
        },
    );
    let object = RoomObject {
        session: session.clone(),
        room: peer.to_owned(),
    };
    let _ = session.bus.object_server().at(&path, object).await;
}

pub async fn unexport(session: &Shared, path: &OwnedObjectPath) {
    let _ = session
        .bus
        .object_server()
        .remove::<RoomObject, _>(path)
        .await;
}

fn default_nick(session: &Shared) -> String {
    session
        .jid
        .node()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "piopio".into())
}

/// Joins `room` (creating its conversation), remembers it and bookmarks it.
pub async fn join(
    session: &Session,
    room: &BareJid,
    nick: Option<String>,
    password: String,
) -> anyhow::Result<OwnedObjectPath> {
    join_with(session, room, nick, password, false).await
}

/// Like `join`; `private` makes a room we create members-only and
/// non-anonymous, so it can be encrypted.
pub async fn join_with(
    session: &Session,
    room: &BareJid,
    nick: Option<String>,
    password: String,
    private: bool,
) -> anyhow::Result<OwnedObjectPath> {
    let (conversation, path) = session
        .conversations
        .get_kind(session, room, "room")
        .await?;
    session
        .rooms
        .update(room.as_str(), |l| l.create_private = private)
        .await;
    let mut row = session
        .store
        .room(conversation)
        .await?
        .unwrap_or_else(|| RoomRow {
            conversation_id: conversation,
            nick: default_nick(session),
            password: String::new(),
            name: String::new(),
            autojoin: true,
            mam_last_id: None,
        });
    if let Some(nick) = nick.filter(|n| !n.trim().is_empty()) {
        row.nick = nick.trim().to_owned();
    }
    if !password.is_empty() {
        row.password = password;
    }
    row.autojoin = true;
    session.store.set_room(row.clone()).await?;
    let nick = row.nick.clone();
    session
        .rooms
        .update(room.as_str(), |l| {
            l.nick = nick;
            l.state = "joining".into();
            l.error.clear();
        })
        .await;
    room_changed(session, room.as_str()).await;
    send_join(session, room, &row).await;
    let s = session.clone();
    let bookmark = row.clone();
    let room = room.clone();
    tokio::spawn(async move { publish_bookmark(&s, &room, &bookmark).await });
    Ok(path)
}

async fn send_join(session: &Shared, room: &BareJid, row: &RoomRow) {
    if !session.is_online() {
        return;
    }
    let Ok(to) = format!("{room}/{}", row.nick).parse::<Jid>() else {
        set_error(session, room.as_str(), "That nickname can't be used").await;
        return;
    };
    // History comes from the room's archive, so ask for none here.
    let muc = Muc {
        password: (!row.password.is_empty()).then(|| row.password.clone()),
        history: Some(History {
            maxchars: Some(0),
            maxstanzas: None,
            seconds: None,
            since: None,
        }),
    };
    let presence = Presence::available().with_to(to).with_payload(muc);
    session.send(presence).await;
}

/// Leaves `room`; it stays in the list with its history, and its bookmark
/// stops joining it automatically.
pub async fn leave(session: &Session, room: &BareJid) {
    let Some(live) = session.rooms.get(room.as_str()).await else {
        return;
    };
    if let Ok(Some(mut row)) = session.store.room(live.conversation).await {
        row.autojoin = false;
        let _ = session.store.set_room(row.clone()).await;
        let s = session.clone();
        let room2 = room.clone();
        tokio::spawn(async move { publish_bookmark(&s, &room2, &row).await });
    }
    if let Ok(to) = format!("{room}/{}", live.nick).parse::<Jid>() {
        session.send(Presence::unavailable().with_to(to)).await;
    }
    session
        .rooms
        .update(room.as_str(), |l| {
            l.state = "left".into();
            l.occupants.clear();
        })
        .await;
    room_changed(session, room.as_str()).await;
}

/// After every login: learn the bookmarks and join the rooms to join.
pub async fn online(session: &Session) {
    if let Some(items) = crate::pep::fetch_all(session, BOOKMARKS).await {
        for (id, conference) in items {
            bookmark_learned(session, &id, &conference).await;
        }
    }
    let rooms: Vec<(String, i64)> = session
        .rooms
        .live
        .lock()
        .await
        .iter()
        .map(|(k, v)| (k.clone(), v.conversation))
        .collect();
    for (room, conversation) in rooms {
        let Ok(Some(row)) = session.store.room(conversation).await else {
            continue;
        };
        let Ok(jid) = room.parse::<BareJid>() else {
            continue;
        };
        let joined = session
            .rooms
            .get(&room)
            .await
            .is_some_and(|l| l.state == "joined");
        if row.autojoin && !joined {
            send_join(session, &jid, &row).await;
        }
    }
}

/// A bookmark from the list or a notification: join the room if it says so.
async fn bookmark_learned(session: &Session, id: &str, conference: &Element) {
    if !conference.is("conference", BOOKMARKS) {
        return;
    }
    let Ok(room) = id.parse::<BareJid>() else {
        return;
    };
    let autojoin = matches!(conference.attr("autojoin"), Some("true" | "1"));
    let nick = conference
        .get_child("nick", BOOKMARKS)
        .map(|n| n.text())
        .filter(|n| !n.is_empty());
    let password = conference
        .get_child("password", BOOKMARKS)
        .map(|p| p.text())
        .unwrap_or_default();
    let name = conference.attr("name").unwrap_or("").to_owned();
    let known = session.rooms.get(room.as_str()).await;
    match (&known, autojoin) {
        (None, false) => {}
        (None, true) => {
            if let Err(e) = join_quietly(session, &room, nick, password, name).await {
                tracing::warn!(%room, "joining a bookmarked room: {e:#}");
            }
        }
        (Some(live), _) => {
            if let Ok(Some(mut row)) = session.store.room(live.conversation).await {
                let changed = row.autojoin != autojoin || (!name.is_empty() && row.name != name);
                row.autojoin = autojoin;
                if !name.is_empty() {
                    row.name = name;
                }
                if changed {
                    let _ = session.store.set_room(row.clone()).await;
                }
                if autojoin && live.state == "left" {
                    send_join(session, &room, &row).await;
                    session
                        .rooms
                        .update(room.as_str(), |l| l.state = "joining".into())
                        .await;
                    room_changed(session, room.as_str()).await;
                }
            }
        }
    }
}

/// Joins without publishing a bookmark (it came from one).
async fn join_quietly(
    session: &Session,
    room: &BareJid,
    nick: Option<String>,
    password: String,
    name: String,
) -> anyhow::Result<()> {
    let (conversation, _) = session
        .conversations
        .get_kind(session, room, "room")
        .await?;
    let row = RoomRow {
        conversation_id: conversation,
        nick: nick.unwrap_or_else(|| default_nick(session)),
        password,
        name,
        autojoin: true,
        mam_last_id: session
            .store
            .room(conversation)
            .await?
            .and_then(|r| r.mam_last_id),
    };
    session.store.set_room(row.clone()).await?;
    let nick = row.nick.clone();
    session
        .rooms
        .update(room.as_str(), |l| {
            l.nick = nick;
            l.state = "joining".into();
        })
        .await;
    send_join(session, room, &row).await;
    Ok(())
}

async fn publish_bookmark(session: &Shared, room: &BareJid, row: &RoomRow) {
    let mut conference = Element::builder("conference", BOOKMARKS)
        .attr(n("autojoin"), if row.autojoin { "true" } else { "false" })
        .append(
            Element::builder("nick", BOOKMARKS)
                .append(row.nick.as_str())
                .build(),
        );
    if !row.name.is_empty() {
        conference = conference.attr(n("name"), row.name.as_str());
    }
    if !row.password.is_empty() {
        conference = conference.append(
            Element::builder("password", BOOKMARKS)
                .append(row.password.as_str())
                .build(),
        );
    }
    crate::pep::publish(
        session,
        BOOKMARKS,
        room.as_str(),
        conference.build(),
        crate::pep::PRIVATE_LIST,
    )
    .await;
}

/// A bookmarks notification from our own account. Returns true if handled.
pub async fn bookmark_event(session: &Session, from: Option<&Jid>, items: &Element) -> bool {
    if items.attr("node") != Some(BOOKMARKS) {
        return false;
    }
    if !session.is_own(from) {
        return true;
    }
    for item in items.children() {
        if item.is("item", ns::PUBSUB_EVENT)
            && let (Some(id), Some(conference)) = (item.attr("id"), item.children().next())
        {
            bookmark_learned(session, id, conference).await;
        }
    }
    true
}

async fn set_error(session: &Shared, room: &str, error: &str) {
    session
        .rooms
        .update(room, |l| {
            l.state = "error".into();
            l.error = error.to_owned();
        })
        .await;
    room_changed(session, room).await;
}

/// Handles presence from a room's occupants. Returns false if `presence`
/// isn't from one of our rooms.
pub async fn presence(session: &Session, presence: &Presence) -> bool {
    let Some(from) = &presence.from else {
        return false;
    };
    let room = from.to_bare();
    let Some(live) = session.rooms.get(room.as_str()).await else {
        return false;
    };
    let Some(nick) = from.resource().map(|r| r.to_string()) else {
        // Presence from the room itself (errors to a join without a nick).
        if presence.type_ == PresenceType::Error {
            set_error(session, room.as_str(), "The room refused us").await;
        }
        return true;
    };
    let user = presence
        .payloads
        .iter()
        .find(|p| p.is("x", ns::MUC_USER))
        .and_then(|p| MucUser::try_from(p.clone()).ok());
    let statuses = user.as_ref().map(|u| u.status.clone()).unwrap_or_default();
    let is_self = statuses.contains(&Status::SelfPresence) || nick == live.nick;

    match presence.type_ {
        PresenceType::Error => {
            if is_self {
                let why = presence
                    .payloads
                    .iter()
                    .find(|p| p.is("error", ns::DEFAULT_NS))
                    .and_then(|e| e.children().next().map(|c| c.name().to_owned()))
                    .unwrap_or_default();
                // A room someone is creating stays locked for a moment:
                // try once more before giving up.
                if why == "item-not-found"
                    && session
                        .rooms
                        .update(room.as_str(), |l| !std::mem::replace(&mut l.retried, true))
                        .await
                        == Some(true)
                {
                    let s = session.clone();
                    let room = room.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        if let Some(live) = s.rooms.get(room.as_str()).await
                            && let Ok(Some(row)) = s.store.room(live.conversation).await
                        {
                            send_join(&s, &room, &row).await;
                        }
                    });
                    return true;
                }
                let text = match why.as_str() {
                    "conflict" => "Someone in the room already uses that nickname",
                    "not-authorized" => "The room needs a password",
                    "registration-required" => "Only members can join this room",
                    "forbidden" => "You are banned from this room",
                    "item-not-found" => "The room doesn't exist",
                    "not-allowed" => "The room can't be created",
                    "service-unavailable" => "The room is full",
                    _ => "The room refused us",
                };
                set_error(session, room.as_str(), text).await;
            }
        }
        PresenceType::Unavailable => {
            let new_nick = user
                .as_ref()
                .filter(|_| statuses.contains(&Status::NewNick))
                .and_then(|u| u.items.first())
                .and_then(|i| i.nick.clone());
            session
                .rooms
                .update(room.as_str(), |l| {
                    let old = l.occupants.remove(&nick);
                    if let (Some(new), Some(old)) = (&new_nick, old) {
                        l.occupants.insert(new.clone(), old);
                    }
                })
                .await;
            if statuses.contains(&Status::SelfPresence) {
                if let Some(new) = new_nick {
                    session.rooms.update(room.as_str(), |l| l.nick = new).await;
                } else {
                    let error = if statuses.contains(&Status::Kicked) {
                        "You were removed from the room"
                    } else if statuses.contains(&Status::Banned) {
                        "You are banned from this room"
                    } else if statuses.contains(&Status::ServiceShutdown) {
                        "The room's service shut down"
                    } else if statuses.contains(&Status::RemovalFromRoom)
                        || statuses.contains(&Status::ConfigMembersOnly)
                    {
                        "You are no longer a member of the room"
                    } else {
                        ""
                    };
                    session
                        .rooms
                        .update(room.as_str(), |l| {
                            l.occupants.clear();
                            if error.is_empty() {
                                l.state = "left".into();
                            } else {
                                l.state = "error".into();
                                l.error = error.into();
                            }
                        })
                        .await;
                }
                room_changed(session, room.as_str()).await;
            }
            occupants_changed(session, room.as_str()).await;
        }
        PresenceType::None => {
            let item = user.as_ref().and_then(|u| u.items.first());
            let occupant = Occupant {
                jid: item
                    .and_then(|i| i.jid.as_ref())
                    .map(|j| j.to_bare().to_string())
                    .unwrap_or_default(),
                role: item.map(|i| lower(&i.role)).unwrap_or_default(),
                affiliation: item.map(|i| lower(&i.affiliation)).unwrap_or_default(),
                show: presence
                    .show
                    .as_ref()
                    .map(lower)
                    .unwrap_or_else(|| "available".into()),
            };
            let joined_now = statuses.contains(&Status::SelfPresence) && live.state != "joined";
            session
                .rooms
                .update(room.as_str(), |l| {
                    l.occupants.insert(nick.clone(), occupant);
                    if statuses.contains(&Status::SelfPresence) {
                        // The room may have changed our nickname (210).
                        l.nick = nick.clone();
                        l.state = "joined".into();
                        l.error.clear();
                        l.retried = false;
                    }
                })
                .await;
            if statuses.contains(&Status::RoomHasBeenCreated) {
                // We created it: accept the default configuration ("instant
                // room"), or nobody else can enter.
                let mut form = Element::builder("x", "jabber:x:data").attr(n("type"), "submit");
                if live.create_private {
                    let field = |var: &str, value: &str| {
                        Element::builder("field", "jabber:x:data")
                            .attr(n("var"), var)
                            .append(
                                Element::builder("value", "jabber:x:data")
                                    .append(value)
                                    .build(),
                            )
                            .build()
                    };
                    form = form
                        .append(field(
                            "FORM_TYPE",
                            "http://jabber.org/protocol/muc#roomconfig",
                        ))
                        .append(field("muc#roomconfig_membersonly", "1"))
                        .append(field("muc#roomconfig_whois", "anyone"))
                        .append(field("muc#roomconfig_persistentroom", "1"))
                        .append(field("muc#roomconfig_publicroom", "0"));
                }
                let form = form.build();
                let query = Element::builder("query", "http://jabber.org/protocol/muc#owner")
                    .append(form)
                    .build();
                let iq = crate::pep::set(Some(Jid::from(room.clone())), query);
                if let Err(e) = session.iq(iq).await {
                    tracing::warn!(%room, "unlocking the new room: {e}");
                }
            }
            if joined_now {
                if let Ok(Some(mut row)) = session.store.room(live.conversation).await
                    && row.nick != nick
                {
                    row.nick = nick.clone();
                    let _ = session.store.set_room(row).await;
                }
                room_changed(session, room.as_str()).await;
                let s = session.clone();
                tokio::spawn(async move {
                    learn(&s, &room).await;
                    catch_up(&s, &room).await
                });
            }
            occupants_changed(session, from.to_bare().as_str()).await;
        }
        _ => {}
    }
    true
}

/// Finds out whether a room is private, and if so who its members are.
async fn learn(session: &Session, room: &BareJid) {
    let to = Some(Jid::from(room.clone()));
    let info = crate::pep::get(
        to.clone(),
        Element::builder("query", ns::DISCO_INFO).build(),
    );
    let features: Vec<String> = match session.iq(info).await {
        Ok(Some(reply)) => reply
            .children()
            .filter(|c| c.is("feature", ns::DISCO_INFO))
            .filter_map(|c| c.attr("var").map(str::to_owned))
            .collect(),
        _ => vec![],
    };
    let private = features.iter().any(|f| f == "muc_nonanonymous")
        && features.iter().any(|f| f == "muc_membersonly");
    let mut members: Vec<BareJid> = Vec::new();
    if private {
        for affiliation in ["owner", "admin", "member"] {
            let query = Element::builder("query", "http://jabber.org/protocol/muc#admin")
                .append(
                    Element::builder("item", "http://jabber.org/protocol/muc#admin")
                        .attr(n("affiliation"), affiliation)
                        .build(),
                )
                .build();
            if let Ok(Some(reply)) = session.iq(crate::pep::get(to.clone(), query)).await {
                for item in reply.children() {
                    if let Some(jid) = item.attr("jid").and_then(|j| j.parse::<Jid>().ok()) {
                        let bare = jid.to_bare();
                        if !members.contains(&bare) {
                            members.push(bare);
                        }
                    }
                }
            }
        }
        // Occupants we can see count too (an admin list may be hidden).
        if let Some(live) = session.rooms.get(room.as_str()).await {
            for o in live.occupants.values() {
                if let Ok(j) = o.jid.parse::<BareJid>()
                    && !members.contains(&j)
                {
                    members.push(j);
                }
            }
        }
    }
    session
        .rooms
        .update(room.as_str(), |l| {
            l.private = private;
            l.members = members;
        })
        .await;
    session
        .conversations
        .encryption_changed(session, room)
        .await;
}

/// Invites someone: in a members-only room they become a member first.
pub async fn invite(
    session: &Session,
    room: &BareJid,
    who: &BareJid,
    reason: &str,
) -> anyhow::Result<()> {
    let live = session
        .rooms
        .get(room.as_str())
        .await
        .ok_or_else(|| anyhow::anyhow!("not one of our rooms"))?;
    if live.private {
        let query = Element::builder("query", "http://jabber.org/protocol/muc#admin")
            .append(
                Element::builder("item", "http://jabber.org/protocol/muc#admin")
                    .attr(n("affiliation"), "member")
                    .attr(n("jid"), who.as_str())
                    .build(),
            )
            .build();
        session
            .iq(crate::pep::set(Some(Jid::from(room.clone())), query))
            .await
            .map_err(|e| anyhow::anyhow!("the room didn't let us add {who}: {e}"))?;
        session
            .rooms
            .update(room.as_str(), |l| {
                if !l.members.contains(who) {
                    l.members.push(who.clone());
                }
            })
            .await;
    }
    // A direct invitation (XEP-0249), which every client understands.
    let mut x = Element::builder("x", "jabber:x:conference").attr(n("jid"), room.as_str());
    if !reason.is_empty() {
        x = x.attr(n("reason"), reason);
    }
    let mut message = tokio_xmpp::parsers::message::Message::new(Some(Jid::from(who.clone())));
    message.payloads.push(x.build());
    session.send(message).await;
    Ok(())
}

/// A subject change from the room.
pub async fn subject(session: &Shared, room: &str, subject: String) {
    session.rooms.update(room, |l| l.subject = subject).await;
    room_changed(session, room).await;
}

/// Fetches what the room said while we were away from its archive.
async fn catch_up(session: &Session, room: &BareJid) {
    let Some(live) = session.rooms.get(room.as_str()).await else {
        return;
    };
    let Ok(Some(row)) = session.store.room(live.conversation).await else {
        return;
    };
    let first = row.mam_last_id.is_none();
    let mut after = row.mam_last_id.clone();
    for _ in 0..MAX_CATCH_UP_PAGES {
        let set = if first {
            SetQuery {
                max: Some(FIRST_JOIN_MESSAGES),
                after: None,
                before: Some(String::new()),
                index: None,
            }
        } else {
            SetQuery {
                max: Some(100),
                after: after.clone(),
                before: None,
                index: None,
            }
        };
        let prefix = if first { "first" } else { "catchup" };
        let query = mam::Query {
            queryid: Some(mam::QueryId(format!("{prefix}-room-{}", new_id()))),
            node: None,
            form: None,
            set: Some(set),
            flip_page: false,
        };
        let iq = Iq::from_set("", query).with_to(Jid::from(room.clone()));
        let fin = match session.iq(iq).await {
            Ok(Some(payload)) => match mam::Fin::try_from(payload) {
                Ok(fin) => fin,
                Err(_) => break,
            },
            Ok(None) => break,
            Err(e) => {
                tracing::info!(%room, "the room's archive: {e}");
                break;
            }
        };
        if let Some(newest) = fin.set.last.clone() {
            if let Ok(Some(mut row)) = session.store.room(live.conversation).await {
                row.mam_last_id = Some(newest.clone());
                let _ = session.store.set_room(row).await;
            }
            after = Some(newest);
        }
        if first || fin.complete || fin.set.last.is_none() {
            break;
        }
    }
    let _ = session.caught_up.load(Ordering::SeqCst);
}

/// Remembers the newest live archive id, so the next catch-up starts there.
pub async fn saw_archive_id(session: &Shared, room: &str, id: String) {
    if let Some(live) = session.rooms.get(room).await
        && live.state == "joined"
        && let Ok(Some(mut row)) = session.store.room(live.conversation).await
    {
        row.mam_last_id = Some(id);
        let _ = session.store.set_room(row).await;
    }
}

async fn room_changed(session: &Shared, room: &str) {
    let Some(path) = session.rooms.get(room).await.and_then(|l| l.path) else {
        return;
    };
    if let Ok(iface) = session
        .bus
        .object_server()
        .interface::<_, RoomObject>(&path)
        .await
    {
        let object = iface.get().await;
        let e = iface.signal_emitter();
        let _ = object.state_changed(e).await;
        let _ = object.error_changed(e).await;
        let _ = object.nick_changed(e).await;
        let _ = object.subject_changed(e).await;
    }
}

async fn occupants_changed(session: &Shared, room: &str) {
    let Some(path) = session.rooms.get(room).await.and_then(|l| l.path) else {
        return;
    };
    if let Ok(emitter) = SignalEmitter::new(&session.bus, path) {
        let _ = RoomObject::occupants_changed(&emitter).await;
    }
}

/// `net.dunevent.Piopio.Room`, beside the Conversation interface of rooms.
pub struct RoomObject {
    session: Session,
    room: String,
}

impl RoomObject {
    async fn live(&self) -> Live {
        self.session.rooms.get(&self.room).await.unwrap_or_default()
    }
}

#[zbus::interface(name = "net.dunevent.Piopio.Room")]
impl RoomObject {
    /// Our nickname in the room.
    #[zbus(property)]
    async fn nick(&self) -> String {
        self.live().await.nick
    }

    #[zbus(property)]
    async fn subject(&self) -> String {
        self.live().await.subject
    }

    /// joining, joined, error or left.
    #[zbus(property)]
    async fn state(&self) -> String {
        self.live().await.state
    }

    /// Why joining failed or we had to leave, when State is error.
    #[zbus(property)]
    async fn error(&self) -> String {
        self.live().await.error
    }

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        ROOM_API_VERSION
    }

    /// Who is in the room: nick, jid (when visible), role, affiliation, show.
    async fn get_occupants(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        access::check(&header)?;
        Ok(self
            .live()
            .await
            .occupants
            .into_iter()
            .map(|(nick, o)| {
                HashMap::from([
                    ("nick".to_owned(), str_value(nick)),
                    ("jid".to_owned(), str_value(o.jid)),
                    ("role".to_owned(), str_value(o.role)),
                    ("affiliation".to_owned(), str_value(o.affiliation)),
                    ("show".to_owned(), str_value(o.show)),
                ])
            })
            .collect())
    }

    /// Joins again (after leaving or an error), optionally with another
    /// nickname or a password.
    async fn join(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        nick: &str,
        password: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let room: BareJid = self
            .room
            .parse()
            .map_err(|_| zbus::fdo::Error::Failed("not a room".into()))?;
        join(
            &self.session,
            &room,
            Some(nick.to_owned()),
            password.to_owned(),
        )
        .await
        .map(drop)
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Invites someone (making them a member of a members-only room).
    async fn invite(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
        reason: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let room: BareJid = self
            .room
            .parse()
            .map_err(|_| zbus::fdo::Error::Failed("not a room".into()))?;
        let who: BareJid = jid
            .parse()
            .map_err(|_| zbus::fdo::Error::InvalidArgs("not a valid JID".into()))?;
        invite(&self.session, &room, &who, reason)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))
    }

    /// Members-only and non-anonymous, so messages are encrypted.
    #[zbus(property)]
    async fn private(&self) -> bool {
        self.live().await.private
    }

    /// Leaves the room. Its history stays.
    async fn leave(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let room: BareJid = self
            .room
            .parse()
            .map_err(|_| zbus::fdo::Error::Failed("not a room".into()))?;
        leave(&self.session, &room).await;
        Ok(())
    }

    /// People came, went or changed.
    #[zbus(signal)]
    async fn occupants_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
