//! OMEMO 2 (`urn:xmpp:omemo:2`) beside legacy OMEMO.
//!
//! One device, one identity: our OMEMO 2 device uses the legacy device's id
//! and identity key (in its Edwards form), and has its own signed pre key,
//! pre keys, sessions and device records. Payloads are SCE envelopes
//! (XEP-0420), so replies and corrections travel inside the encryption.
//!
//! Which protocol a one-to-one conversation uses is all or nothing: OMEMO 2
//! when every device of the peer and every other device of ours can read
//! it, legacy OMEMO otherwise. Group chats stay on legacy OMEMO for now.

use std::collections::HashSet;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;

use piopio_omemo::{self as omemo, v2};

use crate::omemo::{Decrypted, Outgoing, devices_changed, hex, trust, with_own};
use crate::pep::{fetch_item, n};
use crate::session::{Session, Shared};

pub const NS: &str = v2::NAMESPACE;
const SCE: &str = "urn:xmpp:sce:1";
const DEVICE_LIST_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// The text clients without OMEMO 2 show instead.
pub const FALLBACK_BODY: &str = "This message is encrypted with OMEMO 2, which your client doesn’t \
                                 seem to support.";

const BUNDLE_OPTIONS: &[(&str, &str)] =
    &[("pubsub#access_model", "open"), ("pubsub#max_items", "max")];

async fn with_own2<R>(shared: &Shared, f: impl FnOnce(&mut v2::OwnKeys) -> R) -> anyhow::Result<R> {
    let mut guard = shared.omemo.own2.lock().await;
    if guard.is_none() {
        *guard = Some(
            match shared.store.omemo_own_v(2, shared.account_id).await? {
                Some(json) => serde_json::from_str(&json)?,
                None => {
                    let (id, identity) =
                        with_own(shared, |k| (k.device_id, k.identity.clone())).await?;
                    let keys = v2::OwnKeys::new(id, identity);
                    shared
                        .store
                        .set_omemo_own_v(2, shared.account_id, serde_json::to_string(&keys)?)
                        .await?;
                    keys
                }
            },
        );
    }
    let keys = guard.as_mut().expect("loaded above");
    let before = serde_json::to_string(keys)?;
    let result = f(keys);
    let after = serde_json::to_string(keys)?;
    if before != after {
        shared
            .store
            .set_omemo_own_v(2, shared.account_id, after)
            .await?;
    }
    Ok(result)
}

// --- PEP ---------------------------------------------------------------------

fn device_ids(list: &Element) -> Vec<u32> {
    if !list.is("devices", NS) {
        return vec![];
    }
    list.children()
        .filter(|c| c.is("device", NS))
        .filter_map(|c| c.attr("id")?.parse().ok())
        .filter(|id| *id != 0)
        .collect()
}

fn device_list(ids: &[u32]) -> Element {
    let mut list = Element::builder("devices", NS);
    for id in ids {
        list = list.append(
            Element::builder("device", NS)
                .attr(n("id"), id.to_string())
                .build(),
        );
    }
    list.build()
}

fn bundle_element(b: &v2::Bundle) -> Element {
    let mut pre_keys = Element::builder("prekeys", NS);
    for (id, k) in &b.pre_keys {
        pre_keys = pre_keys.append(
            Element::builder("pk", NS)
                .attr(n("id"), id.to_string())
                .append(B64.encode(k))
                .build(),
        );
    }
    Element::builder("bundle", NS)
        .append(
            Element::builder("spk", NS)
                .attr(n("id"), b.signed_pre_key_id.to_string())
                .append(B64.encode(b.signed_pre_key))
                .build(),
        )
        .append(
            Element::builder("spks", NS)
                .append(B64.encode(b.signed_pre_key_signature))
                .build(),
        )
        .append(
            Element::builder("ik", NS)
                .append(B64.encode(b.identity_key))
                .build(),
        )
        .append(pre_keys.build())
        .build()
}

fn parse_bundle(e: &Element) -> Option<v2::Bundle> {
    if !e.is("bundle", NS) {
        return None;
    }
    let bytes = |name: &str| -> Option<(Vec<u8>, &Element)> {
        let c = e.get_child(name, NS)?;
        Some((B64.decode(c.text().trim()).ok()?, c))
    };
    let (spk, spk_el) = bytes("spk")?;
    let (sig, _) = bytes("spks")?;
    let (ik, _) = bytes("ik")?;
    let pre_keys = e
        .get_child("prekeys", NS)?
        .children()
        .filter(|c| c.is("pk", NS))
        .filter_map(|c| {
            let id = c.attr("id")?.parse().ok()?;
            let key: [u8; 32] = B64.decode(c.text().trim()).ok()?.try_into().ok()?;
            Some((id, key))
        })
        .collect();
    Some(v2::Bundle {
        identity_key: ik.try_into().ok()?,
        signed_pre_key_id: spk_el.attr("id")?.parse().ok()?,
        signed_pre_key: spk.try_into().ok()?,
        signed_pre_key_signature: sig.try_into().ok()?,
        pre_keys,
    })
}

