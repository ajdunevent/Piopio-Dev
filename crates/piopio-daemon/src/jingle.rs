//! Calls: Jingle Message Initiation (XEP-0353) to ring every device, then
//! a Jingle RTP session (XEP-0166/0167) with ICE and DTLS-SRTP, as
//! Conversations, Dino and Monal do it.
//!
//! The daemon only signals. Media runs in the UI (piopio-call), which
//! hands us SDP and ICE candidates through the `net.dunevent.Piopio.Call`
//! object; we turn them into Jingle and back.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::Mutex;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::message::{Message, MessageType};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

use crate::access;
use crate::jingle_sdp::{self as sdp, Description, JINGLE};
use crate::pep::n;
use crate::session::{Session, Shared, new_id};

pub const JMI: &str = "urn:xmpp:jingle-message:0";
const HINTS: &str = "urn:xmpp:hints";
const CALL_API_VERSION: u32 = 1;
/// An unanswered incoming call stops ringing after this long.
const RING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

/// One call's signalling state.
pub struct CallState {
    pub sid: String,
    pub peer: BareJid,
    /// The device of the peer we are talking to, once known.
    pub peer_full: Option<Jid>,
    pub direction: Direction,
    /// ringing (incoming, not answered), calling (outgoing, not answered),
    /// connecting, active or ended.
    pub state: String,
    pub end_reason: String,
    pub video: bool,
    pub path: OwnedObjectPath,
    /// Our side, from the UI's SDP.
    local: Option<Description>,
    /// Their side, from Jingle.
    remote: Option<Description>,
    /// session-initiate sent (outgoing) or received (incoming).
    initiated: bool,
    /// The user answered (incoming).
    answered: bool,
    /// Our candidates (by media line) that arrived before they could be sent.
    queued: Vec<(u32, sdp::Candidate)>,
}

#[derive(Default)]
pub struct Calls {
    calls: Mutex<HashMap<String, Arc<Mutex<CallState>>>>,
    next: AtomicU32,
}

impl Calls {
    async fn get(&self, sid: &str) -> Option<Arc<Mutex<CallState>>> {
        self.calls.lock().await.get(sid).cloned()
    }
}

fn jmi(name: &str, sid: &str) -> Element {
    Element::builder(name, JMI).attr(n("id"), sid).build()
}

/// A JMI message with a store hint, so it reaches offline devices' archives.
fn jmi_message(to: Jid, payload: Element) -> Message {
    let mut m = Message::new_with_type(MessageType::Chat, Some(to));
    m.payloads.push(payload);
    m.payloads.push(Element::builder("store", HINTS).build());
    m
}

async fn new_call(
    session: &Session,
    sid: String,
    peer: BareJid,
    peer_full: Option<Jid>,
    direction: Direction,
    video: bool,
) -> anyhow::Result<Arc<Mutex<CallState>>> {
    let number = session.calls.next.fetch_add(1, Ordering::SeqCst) + 1;
    let path = OwnedObjectPath::try_from(format!("{}/call/{number}", session.path))?;
    let state = Arc::new(Mutex::new(CallState {
        sid: sid.clone(),
        peer,
        peer_full,
        direction,
        state: if direction == Direction::Incoming {
            "ringing"
        } else {
            "calling"
        }
        .into(),
        end_reason: String::new(),
        video,
        path: path.clone(),
        local: None,
        remote: None,
        initiated: false,
        answered: false,
        queued: vec![],
    }));
    session
        .calls
        .calls
        .lock()
        .await
        .insert(sid.clone(), state.clone());
    session
        .bus
        .object_server()
        .at(
            &path,
            CallObject {
                session: session.clone(),
                sid,
            },
        )
        .await?;
    Ok(state)
}

