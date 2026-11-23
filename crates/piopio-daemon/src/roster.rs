//! Contacts (RFC 6121 roster), their presence, and subscription requests.
//!
//! The roster lives in the database, so it is available offline and can be
//! fetched with roster versioning. Presence is only kept in memory.

use std::collections::{BTreeSet, HashMap};

use tokio::sync::Mutex;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::presence::{Presence, Show, Type as PresenceType};
use tokio_xmpp::parsers::roster::{Ask, Group, Item, Roster, Subscription};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

use piopio_dbus::ROSTER_API_VERSION;

use crate::access;
use crate::session::{Session, Shared};
use crate::store::ContactRow;

/// Available resources of one contact: resource → (show, status).
type Resources = HashMap<String, (String, String)>;

/// What the account knows about its contacts beyond the stored roster.
#[derive(Default)]
pub struct RosterState {
    /// Available resources per contact: resource → (show, status).
    presence: Mutex<HashMap<BareJid, Resources>>,
    /// Contacts who asked to see our presence and are waiting for an answer.
    requests: Mutex<BTreeSet<BareJid>>,
}

/// Fetches the roster after login, using the stored version when there is one.
pub async fn fetch(shared: &Shared) {
    let version = match shared.store.accounts().await {
        Ok(rows) => rows
            .into_iter()
            .find(|a| a.id == shared.account_id)
            .and_then(|a| a.roster_ver),
        Err(e) => {
            tracing::error!("reading the account: {e:#}");
            None
        }
    };
    let query = Roster {
        ver: Some(version.unwrap_or_default()),
        items: vec![],
    };
    match shared.iq(Iq::from_get("", query)).await {
        // A versioned server says "nothing changed" with an empty result;
        // any changes then arrive as pushes.
        Ok(None) => {}
        Ok(Some(payload)) => match Roster::try_from(payload) {
            Ok(roster) => {
                let old = shared
                    .store
                    .contacts(shared.account_id)
                    .await
                    .unwrap_or_default();
                let new: Vec<ContactRow> = roster.items.into_iter().map(row).collect();
                if let Err(e) = shared
                    .store
                    .replace_roster(shared.account_id, roster.ver, new.clone())
                    .await
                {
                    tracing::error!("storing the roster: {e:#}");
                }
                for gone in old.iter().filter(|o| !new.iter().any(|n| n.jid == o.jid)) {
                    emit_removed(shared, &gone.jid).await;
                }
                for contact in &new {
                    if !old.contains(contact) {
                        emit_contact(shared, contact).await;
                    }
                }
            }
            Err(e) => tracing::warn!("unreadable roster: {e}"),
        },
        Err(e) => tracing::warn!("fetching the roster failed: {e}"),
    }
}

/// Applies a roster push (already checked to come from our own account).
pub async fn push(shared: &Shared, payload: Element) {
    let Ok(roster) = Roster::try_from(payload) else {
        return;
    };
    for item in roster.items {
        let jid = item.jid.to_string();
        if item.subscription == Subscription::Remove {
            let _ = shared
                .store
                .apply_roster_push(shared.account_id, roster.ver.clone(), jid.clone(), None)
                .await;
            emit_removed(shared, &jid).await;
        } else {
            let contact = row(item);
            let _ = shared
                .store
                .apply_roster_push(
                    shared.account_id,
                    roster.ver.clone(),
                    jid,
                    Some(contact.clone()),
                )
                .await;
            emit_contact(shared, &contact).await;
        }
    }
}

/// Handles an incoming presence stanza.
pub async fn presence(shared: &Shared, presence: Presence) {
    let Some(from) = presence.from.clone() else {
        return;
    };
    let bare = from.to_bare();
    match presence.type_ {
        PresenceType::Subscribe => {
            shared.roster.requests.lock().await.insert(bare.clone());
            let emitter = emitter(shared);
            let _ = RosterObject::subscription_request(&emitter, bare.as_str()).await;
        }
        PresenceType::None | PresenceType::Unavailable => {
            if bare == shared.jid {
                return; // our own other devices
            }
            let resource = from.resource().map(|r| r.to_string()).unwrap_or_default();
            let (show, status) = {
                let mut all = shared.roster.presence.lock().await;
                let resources = all.entry(bare.clone()).or_default();
                if presence.type_ == PresenceType::Unavailable {
                    resources.remove(&resource);
                } else {
                    let show = match presence.show {
                        Some(Show::Away) => "away",
                        Some(Show::Chat) => "chat",
                        Some(Show::Dnd) => "dnd",
                        Some(Show::Xa) => "xa",
                        None => "available",
                    };
                    let status = presence
                        .statuses
                        .values()
                        .next()
                        .cloned()
                        .unwrap_or_default();
                    resources.insert(resource, (show.to_owned(), status));
                }
                best(resources)
            };
            let changes = HashMap::from([
                ("presence", Value::from(show)),
                ("status", Value::from(status)),
            ]);
            let emitter = emitter(shared);
            let _ = RosterObject::contact_changed(&emitter, bare.as_str(), changes).await;
        }
        _ => {}
    }
}