async fn publish_bundle(shared: &Shared) {
    let Ok((id, bundle)) = with_own2(shared, |k| (k.device_id, k.bundle())).await else {
        return;
    };
    crate::pep::publish(
        shared,
        v2::BUNDLES_NODE,
        &id.to_string(),
        bundle_element(&bundle),
        BUNDLE_OPTIONS,
    )
    .await;
}

async fn publish_list(shared: &Shared, ids: &[u32]) -> bool {
    crate::pep::publish(
        shared,
        v2::DEVICES_NODE,
        "current",
        device_list(ids),
        crate::pep::OPEN,
    )
    .await
}

/// Tests can run a daemon that only speaks legacy OMEMO.
fn disabled() -> bool {
    std::env::var("PIOPIO_TEST_LEGACY_OMEMO_ONLY").as_deref() == Ok("1")
}

/// After every login, beside legacy OMEMO's.
pub async fn online(shared: &Shared) {
    if disabled() {
        return;
    }
    shared.omemo.fetched2.lock().await.clear();
    let Ok(own_id) = with_own2(shared, |k| {
        k.refill_pre_keys();
        k.device_id
    })
    .await
    else {
        return;
    };
    publish_bundle(shared).await;
    let jid = shared.jid.clone();
    let mut ids = fetch_item(shared, &jid, v2::DEVICES_NODE)
        .await
        .map(|l| device_ids(&l))
        .unwrap_or_default();
    if !ids.contains(&own_id) {
        ids.push(own_id);
        publish_list(shared, &ids).await;
    }
    shared
        .omemo
        .fetched2
        .lock()
        .await
        .insert(jid.to_string(), Instant::now());
    device_list_received(shared, &jid, ids).await;
}

/// Takes our device out of the OMEMO 2 list (the account is being removed).
pub async fn retire(shared: &Shared) {
    let Ok(Some(_)) = shared.store.omemo_own_v(2, shared.account_id).await else {
        return;
    };
    let Ok(own_id) = with_own2(shared, |k| k.device_id).await else {
        return;
    };
    let jid = shared.jid.clone();
    if let Some(list) = fetch_item(shared, &jid, v2::DEVICES_NODE).await {
        let ids: Vec<u32> = device_ids(&list)
            .into_iter()
            .filter(|id| *id != own_id)
            .collect();
        publish_list(shared, &ids).await;
    }
}

/// An OMEMO 2 device list notification. Returns true if it was one.
pub async fn event(shared: &Shared, from: Option<&Jid>, items: &Element) -> bool {
    if items.attr("node") != Some(v2::DEVICES_NODE) {
        return false;
    }
    let jid = match from {
        None => shared.jid.clone(),
        Some(j) if j.resource().is_none() => j.to_bare(),
        Some(_) => return true,
    };
    let ids = items
        .children()
        .filter(|c| c.is("item", tokio_xmpp::parsers::ns::PUBSUB_EVENT))
        .last()
        .and_then(|item| item.children().next())
        .map(device_ids)
        .unwrap_or_default();
    shared
        .omemo
        .fetched2
        .lock()
        .await
        .insert(jid.to_string(), Instant::now());
    if jid == shared.jid
        && !disabled()
        && !shared.omemo.retiring()
        && let Ok(own_id) = with_own2(shared, |k| k.device_id).await
        && !ids.contains(&own_id)
    {
        let mut ids = ids.clone();
        ids.push(own_id);
        publish_list(shared, &ids).await;
    }
    device_list_received(shared, &jid, ids).await;
    true
}