/// Starts a call: rings all of `peer`'s devices.
pub async fn start(
    session: &Session,
    peer: BareJid,
    video: bool,
) -> anyhow::Result<OwnedObjectPath> {
    if !session.is_online() {
        anyhow::bail!("not connected");
    }
    let sid = new_id();
    let call = new_call(
        session,
        sid.clone(),
        peer.clone(),
        None,
        Direction::Outgoing,
        video,
    )
    .await?;
    let mut propose = Element::builder("propose", JMI).attr(n("id"), sid.as_str());
    propose = propose.append(
        Element::builder("description", sdp::RTP)
            .attr(n("media"), "audio")
            .build(),
    );
    if video {
        propose = propose.append(
            Element::builder("description", sdp::RTP)
                .attr(n("media"), "video")
                .build(),
        );
    }
    session
        .send(jmi_message(Jid::from(peer), propose.build()))
        .await;
    let path = call.lock().await.path.clone();
    Ok(path)
}

async fn set_state(session: &Shared, call: &Arc<Mutex<CallState>>, state: &str, reason: &str) {
    let path = {
        let mut c = call.lock().await;
        if c.state == "ended" || c.state == state {
            return;
        }
        c.state = state.to_owned();
        if !reason.is_empty() {
            c.end_reason = reason.to_owned();
        }
        c.path.clone()
    };
    if let Ok(iface) = session
        .bus
        .object_server()
        .interface::<_, CallObject>(&path)
        .await
    {
        let o = iface.get().await;
        let _ = o.state_changed(iface.signal_emitter()).await;
        let _ = o.end_reason_changed(iface.signal_emitter()).await;
    }
}

/// Ends a call locally and takes it off the bus a little later, so the UI
/// sees how it ended.
async fn finish(session: &Session, call: &Arc<Mutex<CallState>>, reason: &str) {
    set_state(session, call, "ended", reason).await;
    let (sid, path) = {
        let c = call.lock().await;
        (c.sid.clone(), c.path.clone())
    };
    let session = session.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        session.calls.calls.lock().await.remove(&sid);
        let _ = session
            .bus
            .object_server()
            .remove::<CallObject, _>(&path)
            .await;
    });
}

async fn emitter(session: &Shared, path: &OwnedObjectPath) -> Option<SignalEmitter<'static>> {
    SignalEmitter::new(&session.bus, path.clone())
        .ok()
        .map(|e| e.into_owned())
}

// --- JMI messages ----------------------------------------------------------------

/// Handles a Jingle Message Initiation element. Returns true if `message`
/// was one.
pub async fn message(session: &Session, message: &Message, outgoing: bool) -> bool {
    let Some(el) = message.payloads.iter().find(|p| p.ns() == JMI) else {
        return false;
    };
    let Some(sid) = el.attr("id").map(str::to_owned) else {
        return true;
    };
    let from = message.from.clone();
    let existing = session.calls.get(&sid).await;
    match (el.name(), outgoing) {
        ("propose", false) => {
            if existing.is_some() {
                return true;
            }
            let Some(from) = from.filter(|f| f.resource().is_some()) else {
                return true;
            };
            let video = el
                .children()
                .any(|d| d.is("description", sdp::RTP) && d.attr("media") == Some("video"));
            let peer = from.to_bare();
            match new_call(
                session,
                sid.clone(),
                peer.clone(),
                Some(from.clone()),
                Direction::Incoming,
                video,
            )
            .await
            {
                Ok(call) => {
                    // Tell the caller we are ringing.
                    session.send(jmi_message(from, jmi("ringing", &sid))).await;
                    crate::notify::incoming_call(session, &peer, video, &call.lock().await.path)
                        .await;
                    let s = session.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(RING_TIMEOUT).await;
                        if let Some(call) = s.calls.get(&sid).await
                            && call.lock().await.state == "ringing"
                        {
                            finish(&s, &call, "missed").await;
                        }
                    });
                }
                Err(e) => tracing::error!("creating a call: {e:#}"),
            }
        }
        ("proceed", false) => {
            let Some(call) = existing else { return true };
            let ready = {
                let mut c = call.lock().await;
                if c.direction != Direction::Outgoing || c.peer_full.is_some() {
                    return true;
                }
                c.peer_full = from.clone();
                c.local.is_some()
            };
            set_state(session, &call, "connecting", "").await;
            if ready {
                send_initiate(session, &call).await;
            }
        }
        ("ringing", false) => {}
        ("reject", false) | ("retract", false) => {
            if let Some(call) = existing {
                let reason = if el.name() == "reject" {
                    "declined"
                } else {
                    "cancelled"
                };
                finish(session, &call, reason).await;
            }
        }
        // One of our other devices answered or declined.
        ("accept", _) | ("proceed", true) | ("reject", true) => {
            if let Some(call) = existing {
                let mine = call.lock().await.direction == Direction::Incoming
                    && !call.lock().await.answered;
                if mine {
                    finish(session, &call, "answered elsewhere").await;
                }
            }
        }
        _ => {}
    }
    true
}