/// The most available resource wins: chat > available > away > dnd > xa.
fn best(resources: &Resources) -> (String, String) {
    let rank = |show: &str| match show {
        "chat" => 5,
        "available" => 4,
        "away" => 3,
        "dnd" => 2,
        "xa" => 1,
        _ => 0,
    };
    resources
        .values()
        .max_by_key(|(show, _)| rank(show))
        .cloned()
        .unwrap_or_else(|| ("offline".into(), String::new()))
}

fn row(item: Item) -> ContactRow {
    ContactRow {
        jid: item.jid.to_string(),
        name: item.name.unwrap_or_default(),
        subscription: match item.subscription {
            Subscription::None => "none",
            Subscription::From => "from",
            Subscription::To => "to",
            Subscription::Both => "both",
            Subscription::Remove => "remove",
        }
        .into(),
        ask: item.ask == Ask::Subscribe,
        groups: item.groups.into_iter().map(|g| g.0).collect(),
    }
}

fn emitter(shared: &Shared) -> SignalEmitter<'static> {
    SignalEmitter::new(&shared.bus, shared.path.clone()).expect("valid path")
}

async fn emit_contact(shared: &Shared, c: &ContactRow) {
    let changes = HashMap::from([
        ("name", Value::from(c.name.clone())),
        ("groups", Value::from(c.groups.clone())),
        ("subscription", Value::from(c.subscription.clone())),
        ("ask", Value::from(c.ask)),
    ]);
    let _ = RosterObject::contact_changed(&emitter(shared), &c.jid, changes).await;
    shared
        .conversations
        .contact_renamed(shared, &c.jid, &c.name)
        .await;
}

/// Tells the UI about changes to one contact.
pub async fn emit_changes(shared: &Shared, jid: &str, changes: HashMap<&str, Value<'_>>) {
    let _ = RosterObject::contact_changed(&emitter(shared), jid, changes).await;
}

async fn emit_removed(shared: &Shared, jid: &str) {
    let changes = HashMap::from([("removed", Value::from(true))]);
    let _ = RosterObject::contact_changed(&emitter(shared), jid, changes).await;
}

/// The display name for `jid`: its roster name, or the JID itself.
pub async fn display_name(shared: &Shared, jid: &str) -> String {
    shared
        .store
        .contacts(shared.account_id)
        .await
        .ok()
        .and_then(|cs| cs.into_iter().find(|c| c.jid == jid))
        .map(|c| c.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| jid.to_owned())
}

fn parse_jid(jid: &str) -> zbus::fdo::Result<BareJid> {
    jid.parse()
        .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("not a valid JID: {e}")))
}

/// `net.dunevent.Piopio.Roster`, on each account object.
pub struct RosterObject {
    pub session: Session,
}

impl RosterObject {
    async fn set_item(&self, item: Item) -> zbus::fdo::Result<()> {
        let query = Roster {
            ver: None,
            items: vec![item],
        };
        self.session
            .iq(Iq::from_set("", query))
            .await
            .map(drop)
            .map_err(|e| zbus::fdo::Error::Failed(format!("the server refused: {e}")))
    }

    async fn presence_to(&self, jid: BareJid, presence: Presence) {
        self.session.send(presence.with_to(Jid::from(jid))).await;
    }
}