async fn device_list_received(shared: &Shared, jid: &BareJid, ids: Vec<u32>) {
    let before = shared
        .store
        .devices_v(2, shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    let _ = shared
        .store
        .set_device_list_v(2, shared.account_id, jid.to_string(), ids)
        .await;
    let after = shared
        .store
        .devices_v(2, shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    if before != after {
        devices_changed(shared, jid).await;
    }
}

pub(crate) async fn ensure_devices(shared: &Shared, jid: &BareJid) {
    if let Some(when) = shared.omemo.fetched2.lock().await.get(jid.as_str())
        && when.elapsed() < DEVICE_LIST_TTL
    {
        return;
    }
    let ids = fetch_item(shared, jid, v2::DEVICES_NODE)
        .await
        .map(|l| device_ids(&l))
        .unwrap_or_default();
    // Stored before it counts as fetched, so nobody reads an empty list.
    device_list_received(shared, jid, ids).await;
    shared
        .omemo
        .fetched2
        .lock()
        .await
        .insert(jid.to_string(), Instant::now());
}

async fn active(shared: &Shared, version: u8, jid: &BareJid) -> HashSet<u32> {
    shared
        .store
        .devices_v(version, shared.account_id, jid.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.active)
        .map(|d| d.device_id)
        .collect()
}

/// Whether to use OMEMO 2 with `peer`, from what we know now: every device
/// of theirs, and every other device of ours, reads OMEMO 2.
pub async fn prefers_v2(shared: &Shared, peer: &BareJid) -> bool {
    if disabled() {
        return false;
    }
    let theirs2 = active(shared, 2, peer).await;
    let theirs1 = active(shared, 1, peer).await;
    tracing::debug!(account = %shared.jid, %peer, ?theirs1, ?theirs2, "choosing the OMEMO version");
    if theirs2.is_empty() || !theirs1.is_subset(&theirs2) {
        return false;
    }
    let own_id = with_own2(shared, |k| k.device_id).await.unwrap_or(0);
    let mut ours1 = active(shared, 1, &shared.jid).await;
    let mut ours2 = active(shared, 2, &shared.jid).await;
    ours1.remove(&own_id);
    ours2.remove(&own_id);
    ours1.is_subset(&ours2)
}

// --- trust and sessions ------------------------------------------------------

async fn learn_identity(
    shared: &Shared,
    jid: &BareJid,
    device: u32,
    identity: &[u8; 32],
) -> String {
    let devices = shared
        .store
        .devices_v(2, shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    let identity = hex(identity);
    let known = devices.iter().find(|d| d.device_id == device);
    if let Some(d) = known
        && d.identity == identity
    {
        return d.trust.clone();
    }
    // The same device's legacy identity is the same key: carry its trust.
    let legacy = shared
        .store
        .devices(shared.account_id, jid.to_string())
        .await
        .unwrap_or_default();
    let same_key = legacy.iter().find(|d| {
        d.device_id == device
            && crate::omemo::unhex(&d.identity)
                .and_then(|k| omemo::xeddsa::edwards_public(&k))
                .is_some_and(|ed| hex(&ed) == identity)
    });
    let trust = match same_key {
        Some(d) => d.trust.clone(),
        None if devices
            .iter()
            .chain(legacy.iter())
            // A record without a key (trust set before it arrived)
            // doesn't vouch for anything.
            .any(|d| !d.identity.is_empty() && d.trust == trust::VERIFIED) =>
        {
            trust::UNDECIDED.to_owned()
        }
        None => trust::BLIND.to_owned(),
    };
    let _ = shared
        .store
        .set_device_v(
            2,
            shared.account_id,
            jid.to_string(),
            device,
            identity,
            trust.clone(),
        )
        .await;
    devices_changed(shared, jid).await;
    trust
}

async fn load_session(shared: &Shared, jid: &BareJid, device: u32) -> Option<v2::Session> {
    let json = shared
        .store
        .omemo_session_v(2, shared.account_id, jid.to_string(), device)
        .await
        .ok()??;
    serde_json::from_str(&json).ok()
}

async fn save_session(shared: &Shared, jid: &BareJid, device: u32, s: &v2::Session) {
    if let Ok(json) = serde_json::to_string(s) {
        let _ = shared
            .store
            .set_omemo_session_v(2, shared.account_id, jid.to_string(), device, json)
            .await;
    }
}

async fn session_for(shared: &Shared, jid: &BareJid, device: u32) -> Option<v2::Session> {
    let known = shared
        .store
        .devices_v(2, shared.account_id, jid.to_string())
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
    let bundle = fetch_bundle(shared, jid, device).await?;
    let trust = learn_identity(shared, jid, device, &bundle.identity_key).await;
    if !trust::is_trusted(&trust) {
        return None;
    }
    let session = match with_own2(shared, |own| v2::initiate(own, &bundle)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(%jid, device, "OMEMO 2 bundle rejected: {e}");
            return None;
        }
        Err(_) => return None,
    };
    save_session(shared, jid, device, &session).await;
    Some(session)
}

/// One device's bundle: the item with its id in the bundles node.
async fn fetch_bundle(shared: &Shared, jid: &BareJid, device: u32) -> Option<v2::Bundle> {
    use tokio_xmpp::parsers::ns;
    let to = (*jid != shared.jid).then(|| Jid::from(jid.clone()));
    let query = crate::pep::pubsub(
        Element::builder("items", ns::PUBSUB)
            .attr(n("node"), v2::BUNDLES_NODE)
            .append(
                Element::builder("item", ns::PUBSUB)
                    .attr(n("id"), device.to_string())
                    .build(),
            )
            .build(),
    );
    let reply = shared.iq(crate::pep::get(to, query)).await.ok()??;
    let bundle = reply
        .get_child("items", ns::PUBSUB)?
        .children()
        .find(|c| c.is("item", ns::PUBSUB) && c.attr("id") == Some(&device.to_string()))?
        .children()
        .next()?
        .clone();
    parse_bundle(&bundle)
}

// --- messages ------------------------------------------------------------------

/// Wraps message content in an SCE envelope.
pub fn envelope(from: &BareJid, content: Vec<Element>) -> String {
    let mut c = Element::builder("content", SCE);
    for e in content {
        c = c.append(e);
    }
    // Random padding hides the message's length.
    let pad_len = (u16::from_le_bytes(rand2()) % 200) as usize;
    let pad: String = std::iter::repeat_n('x', pad_len).collect();
    let env = Element::builder("envelope", SCE)
        .append(c.build())
        .append(Element::builder("rpad", SCE).append(pad.as_str()).build())
        .append(
            Element::builder("from", SCE)
                .attr(n("jid"), from.as_str())
                .build(),
        )
        .build();
    String::from(&env)
}

fn rand2() -> [u8; 2] {
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    b
}

/// Encrypts an envelope for `peer`'s trusted devices and ours.
pub async fn encrypt(shared: &Shared, peer: &BareJid, envelope: &str) -> Outgoing {
    let own_id = match with_own2(shared, |k| k.device_id).await {
        Ok(id) => id,
        Err(_) => return Outgoing::Failed("our OMEMO keys could not be loaded".into()),
    };
    let mut targets: Vec<(BareJid, u32)> = Vec::new();
    for d in active(shared, 2, peer).await {
        targets.push((peer.clone(), d));
    }
    for d in active(shared, 2, &shared.jid).await {
        if d != own_id {
            targets.push((shared.jid.clone(), d));
        }
    }
    let _sessions = shared.omemo.sessions.lock().await;
    let payload = v2::encrypt_payload(envelope.as_bytes());
    let mut by_jid: Vec<(BareJid, Vec<Element>)> = Vec::new();
    for (jid, device) in targets {
        let Some(mut session) = session_for(shared, &jid, device).await else {
            continue;
        };
        let e = session.encrypt(&payload.key_material);
        save_session(shared, &jid, device, &session).await;
        let mut key = Element::builder("key", NS)
            .attr(n("rid"), device.to_string())
            .append(B64.encode(&e.data));
        if e.kex {
            key = key.attr(n("kex"), "true");
        }
        match by_jid.iter_mut().find(|(j, _)| *j == jid) {
            Some((_, keys)) => keys.push(key.build()),
            None => by_jid.push((jid, vec![key.build()])),
        }
    }
    if !by_jid.iter().any(|(j, _)| j == peer) {
        return Outgoing::Failed(format!(
            "none of {peer}'s OMEMO devices is trusted or reachable"
        ));
    }
    let mut header = Element::builder("header", NS).attr(n("sid"), own_id.to_string());
    for (jid, keys) in by_jid {
        let mut k = Element::builder("keys", NS).attr(n("jid"), jid.as_str());
        for key in keys {
            k = k.append(key);
        }
        header = header.append(k.build());
    }
    Outgoing::Encrypted(
        Element::builder("encrypted", NS)
            .append(header.build())
            .append(
                Element::builder("payload", NS)
                    .append(B64.encode(&payload.ciphertext))
                    .build(),
            )
            .build(),
    )
}

/// What an OMEMO 2 message carried.
pub enum Opened {
    /// The envelope's content elements, and whether the device is trusted.
    Content(Vec<Element>, bool),
    Other(Decrypted),
}

/// Decrypts an OMEMO 2 `<encrypted/>` from `sender` and opens its envelope.
pub async fn decrypt(shared: &Session, sender: &BareJid, encrypted: &Element) -> Opened {
    let _sessions = shared.omemo.sessions.lock().await;
    let fail = |why: &str| Opened::Other(Decrypted::Failed(why.to_owned()));
    let Ok(own_id) = with_own2(shared, |k| k.device_id).await else {
        return fail("our OMEMO keys could not be loaded");
    };
    let Some(header) = encrypted.get_child("header", NS) else {
        return fail("no header");
    };
    let Some(device) = header.attr("sid").and_then(|s| s.parse::<u32>().ok()) else {
        return fail("no sender device");
    };
    let own = shared.jid.as_str();
    let key = header
        .children()
        .filter(|k| k.is("keys", NS) && k.attr("jid") == Some(own))
        .flat_map(|k| k.children())
        .find(|k| k.is("key", NS) && k.attr("rid") == Some(&own_id.to_string()));
    let Some(key) = key else {
        return fail("the message was not encrypted for this device");
    };
    let Ok(data) = B64.decode(key.text().trim()) else {
        return fail("unreadable key");
    };
    let kex = matches!(key.attr("kex"), Some("true" | "1"));
    let existing = load_session(shared, sender, device).await;
    let result = if kex {
        let exchange = match v2::parse_key_exchange(&data) {
            Ok(k) => k,
            Err(e) => return fail(&format!("unreadable key exchange: {e}")),
        };
        match existing {
            Some(mut s) if s.base_key == exchange.base_key => {
                exchange.decrypt_with(&mut s).map(|p| (s, p, false))
            }
            _ => match with_own2(shared, |own| exchange.accept(own)).await {
                Ok(r) => r.map(|(s, p)| (s, p, true)),
                Err(e) => return fail(&format!("{e:#}")),
            },
        }
    } else {
        match existing {
            Some(mut s) => s.decrypt(&data).map(|p| (s, p, false)),
            None => return fail("no OMEMO 2 session with the sending device"),
        }
    };
    let (session, key_material, used_pre_key) = match result {
        Ok(r) => r,
        Err(e) => return fail(&format!("{e}")),
    };
    let trust = learn_identity(shared, sender, device, &session.remote_identity).await;
    save_session(shared, sender, device, &session).await;
    if used_pre_key {
        let _ = with_own2(shared, |own| own.refill_pre_keys()).await;
        let s = shared.clone();
        tokio::spawn(async move { publish_bundle(&s).await });
    }
    let Some(payload) = encrypted.get_child("payload", NS) else {
        return Opened::Other(Decrypted::KeyTransport);
    };
    let Ok(ciphertext) = B64.decode(payload.text().trim()) else {
        return fail("unreadable payload");
    };
    let plain = match v2::decrypt_payload(&key_material, &ciphertext) {
        Ok(p) => p,
        Err(e) => return fail(&format!("{e}")),
    };
    let Ok(env) = String::from_utf8_lossy(&plain).parse::<Element>() else {
        return fail("the envelope isn't XML");
    };
    if !env.is("envelope", SCE) {
        return fail("not an SCE envelope");
    }
    // The envelope must say it is from who sent it.
    if let Some(from) = env.get_child("from", SCE)
        && from
            .attr("jid")
            .and_then(|j| j.parse::<BareJid>().ok())
            .is_some_and(|j| j != *sender)
    {
        return fail("the envelope names another sender");
    }
    let content = env
        .get_child("content", SCE)
        .map(|c| c.children().cloned().collect())
        .unwrap_or_default();
    Opened::Content(content, trust::is_trusted(&trust))
}

/// Our OMEMO 2 devices, for the bus (with the protocol named).
pub async fn devices_for_bus(shared: &Shared, jid: &str) -> Vec<crate::store::DeviceRow> {
    shared
        .store
        .devices_v(2, shared.account_id, jid.to_owned())
        .await
        .unwrap_or_default()
}

/// Fetches the bundle of each of `jid`'s devices whose key we don't know
/// yet, so it has a fingerprint to verify (and a trust to decide).
pub async fn learn_missing_identities(shared: &Shared, jid: &BareJid) {
    for d in devices_for_bus(shared, jid.as_str()).await {
        if d.active
            && d.identity.is_empty()
            && let Some(bundle) = fetch_bundle(shared, jid, d.device_id).await
        {
            learn_identity(shared, jid, d.device_id, &bundle.identity_key).await;
        }
    }
}

/// Sets trust on a device's OMEMO 2 record too, if it has one.
pub async fn set_trust(shared: &Shared, jid: &str, device: u32, trust: &str) {
    if let Some(d) = devices_for_bus(shared, jid)
        .await
        .into_iter()
        .find(|d| d.device_id == device)
    {
        let _ = shared
            .store
            .set_device_v(
                2,
                shared.account_id,
                jid.to_owned(),
                device,
                d.identity,
                trust.to_owned(),
            )
            .await;
    }
}
