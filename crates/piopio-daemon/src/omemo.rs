//! OMEMO (XEP-0384 version 0.3, `eu.siacs.conversations.axolotl`), the
//! version Conversations, Dino, Gajim, Monal and Converse.js all speak.
//!
//! The protocol itself lives in the `piopio-omemo` crate; this module does the
//! XMPP side: our device list and bundle in PEP, contacts' device lists and
//! bundles, sessions and trust in the database, and the `<encrypted/>`
//! element on messages.
//!
//! Trust follows Blind Trust Before Verification: a contact's new devices are
//! trusted automatically until the user verifies one of that contact's
//! devices; after that, new devices wait for a decision.
//!
//! All OMEMO state changes happen under one lock per account, so a session is
//! never used by two messages at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use tokio::sync::Mutex;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::legacy_omemo::Encrypted;
use tokio_xmpp::parsers::ns;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Str};

use piopio_omemo as omemo;

use crate::access;
use crate::session::{Session, Shared};

use piopio_dbus::OMEMO_API_VERSION;
/// Device lists of people who don't share presence with us (so send no
/// notifications) are fetched again when older than this.
const DEVICE_LIST_TTL: Duration = Duration::from_secs(600);
const PUBLISH_OPTIONS: &str = "http://jabber.org/protocol/pubsub#publish-options";

/// The text clients without OMEMO show instead of an encrypted message.
pub const FALLBACK_BODY: &str = "I sent you an OMEMO encrypted message but your client doesn’t \
                                 seem to support that. Find more information on \
                                 https://conversations.im/omemo";

pub mod trust {
    /// Trusted automatically (no device of this contact verified yet).
    pub const BLIND: &str = "blind";
    /// The user compared fingerprints.
    pub const VERIFIED: &str = "verified";
    /// The user refused this device.
    pub const UNTRUSTED: &str = "untrusted";
    /// New since the user verified another of the contact's devices.
    pub const UNDECIDED: &str = "undecided";

    pub fn is_trusted(t: &str) -> bool {
        t == BLIND || t == VERIFIED
    }

    pub fn is_valid(t: &str) -> bool {
        matches!(t, BLIND | VERIFIED | UNTRUSTED | UNDECIDED)
    }
}

/// An account's OMEMO state.
#[derive(Default)]
pub struct OmemoState {
    /// Our own keys, loaded (or generated) on first use.
    own: Mutex<Option<omemo::OwnKeys>>,
    /// When we last fetched (or were told) each JID's device list.
    fetched: Mutex<HashMap<String, Instant>>,
    /// Whether our device and bundle are published for this login.
    published: AtomicBool,
    /// Held while sessions are used, so each is used by one message at a time.
    sessions: Mutex<()>,
}

/// What became of an incoming `<encrypted/>`.
pub enum Decrypted {
    /// The message text, and how much we trust the sending device.
    Text { body: String, trusted: bool },
    /// A key transport message (no payload): nothing to show.
    KeyTransport,
    /// We could not decrypt it.
    Failed(String),
}

/// What to do with an outgoing message.
pub enum Outgoing {
    /// The peer has no OMEMO devices (or encryption is off): send plain text.
    Plain,
    /// Send this `<encrypted/>` element (with the fallback body).
    Encrypted(Element),
    /// The peer uses OMEMO, but no device could be encrypted to.
    Failed(String),
}

