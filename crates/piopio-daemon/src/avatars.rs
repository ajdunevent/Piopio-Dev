//! Avatars: contacts' pictures from PEP (XEP-0084), or from their vCard
//! (XEP-0153) when they publish none there, and our own.
//!
//! Pictures are scaled down here (to at most 192px) and kept in the data
//! directory, so the UI only ever loads small files. Conversations show
//! the peer's picture as their Avatar property; roster entries carry it
//! as "avatar".

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha1::{Digest, Sha1};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::ns;
use zbus::zvariant::Value;

use crate::pep::n;
use crate::session::{Session, Shared};

pub const METADATA: &str = "urn:xmpp:avatar:metadata";
pub const DATA: &str = "urn:xmpp:avatar:data";
const VCARD: &str = "vcard-temp";
const VCARD_UPDATE: &str = "vcard-temp:x:update";
/// Pictures larger than this are not fetched.
const MAX_BYTES: usize = 1024 * 1024;
const SIZE: u32 = 192;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn dir() -> std::path::PathBuf {
    crate::files::files_dir().join("avatars")
}

/// Scales a picture down and saves it as PNG; returns (path, png bytes).
fn save_scaled(data: &[u8], name: &str) -> anyhow::Result<(String, Vec<u8>)> {
    let image = image::load_from_memory(data)?;
    let image = if image.width() > SIZE || image.height() > SIZE {
        image.thumbnail(SIZE, SIZE)
    } else {
        image
    };
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    std::fs::create_dir_all(dir())?;
    let path = dir().join(format!("{name}.png"));
    std::fs::write(&path, &png)?;
    Ok((path.to_string_lossy().into_owned(), png))
}

/// Handles an avatar metadata notification. Returns true if it was one.
pub async fn event(session: &Session, from: Option<&Jid>, items: &Element) -> bool {
    if items.attr("node") != Some(METADATA) {
        return false;
    }
    let jid = match from {
        None => session.jid.clone(),
        Some(j) => j.to_bare(),
    };
    let metadata = items
        .children()
        .filter(|c| c.is("item", ns::PUBSUB_EVENT))
        .last()
        .and_then(|i| i.children().next());
    let info = metadata.and_then(|m| {
        m.children()
            .find(|c| c.is("info", METADATA) && c.attr("url").is_none())
    });
    let session = session.clone();
    let wanted = info.and_then(|i| {
        Some((
            i.attr("id")?.to_owned(),
            i.attr("bytes").and_then(|b| b.parse::<usize>().ok()),
        ))
    });
    // Fetching takes an IQ: don't hold up other stanzas.
    tokio::spawn(async move { update(&session, &jid, wanted).await });
    true
}

async fn update(session: &Session, jid: &BareJid, wanted: Option<(String, Option<usize>)>) {
    let current = session
        .store
        .avatar(session.account_id, jid.to_string())
        .await
        .ok()
        .flatten();
    let Some((hash, bytes)) = wanted else {
        if current.is_some_and(|(h, _)| !h.is_empty()) {
            let _ = session
                .store
                .set_avatar(
                    session.account_id,
                    jid.to_string(),
                    String::new(),
                    String::new(),
                )
                .await;
            changed(session, jid, "").await;
        }
        return;
    };
    if current.as_ref().is_some_and(|(h, _)| *h == hash) || bytes.is_some_and(|b| b > MAX_BYTES) {
        return;
    }
    let Some(data) = crate::pep::fetch_item(session, jid, DATA).await else {
        return;
    };
    let Ok(raw) = B64.decode(data.text().split_whitespace().collect::<String>()) else {
        return;
    };
    if raw.len() > MAX_BYTES || hex(&Sha1::digest(&raw)) != hash {
        tracing::info!(%jid, "avatar data doesn't match its metadata");
        return;
    }
    let name = hash.clone();
    let Ok(Ok((path, _))) = tokio::task::spawn_blocking(move || save_scaled(&raw, &name)).await
    else {
        return;
    };
    let _ = session
        .store
        .set_avatar(session.account_id, jid.to_string(), hash, path.clone())
        .await;
    changed(session, jid, &path).await;
}

