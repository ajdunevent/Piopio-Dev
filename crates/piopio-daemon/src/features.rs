//! What the server and contacts support, so nothing is tried or offered
//! that the other side can't do.
//!
//! The server's features (XEP-0030) come from its own disco#info and our
//! account's, right after connecting; "pep" stands for a PEP service and
//! [`UPLOAD`] for an upload service. Until that answer arrives, or if the
//! server doesn't answer, everything is assumed supported, as before.
//!
//! Contacts' features come from the entity capabilities (XEP-0115) in their
//! presence, looked up once per capabilities hash and shared by everyone
//! using the same app. The features of a contact's online devices are
//! combined, and kept in the database for when they are offline.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::ns;
use tokio_xmpp::parsers::presence::{Presence, Type as PresenceType};

use crate::pep::n;
use crate::session::{Session, Shared};

pub const BLOCKING: &str = "urn:xmpp:blocking";
pub const CARBONS: &str = "urn:xmpp:carbons:2";
pub const MAM: &str = "urn:xmpp:mam:2";
/// Our marker for a PEP service (an identity, not a feature, on the wire).
pub const PEP: &str = "pep";
/// Our marker for an HTTP upload service (XEP-0363) on the server.
pub const UPLOAD: &str = "urn:xmpp:http:upload:0";

const CAPS: &str = "http://jabber.org/protocol/caps";

type Set = Arc<BTreeSet<String>>;

#[derive(Default)]
pub struct Features {
    /// None until the server has answered.
    server: std::sync::RwLock<Option<Set>>,
    /// Each contact's online resources and what they support.
    peers: Mutex<HashMap<BareJid, HashMap<String, Set>>>,
    /// Capabilities hashes we have looked up (node#ver → features).
    caps: Mutex<HashMap<String, Set>>,
}

impl Features {
    /// Whether the server supports `feature`; true while unknown.
    pub fn server_has(&self, feature: &str) -> bool {
        match &*self.server.read().expect("lock") {
            Some(set) => set.contains(feature),
            None => true,
        }
    }

    /// The server's features, for the Account object ([] while unknown).
    pub fn server_list(&self) -> Vec<String> {
        self.server
            .read()
            .expect("lock")
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Forgets contacts' online devices (we went offline).
    pub async fn offline(&self) {
        self.peers.lock().await.clear();
    }
}

fn disco_features(reply: &Element) -> impl Iterator<Item = String> + '_ {
    reply
        .children()
        .filter(|c| c.is("feature", ns::DISCO_INFO))
        .filter_map(|c| c.attr("var"))
        .map(str::to_owned)
}

async fn disco_info(session: &Shared, to: Jid, node: Option<&str>) -> Option<Element> {
    let mut query = Element::builder("query", ns::DISCO_INFO);
    if let Some(node) = node {
        query = query.attr(n("node"), node);
    }
    match session.iq(crate::pep::get(Some(to), query.build())).await {
        Ok(reply) => reply,
        Err(e) => {
            tracing::debug!("disco#info: {e}");
            None
        }
    }
}

/// Asks the server and our account what they support. Called first after
/// connecting, before anything that depends on it.
pub async fn discover_server(session: &Session) {
    let domain = Jid::from(session.jid.domain().to_owned());
    let account = Jid::from(session.jid.clone());
    // The upload service is a separate component; look for it meanwhile.
    let (server, own, upload) = tokio::join!(
        disco_info(session, domain, None),
        disco_info(session, account, None),
        crate::files::has_upload_service(session)
    );
    if server.is_none() && own.is_none() {
        tracing::info!("the server didn't say what it supports; assuming everything");
        return;
    }
    let mut set: BTreeSet<String> = server.iter().flat_map(disco_features).collect();
    if let Some(own) = &own {
        set.extend(disco_features(own));
        let pep = own.children().any(|c| {
            c.is("identity", ns::DISCO_INFO)
                && c.attr("category") == Some("pubsub")
                && c.attr("type") == Some("pep")
        });
        if pep {
            set.insert(PEP.to_owned());
        }
    }
    if upload {
        set.insert(UPLOAD.to_owned());
    }
    *session.features.server.write().expect("lock") = Some(Arc::new(set));
    crate::account::features_changed(session).await;
}

/// A contact's presence: learns what the device supports from its caps.
pub fn presence(session: &Session, presence: &Presence) {
    let Some(Ok(from)) = presence.from.clone().map(Jid::try_into_full) else {
        return;
    };
    let bare = from.to_bare();
    if bare == session.jid {
        return;
    }
    let resource = from.resource().to_string();
    let session = session.clone();
    if presence.type_ == PresenceType::Unavailable {
        tokio::spawn(async move {
            let changed = {
                let mut peers = session.features.peers.lock().await;
                let removed = peers
                    .get_mut(&bare)
                    .is_some_and(|r| r.remove(&resource).is_some());
                if peers.get(&bare).is_some_and(HashMap::is_empty) {
                    peers.remove(&bare);
                }
                removed
            };
            if changed {
                peer_changed(&session, &bare).await;
            }
        });
        return;
    }
    if presence.type_ != PresenceType::None {
        return;
    }
    let caps = presence
        .payloads
        .iter()
        .find(|p| p.is("c", CAPS))
        .and_then(|c| Some(format!("{}#{}", c.attr("node")?, c.attr("ver")?)));
    tokio::spawn(async move {
        let known = match &caps {
            Some(key) => session.features.caps.lock().await.get(key).cloned(),
            None => None,
        };
        let set = match known {
            Some(set) => set,
            None => {
                // Ask the device; with caps, by its node#ver so the answer
                // can be shared with everyone using the same app.
                let Some(reply) =
                    disco_info(&session, Jid::from(from.clone()), caps.as_deref()).await
                else {
                    return;
                };
                let set: Set = Arc::new(disco_features(&reply).collect());
                if let Some(key) = caps {
                    session.features.caps.lock().await.insert(key, set.clone());
                }
                set
            }
        };
        let changed = {
            let mut peers = session.features.peers.lock().await;
            let resources = peers.entry(bare.clone()).or_default();
            resources.insert(resource, set.clone()) != Some(set)
        };
        if changed {
            peer_changed(&session, &bare).await;
        }
    });
}

/// Stores the contact's combined features and tells their conversation.
async fn peer_changed(session: &Session, jid: &BareJid) {
    let union = online_union(session, jid).await;
    if let Some(union) = union {
        let list: Vec<String> = union.into_iter().collect();
        let _ = session
            .store
            .set_peer_features(session.account_id, jid.to_string(), list)
            .await;
    }
    session
        .conversations
        .features_changed(session, jid.as_str())
        .await;
}

async fn online_union(session: &Shared, jid: &BareJid) -> Option<BTreeSet<String>> {
    let peers = session.features.peers.lock().await;
    let resources = peers.get(jid)?;
    if resources.is_empty() {
        return None;
    }
    Some(resources.values().flat_map(|s| s.iter().cloned()).collect())
}

/// What `jid`'s apps support: their online devices together, or what they
/// last had when none is online. Empty when unknown.
pub async fn peer(session: &Shared, jid: &str) -> Vec<String> {
    if let Ok(bare) = jid.parse::<BareJid>()
        && let Some(union) = online_union(session, &bare).await
    {
        return union.into_iter().collect();
    }
    session
        .store
        .peer_features(session.account_id, jid.to_owned())
        .await
        .unwrap_or_default()
}
