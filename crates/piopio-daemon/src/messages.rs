//! One-to-one conversations: sending, receiving, Carbons (XEP-0280), receipts
//! (XEP-0184), displayed markers (XEP-0333) and archive catch-up (XEP-0313).
//!
//! Every message, however it arrives (live, as a carbon, or from the archive),
//! goes through [`store_message`], which drops duplicates. That makes the
//! three sources safe to overlap.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::parsers::carbons;
use tokio_xmpp::parsers::chatstates::ChatState;
use tokio_xmpp::parsers::delay::Delay;
use tokio_xmpp::parsers::displayed_markers::{Displayed, Markable};
use tokio_xmpp::parsers::eme::ExplicitMessageEncryption;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::legacy_omemo::Encrypted;
use tokio_xmpp::parsers::mam;
use tokio_xmpp::parsers::message::{Id, Lang, Message, MessageType};
use tokio_xmpp::parsers::ns;
use tokio_xmpp::parsers::receipts;
use tokio_xmpp::parsers::rsm::SetQuery;
use tokio_xmpp::parsers::stanza_id::{OriginId, StanzaId};
use tokio_xmpp::stanzastream::{StanzaStage, StanzaState};
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Str, Value};

use piopio_dbus::CONVERSATION_API_VERSION;

use crate::access;
use crate::rich;
use crate::session::{Session, Shared, new_id, now_ms};
use crate::store::{ConversationRow, MessageRow, NewMessage, state};

/// Messages fetched from the archive on the very first login.
const FIRST_SYNC_MESSAGES: usize = 100;
/// How far back the first sync of an account goes: this many pages of
/// FIRST_SYNC_MESSAGES, newest first.
const FIRST_SYNC_PAGES: usize = 30;
/// Archive pages fetched per catch-up before giving up for this session.
const MAX_CATCH_UP_PAGES: usize = 50;

/// The account's conversations that are exported on the bus, by peer.
#[derive(Default)]
pub struct Conversations {
    by_peer: Mutex<HashMap<String, (i64, OwnedObjectPath)>>,
}

impl Conversations {
    /// Exports every stored conversation of the account.
    pub async fn load(&self, session: &Session) -> anyhow::Result<()> {
        for row in session.store.conversations(session.account_id).await? {
            self.export(session, row).await?;
        }
        Ok(())
    }

    /// The conversation with `peer`, created (as a one-to-one chat) and
    /// exported if new.
    pub async fn get(
        &self,
        session: &Session,
        peer: &BareJid,
    ) -> anyhow::Result<(i64, OwnedObjectPath)> {
        self.get_kind(session, peer, "chat").await
    }

    /// The conversation with `peer`, created as `kind` if new.
    pub async fn get_kind(
        &self,
        session: &Session,
        peer: &BareJid,
        kind: &'static str,
    ) -> anyhow::Result<(i64, OwnedObjectPath)> {
        if let Some(found) = self.by_peer.lock().await.get(peer.as_str()) {
            return Ok(found.clone());
        }
        let (row, created) = session
            .store
            .conversation(session.account_id, peer.to_string(), kind)
            .await?;
        let found = self.export(session, row).await?;
        // Someone new wrote: their picture, which no notification brings
        // unless they're a contact.
        if created && kind == "chat" {
            crate::avatars::refresh(session, peer.clone());
        }
        Ok(found)
    }

    async fn export(
        &self,
        session: &Session,
        row: ConversationRow,
    ) -> anyhow::Result<(i64, OwnedObjectPath)> {
        let mut map = self.by_peer.lock().await;
        if let Some(found) = map.get(&row.peer) {
            return Ok(found.clone());
        }
        let path = OwnedObjectPath::try_from(format!("{}/conversation/{}", session.path, row.id))?;
        let title = if row.kind == "room" {
            room_title(session, &row).await
        } else {
            crate::roster::display_name(session, &row.peer).await
        };
        let entry = (row.id, path.clone());
        map.insert(row.peer.clone(), entry.clone());
        drop(map);
        let is_room = row.kind == "room";
        let (id, peer) = (row.id, row.peer.clone());
        let object = ConversationObject {
            session: session.clone(),
            row,
            title,
            peer_state: String::new(),
            our_state: String::new(),
        };
        session.bus.object_server().at(&path, object).await?;
        if is_room {
            crate::rooms::export(session, id, path.clone(), &peer).await;
        }
        Ok(entry)
    }

    /// Unexports every conversation (when the account goes away).
    /// Forgets one conversation: off the bus, out of the database, its
    /// files deleted. A room is left and its bookmark removed.
    pub async fn delete(&self, session: &Session, peer: &str) {
        let Some((id, path)) = self.by_peer.lock().await.remove(peer) else {
            return;
        };
        if let Ok(room) = peer.parse::<BareJid>()
            && session.rooms.is_room(peer).await
        {
            crate::rooms::forget(session, &room).await;
        }
        crate::rooms::unexport(session, &path).await;
        let _ = session
            .bus
            .object_server()
            .remove::<ConversationObject, _>(&path)
            .await;
        match session.store.delete_conversation(id).await {
            Ok(files) => {
                for f in files {
                    let _ = tokio::fs::remove_file(f).await;
                }
            }
            Err(e) => tracing::error!("deleting a conversation: {e:#}"),
        }
    }

    pub async fn unexport_all(&self, shared: &Shared) {
        for (_, (_, path)) in self.by_peer.lock().await.drain() {
            crate::rooms::unexport(shared, &path).await;
            let _ = shared
                .bus
                .object_server()
                .remove::<ConversationObject, _>(&path)
                .await;
        }
    }

    /// Keeps a conversation's Title in step with the contact's roster name.
    pub async fn contact_renamed(&self, shared: &Shared, jid: &str, name: &str) {
        let Some((_, path)) = self.by_peer.lock().await.get(jid).cloned() else {
            return;
        };
        let Ok(iface) = shared
            .bus
            .object_server()
            .interface::<_, ConversationObject>(&path)
            .await
        else {
            return;
        };
        let mut object = iface.get_mut().await;
        let title = if name.is_empty() {
            jid.to_owned()
        } else {
            name.to_owned()
        };
        if object.title != title {
            object.title = title;
            let _ = object.title_changed(iface.signal_emitter()).await;
        }
    }

    /// The peer started or stopped typing.
    pub async fn peer_typing(&self, shared: &Shared, jid: &BareJid, state: &str) {
        let Some((_, path)) = self.by_peer.lock().await.get(jid.as_str()).cloned() else {
            return;
        };
        if let Some(iface) = self.iface(shared, &path).await {
            let mut object = iface.get_mut().await;
            if object.peer_state != state {
                object.peer_state = state.to_owned();
                let _ = object.peer_state_changed(iface.signal_emitter()).await;
            }
        }
    }

    /// Re-announces a conversation's PeerFeatures.
    pub async fn features_changed(&self, shared: &Shared, jid: &str) {
        let Some((_, path)) = self.by_peer.lock().await.get(jid).cloned() else {
            return;
        };
        if let Some(iface) = self.iface(shared, &path).await {
            let object = iface.get().await;
            let _ = object.peer_features_changed(iface.signal_emitter()).await;
        }
    }

    /// Re-announces a conversation's Blocked (the blocklist changed).
    pub async fn blocked_changed(&self, shared: &Shared, jid: &str) {
        let Some((_, path)) = self.by_peer.lock().await.get(jid).cloned() else {
            return;
        };
        if let Some(iface) = self.iface(shared, &path).await {
            let object = iface.get().await;
            let _ = object.blocked_changed(iface.signal_emitter()).await;
        }
    }

    /// Re-announces every conversation's effective typing and receipt
    /// settings (the account's defaults changed).
    pub async fn privacy_changed(&self, shared: &Shared) {
        let paths: Vec<_> = self
            .by_peer
            .lock()
            .await
            .values()
            .map(|(_, p)| p.clone())
            .collect();
        for path in paths {
            if let Some(iface) = self.iface(shared, &path).await {
                let object = iface.get().await;
                let _ = object
                    .chat_states_enabled_changed(iface.signal_emitter())
                    .await;
                let _ = object
                    .receipts_enabled_changed(iface.signal_emitter())
                    .await;
            }
        }
    }

    /// Re-announces a conversation's Avatar.
    pub async fn avatar_changed(&self, shared: &Shared, jid: &str) {
        let Some((_, path)) = self.by_peer.lock().await.get(jid).cloned() else {
            return;
        };
        if let Some(iface) = self.iface(shared, &path).await {
            let object = iface.get().await;
            let _ = object.avatar_changed(iface.signal_emitter()).await;
        }
    }