/// An attribute name (all of ours are valid XML names).
fn n(name: &'static str) -> tokio_xmpp::minidom::rxml::NcName {
    name.try_into().expect("a valid attribute name")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Runs `f` on our own keys, loading or generating them first; saves them
/// afterwards if `f` changed them.
async fn with_own<R>(
    shared: &Shared,
    f: impl FnOnce(&mut omemo::OwnKeys) -> R,
) -> anyhow::Result<R> {
    let mut guard = shared.omemo.own.lock().await;
    if guard.is_none() {
        *guard = Some(match shared.store.omemo_own(shared.account_id).await? {
            Some(json) => serde_json::from_str(&json)?,
            None => {
                let keys = omemo::OwnKeys::generate(omemo::new_device_id());
                shared
                    .store
                    .set_omemo_own(shared.account_id, serde_json::to_string(&keys)?)
                    .await?;
                tracing::info!(jid = %shared.jid, device = keys.device_id, "created an OMEMO device");
                keys
            }
        });
    }
    let keys = guard.as_mut().expect("loaded above");
    let before = serde_json::to_string(keys)?;
    let result = f(keys);
    let after = serde_json::to_string(keys)?;
    if before != after {
        shared.store.set_omemo_own(shared.account_id, after).await?;
    }
    Ok(result)
}

/// Our device id and identity key.
pub async fn own_device(shared: &Shared) -> anyhow::Result<(u32, [u8; 32])> {
    with_own(shared, |k| (k.device_id, k.identity.public)).await
}

// --- PEP ---------------------------------------------------------------------

fn pubsub(child: Element) -> Element {
    Element::builder("pubsub", ns::PUBSUB).append(child).build()
}

fn field(var: &str, value: &str) -> Element {
    Element::builder("field", "jabber:x:data")
        .attr(n("var"), var)
        .append(
            Element::builder("value", "jabber:x:data")
                .append(value)
                .build(),
        )
        .build()
}

/// Publishes `payload` as the item `id` of `node` on our own PEP service,
/// readable by anyone (contacts need our bundle before they know us).
async fn publish(shared: &Shared, node: &str, id: &str, payload: Element) -> bool {
    let publish = Element::builder("publish", ns::PUBSUB)
        .attr(n("node"), node)
        .append(
            Element::builder("item", ns::PUBSUB)
                .attr(n("id"), id)
                .append(payload)
                .build(),
        )
        .build();
    let options = Element::builder("publish-options", ns::PUBSUB)
        .append(
            Element::builder("x", "jabber:x:data")
                .attr(n("type"), "submit")
                .append(field("FORM_TYPE", PUBLISH_OPTIONS))
                .append(field("pubsub#access_model", "open"))
                .build(),
        )
        .build();
    let with_options = Element::builder("pubsub", ns::PUBSUB)
        .append(publish.clone())
        .append(options)
        .build();
    match shared.iq(set(None, with_options)).await {
        Ok(_) => return true,
        Err(e) => tracing::info!("publishing {node} with options failed ({e}); trying without"),
    }
    match shared.iq(set(None, pubsub(publish))).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("publishing {node} failed: {e}");
            false
        }
    }
}

fn set(to: Option<Jid>, payload: Element) -> Iq {
    Iq::Set {
        from: None,
        to,
        id: String::new(),
        payload,
    }
}

fn get(to: Option<Jid>, payload: Element) -> Iq {
    Iq::Get {
        from: None,
        to,
        id: String::new(),
        payload,
    }
}

/// Fetches the newest item of `node` from `jid`'s PEP service.
async fn fetch_item(shared: &Shared, jid: &BareJid, node: &str) -> Option<Element> {
    let to = (*jid != shared.jid).then(|| Jid::from(jid.clone()));
    let query = pubsub(
        Element::builder("items", ns::PUBSUB)
            .attr(n("node"), node)
            .attr(n("max_items"), "1")
            .build(),
    );
    match shared.iq(get(to, query)).await {
        Ok(Some(reply)) => reply
            .get_child("items", ns::PUBSUB)?
            .children()
            .find(|c| c.is("item", ns::PUBSUB))?
            .children()
            .next()
            .cloned(),
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(%jid, "fetching {node}: {e}");
            None
        }
    }
}

fn device_ids(list: &Element) -> Vec<u32> {
    if !list.is("list", ns::LEGACY_OMEMO) {
        return vec![];
    }
    list.children()
        .filter(|c| c.is("device", ns::LEGACY_OMEMO))
        .filter_map(|c| c.attr("id")?.parse().ok())
        .filter(|id| *id != 0)
        .collect()
}

