//! Jingle file transfer (XEP-0234) over in-band bytestreams (XEP-0261 and
//! XEP-0047): how Conversations, Dino and Gajim send a file straight to a
//! contact's device when there is no upload service.
//!
//! In-band bytestreams go through the server, so they work wherever chat
//! does. Offers over SOCKS5 bytestreams (XEP-0260) are accepted with no
//! candidates of our own and a candidate-error, which the other side answers
//! by replacing the transport with IBB, as XEP-0260 describes.
//!
//! Files are taken only from contacts (and our own other devices), never
//! with Jingle encryption (JET), which no client agrees on yet: Piopio
//! declines those, and only sends this way in unencrypted conversations, so
//! nothing goes in the clear to someone who uses OMEMO.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, oneshot};
use tokio_xmpp::jid::{BareJid, FullJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::stanza_error::DefinedCondition;

use crate::files::state as file_state;
use crate::pep::n;
use crate::session::Session;
use crate::store::{AttachmentRow, NewMessage};

pub const FT: &str = "urn:xmpp:jingle:apps:file-transfer:5";
pub const IBB_TRANSPORT: &str = "urn:xmpp:jingle:transports:ibb:1";
pub const S5B_TRANSPORT: &str = "urn:xmpp:jingle:transports:s5b:1";
pub const IBB: &str = "http://jabber.org/protocol/ibb";
const JINGLE: &str = crate::jingle_sdp::JINGLE;
const HASHES: &str = "urn:xmpp:hashes:2";
/// Bytes per IBB block: what most clients use.
const BLOCK_SIZE: usize = 4096;
/// A transfer that stalls this long is given up.
const STALL: std::time::Duration = std::time::Duration::from_secs(60);

/// The account's file transfers, by Jingle session id.
#[derive(Default)]
pub struct Transfers {
    by_sid: Mutex<HashMap<String, Arc<Mutex<Transfer>>>>,
}

struct Transfer {
    sid: String,
    peer: FullJid,
    incoming: bool,
    content_name: String,
    /// The in-band bytestream, once agreed.
    ibb_sid: Option<String>,
    block_size: usize,
    /// What the file says about itself.
    name: String,
    mime: String,
    size: u64,
    sha256: Option<Vec<u8>>,
    /// Received so far, and the next expected sequence number.
    data: Vec<u8>,
    seq: u16,
    /// Where it shows: the message and its attachment.
    conversation: i64,
    message: i64,
    attachment: AttachmentRow,
    origin_id: String,
    /// Outgoing: told when the other side accepts (true) or ends it (false).
    accepted: Option<oneshot::Sender<bool>>,
    finished: bool,
}

impl Transfers {
    async fn get(&self, sid: &str) -> Option<Arc<Mutex<Transfer>>> {
        self.by_sid.lock().await.get(sid).cloned()
    }

    async fn by_ibb(&self, ibb_sid: &str) -> Option<Arc<Mutex<Transfer>>> {
        for t in self.by_sid.lock().await.values() {
            if t.lock().await.ibb_sid.as_deref() == Some(ibb_sid) {
                return Some(t.clone());
            }
        }
        None
    }

    async fn remove(&self, sid: &str) {
        self.by_sid.lock().await.remove(sid);
    }
}

fn new_sid() -> String {
    let mut b = [0u8; 12];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn jingle(action: &str, sid: &str) -> tokio_xmpp::minidom::ElementBuilder {
    Element::builder("jingle", JINGLE)
        .attr(n("action"), action)
        .attr(n("sid"), sid)
}

fn el(name: &str, ns: &str) -> tokio_xmpp::minidom::ElementBuilder {
    Element::builder(name, ns)
}

fn attr(
    e: tokio_xmpp::minidom::ElementBuilder,
    k: &'static str,
    v: &str,
) -> tokio_xmpp::minidom::ElementBuilder {
    e.attr(n(k), v)
}

fn file_element(name: &str, mime: &str, size: u64, sha256: Option<&[u8]>) -> Element {
    let mut file = el("file", FT)
        .append(el("media-type", FT).append(mime).build())
        .append(el("name", FT).append(name).build())
        .append(el("size", FT).append(size.to_string().as_str()).build());
    if let Some(h) = sha256 {
        file = file.append(
            attr(el("hash", HASHES), "algo", "sha-256")
                .append(B64.encode(h).as_str())
                .build(),
        );
    }
    file.build()
}

fn ibb_transport(sid: &str, block_size: usize) -> Element {
    attr(
        attr(el("transport", IBB_TRANSPORT), "sid", sid),
        "block-size",
        &block_size.to_string(),
    )
    .build()
}

fn content(creator: &str, name: &str, children: Vec<Element>) -> Element {
    let mut c = attr(
        attr(
            attr(el("content", JINGLE), "creator", creator),
            "name",
            name,
        ),
        "senders",
        "initiator",
    );
    for child in children {
        c = c.append(child);
    }
    c.build()
}

fn reason(condition: &str) -> Element {
    el("reason", JINGLE)
        .append(el(condition, JINGLE).build())
        .build()
}

async fn send_set(session: &Session, to: &FullJid, payload: Element) -> bool {
    let iq = crate::pep::set(Some(Jid::from(to.clone())), payload);
    session.iq(iq).await.is_ok()
}

async fn terminate(session: &Session, t: &Arc<Mutex<Transfer>>, why: &str) {
    let (peer, sid) = {
        let mut t = t.lock().await;
        if t.finished {
            return;
        }
        t.finished = true;
        (t.peer.clone(), t.sid.clone())
    };
    let _ = send_set(
        session,
        &peer,
        jingle("session-terminate", &sid)
            .append(reason(why))
            .build(),
    )
    .await;
    session.transfers.remove(&sid).await;
}

/// Updates the attachment (and tells the UI).
async fn show(session: &Session, t: &Transfer) {
    crate::files::save_attachment(session, t.conversation, &t.attachment).await;
}

async fn fail(session: &Session, t: &Arc<Mutex<Transfer>>, why: &str) {
    {
        let mut tr = t.lock().await;
        tracing::info!(peer = %tr.peer, name = %tr.name, "file transfer failed: {why}");
        tr.attachment.state = file_state::FAILED.into();
        show(session, &tr).await;
        if !tr.incoming {
            crate::messages::set_state(session, tr.conversation, &tr.origin_id, "failed").await;
        }
    }
    terminate(session, t, "failed-transport").await;
}

// --- receiving ---------------------------------------------------------------

/// Whether a Jingle session-initiate offers a file.
pub fn is_offer(jingle: &Element) -> bool {
    jingle
        .children()
        .filter(|c| c.is("content", JINGLE))
        .any(|c| c.get_child("description", FT).is_some())
}

/// Whether a Jingle request belongs to one of our transfers.
pub async fn owns(session: &Session, sid: &str) -> bool {
    session.transfers.get(sid).await.is_some()
}

/// Handles a Jingle IQ for a file transfer. Returns the IQ's answer.
pub async fn jingle_iq(session: &Session, from: Jid, id: String, payload: Element) -> Iq {
    let ok = crate::session::result(Some(from.clone()), id.clone());
    let error = |c| crate::session::error(Some(from.clone()), id.clone(), c);
    let Ok(peer) = from.clone().try_into_full() else {
        return error(DefinedCondition::BadRequest);
    };
    let sid = payload.attr("sid").unwrap_or_default().to_owned();
    let action = payload.attr("action").unwrap_or_default().to_owned();
    if action == "session-initiate" {
        if session.transfers.get(&sid).await.is_some() {
            return error(DefinedCondition::Conflict);
        }
        let session2 = session.clone();
        tokio::spawn(async move { offered(&session2, peer, sid, payload).await });
        return ok;
    }
    let Some(t) = session.transfers.get(&sid).await else {
        return error(DefinedCondition::ItemNotFound);
    };
    if t.lock().await.peer != peer {
        return error(DefinedCondition::ItemNotFound);
    }
    let session = session.clone();
    match action.as_str() {
        "session-accept" => {
            let mut tr = t.lock().await;
            // They may have lowered the block size.
            if let Some(size) = payload
                .children()
                .filter_map(|c| c.get_child("transport", IBB_TRANSPORT))
                .find_map(|tp| tp.attr("block-size")?.parse::<usize>().ok())
            {
                tr.block_size = tr.block_size.min(size).max(1);
            }
            if let Some(tx) = tr.accepted.take() {
                let _ = tx.send(true);
            }
        }
        "session-terminate" => {
            let mut tr = t.lock().await;
            if let Some(tx) = tr.accepted.take() {
                let _ = tx.send(false);
            }
            let done = tr.attachment.state == file_state::DONE;
            tr.finished = true;
            if !done {
                tr.attachment.state = file_state::FAILED.into();
                show(&session, &tr).await;
                if !tr.incoming {
                    crate::messages::set_state(&session, tr.conversation, &tr.origin_id, "failed")
                        .await;
                }
            }
            let sid = tr.sid.clone();
            drop(tr);
            session.transfers.remove(&sid).await;
        }
        "transport-replace" => {
            // The other side falls back to IBB (after SOCKS5 failed).
            let Some(tp) = payload
                .children()
                .find_map(|c| c.get_child("transport", IBB_TRANSPORT))
            else {
                return error(DefinedCondition::FeatureNotImplemented);
            };
            let mut tr = t.lock().await;
            tr.ibb_sid = tp.attr("sid").map(str::to_owned);
            tr.block_size = tp
                .attr("block-size")
                .and_then(|b| b.parse::<usize>().ok())
                .unwrap_or(BLOCK_SIZE)
                .clamp(1, 65535);
            let accept = jingle("transport-accept", &tr.sid)
                .append(content(
                    "initiator",
                    &tr.content_name,
                    vec![ibb_transport(
                        tr.ibb_sid.as_deref().unwrap_or(""),
                        tr.block_size,
                    )],
                ))
                .build();
            let peer = tr.peer.clone();
            drop(tr);
            tokio::spawn(async move { send_set(&session, &peer, accept).await });
        }
        // Their side of SOCKS5 (candidate-used/-error) and checksums: the
        // fallback to IBB follows from them.
        "transport-info" | "session-info" | "transport-accept" => {}
        _ => return error(DefinedCondition::FeatureNotImplemented),
    }
    ok
}

/// Someone offers a file: accept it from contacts, decline otherwise.
async fn offered(session: &Session, peer: FullJid, sid: String, payload: Element) {
    let decline = |why: &'static str| {
        let (session, peer, sid) = (session.clone(), peer.clone(), sid.clone());
        async move {
            let _ = send_set(
                &session,
                &peer,
                jingle("session-terminate", &sid)
                    .append(reason(why))
                    .build(),
            )
            .await;
        }
    };
    let Some(offer) = payload.children().find(|c| c.is("content", JINGLE)) else {
        return decline("cancel").await;
    };
    let Some(file) = offer
        .get_child("description", FT)
        .and_then(|d| d.get_child("file", FT))
    else {
        return decline("unsupported-applications").await;
    };
    // Jingle encryption (JET): not interoperable yet.
    if offer.children().any(|c| c.name() == "security") {
        return decline("security-error").await;
    }
    let bare = peer.to_bare();
    let from_contact = bare == session.jid
        || session
            .store
            .contacts(session.account_id)
            .await
            .unwrap_or_default()
            .iter()
            .any(|c| c.jid == bare.as_str());
    if !from_contact {
        tracing::info!(%peer, "declined a file from someone who isn't a contact");
        return decline("decline").await;
    }
    let text = |n: &str| file.get_child(n, FT).map(|e| e.text()).unwrap_or_default();
    let size: u64 = text("size").trim().parse().unwrap_or(0);
    if size == 0 || size > crate::files::MAX_FILE {
        return decline("media-error").await;
    }
    let raw_name = text("name");
    let name = {
        let n = crate::files::name_of(&format!("/{}", raw_name.replace('/', "_")));
        if n.is_empty() { "file".to_owned() } else { n }
    };
    let mime = match text("media-type") {
        m if !m.is_empty() && m.contains('/') => m,
        _ => crate::files::mime_of(&name).to_owned(),
    };
    let sha256 = file
        .children()
        .find(|h| h.is("hash", HASHES) && h.attr("algo") == Some("sha-256"))
        .and_then(|h| B64.decode(h.text().trim()).ok());
    let content_name = offer.attr("name").unwrap_or("file").to_owned();
    let creator = offer.attr("creator").unwrap_or("initiator").to_owned();
    let (ibb, s5b) = (
        offer.get_child("transport", IBB_TRANSPORT),
        offer.get_child("transport", S5B_TRANSPORT),
    );
    if ibb.is_none() && s5b.is_none() {
        return decline("unsupported-transports").await;
    }

    // It shows in the conversation as it arrives.
    let outgoing = bare == session.jid;
    let peer_conv = bare.clone();
    let origin_id = format!("jingle-{sid}");
    let new = NewMessage {
        origin_id: origin_id.clone(),
        outgoing,
        sender: bare.to_string(),
        timestamp: crate::session::now_ms(),
        state: "received",
        encryption: "none",
        ..Default::default()
    };
    let attachment = AttachmentRow {
        message_id: 0,
        url: String::new(),
        name: name.clone(),
        mime: mime.clone(),
        size: Some(size as i64),
        path: String::new(),
        thumbnail: String::new(),
        state: file_state::DOWNLOADING.into(),
    };
    let Some((row, path)) =
        crate::messages::store_incoming(session, &peer_conv, new, Some(attachment)).await
    else {
        return decline("general-error").await;
    };
    let conversation = crate::messages::conversation_id(session, &peer_conv)
        .await
        .unwrap_or_default();
    let attachment = row.attachment.clone().expect("stored with it");
    let mut t = Transfer {
        sid: sid.clone(),
        peer: peer.clone(),
        incoming: true,
        content_name: content_name.clone(),
        ibb_sid: None,
        block_size: BLOCK_SIZE,
        name,
        mime,
        size,
        sha256,
        data: Vec::with_capacity(size.min(16 * 1024 * 1024) as usize),
        seq: 0,
        conversation,
        message: row.id,
        attachment,
        origin_id,
        accepted: None,
        finished: false,
    };
    crate::notify::message(session, &path, &row).await;

    let transport = match ibb {
        Some(ibb) => {
            t.ibb_sid = ibb.attr("sid").map(str::to_owned);
            t.block_size = ibb
                .attr("block-size")
                .and_then(|b| b.parse::<usize>().ok())
                .unwrap_or(BLOCK_SIZE)
                .clamp(1, 65535);
            ibb_transport(t.ibb_sid.as_deref().unwrap_or(""), t.block_size)
        }
        // SOCKS5: accept without candidates of ours; we then report that
        // none of theirs worked, and they replace the transport with IBB.
        None => {
            let s5b_sid = s5b.and_then(|s| s.attr("sid")).unwrap_or_default();
            attr(
                attr(el("transport", S5B_TRANSPORT), "sid", s5b_sid),
                "mode",
                "tcp",
            )
            .build()
        }
    };
    let description = offer
        .get_child("description", FT)
        .cloned()
        .expect("checked above");
    let accept = jingle("session-accept", &sid)
        .attr(n("responder"), session_full(session).await.as_str())
        .append(
            attr(
                attr(
                    attr(el("content", JINGLE), "creator", &creator),
                    "name",
                    &content_name,
                ),
                "senders",
                "initiator",
            )
            .append(description)
            .append(transport.clone())
            .build(),
        )
        .build();
    let t = Arc::new(Mutex::new(t));
    session
        .transfers
        .by_sid
        .lock()
        .await
        .insert(sid.clone(), t.clone());
    if !send_set(session, &peer, accept).await {
        return fail(session, &t, "they didn't take our acceptance").await;
    }
    if transport.is("transport", S5B_TRANSPORT) {
        let s5b_sid = transport.attr("sid").unwrap_or_default().to_owned();
        let info = jingle("transport-info", &sid)
            .append(content(
                &creator,
                &content_name,
                vec![
                    attr(el("transport", S5B_TRANSPORT), "sid", &s5b_sid)
                        .append(el("candidate-error", S5B_TRANSPORT).build())
                        .build(),
                ],
            ))
            .build();
        let _ = send_set(session, &peer, info).await;
    }
    watch_stall(session, t);
}

/// Our own full JID, as the responder.
async fn session_full(session: &Session) -> String {
    session
        .bound
        .lock()
        .await
        .as_ref()
        .map(|j| j.to_string())
        .unwrap_or_else(|| session.jid.to_string())
}

/// Gives a transfer up when nothing arrives for a while.
fn watch_stall(session: &Session, t: Arc<Mutex<Transfer>>) {
    let session = session.clone();
    tokio::spawn(async move {
        let mut last = 0usize;
        loop {
            tokio::time::sleep(STALL).await;
            let (finished, got) = {
                let tr = t.lock().await;
                (tr.finished, tr.data.len())
            };
            if finished {
                return;
            }
            if got == last {
                return fail(&session, &t, "it stalled").await;
            }
            last = got;
        }
    });
}

/// An in-band bytestream request (open, data or close). Returns the answer.
pub async fn ibb_iq(session: &Session, from: Jid, id: String, payload: Element) -> Iq {
    let ok = crate::session::result(Some(from.clone()), id.clone());
    let error = |c| crate::session::error(Some(from.clone()), id.clone(), c);
    let Some(ibb_sid) = payload.attr("sid").map(str::to_owned) else {
        return error(DefinedCondition::BadRequest);
    };
    let Some(t) = session.transfers.by_ibb(&ibb_sid).await else {
        return error(DefinedCondition::ItemNotFound);
    };
    let mut tr = t.lock().await;
    if from != tr.peer || !tr.incoming {
        return error(DefinedCondition::ItemNotFound);
    }
    match payload.name() {
        "open" => {
            if let Some(size) = payload
                .attr("block-size")
                .and_then(|b| b.parse::<usize>().ok())
            {
                if size > 65535 {
                    return error(DefinedCondition::ResourceConstraint);
                }
                tr.block_size = size;
            }
            if payload.attr("stanza").is_some_and(|s| s != "iq") {
                return error(DefinedCondition::FeatureNotImplemented);
            }
            ok
        }
        "data" => {
            let seq = payload.attr("seq").and_then(|s| s.parse::<u16>().ok());
            if seq != Some(tr.seq) {
                drop(tr);
                let session = session.clone();
                tokio::spawn(async move { fail(&session, &t, "a block went missing").await });
                return error(DefinedCondition::UnexpectedRequest);
            }
            let Ok(block) = B64.decode(payload.text().trim()) else {
                return error(DefinedCondition::BadRequest);
            };
            if block.len() > tr.block_size || (tr.data.len() + block.len()) as u64 > tr.size {
                drop(tr);
                let session = session.clone();
                tokio::spawn(async move { fail(&session, &t, "more data than offered").await });
                return error(DefinedCondition::NotAcceptable);
            }
            tr.data.extend_from_slice(&block);
            tr.seq = tr.seq.wrapping_add(1);
            ok
        }
        "close" => {
            drop(tr);
            let session = session.clone();
            tokio::spawn(async move { received_all(&session, &t).await });
            ok
        }
        _ => error(DefinedCondition::BadRequest),
    }
}

/// The bytestream closed: check and keep the file.
async fn received_all(session: &Session, t: &Arc<Mutex<Transfer>>) {
    let (data, ok) = {
        let mut tr = t.lock().await;
        let complete = tr.data.len() as u64 == tr.size;
        let matches = tr
            .sha256
            .as_ref()
            .is_none_or(|h| Sha256::digest(&tr.data).as_slice() == h.as_slice());
        (std::mem::take(&mut tr.data), complete && matches)
    };
    if !ok {
        return fail(session, t, "the file arrived incomplete or changed").await;
    }
    let (message, name, mime) = {
        let tr = t.lock().await;
        (tr.message, tr.name.clone(), tr.mime.clone())
    };
    match crate::files::keep_own_copy(message, &name, &mime, data).await {
        Ok((path, thumbnail)) => {
            let mut tr = t.lock().await;
            tr.attachment.path = path;
            tr.attachment.thumbnail = thumbnail;
            tr.attachment.state = file_state::DONE.into();
            show(session, &tr).await;
        }
        Err(e) => return fail(session, t, &format!("it couldn't be saved: {e:#}")).await,
    }
    terminate(session, t, "success").await;
}

// --- sending -----------------------------------------------------------------

/// The contact's online device that takes files over Jingle with IBB.
pub async fn device_for(session: &Session, peer: &BareJid) -> Option<FullJid> {
    crate::features::resources_with(session, peer, &[FT, IBB_TRANSPORT])
        .await
        .into_iter()
        .next()
}

/// Sends a file to a contact's device. The message (`origin_id` in
/// `conversation`) and its attachment follow the transfer.
#[allow(clippy::too_many_arguments)]
pub async fn send(
    session: &Session,
    to: FullJid,
    conversation: i64,
    message: i64,
    origin_id: String,
    mut attachment: AttachmentRow,
    data: Vec<u8>,
) -> bool {
    let sid = new_sid();
    let ibb_sid = new_sid();
    let sha256 = Sha256::digest(&data).to_vec();
    let size = data.len() as u64;
    let (tx, rx) = oneshot::channel();
    attachment.state = file_state::UPLOADING.into();
    let t = Arc::new(Mutex::new(Transfer {
        sid: sid.clone(),
        peer: to.clone(),
        incoming: false,
        content_name: "a-file-offer".into(),
        ibb_sid: Some(ibb_sid.clone()),
        block_size: BLOCK_SIZE,
        name: attachment.name.clone(),
        mime: attachment.mime.clone(),
        size,
        sha256: Some(sha256.clone()),
        data: Vec::new(),
        seq: 0,
        conversation,
        message,
        attachment: attachment.clone(),
        origin_id: origin_id.clone(),
        accepted: Some(tx),
        finished: false,
    }));
    session
        .transfers
        .by_sid
        .lock()
        .await
        .insert(sid.clone(), t.clone());

    let description = el("description", FT)
        .append(file_element(
            &attachment.name,
            &attachment.mime,
            size,
            Some(&sha256),
        ))
        .build();
    let initiate = jingle("session-initiate", &sid)
        .attr(n("initiator"), session_full(session).await.as_str())
        .append(content(
            "initiator",
            "a-file-offer",
            vec![description, ibb_transport(&ibb_sid, BLOCK_SIZE)],
        ))
        .build();
    if !send_set(session, &to, initiate).await {
        fail(session, &t, "they didn't take the offer").await;
        return false;
    }
    crate::messages::set_state(session, conversation, &origin_id, "sent").await;
    // Waiting for them to accept (a person may have to).
    let accepted = matches!(
        tokio::time::timeout(std::time::Duration::from_secs(300), rx).await,
        Ok(Ok(true))
    );
    if !accepted {
        fail(session, &t, "they didn't accept it").await;
        return false;
    }
    let block = t.lock().await.block_size;
    let open = attr(
        attr(
            attr(el("open", IBB), "sid", &ibb_sid),
            "block-size",
            &block.to_string(),
        ),
        "stanza",
        "iq",
    )
    .build();
    if !send_set(session, &to, open).await {
        fail(session, &t, "the bytestream didn't open").await;
        return false;
    }
    for (seq, chunk) in data.chunks(block).enumerate() {
        if t.lock().await.finished {
            return false;
        }
        let d = attr(
            attr(el("data", IBB), "sid", &ibb_sid),
            "seq",
            &((seq as u32 % 65536) as u16).to_string(),
        )
        .append(B64.encode(chunk).as_str())
        .build();
        if !send_set(session, &to, d).await {
            fail(session, &t, "a block wasn't taken").await;
            return false;
        }
    }
    let close = attr(el("close", IBB), "sid", &ibb_sid).build();
    let _ = send_set(session, &to, close).await;
    {
        let mut tr = t.lock().await;
        tr.attachment.state = file_state::DONE.into();
        show(session, &tr).await;
    }
    crate::messages::set_state(session, conversation, &origin_id, "received").await;
    // They end the session once the file is checked; if they don't, we do.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    terminate(session, &t, "success").await;
    true
}