    /// Re-announces a conversation's Encryption (its peer's devices changed).
    pub async fn encryption_changed(&self, shared: &Shared, jid: &BareJid) {
        let Some((_, path)) = self.by_peer.lock().await.get(jid.as_str()).cloned() else {
            return;
        };
        if let Some(iface) = self.iface(shared, &path).await {
            let object = iface.get().await;
            let _ = object.encryption_changed(iface.signal_emitter()).await;
        }
    }

    async fn iface(
        &self,
        shared: &Shared,
        path: &OwnedObjectPath,
    ) -> Option<InterfaceRef<ConversationObject>> {
        shared
            .bus
            .object_server()
            .interface::<_, ConversationObject>(path)
            .await
            .ok()
    }
}

/// Where a message came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Live,
    Carbon,
    Archive { first_sync: bool },
}

/// Turns on Carbons so messages to and from our other devices show up here.
pub async fn enable_carbons(shared: &Shared) {
    if !shared.features.server_has(crate::features::CARBONS) {
        tracing::info!("the server doesn't support carbons");
        return;
    }
    if let Err(e) = shared.iq(Iq::from_set("", carbons::Enable)).await {
        tracing::info!("the server did not enable carbons: {e}");
    }
}

/// Handles every incoming `<message/>`.
pub async fn incoming(session: &Session, mut message: Message) {
    // PEP notifications (OMEMO device lists).
    if let Some(event) = message
        .payloads
        .iter()
        .find(|p| p.is("event", ns::PUBSUB_EVENT))
        .cloned()
    {
        if crate::omemo::event(session, message.from.as_ref(), &event).await {
            return;
        }
        if let Some(items) = event.get_child("items", ns::PUBSUB_EVENT)
            && crate::rooms::bookmark_event(session, message.from.as_ref(), items).await
        {
            return;
        }
        if let Some(items) = event.get_child("items", ns::PUBSUB_EVENT)
            && crate::avatars::event(session, message.from.as_ref(), items).await
        {
            return;
        }
    }

    // Archive results for our own queries.
    if let Ok(Some(result)) = message.extract_payload::<mam::Result_>() {
        let from_room = match &message.from {
            Some(from) => session.rooms.is_room(from.to_bare().as_str()).await,
            None => false,
        };
        if !session.is_own(message.from.as_ref()) && !from_room {
            tracing::warn!(from = ?message.from, "archive result from someone else ignored");
            return;
        }
        let first_sync = result
            .queryid
            .as_ref()
            .is_some_and(|q| q.0.starts_with("first-"));
        let forwarded = result.forwarded;
        handle(
            session,
            forwarded.message,
            forwarded.delay,
            Some(result.id),
            Source::Archive { first_sync },
        )
        .await;
        return;
    }

    // Carbons are only valid from our own account; anything else is forged.
    let carbon_sent = message.extract_payload::<carbons::Sent>().ok().flatten();
    let carbon_received = message
        .extract_payload::<carbons::Received>()
        .ok()
        .flatten();
    if carbon_sent.is_some() || carbon_received.is_some() {
        if !session.is_own(message.from.as_ref()) {
            tracing::warn!(from = ?message.from, "forged carbon ignored");
            return;
        }
        let forwarded = match (carbon_sent, carbon_received) {
            (Some(sent), _) => sent.forwarded,
            (_, Some(received)) => received.forwarded,
            _ => unreachable!(),
        };
        handle(
            session,
            forwarded.message,
            forwarded.delay,
            None,
            Source::Carbon,
        )
        .await;
        return;
    }

    let delay = message.extract_payload::<Delay>().ok().flatten();
    handle(session, message, delay, None, Source::Live).await;
}