fn device_list(ids: &[u32]) -> Element {
    let mut list = Element::builder("list", ns::LEGACY_OMEMO);
    for id in ids {
        list = list.append(
            Element::builder("device", ns::LEGACY_OMEMO)
                .attr(n("id"), id.to_string())
                .build(),
        );
    }
    list.build()
}

fn bundle_element(b: &omemo::Bundle) -> Element {
    let key = |name: &str, k: &[u8; 32]| {
        Element::builder(name, ns::LEGACY_OMEMO)
            .append(B64.encode(omemo::encode_key(k)))
            .build()
    };
    let mut pre_keys = Element::builder("prekeys", ns::LEGACY_OMEMO);
    for (id, k) in &b.pre_keys {
        pre_keys = pre_keys.append(
            Element::builder("preKeyPublic", ns::LEGACY_OMEMO)
                .attr(n("preKeyId"), id.to_string())
                .append(B64.encode(omemo::encode_key(k)))
                .build(),
        );
    }
    Element::builder("bundle", ns::LEGACY_OMEMO)
        .append(
            Element::builder("signedPreKeyPublic", ns::LEGACY_OMEMO)
                .attr(n("signedPreKeyId"), b.signed_pre_key_id.to_string())
                .append(B64.encode(omemo::encode_key(&b.signed_pre_key)))
                .build(),
        )
        .append(
            Element::builder("signedPreKeySignature", ns::LEGACY_OMEMO)
                .append(B64.encode(b.signed_pre_key_signature))
                .build(),
        )
        .append(key("identityKey", &b.identity_key))
        .append(pre_keys.build())
        .build()
}

fn parse_bundle(e: &Element) -> Option<omemo::Bundle> {
    if !e.is("bundle", ns::LEGACY_OMEMO) {
        return None;
    }
    let text = |name: &str| -> Option<(Vec<u8>, &Element)> {
        let c = e.get_child(name, ns::LEGACY_OMEMO)?;
        Some((B64.decode(c.text().trim()).ok()?, c))
    };
    let (spk, spk_el) = text("signedPreKeyPublic")?;
    let (sig, _) = text("signedPreKeySignature")?;
    let (ik, _) = text("identityKey")?;
    let pre_keys = e
        .get_child("prekeys", ns::LEGACY_OMEMO)?
        .children()
        .filter(|c| c.is("preKeyPublic", ns::LEGACY_OMEMO))
        .filter_map(|c| {
            let id = c.attr("preKeyId")?.parse().ok()?;
            let key = omemo::decode_key(&B64.decode(c.text().trim()).ok()?).ok()?;
            Some((id, key))
        })
        .collect();
    Some(omemo::Bundle {
        identity_key: omemo::decode_key(&ik).ok()?,
        signed_pre_key_id: spk_el.attr("signedPreKeyId")?.parse().ok()?,
        signed_pre_key: omemo::decode_key(&spk).ok()?,
        signed_pre_key_signature: sig.try_into().ok()?,
        pre_keys,
    })
}

/// Publishes our current bundle.
async fn publish_bundle(shared: &Shared) {
    let Ok((id, bundle)) = with_own(shared, |k| (k.device_id, k.bundle())).await else {
        return;
    };
    publish(
        shared,
        &omemo::bundle_node(id),
        "current",
        bundle_element(&bundle),
    )
    .await;
}

