//! Translating between SDP (what WebRTC stacks such as GStreamer's
//! webrtcbin speak) and Jingle RTP sessions (XEP-0167) with ICE-UDP
//! (XEP-0176), DTLS-SRTP (XEP-0320), RTP feedback (XEP-0293), header
//! extensions (XEP-0294), source attributes (XEP-0339) and BUNDLE
//! grouping (XEP-0338), the way Conversations and Dino do it.

use tokio_xmpp::minidom::Element;

use crate::pep::n;

pub const JINGLE: &str = "urn:xmpp:jingle:1";
pub const RTP: &str = "urn:xmpp:jingle:apps:rtp:1";
const RTCP_FB: &str = "urn:xmpp:jingle:apps:rtp:rtcp-fb:0";
const HDREXT: &str = "urn:xmpp:jingle:apps:rtp:rtp-hdrext:0";
const SSMA: &str = "urn:xmpp:jingle:apps:rtp:ssma:0";
pub const ICE: &str = "urn:xmpp:jingle:transports:ice-udp:1";
const DTLS: &str = "urn:xmpp:jingle:apps:dtls:0";
const GROUPING: &str = "urn:xmpp:jingle:apps:grouping:0";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Payload {
    pub id: u8,
    pub name: String,
    pub clockrate: u32,
    pub channels: u32,
    pub parameters: Vec<(String, String)>,
    pub feedback: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Candidate {
    pub component: u8,
    pub foundation: String,
    pub generation: u32,
    pub id: String,
    pub ip: String,
    pub port: u16,
    pub priority: u32,
    pub protocol: String,
    pub kind: String,
    pub rel_addr: Option<String>,
    pub rel_port: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fingerprint {
    pub hash: String,
    pub setup: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Content {
    pub name: String,
    pub creator: String,
    pub senders: String,
    pub media: String,
    pub payloads: Vec<Payload>,
    pub rtcp_mux: bool,
    pub header_extensions: Vec<(u32, String)>,
    /// (ssrc, cname)
    pub sources: Vec<(u32, String)>,
    pub ufrag: String,
    pub pwd: String,
    pub fingerprint: Option<Fingerprint>,
    pub candidates: Vec<Candidate>,
}

/// A whole session description: its contents and BUNDLE group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Description {
    pub contents: Vec<Content>,
    pub bundle: Vec<String>,
}

// --- candidates --------------------------------------------------------------

/// "candidate:…" as in SDP (without "a=").
pub fn candidate_to_sdp(c: &Candidate) -> String {
    let mut s = format!(
        "candidate:{} {} {} {} {} {} typ {}",
        c.foundation,
        c.component,
        c.protocol.to_uppercase(),
        c.priority,
        c.ip,
        c.port,
        c.kind
    );
    if let (Some(a), Some(p)) = (&c.rel_addr, c.rel_port) {
        s.push_str(&format!(" raddr {a} rport {p}"));
    }
    s.push_str(&format!(" generation {}", c.generation));
    s
}

pub fn candidate_from_sdp(line: &str) -> Option<Candidate> {
    let line = line.trim().trim_start_matches("a=");
    let rest = line.strip_prefix("candidate:")?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    if f.len() < 8 || f[6] != "typ" {
        return None;
    }
    let mut c = Candidate {
        foundation: f[0].to_owned(),
        component: f[1].parse().ok()?,
        protocol: f[2].to_lowercase(),
        priority: f[3].parse().ok()?,
        ip: f[4].to_owned(),
        port: f[5].parse().ok()?,
        kind: f[7].to_owned(),
        id: random_id(),
        ..Default::default()
    };
    let mut i = 8;
    while i + 1 < f.len() {
        match f[i] {
            "raddr" => c.rel_addr = Some(f[i + 1].to_owned()),
            "rport" => c.rel_port = f[i + 1].parse().ok(),
            "generation" => c.generation = f[i + 1].parse().unwrap_or(0),
            _ => {}
        }
        i += 2;
    }
    Some(c)
}

fn random_id() -> String {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn candidate_element(c: &Candidate) -> Element {
    let mut e = Element::builder("candidate", ICE)
        .attr(n("component"), c.component.to_string())
        .attr(n("foundation"), c.foundation.as_str())
        .attr(n("generation"), c.generation.to_string())
        .attr(n("id"), c.id.as_str())
        .attr(n("ip"), c.ip.as_str())
        .attr(n("network"), "0")
        .attr(n("port"), c.port.to_string())
        .attr(n("priority"), c.priority.to_string())
        .attr(n("protocol"), c.protocol.as_str())
        .attr(n("type"), c.kind.as_str());
    if let (Some(a), Some(p)) = (&c.rel_addr, c.rel_port) {
        e = e
            .attr(n("rel-addr"), a.as_str())
            .attr(n("rel-port"), p.to_string());
    }
    e.build()
}

pub fn parse_candidate(e: &Element) -> Option<Candidate> {
    Some(Candidate {
        component: e.attr("component")?.parse().ok()?,
        foundation: e.attr("foundation")?.to_owned(),
        generation: e
            .attr("generation")
            .and_then(|g| g.parse().ok())
            .unwrap_or(0),
        id: e.attr("id").unwrap_or("").to_owned(),
        ip: e.attr("ip")?.to_owned(),
        port: e.attr("port")?.parse().ok()?,
        priority: e.attr("priority")?.parse().ok()?,
        protocol: e.attr("protocol").unwrap_or("udp").to_lowercase(),
        kind: e.attr("type").unwrap_or("host").to_owned(),
        rel_addr: e.attr("rel-addr").map(str::to_owned),
        rel_port: e.attr("rel-port").and_then(|p| p.parse().ok()),
    })
}

// --- SDP → Jingle ------------------------------------------------------------

/// Reads an SDP offer or answer written by `author` ("initiator" or
/// "responder"), whose view its send/receive direction is from.
pub fn from_sdp(sdp: &str, author: &str) -> Description {
    let creator = author;
    let mut d = Description::default();
    let mut session_ufrag = String::new();
    let mut session_pwd = String::new();
    let mut session_fp: Option<Fingerprint> = None;
    let mut current: Option<Content> = None;
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(m) = line.strip_prefix("m=") {
            if let Some(c) = current.take() {
                d.contents.push(c);
            }
            let f: Vec<&str> = m.split_whitespace().collect();
            let mut c = Content {
                media: f.first().unwrap_or(&"audio").to_string(),
                creator: "initiator".to_owned(),
                senders: "both".into(),
                ..Default::default()
            };
            for pt in f.iter().skip(3) {
                if let Ok(id) = pt.parse() {
                    c.payloads.push(Payload {
                        id,
                        ..Default::default()
                    });
                }
            }
            current = Some(c);
            continue;
        }
        let Some(a) = line.strip_prefix("a=") else {
            continue;
        };
        let (key, value) = a.split_once(':').unwrap_or((a, ""));
        match (&mut current, key) {
            (None, "group") => {
                if let Some(mids) = value.strip_prefix("BUNDLE") {
                    d.bundle = mids.split_whitespace().map(str::to_owned).collect();
                }
            }
            (None, "ice-ufrag") => session_ufrag = value.to_owned(),
            (None, "ice-pwd") => session_pwd = value.to_owned(),
            (None, "fingerprint") => {
                let (hash, fp) = value.split_once(' ').unwrap_or(("sha-256", value));
                session_fp = Some(Fingerprint {
                    hash: hash.to_lowercase(),
                    setup: String::new(),
                    value: fp.to_owned(),
                });
            }
            (Some(c), "mid") => c.name = value.to_owned(),
            (Some(c), "ice-ufrag") => c.ufrag = value.to_owned(),
            (Some(c), "ice-pwd") => c.pwd = value.to_owned(),
            (Some(c), "rtcp-mux") => c.rtcp_mux = true,
            (Some(c), "sendrecv") => c.senders = "both".into(),
            (Some(c), "sendonly") => {
                c.senders = if creator == "initiator" {
                    "initiator"
                } else {
                    "responder"
                }
                .into()
            }
            (Some(c), "recvonly") => {
                c.senders = if creator == "initiator" {
                    "responder"
                } else {
                    "initiator"
                }
                .into()
            }
            (Some(c), "inactive") => c.senders = "none".into(),
            (Some(c), "setup") => {
                c.fingerprint.get_or_insert_with(Default::default).setup = value.to_owned()
            }
            (Some(c), "fingerprint") => {
                let (hash, fp) = value.split_once(' ').unwrap_or(("sha-256", value));
                let f = c.fingerprint.get_or_insert_with(Default::default);
                f.hash = hash.to_lowercase();
                f.value = fp.to_owned();
            }
            (Some(c), "rtpmap") => {
                let (id, rest) = value.split_once(' ').unwrap_or((value, ""));
                let mut parts = rest.split('/');
                if let Some(p) = id.parse::<u8>().ok().and_then(|id| payload(c, id)) {
                    p.name = parts.next().unwrap_or("").to_owned();
                    p.clockrate = parts.next().and_then(|r| r.parse().ok()).unwrap_or(0);
                    p.channels = parts.next().and_then(|r| r.parse().ok()).unwrap_or(1);
                }
            }
            (Some(c), "fmtp") => {
                let (id, rest) = value.split_once(' ').unwrap_or((value, ""));
                if let Some(p) = id.parse::<u8>().ok().and_then(|id| payload(c, id)) {
                    for kv in rest.split(';') {
                        if let Some((k, v)) = kv.trim().split_once('=') {
                            p.parameters.push((k.to_owned(), v.to_owned()));
                        }
                    }
                }
            }
            (Some(c), "rtcp-fb") => {
                let mut f = value.split_whitespace();
                if let Some(Ok(id)) = f.next().map(|i| i.parse::<u8>())
                    && let Some(p) = payload(c, id)
                {
                    let t = f.next().unwrap_or("").to_owned();
                    let s = f.next().unwrap_or("").to_owned();
                    p.feedback.push((t, s));
                }
            }
            (Some(c), "extmap") => {
                let mut f = value.split_whitespace();
                if let (Some(id), Some(uri)) = (f.next(), f.next())
                    && let Ok(id) = id.split('/').next().unwrap_or(id).parse()
                {
                    c.header_extensions.push((id, uri.to_owned()));
                }
            }
            (Some(c), "ssrc") => {
                let (ssrc, attr) = value.split_once(' ').unwrap_or((value, ""));
                if let (Ok(ssrc), Some(cname)) = (ssrc.parse::<u32>(), attr.strip_prefix("cname:"))
                    && !c.sources.iter().any(|(s, _)| *s == ssrc)
                {
                    c.sources.push((ssrc, cname.to_owned()));
                }
            }
            (Some(c), "candidate") => {
                if let Some(cand) = candidate_from_sdp(a) {
                    c.candidates.push(cand);
                }
            }
            _ => {}
        }
    }
    if let Some(c) = current.take() {
        d.contents.push(c);
    }
    for c in &mut d.contents {
        if c.ufrag.is_empty() {
            c.ufrag = session_ufrag.clone();
        }
        if c.pwd.is_empty() {
            c.pwd = session_pwd.clone();
        }
        if c.fingerprint.as_ref().is_none_or(|f| f.value.is_empty())
            && let Some(fp) = &session_fp
        {
            let setup = c
                .fingerprint
                .as_ref()
                .map(|f| f.setup.clone())
                .unwrap_or_default();
            c.fingerprint = Some(Fingerprint {
                setup,
                ..fp.clone()
            });
        }
        if c.name.is_empty() {
            c.name = c.media.clone();
        }
        // Payload types without a name (static ones) are dropped.
        c.payloads.retain(|p| !p.name.is_empty());
    }
    d
}

fn payload(c: &mut Content, id: u8) -> Option<&mut Payload> {
    c.payloads.iter_mut().find(|p| p.id == id)
}

// --- Jingle → SDP ------------------------------------------------------------

/// Writes an SDP offer or answer for `reader` ("initiator" or "responder"),
/// whose view the send/receive direction is written from.
pub fn to_sdp(d: &Description, reader: &str) -> String {
    let local_creator = reader;
    let mut s = String::new();
    s.push_str("v=0\r\n");
    s.push_str(&format!(
        "o=- {} 2 IN IP4 0.0.0.0\r\n",
        1_000_000 + d.contents.len()
    ));
    s.push_str("s=-\r\nt=0 0\r\n");
    if !d.bundle.is_empty() {
        s.push_str(&format!("a=group:BUNDLE {}\r\n", d.bundle.join(" ")));
    }
    for c in &d.contents {
        let ids: Vec<String> = c.payloads.iter().map(|p| p.id.to_string()).collect();
        s.push_str(&format!(
            "m={} 9 UDP/TLS/RTP/SAVPF {}\r\n",
            c.media,
            ids.join(" ")
        ));
        s.push_str("c=IN IP4 0.0.0.0\r\n");
        s.push_str(&format!("a=mid:{}\r\n", c.name));
        s.push_str(&format!(
            "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\n",
            c.ufrag, c.pwd
        ));
        if let Some(fp) = &c.fingerprint {
            s.push_str(&format!("a=fingerprint:{} {}\r\n", fp.hash, fp.value));
            let setup = if fp.setup.is_empty() {
                "actpass"
            } else {
                &fp.setup
            };
            s.push_str(&format!("a=setup:{setup}\r\n"));
        }
        let direction = match (c.senders.as_str(), local_creator == "initiator") {
            ("none", _) => "inactive",
            ("initiator", true) | ("responder", false) => "sendonly",
            ("initiator", false) | ("responder", true) => "recvonly",
            _ => "sendrecv",
        };
        s.push_str(&format!("a={direction}\r\n"));
        if c.rtcp_mux {
            s.push_str("a=rtcp-mux\r\n");
        }
        for (id, uri) in &c.header_extensions {
            s.push_str(&format!("a=extmap:{id} {uri}\r\n"));
        }
        for p in &c.payloads {
            if p.channels > 1 {
                s.push_str(&format!(
                    "a=rtpmap:{} {}/{}/{}\r\n",
                    p.id, p.name, p.clockrate, p.channels
                ));
            } else {
                s.push_str(&format!("a=rtpmap:{} {}/{}\r\n", p.id, p.name, p.clockrate));
            }
            if !p.parameters.is_empty() {
                let params: Vec<String> = p
                    .parameters
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                s.push_str(&format!("a=fmtp:{} {}\r\n", p.id, params.join(";")));
            }
            for (t, sub) in &p.feedback {
                if sub.is_empty() {
                    s.push_str(&format!("a=rtcp-fb:{} {t}\r\n", p.id));
                } else {
                    s.push_str(&format!("a=rtcp-fb:{} {t} {sub}\r\n", p.id));
                }
            }
        }
        for (ssrc, cname) in &c.sources {
            s.push_str(&format!("a=ssrc:{ssrc} cname:{cname}\r\n"));
        }
        for cand in &c.candidates {
            s.push_str(&format!("a={}\r\n", candidate_to_sdp(cand)));
        }
    }
    s
}

// --- Jingle XML ----------------------------------------------------------------

pub fn content_element(c: &Content, with_candidates: bool) -> Element {
    let mut desc = Element::builder("description", RTP).attr(n("media"), c.media.as_str());
    for p in &c.payloads {
        let mut pe = Element::builder("payload-type", RTP)
            .attr(n("id"), p.id.to_string())
            .attr(n("name"), p.name.as_str())
            .attr(n("clockrate"), p.clockrate.to_string());
        if p.channels > 1 {
            pe = pe.attr(n("channels"), p.channels.to_string());
        }
        for (k, v) in &p.parameters {
            pe = pe.append(
                Element::builder("parameter", RTP)
                    .attr(n("name"), k.as_str())
                    .attr(n("value"), v.as_str())
                    .build(),
            );
        }
        for (t, sub) in &p.feedback {
            let mut fb = Element::builder("rtcp-fb", RTCP_FB).attr(n("type"), t.as_str());
            if !sub.is_empty() {
                fb = fb.attr(n("subtype"), sub.as_str());
            }
            pe = pe.append(fb.build());
        }
        desc = desc.append(pe.build());
    }
    for (id, uri) in &c.header_extensions {
        desc = desc.append(
            Element::builder("rtp-hdrext", HDREXT)
                .attr(n("id"), id.to_string())
                .attr(n("uri"), uri.as_str())
                .build(),
        );
    }
    for (ssrc, cname) in &c.sources {
        desc = desc.append(
            Element::builder("source", SSMA)
                .attr(n("ssrc"), ssrc.to_string())
                .append(
                    Element::builder("parameter", SSMA)
                        .attr(n("name"), "cname")
                        .attr(n("value"), cname.as_str())
                        .build(),
                )
                .build(),
        );
    }
    if c.rtcp_mux {
        desc = desc.append(Element::builder("rtcp-mux", RTP).build());
    }
    Element::builder("content", JINGLE)
        .attr(n("creator"), c.creator.as_str())
        .attr(n("name"), c.name.as_str())
        .attr(n("senders"), c.senders.as_str())
        .append(desc.build())
        .append(transport_element(c, with_candidates))
        .build()
}

pub fn transport_element(c: &Content, with_candidates: bool) -> Element {
    let mut t = Element::builder("transport", ICE)
        .attr(n("ufrag"), c.ufrag.as_str())
        .attr(n("pwd"), c.pwd.as_str());
    if let Some(fp) = &c.fingerprint {
        t = t.append(
            Element::builder("fingerprint", DTLS)
                .attr(n("hash"), fp.hash.as_str())
                .attr(
                    n("setup"),
                    if fp.setup.is_empty() {
                        "actpass"
                    } else {
                        &fp.setup
                    },
                )
                .append(fp.value.as_str())
                .build(),
        );
    }
    if with_candidates {
        for cand in &c.candidates {
            t = t.append(candidate_element(cand));
        }
    }
    t.build()
}

pub fn group_element(d: &Description) -> Option<Element> {
    if d.bundle.is_empty() {
        return None;
    }
    let mut g = Element::builder("group", GROUPING).attr(n("semantics"), "BUNDLE");
    for name in &d.bundle {
        g = g.append(
            Element::builder("content", GROUPING)
                .attr(n("name"), name.as_str())
                .build(),
        );
    }
    Some(g.build())
}

pub fn parse_content(e: &Element) -> Option<Content> {
    let desc = e.get_child("description", RTP)?;
    let mut c = Content {
        name: e.attr("name")?.to_owned(),
        creator: e.attr("creator").unwrap_or("initiator").to_owned(),
        senders: e.attr("senders").unwrap_or("both").to_owned(),
        media: desc.attr("media")?.to_owned(),
        rtcp_mux: desc.has_child("rtcp-mux", RTP),
        ..Default::default()
    };
    for p in desc.children().filter(|x| x.is("payload-type", RTP)) {
        c.payloads.push(Payload {
            id: p.attr("id")?.parse().ok()?,
            name: p.attr("name").unwrap_or("").to_owned(),
            clockrate: p
                .attr("clockrate")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            channels: p.attr("channels").and_then(|v| v.parse().ok()).unwrap_or(1),
            parameters: p
                .children()
                .filter(|x| x.is("parameter", RTP))
                .filter_map(|x| Some((x.attr("name")?.to_owned(), x.attr("value")?.to_owned())))
                .collect(),
            feedback: p
                .children()
                .filter(|x| x.is("rtcp-fb", RTCP_FB))
                .filter_map(|x| {
                    Some((
                        x.attr("type")?.to_owned(),
                        x.attr("subtype").unwrap_or("").to_owned(),
                    ))
                })
                .collect(),
        });
    }
    for h in desc.children().filter(|x| x.is("rtp-hdrext", HDREXT)) {
        if let (Some(id), Some(uri)) = (h.attr("id").and_then(|i| i.parse().ok()), h.attr("uri")) {
            c.header_extensions.push((id, uri.to_owned()));
        }
    }
    for s in desc.children().filter(|x| x.is("source", SSMA)) {
        if let Some(ssrc) = s.attr("ssrc").and_then(|v| v.parse().ok()) {
            let cname = s
                .children()
                .find(|p| p.attr("name") == Some("cname"))
                .and_then(|p| p.attr("value"))
                .unwrap_or("")
                .to_owned();
            c.sources.push((ssrc, cname));
        }
    }
    if let Some(t) = e.get_child("transport", ICE) {
        apply_transport(&mut c, t);
    }
    Some(c)
}

/// Reads an ICE-UDP transport into a content (ufrag, pwd, fingerprint,
/// candidates).
pub fn apply_transport(c: &mut Content, t: &Element) {
    if let Some(u) = t.attr("ufrag") {
        c.ufrag = u.to_owned();
    }
    if let Some(p) = t.attr("pwd") {
        c.pwd = p.to_owned();
    }
    if let Some(fp) = t.get_child("fingerprint", DTLS) {
        c.fingerprint = Some(Fingerprint {
            hash: fp.attr("hash").unwrap_or("sha-256").to_owned(),
            setup: fp.attr("setup").unwrap_or("actpass").to_owned(),
            value: fp.text().trim().to_owned(),
        });
    }
    for cand in t.children().filter(|x| x.is("candidate", ICE)) {
        if let Some(cand) = parse_candidate(cand) {
            c.candidates.push(cand);
        }
    }
}

/// Reads the contents and group of a `<jingle/>` element.
pub fn parse_description(jingle: &Element) -> Description {
    Description {
        contents: jingle
            .children()
            .filter(|c| c.is("content", JINGLE))
            .filter_map(parse_content)
            .collect(),
        bundle: jingle
            .get_child("group", GROUPING)
            .map(|g| {
                g.children()
                    .filter_map(|c| c.attr("name").map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 7614219274584779017 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
a=ice-options:trickle\r\na=group:BUNDLE audio0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=setup:actpass\r\n\
a=ice-ufrag:Zx4f\r\na=ice-pwd:9Qq0kPS7x3m8Lm2oEoVc8B\r\na=rtcp-mux\r\na=rtcp-rsize\r\na=sendrecv\r\n\
a=rtpmap:96 OPUS/48000/2\r\na=rtcp-fb:96 nack pli\r\na=fmtp:96 useinbandfec=1;minptime=10\r\n\
a=ssrc:1234 msid:user1@host-abcd webrtctransceiver0\r\na=ssrc:1234 cname:user1@host-abcd\r\n\
a=mid:audio0\r\na=fingerprint:sha-256 AB:CD:EF:01\r\n\
a=candidate:1 1 UDP 2015363327 192.168.1.20 51234 typ host\r\n\
a=candidate:2 1 UDP 1679819007 203.0.113.5 40000 typ srflx raddr 192.168.1.20 rport 51234\r\n";

    #[test]
    fn sdp_to_jingle_to_sdp() {
        let d = from_sdp(OFFER, "initiator");
        assert_eq!(d.bundle, vec!["audio0"]);
        let c = &d.contents[0];
        assert_eq!(c.name, "audio0");
        assert_eq!(c.media, "audio");
        assert_eq!(c.payloads[0].name, "OPUS");
        assert_eq!(c.payloads[0].clockrate, 48000);
        assert_eq!(c.payloads[0].channels, 2);
        assert_eq!(c.payloads[0].parameters.len(), 2);
        assert_eq!(c.payloads[0].feedback, vec![("nack".into(), "pli".into())]);
        assert_eq!(c.ufrag, "Zx4f");
        assert_eq!(c.fingerprint.as_ref().unwrap().setup, "actpass");
        assert_eq!(c.sources, vec![(1234, "user1@host-abcd".into())]);
        assert_eq!(c.candidates.len(), 2);
        assert_eq!(c.candidates[1].rel_port, Some(51234));
        assert!(c.rtcp_mux);

        // Through XML and back.
        let mut jingle = Element::builder("jingle", JINGLE).build();
        jingle.append_child(content_element(c, true));
        jingle.append_child(group_element(&d).unwrap());
        let parsed = parse_description(&jingle);
        assert_eq!(parsed, d);

        let sdp = to_sdp(&parsed, "responder");
        let again = from_sdp(&sdp, "initiator");
        let (mut a, mut b) = (again.contents[0].clone(), d.contents[0].clone());
        // Candidate ids are made up on each reading.
        a.candidates.iter_mut().for_each(|c| c.id.clear());
        b.candidates.iter_mut().for_each(|c| c.id.clear());
        assert_eq!(a, b);
    }

    #[test]
    fn candidates_round_trip() {
        let line = "candidate:3 1 UDP 41885439 198.51.100.7 3478 typ relay raddr 203.0.113.5 rport 40000 generation 0";
        let c = candidate_from_sdp(line).unwrap();
        assert_eq!(c.kind, "relay");
        assert_eq!(candidate_to_sdp(&c), line);
        assert_eq!(parse_candidate(&candidate_element(&c)).unwrap(), c);
        assert!(candidate_from_sdp("candidate:x").is_none());
    }

    #[test]
    fn senders_follow_direction() {
        let answer = OFFER.replace("a=sendrecv", "a=recvonly");
        let d = from_sdp(&answer, "responder");
        assert_eq!(d.contents[0].senders, "initiator");
        assert!(to_sdp(&d, "initiator").contains("a=sendonly"));
        assert!(to_sdp(&d, "responder").contains("a=recvonly"));
    }
}