async fn handle(
    session: &Session,
    mut message: Message,
    delay: Option<Delay>,
    archive_id: Option<String>,
    source: Source,
) {
    if message.type_ == MessageType::Groupchat {
        handle_room(session, message, delay, archive_id, source).await;
        return;
    }
    let from_bare = message.from.as_ref().map(Jid::to_bare);
    let outgoing = from_bare.as_ref().is_none_or(|b| *b == session.jid);
    let peer = if outgoing {
        message.to.as_ref().map(Jid::to_bare)
    } else {
        from_bare.clone()
    };
    let Some(peer) = peer else { return };
    if peer == session.jid && source != Source::Live {
        // Notes to self arrive twice (sent and received); keep the sent copy.
        if !outgoing {
            return;
        }
    }

    if message.type_ == MessageType::Error {
        if let Some(id) = &message.id {
            mark(session, &peer, &id.0, state::FAILED).await;
        }
        return;
    }
    if !matches!(message.type_, MessageType::Chat | MessageType::Normal) {
        return; // group chats and headlines come later
    }

    // Invitations to group chats: direct (XEP-0249) or through the room.
    if !outgoing && source == Source::Live {
        let direct = message
            .payloads
            .iter()
            .find(|p| p.is("x", "jabber:x:conference"))
            .and_then(|x| {
                Some((
                    x.attr("jid")?.to_owned(),
                    peer.to_string(),
                    x.attr("reason").unwrap_or("").to_owned(),
                    x.attr("password").unwrap_or("").to_owned(),
                ))
            });
        let mediated = message
            .payloads
            .iter()
            .find(|p| p.is("x", ns::MUC_USER))
            .and_then(|x| {
                let invite = x.get_child("invite", ns::MUC_USER)?;
                Some((
                    peer.to_string(),
                    invite
                        .attr("from")
                        .and_then(|f| f.parse::<Jid>().ok())
                        .map(|j| j.to_bare().to_string())
                        .unwrap_or_default(),
                    invite
                        .get_child("reason", ns::MUC_USER)
                        .map(|r| r.text())
                        .unwrap_or_default(),
                    x.get_child("password", ns::MUC_USER)
                        .map(|p| p.text())
                        .unwrap_or_default(),
                ))
            });
        if let Some((room, from, reason, password)) = direct.or(mediated) {
            if let Ok(emitter) = SignalEmitter::new(&session.bus, session.path.clone()) {
                let _ = crate::account::AccountObject::room_invitation(
                    &emitter, &room, &from, &reason, &password,
                )
                .await;
            }
            return;
        }
    }

    // Typing notifications (XEP-0085), only as they happen.
    if let Ok(Some(chat_state)) = message.extract_payload::<ChatState>()
        && source == Source::Live
        && !outgoing
    {
        let state = match chat_state {
            ChatState::Composing => "composing",
            ChatState::Paused => "paused",
            _ => "",
        };
        session
            .conversations
            .peer_typing(session, &peer, state)
            .await;
    }

    // Receipts and markers about our own outgoing messages.
    if let Ok(Some(received)) = message.extract_payload::<receipts::Received>() {
        if !outgoing {
            mark(session, &peer, &received.id, state::RECEIVED).await;
        }
    }
    if let Ok(Some(displayed)) = message.extract_payload::<Displayed>() {
        if outgoing {
            // We read the chat on another device.
            if source != (Source::Archive { first_sync: true }) {
                set_unread(session, &peer, 0).await;
            }
        } else {
            mark(session, &peer, &displayed.id.0, state::DISPLAYED).await;
        }
    }

    let sender_jid = if outgoing {
        session.jid.to_string()
    } else {
        peer.to_string()
    };

    // Calls: Jingle Message Initiation, only as it happens.
    if source == Source::Live && crate::jingle::message(session, &message, outgoing).await {
        return;
    }

    // OMEMO 2: open the envelope first, so what it carries (body, replies,
    // corrections) is handled like the same elements in the clear.
    let mut v2_label: Option<&'static str> = None;
    if let Some(i) = message
        .payloads
        .iter()
        .position(|p| p.is("encrypted", crate::omemo2::NS))
    {
        let encrypted = message.payloads.remove(i);
        let origin = message
            .payloads
            .iter()
            .find(|p| p.is("origin-id", "urn:xmpp:sid:0"))
            .and_then(|o| o.attr("id"))
            .map(str::to_owned)
            .or_else(|| message.id.as_ref().map(|i| i.0.clone()));
        if encrypted.has_child("payload", crate::omemo2::NS)
            && let Some(origin) = &origin
            && let Ok((conversation, _)) = session.conversations.get(session, &peer).await
            && session
                .store
                .has_message(conversation, origin.clone(), outgoing)
                .await
                .unwrap_or(false)
        {
            return;
        }
        let sender = if outgoing {
            session.jid.clone()
        } else {
            peer.clone()
        };
        match crate::omemo2::decrypt(session, &sender, &encrypted).await {
            crate::omemo2::Opened::Content(content, trusted) => {
                message.bodies.clear();
                for c in content {
                    if c.name() == "body" {
                        message.bodies.insert(Lang::default(), c.text());
                    } else {
                        message.payloads.push(c);
                    }
                }
                v2_label = Some(if trusted { "omemo2" } else { "omemo-untrusted" });
            }
            crate::omemo2::Opened::Other(crate::omemo::Decrypted::KeyTransport) => return,
            crate::omemo2::Opened::Other(crate::omemo::Decrypted::Failed(why)) => {
                if !encrypted.has_child("payload", crate::omemo2::NS) {
                    return;
                }
                tracing::info!(%peer, "an OMEMO 2 message could not be decrypted: {why}");
                message.bodies.clear();
                message.bodies.insert(Lang::default(), String::new());
                v2_label = Some("omemo-failed");
            }
            crate::omemo2::Opened::Other(crate::omemo::Decrypted::Text { .. }) => return,
        }
    }

    // Retractions and reactions change an earlier message and are not
    // messages themselves (their bodies are only fallbacks).
    if let Some(target) = rich::retraction(&message) {
        if let Ok((conversation, _)) = session.conversations.get(session, &peer).await
            && let Ok(Some(row)) = session.store.find_message(conversation, target).await
        {
            rich::retract(session, conversation, row, &sender_jid).await;
        }
        return;
    }
    if let Some((target, emojis)) = rich::reactions(&message) {
        if let Ok((conversation, _)) = session.conversations.get(session, &peer).await
            && let Ok(Some(row)) = session.store.find_message(conversation, target).await
        {
            rich::react(session, conversation, row, sender_jid.clone(), emojis).await;
        }
        return;
    }
    let replaces = rich::correction(&message);
    let reply = rich::reply(&message);

    let encrypted = message.extract_payload::<Encrypted>().ok().flatten();
    let plain_body = message.get_best_body_cloned(vec![]).map(|(_, b)| b);
    if encrypted.is_none() && plain_body.is_none() {
        return;
    }
    let wants_receipt = matches!(message.extract_payload::<receipts::Request>(), Ok(Some(_)));
    let origin = message
        .extract_payload::<OriginId>()
        .ok()
        .flatten()
        .map(|o| o.id);
    let stanza_id = message
        .extract_payload::<StanzaId>()
        .ok()
        .flatten()
        .filter(|s| s.by.to_bare() == session.jid && s.by.resource().is_none())
        .map(|s| s.id);
    let server_id = archive_id.or(stanza_id);
    let origin_id = origin
        .or_else(|| message.id.as_ref().map(|i| i.0.clone()))
        .or_else(|| server_id.clone())
        .unwrap_or_else(new_id);
    let timestamp = delay
        .map(|d| d.stamp.0.timestamp_millis())
        .unwrap_or_else(now_ms);
    let sender = if outgoing {
        session.jid.clone()
    } else {
        peer.clone()
    };

    let (body, encryption) = match encrypted {
        None => (plain_body.unwrap_or_default(), v2_label.unwrap_or("none")),
        Some(encrypted) => {
            // Already stored (our own message, or one seen live and again in
            // the archive): decrypting again would only fail.
            if encrypted.payload.is_some()
                && let Ok((conversation, _)) = session.conversations.get(session, &peer).await
                && session
                    .store
                    .has_message(conversation, origin_id.clone(), outgoing)
                    .await
                    .unwrap_or(false)
            {
                return;
            }
            match crate::omemo::decrypt(session, &sender, &encrypted).await {
                crate::omemo::Decrypted::Text { body, trusted } => {
                    (body, if trusted { "omemo" } else { "omemo-untrusted" })
                }
                crate::omemo::Decrypted::KeyTransport => return,
                crate::omemo::Decrypted::Failed(why) => {
                    // A correction we can't read (often one seen before)
                    // must not turn into a message of its own.
                    if encrypted.payload.is_none() || replaces.is_some() {
                        return;
                    }
                    tracing::info!(%peer, "an OMEMO message could not be decrypted: {why}");
                    (String::new(), "omemo-failed")
                }
            }
        }
    };

    // A reply's quote is only there for older clients.
    let (body, quote) = rich::strip_fallback(&message, rich::REPLY, &body);
    let conversation = session
        .conversations
        .get(session, &peer)
        .await
        .ok()
        .map(|(id, _)| id);

    if let (Some(target), Some(conversation)) = (replaces, conversation) {
        let row = session
            .store
            .find_message(conversation, target)
            .await
            .ok()
            .flatten();
        if rich::correct(session, conversation, row, &sender_jid, body.clone()).await {
            return;
        }
    }

    let (reply_to, reply_to_sender, reply_preview) = match (reply, conversation) {
        (Some((id, to)), Some(conversation)) => {
            let preview = match session.store.find_message(conversation, id.clone()).await {
                Ok(Some(row)) if !row.retracted => rich::preview(&row.body),
                _ => rich::unquote(&quote),
            };
            (id, to, preview)
        }
        _ => Default::default(),
    };

    let new = NewMessage {
        origin_id,
        server_id: server_id.clone(),
        outgoing,
        sender: sender.to_string(),
        body,
        timestamp,
        reply_to,
        reply_to_sender,
        reply_preview,
        state: if outgoing {
            state::SENT
        } else {
            state::RECEIVED
        },
        encryption,
    };
    let counts_as_unread = !outgoing && source != (Source::Archive { first_sync: true });
    let stored = store_message(session, &peer, new, counts_as_unread).await;
    if let Some((row, _)) = &stored {
        attach_if_file(session, &peer, &message, row).await;
    }

    // Live archive ids keep the catch-up position current once caught up.
    if source == Source::Live
        && session.caught_up.load(Ordering::SeqCst)
        && let Some(id) = server_id
    {
        let _ = session.store.set_mam_last_id(session.account_id, id).await;
    }

    if source == Source::Live && !outgoing && wants_receipt {
        if let (Some(to), Some(id)) = (message.from.clone(), message.id.clone()) {
            let receipt = Message::new_with_type(MessageType::Chat, Some(to))
                .with_payload(receipts::Received { id: id.0 });
            session.send(receipt).await;
        }
    }

    if let Some((row, conversation)) = stored
        && !outgoing
        && source != (Source::Archive { first_sync: true })
    {
        crate::notify::message(session, &conversation, &row).await;
    }
}

