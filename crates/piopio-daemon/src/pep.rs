//! Personal Eventing (XEP-0163): publishing to and reading from PEP nodes.

use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::ns;

use crate::session::Shared;

const PUBLISH_OPTIONS: &str = "http://jabber.org/protocol/pubsub#publish-options";

/// Anyone may read the node (OMEMO keys).
pub const OPEN: &[(&str, &str)] = &[("pubsub#access_model", "open")];

/// Only we may read the node, and it keeps every item (bookmarks).
pub const PRIVATE_LIST: &[(&str, &str)] = &[
    ("pubsub#access_model", "whitelist"),
    ("pubsub#persist_items", "true"),
    ("pubsub#max_items", "max"),
    ("pubsub#send_last_published_item", "never"),
];

/// An attribute name (all of ours are valid XML names).
pub fn n(name: &'static str) -> tokio_xmpp::minidom::rxml::NcName {
    name.try_into().expect("a valid attribute name")
}

pub fn pubsub(child: Element) -> Element {
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
/// with `options` (pairs of publish-options fields and values).
pub async fn publish(
    shared: &Shared,
    node: &str,
    id: &str,
    payload: Element,
    options: &[(&str, &str)],
) -> bool {
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
                .append_all(options.iter().map(|(k, v)| field(k, v)))
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

pub fn set(to: Option<Jid>, payload: Element) -> Iq {
    Iq::Set {
        from: None,
        to,
        id: String::new(),
        payload,
    }
}

pub fn get(to: Option<Jid>, payload: Element) -> Iq {
    Iq::Get {
        from: None,
        to,
        id: String::new(),
        payload,
    }
}

/// Fetches the newest item of `node` from `jid`'s PEP service.
pub async fn fetch_item(shared: &Shared, jid: &BareJid, node: &str) -> Option<Element> {
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

/// Fetches every item of `node` on our own PEP service: (id, payload).
pub async fn fetch_all(shared: &Shared, node: &str) -> Option<Vec<(String, Element)>> {
    let query = pubsub(
        Element::builder("items", ns::PUBSUB)
            .attr(n("node"), node)
            .build(),
    );
    match shared.iq(get(None, query)).await {
        Ok(Some(reply)) => Some(
            reply
                .get_child("items", ns::PUBSUB)?
                .children()
                .filter(|c| c.is("item", ns::PUBSUB))
                .filter_map(|item| {
                    Some((item.attr("id")?.to_owned(), item.children().next()?.clone()))
                })
                .collect(),
        ),
        Ok(None) => Some(vec![]),
        Err(e) => {
            tracing::debug!("fetching {node}: {e}");
            None
        }
    }
}