async fn changed(session: &Shared, jid: &BareJid, path: &str) {
    let changes = HashMap::from([("avatar", Value::from(path))]);
    crate::roster::emit_changes(session, jid.as_str(), changes).await;
    session
        .conversations
        .avatar_changed(session, jid.as_str())
        .await;
}

/// Fetches `jid`'s picture if we have none yet (for people whose PEP
/// notifications we don't get: not in the roster).
pub fn refresh(session: &Session, jid: BareJid) {
    if !session.is_online() {
        return;
    }
    let session = session.clone();
    tokio::spawn(async move {
        if session
            .store
            .avatar(session.account_id, jid.to_string())
            .await
            .ok()
            .flatten()
            .is_some_and(|(h, _)| !h.is_empty())
        {
            return;
        }
        check(&session, &jid).await;
    });
}

/// After connecting: checks the picture of everyone we have a chat with or
/// in the roster. PEP notifications only come from contacts who share
/// their presence with us, and only while we're online, so pictures
/// changed meanwhile or kept only in a vCard would otherwise never show.
pub async fn online(session: &Session) {
    let mut peers = std::collections::BTreeSet::new();
    if let Ok(rows) = session.store.conversations(session.account_id).await {
        peers.extend(
            rows.into_iter()
                .filter(|r| r.kind == "chat")
                .map(|r| r.peer),
        );
    }
    if let Ok(rows) = session.store.contacts(session.account_id).await {
        peers.extend(rows.into_iter().map(|r| r.jid));
    }
    let session = session.clone();
    tokio::spawn(async move {
        for peer in peers {
            if !session.is_online() {
                return;
            }
            if let Ok(jid) = peer.parse::<BareJid>()
                && jid != session.jid
            {
                check(&session, &jid).await;
            }
        }
    });
}

/// Looks for `jid`'s picture: in PEP first, in the vCard when PEP has
/// nothing at all (an empty PEP item means they removed it).
async fn check(session: &Session, jid: &BareJid) {
    match crate::pep::fetch_item(session, jid, METADATA).await {
        Some(metadata) => {
            let wanted = metadata
                .children()
                .find(|c| c.is("info", METADATA) && c.attr("url").is_none())
                .and_then(|i| {
                    Some((
                        i.attr("id")?.to_owned(),
                        i.attr("bytes").and_then(|b| b.parse().ok()),
                    ))
                });
            update(session, jid, wanted).await;
        }
        None => from_vcard(session, jid, None).await,
    }
}

/// A presence's vCard picture hash (XEP-0153): fetches the vCard when the
/// picture differs from the one we have. Pictures from PEP win, so this
/// only replaces one that also came from a vCard or none at all.
pub fn presence(session: &Session, presence: &tokio_xmpp::parsers::presence::Presence) {
    let Some(from) = presence.from.as_ref() else {
        return;
    };
    let Some(hash) = presence
        .payloads
        .iter()
        .find(|p| p.is("x", VCARD_UPDATE))
        .and_then(|x| x.get_child("photo", VCARD_UPDATE))
        .map(|p| p.text().trim().to_lowercase())
        .filter(|h| !h.is_empty())
    else {
        return;
    };
    let jid = from.to_bare();
    if jid == session.jid {
        return;
    }
    let session = session.clone();
    tokio::spawn(async move {
        let current = session
            .store
            .avatar(session.account_id, jid.to_string())
            .await
            .ok()
            .flatten();
        if current.as_ref().is_some_and(|(h, _)| *h == hash) {
            return;
        }
        // Someone with a PEP picture: that one stays.
        if current.is_some_and(|(h, _)| !h.is_empty())
            && crate::pep::fetch_item(&session, &jid, METADATA)
                .await
                .is_some_and(|m| m.get_child("info", METADATA).is_some())
        {
            return;
        }
        from_vcard(&session, &jid, Some(hash)).await;
    });
}