/// A group chat message (live or from the room's archive).
async fn handle_room(
    session: &Session,
    mut message: Message,
    delay: Option<Delay>,
    archive_id: Option<String>,
    source: Source,
) {
    let Some(from) = message.from.clone() else {
        return;
    };
    let room = from.to_bare();
    let Some(our_nick) = session.rooms.nick(room.as_str()).await else {
        return; // not one of our rooms
    };
    let subject = message.subjects.values().next().cloned();
    if message.bodies.is_empty()
        && let Some(subject) = subject
    {
        crate::rooms::subject(session, room.as_str(), subject).await;
        return;
    }
    let Some(nick) = from.resource().map(|r| r.to_string()) else {
        return; // notices from the room itself
    };
    let sender = from.to_string();
    let outgoing = nick == our_nick;
    let Ok((conversation, _)) = session.conversations.get_kind(session, &room, "room").await else {
        return;
    };

    if let Some(target) = rich::retraction(&message) {
        if let Ok(Some(row)) = session.store.find_message(conversation, target).await {
            rich::retract(session, conversation, row, &sender).await;
        }
        return;
    }
    if let Some((target, emojis)) = rich::reactions(&message) {
        if let Ok(Some(row)) = session.store.find_message(conversation, target).await {
            rich::react(session, conversation, row, sender.clone(), emojis).await;
        }
        return;
    }
    let replaces = rich::correction(&message);
    let reply = rich::reply(&message);
    let encrypted = message.extract_payload::<Encrypted>().ok().flatten();
    let Some((_, plain_body)) = message.get_best_body_cloned(vec![]) else {
        return;
    };
    let origin = message
        .extract_payload::<OriginId>()
        .ok()
        .flatten()
        .map(|o| o.id);
    let stanza_id = message
        .extract_payload::<StanzaId>()
        .ok()
        .flatten()
        .filter(|s| s.by.to_bare() == room && s.by.resource().is_none())
        .map(|s| s.id);
    let server_id = archive_id.or(stanza_id);
    let origin_id = origin
        .or_else(|| message.id.as_ref().map(|i| i.0.clone()))
        .or_else(|| server_id.clone())
        .unwrap_or_else(new_id);
    let timestamp = delay
        .map(|d| d.stamp.0.timestamp_millis())
        .unwrap_or_else(now_ms);

    // Our own message, reflected by the room: it got there.
    if outgoing && source == Source::Live {
        mark(session, &room, &origin_id, state::RECEIVED).await;
    }

    let (body, encryption) = match encrypted {
        None => (plain_body, "none"),
        Some(encrypted) => {
            if session
                .store
                .has_message(conversation, origin_id.clone(), outgoing)
                .await
                .unwrap_or(false)
            {
                // Ours, or seen before: decrypting again would only fail. Its
                // archive id may be new, though.
                if let Some(id) = &server_id {
                    let new = NewMessage {
                        origin_id: origin_id.clone(),
                        server_id: Some(id.clone()),
                        outgoing,
                        ..Default::default()
                    };
                    let _ = session.store.insert_message(conversation, new).await;
                }
                return;
            }
            // Who really sent it: from the occupant list, or by whose
            // device it came from.
            let real = if outgoing {
                Some(session.jid.clone())
            } else {
                let live = session.rooms.get(room.as_str()).await.unwrap_or_default();
                let known = live
                    .occupants
                    .get(&nick)
                    .and_then(|o| o.jid.parse::<BareJid>().ok());
                match known {
                    Some(j) => Some(j),
                    None => {
                        crate::omemo::owner_of_device(session, &live.members, encrypted.header.sid)
                            .await
                    }
                }
            };
            let result = match &real {
                Some(jid) => crate::omemo::decrypt(session, jid, &encrypted).await,
                None => crate::omemo::Decrypted::Failed("unknown sender".into()),
            };
            match result {
                crate::omemo::Decrypted::Text { body, trusted } => {
                    (body, if trusted { "omemo" } else { "omemo-untrusted" })
                }
                crate::omemo::Decrypted::KeyTransport => return,
                crate::omemo::Decrypted::Failed(why) => {
                    if encrypted.payload.is_none() || replaces.is_some() {
                        return;
                    }
                    tracing::info!(%room, "an OMEMO message could not be decrypted: {why}");
                    (String::new(), "omemo-failed")
                }
            }
        }
    };

    let (body, quote) = rich::strip_fallback(&message, rich::REPLY, &body);
    if let Some(target) = replaces {
        let row = session
            .store
            .find_message(conversation, target)
            .await
            .ok()
            .flatten();
        if rich::correct(session, conversation, row, &sender, body.clone()).await {
            return;
        }
    }
    let (reply_to, reply_to_sender, reply_preview) = match reply {
        Some((id, to)) => {
            let preview = match session.store.find_message(conversation, id.clone()).await {
                Ok(Some(row)) if !row.retracted => rich::preview(&row.body),
                _ => rich::unquote(&quote),
            };
            (id, to, preview)
        }
        None => Default::default(),
    };
    let mentions_us = !outgoing && body.to_lowercase().contains(&our_nick.to_lowercase());
    let new = NewMessage {
        origin_id,
        server_id: server_id.clone(),
        outgoing,
        sender,
        body,
        timestamp,
        state: state::RECEIVED,
        encryption,
        reply_to,
        reply_to_sender,
        reply_preview,
    };
    let first_sync = source == (Source::Archive { first_sync: true });
    let stored = store_message(session, &room, new, !outgoing && !first_sync).await;
    if let Some((row, _)) = &stored {
        attach_if_file(session, &room, &message, row).await;
    }
    if source == Source::Live
        && let Some(id) = server_id
    {
        crate::rooms::saw_archive_id(session, room.as_str(), id).await;
    }
    if let Some((row, path)) = stored
        && mentions_us
        && !first_sync
    {
        crate::notify::message(session, &path, &row).await;
    }
}

/// Records the file a just-stored message shares, if it shares one.
async fn attach_if_file(session: &Session, peer: &BareJid, message: &Message, row: &MessageRow) {
    let Some(url) = crate::files::shared_link(message, &row.body) else {
        return;
    };
    if let Ok((conversation, _)) = session.conversations.get(session, peer).await {
        crate::files::received(session, conversation, row, url).await;
    }
}

/// A room's title: its bookmark name, or the room's local part.
async fn room_title(session: &Session, row: &ConversationRow) -> String {
    if let Ok(Some(room)) = session.store.room(row.id).await
        && !room.name.is_empty()
    {
        return room.name;
    }
    row.peer
        .split('@')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&row.peer)
        .to_owned()
}

/// Stores a message, updates its conversation and tells the UI. Returns the
/// stored row and the conversation path, or None for a duplicate.
async fn store_message(
    session: &Session,
    peer: &BareJid,
    new: NewMessage,
    unread: bool,
) -> Option<(MessageRow, OwnedObjectPath)> {
    let (id, path) = match session.conversations.get(session, peer).await {
        Ok(found) => found,
        Err(e) => {
            tracing::error!("opening the conversation with {peer}: {e:#}");
            return None;
        }
    };
    let row = match session.store.insert_message(id, new).await {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!("storing a message: {e:#}");
            return None;
        }
    };
    if let Some(iface) = session.conversations.iface(session, &path).await {
        let emitter = iface.signal_emitter().clone();
        let _ = ConversationObject::message_added(&emitter, message_dict(&row)).await;
        let mut object = iface.get_mut().await;
        let mut changed = false;
        if row.timestamp > object.row.last_activity {
            object.row.last_activity = row.timestamp;
            let _ = object.last_activity_changed(&emitter).await;
            changed = true;
        }
        if unread {
            object.row.unread += 1;
            let _ = object.unread_changed(&emitter).await;
            changed = true;
        }
        if changed {
            let _ = session.store.update_conversation(object.row.clone()).await;
        }
    }
    Some((row, path))
}

/// Moves one of our outgoing messages forward (sent → received → displayed).
async fn mark(session: &Session, peer: &BareJid, origin_id: &str, new_state: &'static str) {
    let Ok((id, path)) = session.conversations.get(session, peer).await else {
        return;
    };
    let rows = if new_state == state::DISPLAYED {
        session
            .store
            .displayed_up_to(id, origin_id.to_owned())
            .await
            .unwrap_or_default()
    } else {
        session
            .store
            .advance_state(id, origin_id.to_owned(), new_state)
            .await
            .ok()
            .flatten()
            .into_iter()
            .collect()
    };
    if rows.is_empty() {
        return;
    }
    if let Some(iface) = session.conversations.iface(session, &path).await {
        for row in rows {
            let changes = HashMap::from([("state", Value::from(row.state.clone()))]);
            let _ = ConversationObject::message_changed(
                iface.signal_emitter(),
                &row.id.to_string(),
                changes,
            )
            .await;
        }
    }
}

async fn set_unread(session: &Session, peer: &BareJid, unread: u32) {
    let Ok((_, path)) = session.conversations.get(session, peer).await else {
        return;
    };
    if let Some(iface) = session.conversations.iface(session, &path).await {
        let mut object = iface.get_mut().await;
        if object.row.unread != unread {
            object.row.unread = unread;
            let _ = object.unread_changed(iface.signal_emitter()).await;
            let _ = session.store.update_conversation(object.row.clone()).await;
        }
    }
    // Read on another device: its notification here is old news.
    if unread == 0 {
        crate::notify::withdraw(session, &path).await;
    }
}