// --- Jingle IQs ------------------------------------------------------------------

fn jingle(action: &str, sid: &str) -> tokio_xmpp::minidom::ElementBuilder {
    Element::builder("jingle", JINGLE)
        .attr(n("action"), action)
        .attr(n("sid"), sid)
}

async fn send_initiate(session: &Session, call: &Arc<Mutex<CallState>>) {
    let (to, el) = {
        let mut c = call.lock().await;
        let (Some(to), Some(local)) = (c.peer_full.clone(), c.local.clone()) else {
            return;
        };
        let initiator = session
            .bound
            .lock()
            .await
            .clone()
            .map(|j| j.to_string())
            .unwrap_or_default();
        let mut j = jingle("session-initiate", &c.sid).attr(n("initiator"), initiator);
        for content in &local.contents {
            j = j.append(sdp::content_element(content, true));
        }
        if let Some(g) = sdp::group_element(&local) {
            j = j.append(g);
        }
        c.initiated = true;
        c.queued.clear(); // already in the contents
        (to, j.build())
    };
    let iq = crate::pep::set(Some(to), el);
    if let Err(e) = session.iq(iq).await {
        tracing::warn!("session-initiate refused: {e}");
        finish(session, call, "failed").await;
    }
}

async fn send_accept(session: &Session, call: &Arc<Mutex<CallState>>) {
    let (to, el, queued) = {
        let mut c = call.lock().await;
        let (Some(to), Some(local)) = (c.peer_full.clone(), c.local.clone()) else {
            return;
        };
        let responder = session
            .bound
            .lock()
            .await
            .clone()
            .map(|j| j.to_string())
            .unwrap_or_default();
        let mut j = jingle("session-accept", &c.sid).attr(n("responder"), responder);
        for content in &local.contents {
            j = j.append(sdp::content_element(content, true));
        }
        if let Some(g) = sdp::group_element(&local) {
            j = j.append(g);
        }
        (to, j.build(), std::mem::take(&mut c.queued))
    };
    let iq = crate::pep::set(Some(to), el);
    if let Err(e) = session.iq(iq).await {
        tracing::warn!("session-accept refused: {e}");
        finish(session, call, "failed").await;
        return;
    }
    set_state(session, call, "active", "").await;
    for (mline, candidate) in queued {
        let name = call
            .lock()
            .await
            .local
            .as_ref()
            .and_then(|d| d.contents.get(mline as usize))
            .map(|x| x.name.clone());
        if let Some(name) = name {
            send_candidate(session, call, &name, candidate).await;
        }
    }
}

async fn send_candidate(
    session: &Session,
    call: &Arc<Mutex<CallState>>,
    content: &str,
    candidate: sdp::Candidate,
) {
    let (to, el) = {
        let c = call.lock().await;
        let Some(to) = c.peer_full.clone() else {
            return;
        };
        let Some(local) = &c.local else { return };
        let Some(content_desc) = local.contents.iter().find(|x| x.name == content) else {
            return;
        };
        let mut transport = content_desc.clone();
        transport.candidates = vec![candidate];
        let el = jingle("transport-info", &c.sid)
            .append(
                Element::builder("content", JINGLE)
                    .attr(n("creator"), content_desc.creator.as_str())
                    .attr(n("name"), content)
                    .append(sdp::transport_element(&transport, true))
                    .build(),
            )
            .build();
        (to, el)
    };
    let _ = session.iq(crate::pep::set(Some(to), el)).await;
}

async fn send_terminate(session: &Session, call: &Arc<Mutex<CallState>>, reason: &str) {
    let (to, sid) = {
        let c = call.lock().await;
        let Some(to) = c.peer_full.clone() else {
            return;
        };
        (to, c.sid.clone())
    };
    let el = jingle("session-terminate", &sid)
        .append(
            Element::builder("reason", JINGLE)
                .append(Element::builder(reason, JINGLE).build())
                .build(),
        )
        .build();
    let _ = session.iq(crate::pep::set(Some(to), el)).await;
}

