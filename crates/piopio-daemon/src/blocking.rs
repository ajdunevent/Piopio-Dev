//! Blocking (XEP-0191): the server drops everything from people on our
//! blocklist and hides our presence from them.
//!
//! The list lives on the server, so blocking from another device shows here
//! too: it is fetched after connecting and kept current by the server's
//! pushes. Conversations show whether their peer is blocked.

use std::collections::HashSet;

use tokio::sync::Mutex;
use tokio_xmpp::jid::BareJid;
use tokio_xmpp::minidom::Element;

use crate::pep::n;
use crate::session::{IqError, Session, Shared};

pub const NS: &str = "urn:xmpp:blocking";

/// The blocked addresses, as the server last told us.
#[derive(Default)]
pub struct Blocklist(Mutex<HashSet<String>>);

impl Blocklist {
    pub async fn contains(&self, jid: &str) -> bool {
        self.0.lock().await.contains(jid)
    }

    /// Everyone blocked, sorted.
    pub async fn list(&self) -> Vec<String> {
        let mut all: Vec<String> = self.0.lock().await.iter().cloned().collect();
        all.sort();
        all
    }
}

fn items(element: &Element) -> impl Iterator<Item = String> + '_ {
    element
        .children()
        .filter(|c| c.is("item", NS))
        .filter_map(|i| i.attr("jid"))
        .filter_map(|j| j.parse::<BareJid>().ok())
        .map(|j| j.to_string())
}

/// Fetches the list after connecting.
pub async fn online(session: &Session) {
    if !session.features.server_has(crate::features::BLOCKING) {
        return;
    }
    // The list stays locked until the answer is in, so a block made
    // meanwhile waits rather than being overwritten by the older answer.
    // (Pushes are handled outside the task that matches IQ answers, so
    // waiting here can't hold up the answer.)
    let mut list = session.blocked.0.lock().await;
    let query = Element::builder("blocklist", NS).build();
    let Ok(Some(reply)) = session.iq(crate::pep::get(None, query)).await else {
        return;
    };
    let fresh: HashSet<String> = items(&reply).collect();
    let changed: Vec<String> = list.symmetric_difference(&fresh).cloned().collect();
    *list = fresh;
    drop(list);
    let any = !changed.is_empty();
    for jid in changed {
        session.conversations.blocked_changed(session, &jid).await;
    }
    if any {
        crate::account::blocklist_changed(session).await;
    }
}

/// A push from our server (block or unblock). Returns false for anything
/// else.
pub async fn push(session: &Shared, payload: &Element) -> bool {
    let block = payload.is("block", NS);
    if !block && !payload.is("unblock", NS) {
        return false;
    }
    let jids: Vec<String> = items(payload).collect();
    let changed: Vec<String> = {
        let mut list = session.blocked.0.lock().await;
        if block {
            jids.into_iter()
                .filter(|j| list.insert(j.clone()))
                .collect()
        } else if jids.is_empty() {
            // An empty unblock clears the whole list.
            list.drain().collect()
        } else {
            jids.into_iter().filter(|j| list.remove(j)).collect()
        }
    };
    let any = !changed.is_empty();
    for jid in changed {
        session.conversations.blocked_changed(session, &jid).await;
    }
    if any {
        crate::account::blocklist_changed(session).await;
    }
    true
}

/// Blocks or unblocks `jid`. The server's push updates the list.
pub async fn set(session: &Session, jid: &BareJid, blocked: bool) -> Result<(), IqError> {
    let item = Element::builder("item", NS)
        .attr(n("jid"), jid.as_str())
        .build();
    let request = Element::builder(if blocked { "block" } else { "unblock" }, NS)
        .append(item)
        .build();
    // Locked first: waits for the list fetched after connecting.
    let mut list = session.blocked.0.lock().await;
    session.iq(crate::pep::set(None, request)).await?;
    // Servers push to every resource, us included, but don't wait on it.
    let changed = if blocked {
        list.insert(jid.to_string())
    } else {
        list.remove(jid.as_str())
    };
    drop(list);
    if changed {
        session
            .conversations
            .blocked_changed(session, jid.as_str())
            .await;
        crate::account::blocklist_changed(session).await;
    }
    Ok(())
}