/// Fetches what arrived while we were offline from our own archive.
pub async fn catch_up(session: &Session) {
    if !session.features.server_has(crate::features::MAM) {
        tracing::info!("the server keeps no message archive");
        session.caught_up.store(true, Ordering::SeqCst);
        return;
    }
    let last = session
        .store
        .accounts()
        .await
        .ok()
        .and_then(|rows| rows.into_iter().find(|a| a.id == session.account_id))
        .and_then(|a| a.mam_last_id);
    let first_sync = last.is_none();
    let mut after = last;
    // First sync: pages go backwards from the newest.
    let mut before = String::new();
    let pages = if first_sync {
        FIRST_SYNC_PAGES
    } else {
        MAX_CATCH_UP_PAGES
    };

    for page in 0..pages {
        let queryid = if first_sync {
            format!("first-{}", new_id())
        } else {
            format!("catchup-{}", new_id())
        };
        let set = if first_sync {
            // The newest page: an empty <before/>.
            SetQuery {
                max: Some(FIRST_SYNC_MESSAGES),
                after: None,
                before: Some(before.clone()),
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
        let query = mam::Query {
            queryid: Some(mam::QueryId(queryid)),
            node: None,
            form: None,
            set: Some(set),
            flip_page: false,
        };
        let fin = match session.iq(Iq::from_set("", query)).await {
            Ok(Some(payload)) => match mam::Fin::try_from(payload) {
                Ok(fin) => fin,
                Err(e) => {
                    tracing::warn!("unreadable archive answer: {e}");
                    break;
                }
            },
            Ok(None) => break,
            Err(e) => {
                tracing::info!("archive query failed: {e}");
                // A stored position the server no longer knows: start over.
                if page == 0 && !first_sync {
                    let _ = session
                        .store
                        .set_mam_last_id(session.account_id, String::new())
                        .await;
                }
                break;
            }
        };
        tracing::debug!(page, complete = fin.complete, first = ?fin.set.first, "archive page");
        if first_sync {
            // Later catch-ups continue from the newest message of all.
            if page == 0
                && let Some(newest) = fin.set.last.clone()
            {
                let _ = session
                    .store
                    .set_mam_last_id(session.account_id, newest)
                    .await;
                // The newest are in: live messages may count again.
                session.caught_up.store(true, Ordering::SeqCst);
            }
            match fin.set.first.clone() {
                Some(oldest) if !fin.complete => before = oldest.item,
                _ => break,
            }
            continue;
        }
        if let Some(newest) = fin.set.last.clone() {
            let _ = session
                .store
                .set_mam_last_id(session.account_id, newest.clone())
                .await;
            after = Some(newest);
        }
        if fin.complete || fin.set.last.is_none() {
            break;
        }
    }
    if first_sync {
        // Everyone in the roster gets a conversation, history or not.
        if let Ok(contacts) = session.store.contacts(session.account_id).await {
            for contact in contacts {
                if let Ok(jid) = contact.jid.parse::<BareJid>()
                    && jid.node().is_some()
                {
                    let _ = session.conversations.get(session, &jid).await;
                }
            }
        }
    }
    session.caught_up.store(true, Ordering::SeqCst);
    tracing::info!(jid = %session.jid, "caught up with the archive");
}

/// Sends a chat message carrying `body`, encrypted when OMEMO applies, plus
/// `extra` elements. Tracked messages ask for receipts and markers. Returns
/// the state reached and the encryption used.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    session: &Session,
    peer: &BareJid,
    kind: MessageType,
    wanted: bool,
    id: &str,
    body: &str,
    extra: Vec<tokio_xmpp::minidom::Element>,
    tracked: bool,
) -> (&'static str, &'static str) {
    let mut message = Message::new_with_type(kind, Some(Jid::from(peer.clone())))
        .with_payload(OriginId { id: id.to_owned() });
    if tracked {
        message = message
            .with_payload(receipts::Request)
            .with_payload(Markable)
            .with_payload(ChatState::Active);
    }
    message.id = Some(Id(id.to_owned()));
    let outcome = if message.type_ == MessageType::Groupchat {
        if wanted {
            let members = session
                .rooms
                .get(peer.as_str())
                .await
                .map(|l| l.members)
                .unwrap_or_default();
            crate::omemo::encrypt_group(session, &members, body).await
        } else {
            crate::omemo::Outgoing::Plain
        }
    } else if wanted && crate::omemo::use_v2(session, peer).await {
        // OMEMO 2: the body and what refers to other messages go inside
        // the encrypted envelope.
        let mut content = vec![
            tokio_xmpp::minidom::Element::builder("body", "jabber:client")
                .append(body)
                .build(),
        ];
        content.extend(extra);
        let envelope = crate::omemo2::envelope(&session.jid, content);
        let outcome = crate::omemo2::encrypt(session, peer, &envelope).await;
        return match outcome {
            crate::omemo::Outgoing::Encrypted(encrypted) => {
                message
                    .bodies
                    .insert(Lang::default(), crate::omemo2::FALLBACK_BODY.to_owned());
                message.payloads.push(encrypted);
                message.payloads.push(
                    ExplicitMessageEncryption {
                        namespace: crate::omemo2::NS.to_owned(),
                        name: Some("OMEMO".to_owned()),
                    }
                    .into(),
                );
                message.payloads.push(rich::store_hint());
                (wait_sent(session, message).await, "omemo2")
            }
            crate::omemo::Outgoing::Failed(why) => {
                tracing::warn!(%peer, "not sending: {why}");
                (state::FAILED, "none")
            }
            crate::omemo::Outgoing::Plain => (state::FAILED, "none"),
        };
    } else {
        crate::omemo::encrypt(session, peer, wanted, body).await
    };
    let used = match outcome {
        crate::omemo::Outgoing::Plain => {
            message.bodies.insert(Lang::default(), body.to_owned());
            "none"
        }
        crate::omemo::Outgoing::Encrypted(encrypted) => {
            message
                .bodies
                .insert(Lang::default(), crate::omemo::FALLBACK_BODY.to_owned());
            message.payloads.push(encrypted);
            message.payloads.push(
                ExplicitMessageEncryption {
                    namespace: ns::LEGACY_OMEMO.to_owned(),
                    name: Some("OMEMO".to_owned()),
                }
                .into(),
            );
            message.payloads.push(rich::store_hint());
            "omemo"
        }
        crate::omemo::Outgoing::Failed(why) => {
            tracing::warn!(%peer, "not sending: {why}");
            return (state::FAILED, "none");
        }
    };
    message.payloads.extend(extra);
    (wait_sent(session, message).await, used)
}

/// Sends a message whose content needs no encryption (reactions, retractions).
async fn send_plain(
    session: &Session,
    peer: &BareJid,
    kind: MessageType,
    body: Option<&str>,
    extra: Vec<tokio_xmpp::minidom::Element>,
) -> &'static str {
    let id = new_id();
    let mut message = Message::new_with_type(kind, Some(Jid::from(peer.clone())))
        .with_payload(OriginId { id: id.clone() });
    message.id = Some(Id(id));
    if let Some(body) = body {
        message.bodies.insert(Lang::default(), body.to_owned());
    }
    message.payloads.extend(extra);
    wait_sent(session, message).await
}

async fn wait_sent(session: &Session, message: Message) -> &'static str {
    match session.send(message).await {
        None => state::FAILED,
        Some(mut token) => match token.wait_for(StanzaStage::Sent).await {
            Some(StanzaState::Sent { .. } | StanzaState::Acked { .. }) => state::SENT,
            _ => state::FAILED,
        },
    }
}

fn str_value(s: impl Into<String>) -> OwnedValue {
    OwnedValue::from(Str::from(s.into()))
}

/// A message as the UI sees it.
pub fn message_dict(m: &MessageRow) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    map.insert("id".into(), str_value(m.id.to_string()));
    map.insert("origin-id".into(), str_value(m.origin_id.clone()));
    map.insert(
        "server-id".into(),
        str_value(m.server_id.clone().unwrap_or_default()),
    );
    map.insert("outgoing".into(), OwnedValue::from(m.outgoing));
    map.insert("sender".into(), str_value(m.sender.clone()));
    map.insert("body".into(), str_value(m.body.clone()));
    map.insert("timestamp".into(), OwnedValue::from(m.timestamp));
    map.insert("state".into(), str_value(m.state.clone()));
    map.insert("encryption".into(), str_value(m.encryption.clone()));
    map.insert("edited".into(), OwnedValue::from(m.edited));
    map.insert("retracted".into(), OwnedValue::from(m.retracted));
    map.insert("reply-to".into(), str_value(m.reply_to.clone()));
    map.insert(
        "reply-to-sender".into(),
        str_value(m.reply_to_sender.clone()),
    );
    map.insert("reply-preview".into(), str_value(m.reply_preview.clone()));
    map.insert("reactions".into(), rich::reactions_owned(&m.reactions));
    if let Some(a) = &m.attachment {
        crate::files::dict_entries(a, &mut map);
    }
    map
}

/// Tells the UI that something about a stored message changed.
pub async fn emit_changed(
    session: &Session,
    conversation: i64,
    message: i64,
    changes: HashMap<&str, Value<'_>>,
) {
    let Ok(path) =
        OwnedObjectPath::try_from(format!("{}/conversation/{conversation}", session.path))
    else {
        return;
    };
    if let Some(iface) = session.conversations.iface(session, &path).await {
        let _ = ConversationObject::message_changed(
            iface.signal_emitter(),
            &message.to_string(),
            changes,
        )
        .await;
    }
}

