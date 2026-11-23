//! Sharing files: uploads through the server's HTTP upload service
//! (XEP-0363), links with out-of-band data (XEP-0066), and `aesgcm://`
//! links for files shared in encrypted conversations (XEP-0454), as
//! Conversations, Dino, Gajim, Monal and Converse.js do.
//!
//! Received files are downloaded into the data directory (pictures
//! automatically, other files when asked), and pictures get a thumbnail
//! here so the UI never decodes a full-size image.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::{AesGcm, Nonce};
use futures_util::StreamExt;
use tokio::sync::Mutex;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::http_upload::{SlotRequest, SlotResult};
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::message::Message;
use tokio_xmpp::parsers::ns;
use zbus::zvariant::{OwnedValue, Str, Value};

use crate::messages::emit_changed;
use crate::session::{Session, Shared};
use crate::store::{AttachmentRow, MessageRow};

const UPLOAD: &str = "urn:xmpp:http:upload:0";
pub const OOB: &str = "jabber:x:oob";
/// Pictures up to this size are fetched without asking.
const AUTO_DOWNLOAD_LIMIT: u64 = 10 * 1024 * 1024;
/// The largest file we fetch or send at all.
pub const MAX_FILE: u64 = 512 * 1024 * 1024;
const THUMBNAIL_SIZE: u32 = 480;

pub mod state {
    /// Known only by its link.
    pub const REMOTE: &str = "remote";
    pub const DOWNLOADING: &str = "downloading";
    pub const UPLOADING: &str = "uploading";
    /// The file is here (Path is set).
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

/// The account's upload service, found once per login.
#[derive(Default)]
pub struct Uploads {
    /// None until looked for; then the service and its size limit, if any.
    service: Mutex<Option<Option<Service>>>,
}

/// An upload service and the largest file it takes.
type Service = (Jid, Option<u64>);

impl Uploads {
    pub async fn forget(&self) {
        *self.service.lock().await = None;
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("Piopio")
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("an HTTP client")
    })
}

/// Plain HTTP is only for the local test server.
fn allowed(url: &str) -> bool {
    url.starts_with("https://")
        || (url.starts_with("http://")
            && std::env::var("PIOPIO_ALLOW_PLAINTEXT").as_deref() == Ok("1"))
}

pub fn files_dir() -> PathBuf {
    crate::store::data_dir().join("files")
}

