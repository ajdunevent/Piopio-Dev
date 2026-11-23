//! Service discovery (XEP-0030): tells other clients what Piopio understands.

use std::collections::BTreeSet;

use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::disco::{DiscoInfoQuery, DiscoInfoResult, Identity};
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
];

/// Answers a disco#info query, or returns None if `payload` isn't one.
pub fn answer(_shared: &Shared, from: &Option<Jid>, id: &str, payload: &Element) -> Option<Iq> {
    let query = DiscoInfoQuery::try_from(payload.clone()).ok()?;
    let result = DiscoInfoResult {
        node: query.node,
        identities: vec![Identity::new("client", "pc", "en", "Piopio")],
        features: FEATURES
            .iter()
            .map(|f| f.to_string())
            .collect::<BTreeSet<_>>(),
        extensions: vec![],
    };
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