/// Handles a Jingle IQ set. Returns the answer to send.
pub async fn iq(session: &Session, from: Option<Jid>, id: String, payload: Element) -> Iq {
    use tokio_xmpp::parsers::stanza_error::DefinedCondition;
    let reply_ok = crate::session::result(from.clone(), id.clone());
    let unknown = crate::session::error(from.clone(), id.clone(), DefinedCondition::ItemNotFound);
    let Some(from) = from else { return unknown };
    let Some(sid) = payload.attr("sid").map(str::to_owned) else {
        return crate::session::error(Some(from), id, DefinedCondition::BadRequest);
    };
    let action = payload.attr("action").unwrap_or("").to_owned();
    let call = session.calls.get(&sid).await;
    // Only the device we talk to may act on a session.
    if let Some(call) = &call {
        let c = call.lock().await;
        if let Some(full) = &c.peer_full
            && *full != from
        {
            return unknown;
        }
    }
    match action.as_str() {
        "session-initiate" => {
            let call = match call {
                Some(call) => call,
                None => {
                    // Called without a JMI proposal first (older Dino):
                    // ring now.
                    let description = sdp::parse_description(&payload);
                    let video = description.contents.iter().any(|c| c.media == "video");
                    match new_call(
                        session,
                        sid.clone(),
                        from.to_bare(),
                        Some(from.clone()),
                        Direction::Incoming,
                        video,
                    )
                    .await
                    {
                        Ok(call) => {
                            crate::notify::incoming_call(
                                session,
                                &from.to_bare(),
                                video,
                                &call.lock().await.path,
                            )
                            .await;
                            call
                        }
                        Err(_) => return unknown,
                    }
                }
            };
            let answered = {
                let mut c = call.lock().await;
                if c.direction != Direction::Incoming || c.initiated {
                    return crate::session::error(
                        Some(from),
                        id,
                        DefinedCondition::UnexpectedRequest,
                    );
                }
                c.peer_full = Some(from.clone());
                c.remote = Some(sdp::parse_description(&payload));
                c.initiated = true;
                c.answered
            };
            if answered {
                offer_to_ui(session, &call).await;
            }
            reply_ok
        }
        "session-accept" => {
            let Some(call) = call else { return unknown };
            let (path, answer) = {
                let mut c = call.lock().await;
                let d = sdp::parse_description(&payload);
                let answer = sdp::to_sdp(&d, "initiator");
                c.remote = Some(d);
                (c.path.clone(), answer)
            };
            set_state(session, &call, "active", "").await;
            if let Some(e) = emitter(session, &path).await {
                let _ = CallObject::remote_description(&e, "answer", &answer).await;
            }
            reply_ok
        }
        "transport-info" => {
            let Some(call) = call else { return unknown };
            let (path, lines) = {
                let c = call.lock().await;
                let order: Vec<String> = c
                    .local
                    .as_ref()
                    .or(c.remote.as_ref())
                    .map(|d| d.contents.iter().map(|x| x.name.clone()).collect())
                    .unwrap_or_default();
                let mut lines = vec![];
                for content in payload.children().filter(|x| x.is("content", JINGLE)) {
                    let name = content.attr("name").unwrap_or("");
                    let mline = order.iter().position(|x| x == name).unwrap_or(0) as u32;
                    if let Some(t) = content.get_child("transport", sdp::ICE) {
                        for cand in t.children().filter_map(sdp::parse_candidate) {
                            lines.push((mline, sdp::candidate_to_sdp(&cand)));
                        }
                    }
                }
                (c.path.clone(), lines)
            };
            if let Some(e) = emitter(session, &path).await {
                for (mline, line) in lines {
                    let _ = CallObject::remote_candidate(&e, mline, &line).await;
                }
            }
            reply_ok
        }
        "session-terminate" => {
            if let Some(call) = call {
                let reason = payload
                    .get_child("reason", JINGLE)
                    .and_then(|r| r.children().next())
                    .map(|r| r.name().to_owned())
                    .unwrap_or_else(|| "success".into());
                let reason = match reason.as_str() {
                    "decline" => "declined",
                    "busy" => "busy",
                    "success" => "hung up",
                    _ => "failed",
                };
                finish(session, &call, reason).await;
            }
            reply_ok
        }
        // Content changes and the like are not supported yet; say so.
        _ => crate::session::error(Some(from), id, DefinedCondition::FeatureNotImplemented),
    }
}