/// After every login: make sure our device is announced and our bundle is
/// current, and learn our other devices.
pub async fn online(shared: &Shared) {
    shared.omemo.fetched.lock().await.clear();
    shared.omemo.published.store(false, Ordering::SeqCst);
    let Ok((own_id, _)) = own_device(shared).await else {
        tracing::error!("OMEMO keys could not be loaded");
        return;
    };
    let _ = with_own(shared, |k| k.refill_pre_keys()).await;
    publish_bundle(shared).await;
    let jid = shared.jid.clone();
    let mut ids = match fetch_item(shared, &jid, ns::LEGACY_OMEMO_DEVICELIST).await {
        Some(list) => device_ids(&list),
        None => vec![],
    };
    let mut ok = true;
    if !ids.contains(&own_id) {
        ids.push(own_id);
        ok = publish(
            shared,
            ns::LEGACY_OMEMO_DEVICELIST,
            "current",
            device_list(&ids),
        )
        .await;
    }
    shared
        .omemo
        .fetched
        .lock()
        .await
        .insert(jid.to_string(), Instant::now());
    device_list_received(shared, &jid, ids).await;
    if ok {
        shared.omemo.published.store(true, Ordering::SeqCst);
        let emitter = SignalEmitter::new(&shared.bus, shared.path.clone()).expect("valid path");
        if let Ok(iface) = shared
            .bus
            .object_server()
            .interface::<_, OmemoObject>(&shared.path)
            .await
        {
            let _ = iface.get().await.published_changed(&emitter).await;
        }
    }
}

/// Handles a PEP notification. Returns true if it was an OMEMO one.
pub async fn event(shared: &Shared, from: Option<&Jid>, event: &Element) -> bool {
    let Some(items) = event.get_child("items", ns::PUBSUB_EVENT) else {
        return false;
    };
    if items.attr("node") != Some(ns::LEGACY_OMEMO_DEVICELIST) {
        return false;
    }
    // PEP notifications come from the bare JID that owns the node.
    let jid = match from {
        None => shared.jid.clone(),
        Some(j) if j.resource().is_none() => j.to_bare(),
        Some(other) => {
            tracing::warn!(from = %other, "device list from a full JID ignored");
            return true;
        }
    };
    let ids = items
        .children()
        .filter(|c| c.is("item", ns::PUBSUB_EVENT))
        .last()
        .and_then(|item| item.children().next())
        .map(device_ids)
        .unwrap_or_default();
    shared
        .omemo
        .fetched
        .lock()
        .await
        .insert(jid.to_string(), Instant::now());
    if jid == shared.jid {
        // Another of our clients may have replaced the list without us.
        if let Ok((own_id, _)) = own_device(shared).await
            && !ids.contains(&own_id)
        {
            let mut ids = ids.clone();
            ids.push(own_id);
            publish(
                shared,
                ns::LEGACY_OMEMO_DEVICELIST,
                "current",
                device_list(&ids),
            )
            .await;
        }
    }
    device_list_received(shared, &jid, ids).await;
    true
}