#[zbus::interface(name = "net.dunevent.Piopio.Roster")]
impl RosterObject {
    /// Every contact: jid, name, groups, subscription, ask, presence, status,
    /// and pending-in (they asked to see our presence).
    async fn get_contacts(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        access::check(&header)?;
        let shared = &self.session;
        let contacts = shared
            .store
            .contacts(shared.account_id)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let presence = shared.roster.presence.lock().await;
        let requests = shared.roster.requests.lock().await;
        let mut out = Vec::new();
        for c in contacts {
            let bare: Option<BareJid> = c.jid.parse().ok();
            let (show, status) = bare
                .as_ref()
                .and_then(|b| presence.get(b))
                .map(best)
                .unwrap_or_else(|| ("offline".into(), String::new()));
            let pending = bare.as_ref().is_some_and(|b| requests.contains(b));
            let mut map = HashMap::new();
            map.insert(
                "avatar".into(),
                OwnedValue::from(zbus::zvariant::Str::from(
                    crate::avatars::path_of(shared, &c.jid).await,
                )),
            );
            map.insert(
                "jid".into(),
                OwnedValue::from(zbus::zvariant::Str::from(c.jid)),
            );
            map.insert(
                "name".into(),
                OwnedValue::from(zbus::zvariant::Str::from(c.name)),
            );
            map.insert(
                "groups".into(),
                Value::from(c.groups).try_into().expect("strings convert"),
            );
            map.insert(
                "subscription".into(),
                OwnedValue::from(zbus::zvariant::Str::from(c.subscription)),
            );
            map.insert("ask".into(), OwnedValue::from(c.ask));
            map.insert(
                "presence".into(),
                OwnedValue::from(zbus::zvariant::Str::from(show)),
            );
            map.insert(
                "status".into(),
                OwnedValue::from(zbus::zvariant::Str::from(status)),
            );
            map.insert("pending-in".into(), OwnedValue::from(pending));
            out.push(map);
        }
        // People who asked to see our presence but aren't contacts yet.
        for jid in requests.iter() {
            if out
                .iter()
                .any(|m| m.get("jid").and_then(|v| <&str>::try_from(v).ok()) == Some(jid.as_str()))
            {
                continue;
            }
            let mut map = HashMap::new();
            map.insert(
                "jid".into(),
                OwnedValue::from(zbus::zvariant::Str::from(jid.to_string())),
            );
            map.insert(
                "name".into(),
                OwnedValue::from(zbus::zvariant::Str::from("")),
            );
            map.insert(
                "groups".into(),
                Value::from(Vec::<String>::new())
                    .try_into()
                    .expect("strings convert"),
            );
            map.insert(
                "subscription".into(),
                OwnedValue::from(zbus::zvariant::Str::from("none")),
            );
            map.insert("ask".into(), OwnedValue::from(false));
            map.insert(
                "presence".into(),
                OwnedValue::from(zbus::zvariant::Str::from("offline")),
            );
            map.insert(
                "status".into(),
                OwnedValue::from(zbus::zvariant::Str::from("")),
            );
            map.insert("pending-in".into(), OwnedValue::from(true));
            out.push(map);
        }
        Ok(out)
    }

    /// Adds a contact and asks to see their presence.
    async fn add_contact(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
        name: &str,
        groups: Vec<String>,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let jid = parse_jid(jid)?;
        self.set_item(Item {
            jid: jid.clone(),
            name: (!name.is_empty()).then(|| name.to_owned()),
            subscription: Subscription::None,
            ask: Ask::None,
            groups: groups.into_iter().map(Group).collect(),
            approved: None,
        })
        .await?;
        self.presence_to(jid, Presence::subscribe()).await;
        Ok(())
    }

    /// Removes a contact; the server cancels subscriptions both ways.
    async fn remove_contact(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        self.set_item(Item {
            jid: parse_jid(jid)?,
            name: None,
            subscription: Subscription::Remove,
            ask: Ask::None,
            groups: vec![],
            approved: None,
        })
        .await
    }

    /// Changes a contact's name, keeping their groups.
    async fn rename_contact(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
        name: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let bare = parse_jid(jid)?;
        let groups = self
            .session
            .store
            .contacts(self.session.account_id)
            .await
            .ok()
            .and_then(|cs| cs.into_iter().find(|c| c.jid == bare.as_str()))
            .map(|c| c.groups)
            .unwrap_or_default();
        self.set_item(Item {
            jid: bare,
            name: (!name.is_empty()).then(|| name.to_owned()),
            subscription: Subscription::None,
            ask: Ask::None,
            groups: groups.into_iter().map(Group).collect(),
            approved: None,
        })
        .await
    }

    /// Lets `jid` see our presence, and asks to see theirs if we don't yet.
    async fn approve_subscription(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let bare = parse_jid(jid)?;
        self.session.roster.requests.lock().await.remove(&bare);
        self.presence_to(bare.clone(), Presence::subscribed()).await;
        let theirs = self
            .session
            .store
            .contacts(self.session.account_id)
            .await
            .ok()
            .and_then(|cs| cs.into_iter().find(|c| c.jid == bare.as_str()));
        let see_them = theirs.is_some_and(|c| c.subscription == "to" || c.subscription == "both");
        if !see_them {
            self.presence_to(bare, Presence::subscribe()).await;
        }
        Ok(())
    }

    /// Refuses to share our presence with `jid`.
    async fn deny_subscription(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        jid: &str,
    ) -> zbus::fdo::Result<()> {
        access::check(&header)?;
        let bare = parse_jid(jid)?;
        self.session.roster.requests.lock().await.remove(&bare);
        self.presence_to(bare, Presence::new(PresenceType::Unsubscribed))
            .await;
        Ok(())
    }

    /// Presence, name, groups or subscription of a contact changed. A
    /// `removed` key means the contact is gone.
    #[zbus(signal)]
    async fn contact_changed(
        emitter: &SignalEmitter<'_>,
        jid: &str,
        changes: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    /// Someone asked to see our presence.
    #[zbus(signal)]
    async fn subscription_request(emitter: &SignalEmitter<'_>, jid: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn api_version(&self) -> u32 {
        ROSTER_API_VERSION
    }
}
