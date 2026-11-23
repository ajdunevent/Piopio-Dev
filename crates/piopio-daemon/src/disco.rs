//! Service discovery (XEP-0030): tells other clients what Piopio understands.

use std::collections::BTreeSet;

use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::caps::{Caps, compute_disco, hash_caps};
use tokio_xmpp::parsers::disco::{DiscoInfoQuery, DiscoInfoResult, Identity};
use tokio_xmpp::parsers::hashes::Algo;
use tokio_xmpp::parsers::iq::Iq;

use crate::session::Shared;

/// Features this build supports. A feature is only listed once it works.
pub const FEATURES: &[&str] = &[
    "http://jabber.org/protocol/disco#info",
    "urn:xmpp:ping",
    "urn:xmpp:receipts",
    "urn:xmpp:chat-markers:0",
    "urn:xmpp:sid:0",
    "urn:xmpp:carbons:2",
    "eu.siacs.conversations.axolotl.devicelist+notify",
    "urn:xmpp:message-correct:0",
    "urn:xmpp:message-retract:0",
    "urn:xmpp:message-retract:1",
    "urn:xmpp:reactions:0",
    "urn:xmpp:reply:0",
    "urn:xmpp:fallback:0",
];

/// The node our entity capabilities (XEP-0115) point at.
pub const CAPS_NODE: &str = "https://dunevent.net/piopio";

/// What we tell others about ourselves.
pub fn info(node: Option<String>) -> DiscoInfoResult {
    DiscoInfoResult {
        node,
        identities: vec![Identity::new("client", "pc", "en", "Piopio")],
        features: FEATURES
            .iter()
            .map(|f| f.to_string())
            .collect::<BTreeSet<_>>(),
        extensions: vec![],
    }
}

/// The <c/> element for our presence. Servers use it to send us the PEP
/// notifications we asked for with +notify (OMEMO device lists).
pub fn caps() -> Caps {
    let hash = hash_caps(&compute_disco(&info(None)), Algo::Sha_1).expect("sha-1 is supported");
    Caps::new(CAPS_NODE, hash)
}

/// Answers a disco#info query, or returns None if `payload` isn't one.
pub fn answer(_shared: &Shared, from: &Option<Jid>, id: &str, payload: &Element) -> Option<Iq> {
    let query = DiscoInfoQuery::try_from(payload.clone()).ok()?;
    let result = info(query.node);
    Some(Iq::from_result(id, Some(result)).with_to_opt(from.clone()))
}

trait WithToOpt {
    fn with_to_opt(self, to: Option<Jid>) -> Self;
}

impl WithToOpt for Iq {
    fn with_to_opt(self, to: Option<Jid>) -> Self {
        match to {
            Some(to) => self.with_to(to),
            None => self,
        }
    }
}