fn check_override(value: &str) -> zbus::fdo::Result<()> {
    if matches!(value, "default" | "on" | "off") {
        Ok(())
    } else {
        Err(zbus::fdo::Error::InvalidArgs(
            "the setting is default, on or off".into(),
        ))
    }
}

/// `net.dunevent.Piopio.Conversation`, at `<account>/conversation/<id>`.
pub struct ConversationObject {
    session: Session,
    row: ConversationRow,
    title: String,
    /// composing, paused or "" (XEP-0085).
    peer_state: String,
    /// The state we last told the peer.
    our_state: String,
}

impl ConversationObject {
    /// The Muted setting, for code outside the D-Bus interface.
    /// How this conversation's notifications behave.
    pub fn notification_style(&self) -> crate::notify::Style {
        crate::notify::Style {
            muted: self.row.muted == "always",
            sound: self.row.notification_sound.clone(),
            override_quiet: self.row.override_quiet,
            conversation_id: self.row.id,
        }
    }

    fn object_path(&self) -> Option<OwnedObjectPath> {
        OwnedObjectPath::try_from(format!(
            "{}/conversation/{}",
            self.session.path, self.row.id
        ))
        .ok()
    }

    /// Whether this conversation tells the peer we're typing.
    fn sends_chat_states(&self) -> bool {
        match self.row.send_chat_states.as_str() {
            "on" => true,
            "off" => false,
            _ => self.session.send_chat_states.load(Ordering::SeqCst),
        }
    }

    /// Whether this conversation tells the peer we've read their messages.
    fn sends_receipts(&self) -> bool {
        match self.row.send_receipts.as_str() {
            "on" => true,
            "off" => false,
            _ => self.session.send_receipts.load(Ordering::SeqCst),
        }
    }

    fn peer_jid(&self) -> Option<BareJid> {
        self.row.peer.parse().ok()
    }

    fn valid_peer(&self) -> zbus::fdo::Result<BareJid> {
        self.peer_jid()
            .ok_or_else(|| zbus::fdo::Error::Failed("this conversation has no valid peer".into()))
    }

    /// Whether the peer may see our presence (so may know we're typing
    /// or have read their messages).
    async fn shares_presence(&self) -> bool {
        self.session
            .store
            .contacts(self.session.account_id)
            .await
            .ok()
            .and_then(|cs| cs.into_iter().find(|c| c.jid == self.row.peer))
            .is_some_and(|c| c.subscription == "from" || c.subscription == "both")
    }

    /// Whether to encrypt what we send now. In one-to-one chats that is
    /// the setting (whether the peer has OMEMO is decided when sending,
    /// after fetching their devices); in rooms, the room must be private.
    async fn wants_encryption(&self) -> bool {
        if self.is_room() {
            crate::omemo::conversation_encryption(
                &self.session,
                &self.row.peer,
                self.row.encryption_wanted,
            )
            .await
                == "omemo"
        } else {
            self.row.encryption_wanted
        }
    }

    fn is_room(&self) -> bool {
        self.row.kind == "room"
    }

    fn message_type(&self) -> MessageType {
        if self.is_room() {
            MessageType::Groupchat
        } else {
            MessageType::Chat
        }
    }

    /// Who we are in this conversation: our JID, or our place in the room.
    async fn me(&self) -> String {
        if self.is_room() {
            let nick = self
                .session
                .rooms
                .nick(&self.row.peer)
                .await
                .unwrap_or_default();
            format!("{}/{nick}", self.row.peer)
        } else {
            self.session.jid.to_string()
        }
    }

    /// The id others know `m` by for reactions and replies: in rooms the
    /// room's archive id, elsewhere the message id.
    fn reference(&self, m: &MessageRow) -> String {
        match (&m.server_id, self.is_room()) {
            (Some(id), true) => id.clone(),
            _ => m.origin_id.clone(),
        }
    }

    /// Refuses to send to a room we are not in.
    async fn check_joined(&self) -> zbus::fdo::Result<()> {
        if self.is_room()
            && self
                .session
                .rooms
                .get(&self.row.peer)
                .await
                .is_none_or(|l| l.state != "joined")
        {
            return Err(zbus::fdo::Error::Failed("you are not in this room".into()));
        }
        Ok(())
    }

    /// One of this conversation's messages, by the id the bus uses.
    async fn target(&self, id: &str) -> zbus::fdo::Result<MessageRow> {
        let unknown = || zbus::fdo::Error::InvalidArgs("unknown message id".into());
        let id: i64 = id.parse().map_err(|_| unknown())?;
        self.session
            .store
            .message_by_id(self.row.id, id)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .ok_or_else(unknown)
    }

    async fn save(&self) {
        let _ = self
            .session
            .store
            .update_conversation(self.row.clone())
            .await;
    }
}

#[zbus::interface(name = "net.dunevent.Piopio.Conversation")]
impl ConversationObject {
    #[zbus(property)]
    fn peer(&self) -> String {
        self.row.peer.clone()
    }

    /// chat, room or room-private.
    #[zbus(property)]
    fn kind(&self) -> String {
        self.row.kind.clone()
    }

    /// Whether the peer is typing: composing, paused or "".
    #[zbus(property)]
    fn peer_state(&self) -> String {
        self.peer_state.clone()
    }