/// Hands the remote offer to the UI (once the user answered and it came).
async fn offer_to_ui(session: &Shared, call: &Arc<Mutex<CallState>>) {
    let (path, offer) = {
        let c = call.lock().await;
        let Some(remote) = &c.remote else { return };
        (c.path.clone(), sdp::to_sdp(remote, "responder"))
    };
    if let Some(e) = emitter(session, &path).await {
        let _ = CallObject::remote_description(&e, "offer", &offer).await;
    }
}

/// Our account went offline: every call ends.
pub async fn offline(session: &Session) {
    let calls: Vec<_> = session.calls.calls.lock().await.values().cloned().collect();
    for call in calls {
        finish(session, &call, "connection lost").await;
    }
}

// --- D-Bus -------------------------------------------------------------------------

/// `net.dunevent.Piopio.Call`, at `<account>/call/<n>`.
pub struct CallObject {
    session: Session,
    sid: String,
}

impl CallObject {
    async fn call(&self) -> zbus::fdo::Result<Arc<Mutex<CallState>>> {
        self.session
            .calls
            .get(&self.sid)
            .await
            .ok_or_else(|| zbus::fdo::Error::Failed("the call is over".into()))
    }
}

#[zbus::interface(name = "net.dunevent.Piopio.Call")]
impl CallObject {
    #[zbus(property)]
    async fn peer(&self) -> String {
        match self.call().await {
            Ok(c) => c.lock().await.peer.to_string(),
            Err(_) => String::new(),
        }
    }

    /// incoming or outgoing.
    #[zbus(property)]
    async fn direction(&self) -> String {
        match self.call().await {
            Ok(c) if c.lock().await.direction == Direction::Incoming => "incoming".into(),
            _ => "outgoing".into(),
        }
    }

    /// ringing, calling, connecting, active or ended.
    #[zbus(property)]
    async fn state(&self) -> String {
        match self.call().await {
            Ok(c) => c.lock().await.state.clone(),
            Err(_) => "ended".into(),
        }
    }

    /// Why it ended: hung up, declined, cancelled, missed, busy,
    /// answered elsewhere, connection lost or failed.
    #[zbus(property)]
    async fn end_reason(&self) -> String {
        match self.call().await {
            Ok(c) => c.lock().await.end_reason.clone(),
            Err(_) => String::new(),
        }
    }

