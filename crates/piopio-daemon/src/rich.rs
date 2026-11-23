//! Messages that change other messages, and how messages refer to each
//! other: corrections (XEP-0308), retractions (XEP-0424), reactions
//! (XEP-0444) and replies (XEP-0461) with their fallbacks (XEP-0428).
//!
//! Every change is checked against who sent the original: only its author
//! can correct or retract it, and each person has one set of reactions.

use std::collections::HashMap;

use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::Message;
use zbus::zvariant::{OwnedValue, Value};

use crate::messages::emit_changed;
use crate::session::Session;
use crate::store::MessageRow;

pub const CORRECT: &str = "urn:xmpp:message-correct:0";
pub const RETRACT: &str = "urn:xmpp:message-retract:1";
pub const RETRACT_0: &str = "urn:xmpp:message-retract:0";
pub const FASTEN: &str = "urn:xmpp:fasten:0";
pub const REACTIONS: &str = "urn:xmpp:reactions:0";
pub const REPLY: &str = "urn:xmpp:reply:0";
pub const FALLBACK: &str = "urn:xmpp:fallback:0";
pub const HINTS: &str = "urn:xmpp:hints";

/// Shown by clients that don't understand retractions.
pub const RETRACT_FALLBACK: &str =
    "This person attempted to retract a previous message, but it's unsupported by your client.";

fn n(name: &'static str) -> tokio_xmpp::minidom::rxml::NcName {
    name.try_into().expect("a valid attribute name")
}

fn child<'a>(message: &'a Message, name: &str, ns: &str) -> Option<&'a Element> {
    message.payloads.iter().find(|p| p.is(name, ns))
}

/// The id of the message a retraction is about, in either version.
pub fn retraction(message: &Message) -> Option<String> {
    if let Some(r) = child(message, "retract", RETRACT) {
        return r.attr("id").map(str::to_owned);
    }
    let fasten = child(message, "apply-to", FASTEN)?;
    fasten
        .has_child("retract", RETRACT_0)
        .then(|| fasten.attr("id").map(str::to_owned))?
}

/// The target and emojis of a reactions update.
pub fn reactions(message: &Message) -> Option<(String, Vec<String>)> {
    let r = child(message, "reactions", REACTIONS)?;
    let mut emojis: Vec<String> = Vec::new();
    for e in r.children().filter(|c| c.is("reaction", REACTIONS)) {
        let t = e.text().trim().to_owned();
        if !t.is_empty() && t.chars().count() <= 16 && !emojis.contains(&t) {
            emojis.push(t);
        }
    }
    emojis.truncate(20);
    Some((r.attr("id")?.to_owned(), emojis))
}

/// The id of the message a correction replaces.
pub fn correction(message: &Message) -> Option<String> {
    child(message, "replace", CORRECT)?
        .attr("id")
        .map(str::to_owned)
}

/// A reply: the id of the message it answers and that message's author.
pub fn reply(message: &Message) -> Option<(String, String)> {
    let r = child(message, "reply", REPLY)?;
    let to = r
        .attr("to")
        .and_then(|j| j.parse::<tokio_xmpp::jid::Jid>().ok())
        .map(|j| j.to_bare().to_string())
        .unwrap_or_default();
    Some((r.attr("id")?.to_owned(), to))
}

/// Removes the part of `body` that `<fallback for=ns>` marks (such as the
/// quote a reply carries for older clients). Offsets count characters.
pub fn strip_fallback(message: &Message, ns: &str, body: &str) -> (String, String) {
    let Some(fallback) = message
        .payloads
        .iter()
        .find(|p| p.is("fallback", FALLBACK) && p.attr("for") == Some(ns))
    else {
        return (body.to_owned(), String::new());
    };
    let chars: Vec<char> = body.chars().collect();
    let mut keep = vec![true; chars.len()];
    let mut removed = String::new();
    for range in fallback.children().filter(|c| c.is("body", FALLBACK)) {
        let start = range
            .attr("start")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0usize);
        let end = range
            .attr("end")
            .and_then(|s| s.parse().ok())
            .unwrap_or(chars.len());
        if start >= end || end > chars.len() {
            continue;
        }
        for (i, k) in keep.iter_mut().enumerate().take(end).skip(start) {
            if *k {
                removed.push(chars[i]);
            }
            *k = false;
        }
    }
    let kept: String = chars
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(c, _)| c)
        .collect();
    (kept, removed)
}