    /// Tells the peer whether we are typing: composing, paused or active.
    /// Repeats are not sent again; group chats get nothing.
    async fn set_chat_state(
        &mut self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        state: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let chat_state = match state {
            "composing" => ChatState::Composing,
            "paused" => ChatState::Paused,
            "active" => ChatState::Active,
            _ => {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "composing, paused or active".into(),
                ));
            }
        };
        if self.is_room()
            || self.our_state == state
            || !self.session.is_online()
            || !self.sends_chat_states()
        {
            return Ok(());
        }
        // Only tell contacts who share presence with us that we're typing.
        if !self.shares_presence().await {
            return Ok(());
        }
        self.our_state = state.to_owned();
        let peer = self.valid_peer()?;
        let mut message = Message::new_with_type(MessageType::Chat, Some(Jid::from(peer)))
            .with_payload(chat_state);
        message
            .payloads
            .push(tokio_xmpp::minidom::Element::builder("no-store", rich::HINTS).build());
        self.session.send(message).await;
        Ok(())
    }

    /// The peer's picture (a small PNG), or "".
    #[zbus(property)]
    async fn avatar(&self) -> String {
        crate::avatars::path_of(&self.session, &self.row.peer).await
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.title.clone()
    }

    #[zbus(property)]
    fn unread(&self) -> u32 {
        self.row.unread
    }

    /// Milliseconds since the Unix epoch.
    #[zbus(property)]
    fn last_activity(&self) -> i64 {
        self.row.last_activity
    }

    #[zbus(property)]
    fn pinned(&self) -> bool {
        self.row.pinned
    }

    #[zbus(property)]
    async fn set_pinned(&mut self, pinned: bool) {
        self.row.pinned = pinned;
        self.save().await;
    }

    /// "default" (the account's setting), "on" or "off": whether the peer
    /// sees that we're typing. Writable.
    #[zbus(property)]
    fn send_chat_states(&self) -> String {
        self.row.send_chat_states.clone()
    }

    #[zbus(property)]
    async fn set_send_chat_states(&mut self, value: String) -> zbus::fdo::Result<()> {
        check_override(&value)?;
        self.row.send_chat_states = value;
        self.save().await;
        Ok(())
    }

    /// Whether the peer sees that we're typing, after applying the
    /// account's default.
    #[zbus(property)]
    fn chat_states_enabled(&self) -> bool {
        self.sends_chat_states()
    }

    /// "default", "on" or "off": whether the peer sees that we've read
    /// their messages. Writable.
    #[zbus(property)]
    fn send_receipts(&self) -> String {
        self.row.send_receipts.clone()
    }

    #[zbus(property)]
    async fn set_send_receipts(&mut self, value: String) -> zbus::fdo::Result<()> {
        check_override(&value)?;
        self.row.send_receipts = value;
        self.save().await;
        Ok(())
    }

    /// Whether the peer sees that we've read their messages, after
    /// applying the account's default.
    #[zbus(property)]
    fn receipts_enabled(&self) -> bool {
        self.sends_receipts()
    }

    /// What the peer's apps support (disco#info features of their online
    /// devices, or what they last had). Empty when unknown.
    #[zbus(property)]
    async fn peer_features(&self) -> Vec<String> {
        if self.is_room() {
            return vec![];
        }
        crate::features::peer(&self.session, &self.row.peer).await
    }

    /// Whether the peer is on our blocklist (XEP-0191). Account.Block and
    /// Account.Unblock change it.
    #[zbus(property)]
    async fn blocked(&self) -> bool {
        self.session.blocked.contains(&self.row.peer).await
    }

    /// "default" (the app's sound), "silent" or "custom" (set with
    /// SetNotificationSound). Writable with "default" or "silent".
    #[zbus(property)]
    fn notification_sound(&self) -> String {
        self.row.notification_sound.clone()
    }

    #[zbus(property)]
    async fn set_notification_sound(&mut self, value: String) -> zbus::fdo::Result<()> {
        if !matches!(value.as_str(), "default" | "silent") {
            return Err(zbus::fdo::Error::InvalidArgs(
                "default or silent; a custom sound is set with SetNotificationSound".into(),
            ));
        }
        self.row.notification_sound = value;
        self.save().await;
        Ok(())
    }

    /// Plays this sound file (read from `file`, at most 2 MiB) for this
    /// conversation's notifications.
    async fn set_custom_sound(
        &mut self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        file: zbus::zvariant::OwnedFd,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        crate::settings::save_sound(self.row.id.to_string(), file.into())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
        self.row.notification_sound = "custom".into();
        self.save().await;
        let _ = self.notification_sound_changed(&emitter).await;
        Ok(())
    }

    /// Plays this conversation's notification sound (through a test
    /// notification that is taken back after a few seconds).
    async fn test_notification_sound(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let (sound, file) = crate::notify::sound_of(&self.notification_style());
        crate::notify::test(&self.session.bus, &sound, file).await
    }

    /// Whether notifications ring through Do Not Disturb (GNOME) and the
    /// Quiet and Silent profiles (Phosh). Writable.
    #[zbus(property)]
    fn override_quiet(&self) -> bool {
        self.row.override_quiet
    }

    #[zbus(property)]
    async fn set_override_quiet(&mut self, on: bool) {
        self.row.override_quiet = on;
        self.save().await;
    }

    /// never, mentions or always.
    #[zbus(property)]
    fn muted(&self) -> String {
        self.row.muted.clone()
    }

    #[zbus(property)]
    async fn set_muted(&mut self, muted: String) -> zbus::fdo::Result<()> {
        if !matches!(muted.as_str(), "never" | "mentions" | "always") {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Muted is never, mentions or always".into(),
            ));
        }
        self.row.muted = muted;
        self.save().await;
        Ok(())
    }

    /// The encryption new messages get: "omemo" when EncryptionWanted is set
    /// and the peer has announced OMEMO devices, otherwise "none".
    #[zbus(property)]
    async fn encryption(&self) -> String {
        // Learn the peer's devices if we haven't lately (a contact whose
        // conversation came from the roster, say); EncryptionChanged
        // follows if that changes the answer.
        if !self.is_room()
            && let Some(peer) = self.peer_jid()
        {
            crate::omemo::refresh(&self.session, peer);
        }
        crate::omemo::conversation_encryption(
            &self.session,
            &self.row.peer,
            self.row.encryption_wanted,
        )
        .await
        .into()
    }

    #[zbus(property)]
    fn encryption_wanted(&self) -> bool {
        self.row.encryption_wanted
    }

    #[zbus(property)]
    async fn set_encryption_wanted(
        &mut self,
        wanted: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        self.row.encryption_wanted = wanted;
        self.save().await;
        let _ = self.encryption_changed(&emitter).await;
    }

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        CONVERSATION_API_VERSION
    }

    /// One page of history, newest last: up to `limit` messages older than
    /// the message `before_id` ("" for the newest).
    async fn get_messages(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        before_id: &str,
        limit: u32,
    ) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        access::check(&header)?;
        let before = if before_id.is_empty() {
            0
        } else {
            before_id
                .parse()
                .map_err(|_| zbus::fdo::Error::InvalidArgs("unknown message id".into()))?
        };
        let rows = self
            .session
            .store
            .messages(self.row.id, before, limit.clamp(1, 500))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(rows.iter().map(message_dict).collect())
    }

    /// Sends a message. Returns its id right away; its state follows as
    /// MessageChanged signals (pending → sent → received → displayed).
    /// Options: "reply-to" (s), the id of the message this one answers.
    async fn send(
        &mut self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        body: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::fdo::Result<String> {
        access::check(&header)?;
        if body.trim().is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("the message is empty".into()));
        }
        let peer = self.valid_peer()?;
        self.check_joined().await?;
        // Sending ends typing; the message itself says we're active.
        self.our_state = "active".into();
        let reply_to = match options.get("reply-to") {
            Some(v) => {
                let id = <&str>::try_from(v)
                    .map_err(|_| zbus::fdo::Error::InvalidArgs("reply-to is a string".into()))?;
                Some(self.target(id).await?)
            }
            None => None,
        };
        let origin_id = new_id();
        let encryption = crate::omemo::conversation_encryption(
            &self.session,
            &self.row.peer,
            self.row.encryption_wanted,
        )
        .await;
        let mut new = NewMessage {
            origin_id: origin_id.clone(),
            server_id: None,
            outgoing: true,
            sender: self.me().await,
            body: body.to_owned(),
            timestamp: now_ms(),
            state: state::PENDING,
            encryption,
            ..Default::default()
        };
        let mut extra = vec![];
        let mut wire_body = body.to_owned();
        if let Some(target) = &reply_to {
            new.reply_to = self.reference(target);
            new.reply_to_sender = target.sender.clone();
            new.reply_preview = rich::preview(&target.body);
            let (elements, quoted) =
                rich::reply_elements(&new.reply_to, &target.sender, &new.reply_preview, body);
            extra = elements;
            wire_body = quoted;
        }
        let row = self
            .session
            .store
            .insert_message(self.row.id, new)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .expect("a fresh origin id is never a duplicate");

        Self::message_added(&emitter, message_dict(&row)).await?;
        self.row.last_activity = row.timestamp;
        self.last_activity_changed(&emitter).await?;
        self.save().await;

        let session = self.session.clone();
        let conversation = self.row.id;
        // Group chats are not encrypted yet.
        let wanted = self.wants_encryption().await;
        let kind = self.message_type();
        let tracked = !self.is_room();
        let message_id = row.id;
        tokio::spawn(async move {
            let (new_state, used) = deliver(
                &session, &peer, kind, wanted, &origin_id, &wire_body, extra, tracked,
            )
            .await;
            if used != encryption
                && let Ok(true) = session
                    .store
                    .set_message_encryption(conversation, origin_id.clone(), used)
                    .await
            {
                let changes = HashMap::from([("encryption", Value::from(used))]);
                emit_changed(&session, conversation, message_id, changes).await;
            }
            if let Ok(Some(row)) = session
                .store
                .advance_state(conversation, origin_id, new_state)
                .await
            {
                let changes = HashMap::from([("state", Value::from(row.state))]);
                emit_changed(&session, conversation, message_id, changes).await;
            }
        });
        Ok(row.id.to_string())
    }

    /// Shares a file (read from `file`, an open file descriptor). Returns
    /// the message's id right away; the upload follows as MessageChanged
    /// (file-state uploading → done, then the message's state).
    async fn send_file(
        &mut self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        file: zbus::zvariant::OwnedFd,
        name: &str,
        _options: HashMap<&str, Value<'_>>,
    ) -> zbus::fdo::Result<String> {
        access::check(&header)?;
        let peer = self.valid_peer()?;
        self.check_joined().await?;
        let fd: std::os::fd::OwnedFd = file.into();
        let data = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut data = Vec::new();
            std::fs::File::from(fd)
                .take(crate::files::MAX_FILE + 1)
                .read_to_end(&mut data)?;
            Ok(data)
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        .map_err(|e| zbus::fdo::Error::Failed(format!("the file can't be read: {e}")))?;
        if data.len() as u64 > crate::files::MAX_FILE {
            return Err(zbus::fdo::Error::InvalidArgs(
                "the file is too large".into(),
            ));
        }
        let name = {
            let n = crate::files::name_of(&format!("/{}", name.replace('/', "_")));
            if n.is_empty() { "file".to_owned() } else { n }
        };
        let mime = crate::files::mime_of(&name).to_owned();
        let encryption = crate::omemo::conversation_encryption(
            &self.session,
            &self.row.peer,
            self.row.encryption_wanted,
        )
        .await;
        let origin_id = new_id();
        let new = NewMessage {
            origin_id: origin_id.clone(),
            server_id: None,
            outgoing: true,
            sender: self.me().await,
            body: String::new(),
            timestamp: now_ms(),
            state: state::PENDING,
            encryption,
            ..Default::default()
        };
        let mut row = self
            .session
            .store
            .insert_message(self.row.id, new)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .expect("a fresh origin id is never a duplicate");
        let (path, thumbnail) = crate::files::keep_own_copy(row.id, &name, &mime, data.clone())
            .await
            .unwrap_or_default();
        let mut attachment = crate::store::AttachmentRow {
            message_id: row.id,
            url: String::new(),
            name: name.clone(),
            mime: mime.clone(),
            size: Some(data.len() as i64),
            path,
            thumbnail,
            state: crate::files::state::UPLOADING.into(),
        };
        let _ = self.session.store.set_attachment(attachment.clone()).await;
        row.attachment = Some(attachment.clone());
        Self::message_added(&emitter, message_dict(&row)).await?;
        self.row.last_activity = row.timestamp;
        self.last_activity_changed(&emitter).await?;
        self.save().await;

        let session = self.session.clone();
        let conversation = self.row.id;
        let kind = self.message_type();
        let tracked = !self.is_room();
        let wanted = self.wants_encryption().await;
        let message_id = row.id;
        tokio::spawn(async move {
            // Only encrypt the file if the message carrying its key will be.
            let wanted = wanted
                && (kind == MessageType::Groupchat
                    || crate::omemo::peer_uses_omemo(&session, &peer).await);
            let link = crate::files::upload(&session, &name, &mime, data, wanted).await;
            let new_state = match link {
                Err(e) => {
                    tracing::warn!("upload of {name} failed: {e:#}");
                    attachment.state = crate::files::state::FAILED.into();
                    state::FAILED
                }
                Ok(link) => {
                    attachment.url = link.clone();
                    attachment.state = crate::files::state::DONE.into();
                    let _ = session.store.set_body(message_id, link.clone()).await;
                    // An encrypted file's link only travels encrypted.
                    let extra = if wanted {
                        vec![]
                    } else {
                        vec![crate::files::oob_element(&link)]
                    };
                    let (s, _) = deliver(
                        &session, &peer, kind, wanted, &origin_id, &link, extra, tracked,
                    )
                    .await;
                    let changes = HashMap::from([("body", Value::from(link))]);
                    emit_changed(&session, conversation, message_id, changes).await;
                    s
                }
            };
            let _ = session.store.set_attachment(attachment.clone()).await;
            let changes = HashMap::from([
                ("file-url", Value::from(attachment.url.clone())),
                ("file-state", Value::from(attachment.state.clone())),
            ]);
            emit_changed(&session, conversation, message_id, changes).await;
            if let Ok(Some(row)) = session
                .store
                .advance_state(conversation, origin_id, new_state)
                .await
            {
                let changes = HashMap::from([("state", Value::from(row.state))]);
                emit_changed(&session, conversation, message_id, changes).await;
            }
        });
        Ok(row.id.to_string())
    }

    /// Downloads a message's file (pictures come by themselves). Progress
    /// follows as MessageChanged (file-state, file-path).
    async fn download_file(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        id: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let target = self.target(id).await?;
        let Some(mut a) = target.attachment else {
            return Err(zbus::fdo::Error::InvalidArgs(
                "the message has no file".into(),
            ));
        };
        if a.state == crate::files::state::FAILED {
            a.state = crate::files::state::REMOTE.into();
        }
        let session = self.session.clone();
        let conversation = self.row.id;
        tokio::spawn(async move {
            crate::files::download(&session, conversation, a, crate::files::MAX_FILE).await
        });
        Ok(())
    }

    /// Deletes the conversation and its history from this computer (the
    /// server's archive keeps its copy). Leaves a group chat for good.
    async fn delete(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let session = self.session.clone();
        let peer = self.row.peer.clone();
        // Not from inside this object's own method call.
        tokio::spawn(async move { session.conversations.delete(&session, &peer).await });
        Ok(())
    }

    /// Corrects one of our messages (XEP-0308).
    async fn correct(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        id: &str,
        body: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        if body.trim().is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "a correction can't be empty; retract the message instead".into(),
            ));
        }
        let peer = self.valid_peer()?;
        self.check_joined().await?;
        let target = self.target(id).await?;
        let me = self.me().await;
        if !target.outgoing || target.retracted || target.sender != me {
            return Err(zbus::fdo::Error::InvalidArgs(
                "only your own messages can be corrected".into(),
            ));
        }
        let mut wire_body = body.to_owned();
        let mut extra = vec![rich::replace(&target.origin_id)];
        if !target.reply_to.is_empty() {
            let (elements, quoted) = rich::reply_elements(
                &target.reply_to,
                &target.reply_to_sender,
                &target.reply_preview,
                body,
            );
            extra.extend(elements);
            wire_body = quoted;
        }
        let encrypt = self.wants_encryption().await;
        let (state, _) = deliver(
            &self.session,
            &peer,
            self.message_type(),
            encrypt,
            &new_id(),
            &wire_body,
            extra,
            false,
        )
        .await;
        if state == state::FAILED {
            return Err(zbus::fdo::Error::Failed(
                "the correction could not be sent".into(),
            ));
        }
        rich::correct(
            &self.session,
            self.row.id,
            Some(target),
            &me,
            body.to_owned(),
        )
        .await;
        Ok(())
    }

    /// Sets our reactions to a message (XEP-0444); an empty list removes them.
    async fn react(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        id: &str,
        emojis: Vec<String>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let peer = self.valid_peer()?;
        self.check_joined().await?;
        let target = self.target(id).await?;
        let mut unique: Vec<String> = Vec::new();
        for e in emojis {
            let e = e.trim().to_owned();
            if !e.is_empty() && !unique.contains(&e) {
                unique.push(e);
            }
        }
        let extra = vec![
            rich::reactions_element(&self.reference(&target), &unique),
            rich::store_hint(),
        ];
        if send_plain(&self.session, &peer, self.message_type(), None, extra).await == state::FAILED
        {
            return Err(zbus::fdo::Error::Failed(
                "the reaction could not be sent".into(),
            ));
        }
        rich::react(&self.session, self.row.id, target, self.me().await, unique).await;
        Ok(())
    }

    /// Retracts one of our messages (XEP-0424): everyone's clients that
    /// support it remove it.
    async fn retract(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        id: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let peer = self.valid_peer()?;
        self.check_joined().await?;
        let target = self.target(id).await?;
        let me = self.me().await;
        if !target.outgoing || target.sender != me {
            return Err(zbus::fdo::Error::InvalidArgs(
                "only your own messages can be retracted".into(),
            ));
        }
        let extra = rich::retract_elements(&target.origin_id);
        let kind = self.message_type();
        if send_plain(
            &self.session,
            &peer,
            kind,
            Some(rich::RETRACT_FALLBACK),
            extra,
        )
        .await
            == state::FAILED
        {
            return Err(zbus::fdo::Error::Failed(
                "the retraction could not be sent".into(),
            ));
        }
        rich::retract(&self.session, self.row.id, target, &me).await;
        Ok(())
    }

    /// The user has seen everything: clears Unread and, for contacts who share
    /// presence with us, sends a displayed marker for the newest message.
    async fn mark_read(
        &mut self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        if self.row.unread != 0 {
            self.row.unread = 0;
            self.unread_changed(&emitter).await?;
            self.save().await;
        }
        if let Some(path) = self.object_path() {
            crate::notify::withdraw(&self.session, &path).await;
        }
        let Some(peer) = self.peer_jid() else {
            return Ok(());
        };
        if self.is_room() || !self.sends_receipts() || !self.shares_presence().await {
            return Ok(());
        }
        let newest_incoming = self
            .session
            .store
            .messages(self.row.id, 0, 50)
            .await
            .ok()
            .and_then(|ms| ms.into_iter().rev().find(|m| !m.outgoing));
        if let Some(m) = newest_incoming {
            let marker = Message::chat(Some(Jid::from(peer))).with_payload(Displayed {
                id: Id(m.origin_id),
            });
            self.session.send(marker).await;
        }
        Ok(())
    }

    /// A message was stored: sent from here, received, or found in the archive.
    #[zbus(signal)]
    async fn message_added(
        emitter: &SignalEmitter<'_>,
        message: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// Something about a stored message changed, such as its state.
    #[zbus(signal)]
    async fn message_changed(
        emitter: &SignalEmitter<'_>,
        id: &str,
        changes: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}