/// Takes the picture from `jid`'s vCard (XEP-0054), if it has one.
async fn from_vcard(session: &Session, jid: &BareJid, expected: Option<String>) {
    let query = Element::builder("vCard", VCARD).build();
    let to = Some(Jid::from(jid.clone()));
    let reply = match session.iq(crate::pep::get(to, query)).await {
        Ok(Some(reply)) => reply,
        _ => return,
    };
    let photo = reply.get_child("PHOTO", VCARD);
    let raw = photo
        .and_then(|p| p.get_child("BINVAL", VCARD))
        .and_then(|b| {
            B64.decode(b.text().split_whitespace().collect::<String>())
                .ok()
        })
        .filter(|raw| !raw.is_empty() && raw.len() <= MAX_BYTES);
    let current = session
        .store
        .avatar(session.account_id, jid.to_string())
        .await
        .ok()
        .flatten();
    let Some(raw) = raw else {
        // No picture there (and none in PEP): forget an old one.
        if expected.is_none() && current.is_some_and(|(h, _)| !h.is_empty()) {
            let _ = session
                .store
                .set_avatar(
                    session.account_id,
                    jid.to_string(),
                    String::new(),
                    String::new(),
                )
                .await;
            changed(session, jid, "").await;
        }
        return;
    };
    let hash = hex(&Sha1::digest(&raw));
    if current.as_ref().is_some_and(|(h, _)| *h == hash) {
        return;
    }
    if expected.as_ref().is_some_and(|e| *e != hash) {
        tracing::debug!(%jid, "vCard picture doesn't match the presence's hash");
    }
    let name = hash.clone();
    let Ok(Ok((path, _))) = tokio::task::spawn_blocking(move || save_scaled(&raw, &name)).await
    else {
        return;
    };
    let _ = session
        .store
        .set_avatar(session.account_id, jid.to_string(), hash, path.clone())
        .await;
    changed(session, jid, &path).await;
}

pub async fn path_of(session: &Shared, jid: &str) -> String {
    session
        .store
        .avatar(session.account_id, jid.to_owned())
        .await
        .ok()
        .flatten()
        .map(|(_, p)| p)
        .unwrap_or_default()
}

/// Sets our own picture (any common image format), or removes it when
/// `data` is empty.
pub async fn set_own(session: &Session, data: Vec<u8>) -> anyhow::Result<()> {
    if data.is_empty() {
        let empty = Element::builder("metadata", METADATA).build();
        crate::pep::publish(session, METADATA, "current", empty, crate::pep::OPEN).await;
        let own = session.jid.clone();
        let _ = session
            .store
            .set_avatar(
                session.account_id,
                own.to_string(),
                String::new(),
                String::new(),
            )
            .await;
        changed(session, &own, "").await;
        return Ok(());
    }
    let (path, png) = tokio::task::spawn_blocking(move || {
        let hash = hex(&Sha1::digest(&data));
        save_scaled(&data, &format!("own-{hash}"))
    })
    .await??;
    let image = image::load_from_memory(&png)?;
    let hash = hex(&Sha1::digest(&png));
    let payload = Element::builder("data", DATA)
        .append(B64.encode(&png))
        .build();
    if !crate::pep::publish(session, DATA, &hash, payload, crate::pep::OPEN).await {
        anyhow::bail!("your server didn't accept the picture");
    }
    let metadata = Element::builder("metadata", METADATA)
        .append(
            Element::builder("info", METADATA)
                .attr(n("id"), hash.as_str())
                .attr(n("bytes"), png.len().to_string())
                .attr(n("type"), "image/png")
                .attr(n("width"), image.width().to_string())
                .attr(n("height"), image.height().to_string())
                .build(),
        )
        .build();
    if !crate::pep::publish(session, METADATA, &hash, metadata, crate::pep::OPEN).await {
        anyhow::bail!("your server didn't accept the picture");
    }
    let own = session.jid.clone();
    session
        .store
        .set_avatar(session.account_id, own.to_string(), hash, path.clone())
        .await?;
    changed(session, &own, &path).await;
    Ok(())
}