fn sanitize(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim_start_matches('.').trim();
    let name: String = name.chars().take(120).collect();
    if name.is_empty() { "file".into() } else { name }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The file name a link points at.
pub fn name_of(url: &str) -> String {
    let without = url.split(['#', '?']).next().unwrap_or(url);
    let last = without.rsplit('/').next().unwrap_or("");
    sanitize(&percent_decode(last))
}

pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn is_picture(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}

/// The link a message shares, if it is only a link to a file: an
/// `aesgcm://` link, or a link the message also carries as OOB data.
pub fn shared_link(message: &Message, body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() || body.contains(char::is_whitespace) {
        return None;
    }
    if body.starts_with("aesgcm://") {
        return Some(body.to_owned());
    }
    let oob = message
        .payloads
        .iter()
        .find(|p| p.is("x", OOB))
        .and_then(|x| x.get_child("url", OOB))
        .map(|u| u.text());
    (oob.as_deref() == Some(body) && allowed(body)).then(|| body.to_owned())
}

pub fn oob_element(url: &str) -> Element {
    Element::builder("x", OOB)
        .append(Element::builder("url", OOB).append(url).build())
        .build()
}

pub fn dict_entries(a: &AttachmentRow, map: &mut std::collections::HashMap<String, OwnedValue>) {
    let s = |v: &str| OwnedValue::from(Str::from(v.to_owned()));
    map.insert("file-url".into(), s(&a.url));
    map.insert("file-name".into(), s(&a.name));
    map.insert("file-mime".into(), s(&a.mime));
    map.insert("file-size".into(), OwnedValue::from(a.size.unwrap_or(-1)));
    map.insert("file-path".into(), s(&a.path));
    map.insert("file-thumbnail".into(), s(&a.thumbnail));
    map.insert("file-state".into(), s(&a.state));
}

fn changes(a: &AttachmentRow) -> std::collections::HashMap<&'static str, Value<'static>> {
    std::collections::HashMap::from([
        ("file-url", Value::from(a.url.clone())),
        ("file-name", Value::from(a.name.clone())),
        ("file-mime", Value::from(a.mime.clone())),
        ("file-size", Value::from(a.size.unwrap_or(-1))),
        ("file-path", Value::from(a.path.clone())),
        ("file-thumbnail", Value::from(a.thumbnail.clone())),
        ("file-state", Value::from(a.state.clone())),
    ])
}

async fn save(session: &Session, conversation: i64, a: &AttachmentRow) {
    if let Err(e) = session.store.set_attachment(a.clone()).await {
        tracing::error!("storing an attachment: {e:#}");
        return;
    }
    emit_changed(session, conversation, a.message_id, changes(a)).await;
}

// --- receiving -------------------------------------------------------------

/// A stored message turned out to share a file: record it, and fetch it
/// if it is a picture.
pub async fn received(session: &Session, conversation: i64, row: &MessageRow, url: String) {
    let name = name_of(&url);
    let mime = mime_of(&name).to_owned();
    let a = AttachmentRow {
        message_id: row.id,
        url,
        name,
        mime: mime.clone(),
        size: None,
        path: String::new(),
        thumbnail: String::new(),
        state: state::REMOTE.into(),
    };
    save(session, conversation, &a).await;
    if is_picture(&mime) {
        let session = session.clone();
        tokio::spawn(async move { download(&session, conversation, a, AUTO_DOWNLOAD_LIMIT).await });
    }
}

/// The https link and decryption key of an `aesgcm://` link.
fn split_aesgcm(url: &str) -> Option<(String, Vec<u8>, [u8; 32])> {
    let rest = url.strip_prefix("aesgcm://")?;
    let (address, fragment) = rest.split_once('#')?;
    let bytes: Vec<u8> = (0..fragment.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(fragment.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let (iv, key) = match bytes.len() {
        44 => bytes.split_at(12),
        48 => bytes.split_at(16),
        _ => return None,
    };
    // aesgcm:// stands for https://. The local test server only has http.
    let local = address.starts_with("127.0.0.1") || address.starts_with("localhost");
    let scheme = if local && std::env::var("PIOPIO_ALLOW_PLAINTEXT").as_deref() == Ok("1") {
        "http"
    } else {
        "https"
    };
    Some((
        format!("{scheme}://{address}"),
        iv.to_vec(),
        key.try_into().ok()?,
    ))
}

fn decrypt(iv: &[u8], key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    match iv.len() {
        12 => AesGcm::<Aes256, aes_gcm::aead::consts::U12>::new(key.into())
            .decrypt(Nonce::from_slice(iv), data)
            .ok(),
        16 => AesGcm::<Aes256, aes_gcm::aead::consts::U16>::new(key.into())
            .decrypt(Nonce::from_slice(iv), data)
            .ok(),
        _ => None,
    }
}

/// Fetches an attachment (up to `limit` bytes) and stores it locally.
pub async fn download(session: &Session, conversation: i64, mut a: AttachmentRow, limit: u64) {
    if a.state == state::DOWNLOADING || a.state == state::DONE {
        return;
    }
    let (url, secret) = match split_aesgcm(&a.url) {
        Some((url, iv, key)) => (url, Some((iv, key))),
        None => (a.url.clone(), None),
    };
    if !allowed(&url) {
        a.state = state::FAILED.into();
        save(session, conversation, &a).await;
        return;
    }
    a.state = state::DOWNLOADING.into();
    save(session, conversation, &a).await;
    let result = fetch(&url, limit).await;
    let data = match result {
        Ok(data) => data,
        Err(e) => {
            tracing::info!("downloading {}: {e:#}", a.name);
            a.state = if e.to_string().contains("too large") {
                state::REMOTE.into()
            } else {
                state::FAILED.into()
            };
            save(session, conversation, &a).await;
            return;
        }
    };
    let data = match secret {
        Some((iv, key)) => match decrypt(&iv, &key, &data) {
            Some(plain) => plain,
            None => {
                tracing::warn!("{} could not be decrypted", a.name);
                a.state = state::FAILED.into();
                save(session, conversation, &a).await;
                return;
            }
        },
        None => data,
    };
    a.size = Some(data.len() as i64);
    match store_file(a.message_id, &a.name, &a.mime, data).await {
        Ok((path, thumbnail)) => {
            a.path = path;
            a.thumbnail = thumbnail;
            a.state = state::DONE.into();
        }
        Err(e) => {
            tracing::error!("saving {}: {e:#}", a.name);
            a.state = state::FAILED.into();
        }
    }
    save(session, conversation, &a).await;
}

async fn fetch(url: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    let response = client().get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|l| l > limit) {
        anyhow::bail!("too large");
    }
    let mut data = Vec::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if (data.len() + chunk.len()) as u64 > limit {
            anyhow::bail!("too large");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// Writes a file into the files directory (and a thumbnail for pictures).
/// Returns their paths.
async fn store_file(
    message: i64,
    name: &str,
    mime: &str,
    data: Vec<u8>,
) -> anyhow::Result<(String, String)> {
    let dir = files_dir();
    let path = dir.join(format!("{message}-{}", sanitize(name)));
    let thumb = dir.join("thumbnails").join(format!("{message}.png"));
    let picture = is_picture(mime);
    tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
        std::fs::create_dir_all(dir.join("thumbnails"))?;
        std::fs::write(&path, &data)?;
        let mut thumbnail = String::new();
        if picture {
            match make_thumbnail(&data, &thumb) {
                Ok(()) => thumbnail = thumb.to_string_lossy().into_owned(),
                Err(e) => tracing::info!("no thumbnail: {e}"),
            }
        }
        Ok((path.to_string_lossy().into_owned(), thumbnail))
    })
    .await?
}

fn make_thumbnail(data: &[u8], to: &Path) -> anyhow::Result<()> {
    let image = image::load_from_memory(data)?;
    let small = image.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE);
    small.save_with_format(to, image::ImageFormat::Png)?;
    Ok(())
}

// --- sending ---------------------------------------------------------------

/// Finds the server's upload service and its size limit.
async fn service(session: &Shared) -> Option<Service> {
    let mut cached = session.uploads.service.lock().await;
    if let Some(found) = cached.clone() {
        return found;
    }
    let domain = Jid::from(session.jid.domain().to_owned());
    let mut candidates = vec![domain.clone()];
    let items = crate::pep::get(
        Some(domain.clone()),
        Element::builder("query", ns::DISCO_ITEMS).build(),
    );
    if let Ok(Some(reply)) = session.iq(items).await {
        candidates.extend(
            reply
                .children()
                .filter(|c| c.is("item", ns::DISCO_ITEMS))
                .filter_map(|c| c.attr("jid")?.parse::<Jid>().ok()),
        );
    }
    let mut found = None;
    for jid in candidates {
        let info = crate::pep::get(
            Some(jid.clone()),
            Element::builder("query", ns::DISCO_INFO).build(),
        );
        let Ok(Some(reply)) = session.iq(info).await else {
            continue;
        };
        let supports = reply
            .children()
            .any(|c| c.is("feature", ns::DISCO_INFO) && c.attr("var") == Some(UPLOAD));
        if !supports {
            continue;
        }
        let max = reply
            .children()
            .filter(|c| c.is("x", "jabber:x:data"))
            .flat_map(|x| x.children())
            .find(|f| f.attr("var") == Some("max-file-size"))
            .and_then(|f| f.get_child("value", "jabber:x:data"))
            .and_then(|v| v.text().trim().parse().ok());
        found = Some((jid, max));
        break;
    }
    *cached = Some(found.clone());
    found
}

/// Uploads a file; returns the link to share (an `aesgcm://` link when
/// `encrypt` is set).
pub async fn upload(
    session: &Shared,
    name: &str,
    mime: &str,
    data: Vec<u8>,
    encrypt: bool,
) -> anyhow::Result<String> {
    let Some((service, max)) = service(session).await else {
        anyhow::bail!("your server doesn't offer file uploads");
    };
    let (body, secret) = if encrypt {
        let mut iv = [0u8; 12];
        let mut key = [0u8; 32];
        getrandom::fill(&mut iv).map_err(|e| anyhow::anyhow!("{e}"))?;
        getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("{e}"))?;
        let cipher = AesGcm::<Aes256, aes_gcm::aead::consts::U12>::new((&key).into());
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&iv), data.as_slice())
            .map_err(|_| anyhow::anyhow!("encrypting the file failed"))?;
        let mut secret = iv.to_vec();
        secret.extend_from_slice(&key);
        (encrypted, Some(secret))
    } else {
        (data, None)
    };
    let size = body.len() as u64;
    if max.is_some_and(|m| size > m) {
        anyhow::bail!(
            "the file is larger than your server allows ({} MB)",
            max.unwrap_or(0) / 1_000_000
        );
    }
    // Encrypted files go up as opaque data, as other clients do.
    let content_type = if encrypt {
        "application/octet-stream"
    } else {
        mime
    };
    let request = SlotRequest {
        filename: name.to_owned(),
        size,
        content_type: Some(content_type.to_owned()),
    };
    let iq = Iq::from_get("", request).with_to(service);
    let slot = match session.iq(iq).await {
        Ok(Some(payload)) => SlotResult::try_from(payload)?,
        Ok(None) => anyhow::bail!("the upload service gave no slot"),
        Err(e) => anyhow::bail!("the upload service refused the file: {e}"),
    };
    if !allowed(&slot.put.url) || !allowed(&slot.get.url) {
        anyhow::bail!("the upload service doesn't use https");
    }
    let mut put = client()
        .put(&slot.put.url)
        .header("Content-Type", content_type)
        .body(body);
    for header in &slot.put.headers {
        put = put.header(header.name.as_str(), header.value.as_str());
    }
    put.send().await?.error_for_status()?;
    Ok(match secret {
        Some(secret) => {
            let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
            let address = slot
                .get
                .url
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(&slot.get.url);
            format!("aesgcm://{address}#{hex}")
        }
        None => slot.get.url,
    })
}