/// A short preview of a message, for quotes.
pub fn preview(body: &str) -> String {
    let text: String = body
        .lines()
        .filter(|l| !l.starts_with('>'))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// A quote of a fallback ("> text") without its markers.
pub fn unquote(fallback: &str) -> String {
    preview(
        &fallback
            .lines()
            .map(|l| l.trim_start_matches('>').trim_start())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Applies a retraction, if `sender` wrote the message.
pub async fn retract(session: &Session, conversation: i64, target: MessageRow, sender: &str) {
    if target.sender != sender || target.retracted {
        tracing::info!("retraction of someone else's message ignored");
        return;
    }
    if session.store.retract_message(target.id).await.is_ok() {
        let changes = HashMap::from([
            ("retracted", Value::from(true)),
            ("body", Value::from("")),
            (
                "reactions",
                Value::from(HashMap::<String, Vec<String>>::new()),
            ),
        ]);
        emit_changed(session, conversation, target.id, changes).await;
    }
}

/// Applies someone's reactions.
pub async fn react(
    session: &Session,
    conversation: i64,
    target: MessageRow,
    sender: String,
    emojis: Vec<String>,
) {
    if target.retracted {
        return;
    }
    if let Ok(all) = session.store.set_reactions(target.id, sender, emojis).await {
        let changes = HashMap::from([("reactions", reactions_value(&all))]);
        emit_changed(session, conversation, target.id, changes).await;
    }
}

/// Applies a correction. Returns false if it doesn't apply (then the
/// message is stored as a new one, as XEP-0308 suggests).
pub async fn correct(
    session: &Session,
    conversation: i64,
    target: Option<MessageRow>,
    sender: &str,
    body: String,
) -> bool {
    let Some(target) = target else { return false };
    if target.sender != sender {
        tracing::info!("correction of someone else's message ignored");
        return true;
    }
    if target.retracted {
        return true;
    }
    if session
        .store
        .correct_message(target.id, body.clone())
        .await
        .is_ok()
    {
        let changes = HashMap::from([("body", Value::from(body)), ("edited", Value::from(true))]);
        emit_changed(session, conversation, target.id, changes).await;
    }
    true
}

pub fn reactions_value(all: &[(String, Vec<String>)]) -> Value<'static> {
    let map: HashMap<String, Vec<String>> = all.iter().cloned().collect();
    Value::from(map)
}

pub fn reactions_owned(all: &[(String, Vec<String>)]) -> OwnedValue {
    OwnedValue::try_from(reactions_value(all)).expect("no file descriptors")
}

// --- building outgoing elements ---------------------------------------------

pub fn replace(id: &str) -> Element {
    Element::builder("replace", CORRECT)
        .attr(n("id"), id)
        .build()
}

pub fn retract_elements(id: &str) -> Vec<Element> {
    vec![
        Element::builder("retract", RETRACT)
            .attr(n("id"), id)
            .build(),
        Element::builder("apply-to", FASTEN)
            .attr(n("id"), id)
            .append(Element::builder("retract", RETRACT_0).build())
            .build(),
        Element::builder("fallback", FALLBACK)
            .attr(n("for"), RETRACT)
            .build(),
        store_hint(),
    ]
}

pub fn reactions_element(id: &str, emojis: &[String]) -> Element {
    let mut e = Element::builder("reactions", REACTIONS).attr(n("id"), id);
    for emoji in emojis {
        e = e.append(
            Element::builder("reaction", REACTIONS)
                .append(emoji.as_str())
                .build(),
        );
    }
    e.build()
}

pub fn store_hint() -> Element {
    Element::builder("store", HINTS).build()
}

/// The reply element and the body with its quote for older clients.
pub fn reply_elements(id: &str, to: &str, quoted: &str, body: &str) -> (Vec<Element>, String) {
    let quote: String = quoted
        .lines()
        .map(|l| format!("> {l}\n"))
        .collect::<String>();
    let end = quote.chars().count();
    let reply = Element::builder("reply", REPLY)
        .attr(n("id"), id)
        .attr(n("to"), to)
        .build();
    let fallback = Element::builder("fallback", FALLBACK)
        .attr(n("for"), REPLY)
        .append(
            Element::builder("body", FALLBACK)
                .attr(n("start"), "0")
                .attr(n("end"), end.to_string())
                .build(),
        )
        .build();
    (vec![reply, fallback], format!("{quote}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_fallback_round_trips() {
        let (elements, body) = reply_elements("abc", "bob@example.org", "Hi\nthere", "Hello!");
        assert_eq!(body, "> Hi\n> there\nHello!");
        let mut message = Message::chat(None);
        message.payloads = elements;
        assert_eq!(
            reply(&message).unwrap(),
            ("abc".into(), "bob@example.org".into())
        );
        let (kept, removed) = strip_fallback(&message, REPLY, &body);
        assert_eq!(kept, "Hello!");
        assert_eq!(unquote(&removed), "Hi there");
    }

    #[test]
    fn fallback_offsets_count_characters() {
        let (elements, body) = reply_elements("x", "a@b", "ünïcode 🐦", "ok");
        let mut message = Message::chat(None);
        message.payloads = elements;
        assert_eq!(strip_fallback(&message, REPLY, &body).0, "ok");
    }

    #[test]
    fn bad_fallback_ranges_are_ignored() {
        let mut message = Message::chat(None);
        message.payloads = vec![
            Element::builder("fallback", FALLBACK)
                .attr(n("for"), REPLY)
                .append(
                    Element::builder("body", FALLBACK)
                        .attr(n("start"), "5")
                        .attr(n("end"), "500")
                        .build(),
                )
                .build(),
        ];
        assert_eq!(strip_fallback(&message, REPLY, "short").0, "short");
    }

    #[test]
    fn reactions_are_deduplicated_and_bounded() {
        let mut message = Message::chat(None);
        message.payloads = vec![reactions_element(
            "m1",
            &["👍".into(), "👍".into(), "a very long reaction text".into()],
        )];
        assert_eq!(
            reactions(&message).unwrap(),
            ("m1".into(), vec!["👍".into()])
        );
    }

    #[test]
    fn both_retraction_versions_are_understood() {
        let mut message = Message::chat(None);
        message.payloads = retract_elements("m2");
        assert_eq!(retraction(&message).as_deref(), Some("m2"));
        message.payloads.remove(0);
        assert_eq!(retraction(&message).as_deref(), Some("m2"));
    }
}