async fn device_list_received(shared: &Shared, jid: &BareJid, ids: Vec<u32>) {
    let before = shared
        .store
        .devices(shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    if let Err(e) = shared
        .store
        .set_device_list(shared.account_id, jid.to_string(), ids)
        .await
    {
        tracing::error!("storing {jid}'s devices: {e:#}");
        return;
    }
    let after = shared
        .store
        .devices(shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    if before != after {
        devices_changed(shared, jid).await;
    }
}

async fn devices_changed(shared: &Shared, jid: &BareJid) {
    let emitter = SignalEmitter::new(&shared.bus, shared.path.clone()).expect("valid path");
    let _ = OmemoObject::devices_changed(&emitter, jid.as_str()).await;
    shared.conversations.encryption_changed(shared, jid).await;
}

/// Makes sure we know `jid`'s devices: fetched once per login, and again
/// once the list is old (notifications keep contacts' lists current).
async fn ensure_devices(shared: &Shared, jid: &BareJid) {
    if let Some(when) = shared.omemo.fetched.lock().await.get(jid.as_str())
        && when.elapsed() < DEVICE_LIST_TTL
    {
        return;
    }
    let ids = match fetch_item(shared, jid, ns::LEGACY_OMEMO_DEVICELIST).await {
        Some(list) => device_ids(&list),
        None => vec![],
    };
    shared
        .omemo
        .fetched
        .lock()
        .await
        .insert(jid.to_string(), Instant::now());
    device_list_received(shared, jid, ids).await;
}

/// Fetches `jid`'s device list in the background if ours is missing or old.
pub fn refresh(session: &Session, jid: BareJid) {
    if !session.is_online() {
        return;
    }
    let session = session.clone();
    tokio::spawn(async move { ensure_devices(&session, &jid).await });
}

/// The encryption a conversation with `peer` uses: "omemo" when wanted and
/// the peer has announced OMEMO devices, otherwise "none".
pub async fn conversation_encryption(shared: &Shared, peer: &str, wanted: bool) -> &'static str {
    if !wanted {
        return "none";
    }
    let devices = shared
        .store
        .devices(shared.account_id, peer.to_owned())
        .await
        .unwrap_or_default();
    if devices.iter().any(|d| d.active) {
        "omemo"
    } else {
        "none"
    }
}

// --- trust -----------------------------------------------------------------

/// Records the identity key a device uses and returns its trust. A device
/// we have not seen before (or whose key changed) gets BTBV's default.
async fn learn_identity(
    shared: &Shared,
    jid: &BareJid,
    device: u32,
    identity: &[u8; 32],
) -> String {
    let devices = shared
        .store
        .devices(shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    let identity = hex(identity);
    let known = devices.iter().find(|d| d.device_id == device);
    if let Some(d) = known
        && d.identity == identity
    {
        return d.trust.clone();
    }
    if let Some(d) = known
        && !d.identity.is_empty()
    {
        tracing::warn!(%jid, device, "OMEMO device changed its identity key");
    }
    let trust = if devices.iter().any(|d| d.trust == trust::VERIFIED) {
        trust::UNDECIDED
    } else {
        trust::BLIND
    };
    let _ = shared
        .store
        .set_device(
            shared.account_id,
            jid.to_string(),
            device,
            identity,
            trust.to_owned(),
        )
        .await;
    devices_changed(shared, jid).await;
    trust.to_owned()
}

async fn load_session(shared: &Shared, jid: &BareJid, device: u32) -> Option<omemo::Session> {
    let json = shared
        .store
        .omemo_session(shared.account_id, jid.to_string(), device)
        .await
        .ok()??;
    serde_json::from_str(&json).ok()
}

async fn save_session(shared: &Shared, jid: &BareJid, device: u32, s: &omemo::Session) {
    let Ok(json) = serde_json::to_string(s) else {
        return;
    };
    if let Err(e) = shared
        .store
        .set_omemo_session(shared.account_id, jid.to_string(), device, json)
        .await
    {
        tracing::error!("storing an OMEMO session: {e:#}");
    }
}

// --- sending ---------------------------------------------------------------

/// Decides how to send `body` to `peer`, and encrypts it if OMEMO is used.
pub async fn encrypt(shared: &Shared, peer: &BareJid, wanted: bool, body: &str) -> Outgoing {
    if !wanted {
        return Outgoing::Plain;
    }
    ensure_devices(shared, peer).await;
    ensure_devices(shared, &shared.jid.clone()).await;
    let Ok((own_id, _)) = own_device(shared).await else {
        return Outgoing::Failed("our OMEMO keys could not be loaded".into());
    };
    let peer_devices: Vec<_> = shared
        .store
        .devices(shared.account_id, peer.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.active)
        .collect();
    if peer_devices.is_empty() {
        return Outgoing::Plain;
    }
    let own_devices: Vec<_> = shared
        .store
        .devices(shared.account_id, shared.jid.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.active && d.device_id != own_id)
        .collect();

    let _sessions = shared.omemo.sessions.lock().await;
    let payload = omemo::encrypt_payload(body.as_bytes());
    let mut keys = Vec::new();
    let mut peer_keys = 0;
    for (jid, device) in peer_devices
        .iter()
        .map(|d| (peer.clone(), d.device_id))
        .chain(
            own_devices
                .iter()
                .map(|d| (shared.jid.clone(), d.device_id)),
        )
    {
        let Some(mut session) = session_for(shared, &jid, device).await else {
            continue;
        };
        let e = session.encrypt(&payload.key_material);
        save_session(shared, &jid, device, &session).await;
        let mut key = Element::builder("key", ns::LEGACY_OMEMO)
            .attr(n("rid"), device.to_string())
            .append(B64.encode(&e.data));
        if e.prekey {
            key = key.attr(n("prekey"), "true");
        }
        keys.push(key.build());
        if jid == *peer {
            peer_keys += 1;
        }
    }
    if peer_keys == 0 {
        return Outgoing::Failed(format!(
            "none of {peer}'s OMEMO devices is trusted or reachable"
        ));
    }
    let mut header =
        Element::builder("header", ns::LEGACY_OMEMO).attr(n("sid"), own_id.to_string());
    for k in keys {
        header = header.append(k);
    }
    header = header.append(
        Element::builder("iv", ns::LEGACY_OMEMO)
            .append(B64.encode(payload.iv))
            .build(),
    );
    Outgoing::Encrypted(
        Element::builder("encrypted", ns::LEGACY_OMEMO)
            .append(header.build())
            .append(
                Element::builder("payload", ns::LEGACY_OMEMO)
                    .append(B64.encode(&payload.ciphertext))
                    .build(),
            )
            .build(),
    )
}

/// A session with a trusted device, built from its bundle if needed.
async fn session_for(shared: &Shared, jid: &BareJid, device: u32) -> Option<omemo::Session> {
    let known = shared
        .store
        .devices(shared.account_id, jid.to_string())
        .await
        .ok()?
        .into_iter()
        .find(|d| d.device_id == device);
    if let Some(d) = &known
        && !d.identity.is_empty()
        && !trust::is_trusted(&d.trust)
    {
        return None;
    }
    if let Some(s) = load_session(shared, jid, device).await {
        return Some(s);
    }
    let bundle = fetch_item(shared, jid, &omemo::bundle_node(device))
        .await
        .and_then(|e| parse_bundle(&e));
    let Some(bundle) = bundle else {
        tracing::info!(%jid, device, "no usable OMEMO bundle");
        return None;
    };
    let trust = learn_identity(shared, jid, device, &bundle.identity_key).await;
    if !trust::is_trusted(&trust) {
        return None;
    }
    let session = match with_own(shared, |own| omemo::initiate(own, &bundle)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(%jid, device, "OMEMO bundle rejected: {e}");
            return None;
        }
        Err(e) => {
            tracing::error!("{e:#}");
            return None;
        }
    };
    save_session(shared, jid, device, &session).await;
    Some(session)
}

// --- receiving ---------------------------------------------------------------

/// Decrypts an `<encrypted/>` from `sender` (a bare JID: a contact, or us).
pub async fn decrypt(shared: &Session, sender: &BareJid, encrypted: &Encrypted) -> Decrypted {
    let _sessions = shared.omemo.sessions.lock().await;
    let Ok((own_id, _)) = own_device(shared).await else {
        return Decrypted::Failed("our OMEMO keys could not be loaded".into());
    };
    let header = &encrypted.header;
    let device = header.sid;
    let Some(key) = header.keys.iter().find(|k| k.rid == own_id) else {
        return Decrypted::Failed("the message was not encrypted for this device".into());
    };
    let existing = load_session(shared, sender, device).await;
    let result = if key.prekey {
        let kex = match omemo::parse_key_exchange(&key.data) {
            Ok(k) => k,
            Err(e) => return Decrypted::Failed(format!("unreadable key exchange: {e}")),
        };
        match existing {
            // A repeat of the key exchange that started our current session.
            Some(mut s) if s.base_key == kex.base_key => {
                kex.decrypt_with(&mut s).map(|plain| (s, plain, false))
            }
            _ => match with_own(shared, |own| kex.accept(own)).await {
                Ok(r) => r.map(|(s, plain)| (s, plain, true)),
                Err(e) => return Decrypted::Failed(format!("{e:#}")),
            },
        }
    } else {
        match existing {
            Some(mut s) => s.decrypt(&key.data).map(|plain| (s, plain, false)),
            None => return Decrypted::Failed("no OMEMO session with the sending device".into()),
        }
    };
    let (session, key_material, used_pre_key) = match result {
        Ok(r) => r,
        Err(e) => return Decrypted::Failed(format!("{e}")),
    };
    let trust = learn_identity(shared, sender, device, &session.remote_identity).await;
    save_session(shared, sender, device, &session).await;
    if used_pre_key {
        // Replace the one-time pre key the sender used.
        let _ = with_own(shared, |own| own.refill_pre_keys()).await;
        let session = shared.clone();
        tokio::spawn(async move { publish_bundle(&session).await });
    }
    let Some(payload) = &encrypted.payload else {
        return Decrypted::KeyTransport;
    };
    match omemo::decrypt_payload(&key_material, &header.iv.data, &payload.data) {
        Ok(plain) => Decrypted::Text {
            body: String::from_utf8_lossy(&plain).into_owned(),
            trusted: trust::is_trusted(&trust),
        },
        Err(e) => Decrypted::Failed(format!("{e}")),
    }
}

// --- D-Bus -------------------------------------------------------------------

/// `net.dunevent.Piopio.Omemo`, at the account's path.
pub struct OmemoObject {
    pub session: Session,
}

fn device_dict(d: &crate::store::DeviceRow) -> HashMap<String, OwnedValue> {
    let mut map = HashMap::new();
    map.insert("device-id".into(), OwnedValue::from(d.device_id));
    map.insert("active".into(), OwnedValue::from(d.active));
    map.insert("trust".into(), OwnedValue::from(Str::from(d.trust.clone())));
    let fingerprint = unhex(&d.identity)
        .map(|k| omemo::fingerprint(&k))
        .unwrap_or_default();
    map.insert(
        "fingerprint".into(),
        OwnedValue::from(Str::from(fingerprint)),
    );
    map
}

#[zbus::interface(name = "net.dunevent.Piopio.Omemo")]
impl OmemoObject {
    #[zbus(property)]
    async fn device_id(&self) -> u32 {
        own_device(&self.session)
            .await
            .map(|(id, _)| id)
            .unwrap_or(0)
    }

    /// Our own fingerprint, as contacts see it.
    #[zbus(property)]
    async fn fingerprint(&self) -> String {
        own_device(&self.session)
            .await
            .map(|(_, k)| omemo::fingerprint(&k))
            .unwrap_or_default()
    }

    /// True once our device and bundle are published for this login.
    #[zbus(property)]
    fn published(&self) -> bool {
        self.session.omemo.published.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        OMEMO_API_VERSION
    }

    /// The devices of `jid` (our own JID for our other devices): device-id,
    /// active, trust and fingerprint (empty until we have its bundle).
    async fn get_devices(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
    ) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        access::check(&header)?;
        let devices = self
            .session
            .store
            .devices(self.session.account_id, jid.to_owned())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let own = own_device(&self.session).await.ok().map(|(id, _)| id);
        Ok(devices
            .iter()
            .filter(|d| !(jid == self.session.jid.as_str() && Some(d.device_id) == own))
            .map(device_dict)
            .collect())
    }

    /// Sets a device's trust: verified, untrusted, blind or undecided.
    async fn set_trust(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
        device_id: u32,
        trust: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        if !trust::is_valid(trust) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "trust is verified, untrusted, blind or undecided".into(),
            ));
        }
        let bare: BareJid = jid
            .parse()
            .map_err(|_| zbus::fdo::Error::InvalidArgs("not a JID".into()))?;
        let device = self
            .session
            .store
            .devices(self.session.account_id, jid.to_owned())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .into_iter()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown device".into()))?;
        self.session
            .store
            .set_device(
                self.session.account_id,
                jid.to_owned(),
                device_id,
                device.identity,
                trust.to_owned(),
            )
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        devices_changed(&self.session, &bare).await;
        Ok(())
    }

    /// The devices of `jid`, or their trust, changed.
    #[zbus(signal)]
    async fn devices_changed(emitter: &SignalEmitter<'_>, jid: &str) -> zbus::Result<()>;
}