/// Keeps our own copy of a file we send, so it shows without a download.
pub async fn keep_own_copy(
    message: i64,
    name: &str,
    mime: &str,
    data: Vec<u8>,
) -> anyhow::Result<(String, String)> {
    store_file(message, name, mime, data).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_link() {
        assert_eq!(
            name_of("https://up.example/abc/My%20Photo.jpg"),
            "My Photo.jpg"
        );
        assert_eq!(name_of("aesgcm://up.example/x/a.png#00ff"), "a.png");
        assert_eq!(name_of("https://up.example/x/..%2F..%2Fetc"), "_.._etc");
        assert_eq!(mime_of("A.JPG"), "image/jpeg");
    }

    #[test]
    fn aesgcm_links_split_and_decrypt() {
        let key = [7u8; 32];
        let iv = [9u8; 12];
        let cipher = AesGcm::<Aes256, aes_gcm::aead::consts::U12>::new((&key).into());
        let data = cipher
            .encrypt(Nonce::from_slice(&iv), &b"hello"[..])
            .unwrap();
        let hex: String = iv
            .iter()
            .chain(key.iter())
            .map(|b| format!("{b:02x}"))
            .collect();
        let (url, got_iv, got_key) = split_aesgcm(&format!("aesgcm://h/p.txt#{hex}")).unwrap();
        assert_eq!(url, "https://h/p.txt");
        assert_eq!(decrypt(&got_iv, &got_key, &data).unwrap(), b"hello");
        assert!(split_aesgcm("aesgcm://h/p#abcd").is_none());
    }

    #[test]
    fn only_lone_links_are_files() {
        let mut m = Message::chat(None);
        assert!(shared_link(&m, "aesgcm://h/p#00").is_some());
        assert!(shared_link(&m, "look aesgcm://h/p#00").is_none());
        assert!(shared_link(&m, "https://h/p.jpg").is_none(), "needs OOB");
        m.payloads.push(oob_element("https://h/p.jpg"));
        assert!(shared_link(&m, "https://h/p.jpg").is_some());
    }
}