    #[zbus(property)]
    async fn video(&self) -> bool {
        match self.call().await {
            Ok(c) => c.lock().await.video,
            Err(_) => false,
        }
    }

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        CALL_API_VERSION
    }

    /// Answers an incoming call. The offer then arrives as RemoteDescription.
    async fn accept(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let call = self.call().await?;
        let (to, sid, initiated) = {
            let mut c = call.lock().await;
            if c.direction != Direction::Incoming || c.state != "ringing" {
                return Err(zbus::fdo::Error::Failed(
                    "there is nothing to answer".into(),
                ));
            }
            c.answered = true;
            (c.peer_full.clone(), c.sid.clone(), c.initiated)
        };
        // Other devices stop ringing; the caller goes ahead with us.
        let own = Jid::from(self.session.jid.clone());
        self.session
            .send(jmi_message(own, jmi("accept", &sid)))
            .await;
        if let Some(to) = to {
            self.session
                .send(jmi_message(to, jmi("proceed", &sid)))
                .await;
        }
        set_state(&self.session, &call, "connecting", "").await;
        if initiated {
            offer_to_ui(&self.session, &call).await;
        }
        Ok(())
    }

    /// Declines, cancels or hangs up.
    async fn hangup(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let call = self.call().await?;
        let (direction, state, initiated, sid, peer, peer_full) = {
            let c = call.lock().await;
            (
                c.direction,
                c.state.clone(),
                c.initiated,
                c.sid.clone(),
                c.peer.clone(),
                c.peer_full.clone(),
            )
        };
        let reason = match (direction, state.as_str()) {
            (_, "ended") => return Ok(()),
            (Direction::Incoming, "ringing") => {
                // Decline, and stop our other devices ringing too.
                if let Some(to) = &peer_full {
                    self.session
                        .send(jmi_message(to.clone(), jmi("reject", &sid)))
                        .await;
                }
                let own = Jid::from(self.session.jid.clone());
                self.session
                    .send(jmi_message(own, jmi("reject", &sid)))
                    .await;
                if initiated {
                    send_terminate(&self.session, &call, "decline").await;
                }
                "declined"
            }
            (Direction::Outgoing, "calling") => {
                self.session
                    .send(jmi_message(Jid::from(peer), jmi("retract", &sid)))
                    .await;
                "cancelled"
            }
            _ => {
                if initiated {
                    send_terminate(&self.session, &call, "success").await;
                } else if let Some(to) = peer_full {
                    self.session
                        .send(jmi_message(to, jmi("retract", &sid)))
                        .await;
                }
                "hung up"
            }
        };
        finish(&self.session, &call, reason).await;
        Ok(())
    }

    /// Our offer (outgoing) or answer (incoming), as SDP from WebRTC.
    async fn set_local_description(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        kind: &str,
        description: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let call = self.call().await?;
        let (direction, ready) = {
            let mut c = call.lock().await;
            let author = if c.direction == Direction::Outgoing {
                "initiator"
            } else {
                "responder"
            };
            if (kind == "offer") != (c.direction == Direction::Outgoing) {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "an outgoing call offers, an incoming one answers".into(),
                ));
            }
            let mut d = sdp::from_sdp(description, author);
            // Candidates that came before the description.
            if c.direction == Direction::Outgoing {
                for (mline, cand) in std::mem::take(&mut c.queued) {
                    if let Some(x) = d.contents.get_mut(mline as usize) {
                        x.candidates.push(cand);
                    }
                }
            }
            c.local = Some(d);
            (c.direction, c.peer_full.is_some())
        };
        match direction {
            Direction::Outgoing if ready => send_initiate(&self.session, &call).await,
            Direction::Incoming => send_accept(&self.session, &call).await,
            _ => {}
        }
        Ok(())
    }

    /// One of our ICE candidates ("candidate:…") for media line `mline`.
    async fn add_local_candidate(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        mline: u32,
        candidate: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let call = self.call().await?;
        let Some(cand) = sdp::candidate_from_sdp(candidate) else {
            return Err(zbus::fdo::Error::InvalidArgs("not an ICE candidate".into()));
        };
        let send_now = {
            let mut c = call.lock().await;
            let content = c
                .local
                .as_ref()
                .and_then(|d| d.contents.get(mline as usize))
                .map(|x| x.name.clone());
            let sent = match c.direction {
                Direction::Outgoing => c.initiated,
                Direction::Incoming => c.state == "active",
            };
            match (content, sent, c.direction) {
                (Some(name), true, _) => Some(name),
                // Not offered yet: it goes into the session-initiate.
                (Some(_), false, Direction::Outgoing) => {
                    if let Some(x) = c
                        .local
                        .as_mut()
                        .and_then(|d| d.contents.get_mut(mline as usize))
                    {
                        x.candidates.push(cand.clone());
                    }
                    None
                }
                _ => {
                    c.queued.push((mline, cand.clone()));
                    None
                }
            }
        };
        if let Some(name) = send_now {
            send_candidate(&self.session, &call, &name, cand).await;
        }
        Ok(())
    }

    /// The other side's offer or answer, for WebRTC.
    #[zbus(signal)]
    async fn remote_description(
        emitter: &SignalEmitter<'_>,
        kind: &str,
        description: &str,
    ) -> zbus::Result<()>;

    /// One of the other side's ICE candidates.
    #[zbus(signal)]
    async fn remote_candidate(
        emitter: &SignalEmitter<'_>,
        mline: u32,
        candidate: &str,
    ) -> zbus::Result<()>;
}
