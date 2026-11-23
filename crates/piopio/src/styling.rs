//! Message styling (XEP-0393) and links, turned into Pango markup.
//!
//! Spans: *strong*, _emphasis_, ~strike~ and `code`, each within one line,
//! opened before a non-space and closed after one. Blocks: ``` fences for
//! preformatted text and "> " for quotes. Links (http, https, xmpp, mailto)
//! become clickable. Everything else is escaped.

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const SCHEMES: &[&str] = &["https://", "http://", "xmpp:", "mailto:"];

/// Escapes text and makes its links clickable.
fn linkify(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    'outer: while !rest.is_empty() {
        // Find the earliest link start that begins a word.
        let mut best: Option<usize> = None;
        for scheme in SCHEMES {
            let mut from = 0;
            while let Some(i) = rest[from..].find(scheme) {
                let at = from + i;
                let starts_word = rest[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| c.is_whitespace() || "([<\"'".contains(c));
                if starts_word {
                    best = Some(best.map_or(at, |b: usize| b.min(at)));
                    break;
                }
                from = at + scheme.len();
            }
        }
        let Some(start) = best else {
            out.push_str(&escape(rest));
            break 'outer;
        };
        out.push_str(&escape(&rest[..start]));
        let tail = &rest[start..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"')
            .unwrap_or(tail.len());
        // Trailing punctuation belongs to the sentence, not the link.
        while end > 0 {
            let last = tail[..end].chars().next_back().unwrap_or(' ');
            if ".,;:!?'".contains(last) || (last == ')' && !tail[..end].contains('(')) {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        let url = &tail[..end];
        if SCHEMES
            .iter()
            .any(|s| url.len() > s.len() && url.starts_with(s))
        {
            out.push_str(&format!("<a href=\"{}\">{}</a>", escape(url), escape(url)));
        } else {
            out.push_str(&escape(url));
        }
        rest = &tail[end..];
        if end == 0 {
            // Not a link after all: move past one character.
            let c = rest.chars().next().unwrap();
            out.push_str(&escape(&c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// One line's spans, as markup.
fn spans(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let tag = match c {
            '*' => Some(("<b>", "</b>")),
            '_' => Some(("<i>", "</i>")),
            '~' => Some(("<s>", "</s>")),
            '`' => Some(("<tt>", "</tt>")),
            _ => None,
        };
        let opens = tag.is_some()
            && (i == 0 || chars[i - 1].is_whitespace() || "*_~`(".contains(chars[i - 1]))
            && chars
                .get(i + 1)
                .is_some_and(|n| !n.is_whitespace() && *n != c);
        if let (true, Some((open, close))) = (opens, tag)
            && let Some(len) = chars[i + 1..]
                .iter()
                .enumerate()
                .position(|(k, &x)| x == c && !chars[i + k].is_whitespace())
        {
            let inner: String = chars[i + 1..i + 1 + len].iter().collect();
            out.push_str(&linkify(&plain));
            plain.clear();
            let inner = if c == '`' {
                escape(&inner)
            } else {
                spans(&inner)
            };
            out.push_str(&format!(
                "<span alpha=\"60%\">{}</span>{open}{inner}{close}<span alpha=\"60%\">{}</span>",
                escape(&c.to_string()),
                escape(&c.to_string())
            ));
            i += len + 2;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    out.push_str(&linkify(&plain));
    out
}

/// A message body as Pango markup.
pub fn markup(body: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut lines = body.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("```") {
            // Preformatted until a closing fence (or the end).
            let mut block = vec![escape(line)];
            for next in lines.by_ref() {
                let closing = next == "```";
                block.push(escape(next));
                if closing {
                    break;
                }
            }
            out.push(format!("<tt>{}</tt>", block.join("\n")));
        } else if let Some(quoted) = line.strip_prefix('>') {
            let quoted = quoted.strip_prefix(' ').unwrap_or(quoted);
            out.push(format!("<span alpha=\"70%\">▎ {}</span>", spans(quoted)));
        } else {
            out.push(spans(line));
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_styled() {
        assert!(markup("a *bold* word").contains("<b>bold</b>"));
        assert!(markup("_it_").contains("<i>it</i>"));
        assert!(markup("~gone~").contains("<s>gone</s>"));
        assert!(markup("`x < y`").contains("<tt>x &lt; y</tt>"));
    }

    #[test]
    fn not_spans() {
        assert_eq!(markup("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(markup("snake_case_name"), "snake_case_name");
        assert_eq!(markup("**"), "**");
        assert_eq!(markup("* not *"), "* not *");
    }

    #[test]
    fn escaping_and_links() {
        assert_eq!(markup("<b>&"), "&lt;b&gt;&amp;");
        let m = markup("see https://dunevent.net/a?b=1&c=2.");
        assert!(
            m.contains("<a href=\"https://dunevent.net/a?b=1&amp;c=2\">"),
            "{m}"
        );
        assert!(m.ends_with("</a>."));
        assert!(markup("join xmpp:room@conf.example.org?join").contains("<a href=\"xmpp:"));
        assert_eq!(markup("nothttps://x"), "nothttps://x");
    }

    #[test]
    fn blocks() {
        let m = markup("> quoted *text*\nplain");
        assert!(m.starts_with("<span alpha=\"70%\">▎ quoted"));
        assert!(m.contains("<b>text</b>"));
        let m = markup("```\nlet *x* = 1;\n```\nafter");
        assert!(m.contains("<tt>```\nlet *x* = 1;\n```</tt>"));
        assert!(m.ends_with("after"));
    }

    #[test]
    fn markup_always_parses() {
        for s in [
            "*a _b* c_",
            "`*`*",
            "> > >",
            "~~~",
            "*\u{1F426}*",
            "http://",
            "a & *b < c*",
        ] {
            let m = markup(s);
            assert!(
                adw::gtk::pango::parse_markup(&m, '\0').is_ok(),
                "bad markup for {s:?}: {m}"
            );
        }
    }
}
