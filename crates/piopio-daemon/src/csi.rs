//! Client State Indication (XEP-0352): telling the server whether anyone is
//! looking, so it can hold back presence floods and other unurgent traffic
//! while no window is open (which saves battery on phones).
//!
//! CSI is a nonza, and tokio-xmpp 6's `StanzaStream` only sends stanzas, so
//! the daemon writes it itself: every connection's plaintext (above TLS) goes
//! through [`CsiIo`], which follows the XML being written and slips
//! `<active/>` or `<inactive/>` in between two top-level elements. Nothing is
//! written until the stream is bound and the server has offered CSI, and the
//! session sends a ping whenever the state changes so an idle connection
//! carries it at once.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub const NS: &str = "urn:xmpp:csi:0";

const UNKNOWN: u8 = 0;
const ACTIVE: u8 = 1;
const INACTIVE: u8 = 2;

/// An account's CSI state, shared by its session and its connections.
#[derive(Debug, Default)]
pub struct Csi {
    /// The server offered CSI on the last bound stream.
    supported: AtomicBool,
    /// Whether a window is open.
    want_active: AtomicBool,
    /// What the current connection last told the server.
    sent: AtomicU8,
    /// The newest connection.
    newest: AtomicU64,
    /// The connection that is bound and may carry CSI.
    armed: AtomicU64,
}

impl Csi {
    pub fn new(active: bool) -> Arc<Self> {
        let csi = Self::default();
        csi.want_active.store(active, Ordering::SeqCst);
        Arc::new(csi)
    }

    /// The stream was bound (or resumed) on the newest connection.
    pub fn bound(&self, supported: Option<bool>) {
        if let Some(supported) = supported {
            self.supported.store(supported, Ordering::SeqCst);
        }
        self.armed
            .store(self.newest.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Whether a window is open. True when something needs to be written.
    pub fn set_active(&self, active: bool) -> bool {
        self.want_active.store(active, Ordering::SeqCst);
        self.pending()
    }

    /// The server hasn't been told the current state yet.
    pub fn pending(&self) -> bool {
        self.supported.load(Ordering::SeqCst) && self.sent.load(Ordering::SeqCst) != self.wanted()
    }

    fn wanted(&self) -> u8 {
        if self.want_active.load(Ordering::SeqCst) {
            ACTIVE
        } else {
            INACTIVE
        }
    }

    /// Wraps a new connection's plaintext.
    pub fn wrap<S>(self: &Arc<Self>, inner: S) -> CsiIo<S> {
        let id = self.newest.fetch_add(1, Ordering::SeqCst) + 1;
        // A new connection: the server's idea of our state is unknown
        // (a resumed session keeps it; a new one starts active).
        self.sent.store(UNKNOWN, Ordering::SeqCst);
        CsiIo {
            inner,
            csi: self.clone(),
            id,
            xml: Tracker::default(),
            out: Vec::new(),
            out_at: 0,
            out_state: UNKNOWN,
        }
    }
}

/// A connection's plaintext, with CSI written in at element boundaries.
pub struct CsiIo<S> {
    inner: S,
    csi: Arc<Csi>,
    id: u64,
    xml: Tracker,
    /// A CSI nonza being written, and how much of it is out.
    out: Vec<u8>,
    out_at: usize,
    out_state: u8,
}

impl<S: AsyncWrite + Unpin> CsiIo<S> {
    /// Writes any CSI nonza that is due. Ready once nothing is left of it.
    fn poll_csi(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.out.is_empty()
            && self.xml.at_boundary()
            && self.csi.armed.load(Ordering::SeqCst) == self.id
            && self.csi.pending()
        {
            let state = self.csi.wanted();
            let name = if state == ACTIVE {
                "active"
            } else {
                "inactive"
            };
            self.out = format!("<{name} xmlns='{NS}'/>").into_bytes();
            self.out_at = 0;
            self.out_state = state;
        }
        while self.out_at < self.out.len() {
            let n = std::task::ready!(
                Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_at..])
            )?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.out_at += n;
        }
        if !self.out.is_empty() {
            tracing::debug!(
                active = self.out_state == ACTIVE,
                "told the server whether a window is open (CSI)"
            );
            self.csi.sent.store(self.out_state, Ordering::SeqCst);
            self.out.clear();
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CsiIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CsiIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        std::task::ready!(this.poll_csi(cx))?;
        // Stop at the next boundary between top-level elements, so a
        // nonza that falls due can go in there.
        let len = if this.csi.armed.load(Ordering::SeqCst) == this.id && this.csi.pending() {
            this.xml.span_to_boundary(buf)
        } else {
            buf.len()
        };
        let n = std::task::ready!(Pin::new(&mut this.inner).poll_write(cx, &buf[..len]))?;
        this.xml.feed(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        std::task::ready!(this.poll_csi(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Follows the depth of the XML written, byte by byte, so it can tell when
/// it is between two top-level elements (children of `<stream:stream>`).
/// The writer escapes `<` in text and quotes attribute values, which is all
/// this relies on.
#[derive(Debug, Default)]
struct Tracker {
    depth: i32,
    state: State,
    closing: bool,
    self_closing: bool,
    quote: Option<u8>,
    name: [u8; 16],
    name_len: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Text,
    /// Just after `<`.
    Open,
    /// In the element's name.
    Name,
    /// After the name, up to `>`.
    Attributes,
    /// In `<?…?>`.
    Instruction,
}

impl Tracker {
    fn at_boundary(&self) -> bool {
        self.state == State::Text && self.depth == 1
    }

    /// How much of `buf` can be written before reaching a boundary (all of
    /// it, if there is none; nothing, if it starts at one).
    fn span_to_boundary(&self, buf: &[u8]) -> usize {
        if self.at_boundary() {
            return buf.len();
        }
        let mut probe = Tracker { ..*self };
        for (i, &b) in buf.iter().enumerate() {
            probe.feed_byte(b);
            if probe.at_boundary() {
                return i + 1;
            }
        }
        buf.len()
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
    }

    fn feed_byte(&mut self, b: u8) {
        match self.state {
            State::Text => {
                if b == b'<' {
                    self.state = State::Open;
                }
            }
            State::Open => match b {
                b'/' => {
                    self.closing = true;
                    self.self_closing = false;
                    self.name_len = 0;
                    self.state = State::Name;
                }
                b'?' | b'!' => self.state = State::Instruction,
                _ => {
                    self.closing = false;
                    self.self_closing = false;
                    self.quote = None;
                    self.name_len = 0;
                    self.push_name(b);
                    self.state = State::Name;
                }
            },
            State::Name => match b {
                b'>' => self.close_tag(),
                b'/' => {
                    self.self_closing = true;
                    self.state = State::Attributes;
                }
                b' ' | b'\t' | b'\n' | b'\r' => self.state = State::Attributes,
                _ => self.push_name(b),
            },
            State::Attributes => match (self.quote, b) {
                (Some(q), _) if b == q => self.quote = None,
                (Some(_), _) => {}
                (None, b'"' | b'\'') => {
                    self.quote = Some(b);
                    self.self_closing = false;
                }
                (None, b'/') => self.self_closing = true,
                (None, b'>') => self.close_tag(),
                (None, b' ' | b'\t' | b'\n' | b'\r') => {}
                (None, _) => self.self_closing = false,
            },
            State::Instruction => {
                if b == b'>' {
                    self.state = State::Text;
                }
            }
        }
    }

    fn push_name(&mut self, b: u8) {
        if self.name_len < self.name.len() {
            self.name[self.name_len] = b;
        }
        self.name_len += 1;
    }

    fn close_tag(&mut self) {
        let is_stream = &self.name[..self.name_len.min(self.name.len())] == b"stream:stream";
        if self.closing {
            self.depth -= 1;
        } else if self.self_closing {
        } else if is_stream {
            // A stream header, the first or after a restart.
            self.depth = 1;
        } else {
            self.depth += 1;
        }
        self.state = State::Text;
        self.quote = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundaries(xml: &str) -> Vec<usize> {
        let mut t = Tracker::default();
        let mut out = Vec::new();
        for (i, b) in xml.bytes().enumerate() {
            t.feed_byte(b);
            if t.at_boundary() {
                out.push(i + 1);
            }
        }
        out
    }

    #[test]
    fn finds_the_gaps_between_top_level_elements() {
        let header = "<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' to='a.example' version='1.0'>";
        let msg = "<message to='b@a.example' id='x'><body>a &lt; b > c / d</body></message>";
        let sm = "<r xmlns='urn:xmpp:sm:3'/>";
        let tricky = "<iq id='a&gt;b' type=\"get\"><query xmlns='q' node='x/y>z'/></iq>";
        let xml = format!("{header}{msg}{sm}{tricky}");
        let ends = [
            header.len(),
            header.len() + msg.len(),
            header.len() + msg.len() + sm.len(),
            xml.len(),
        ];
        assert_eq!(boundaries(&xml), ends);
        // A restarted stream (after SASL) starts at depth 1 again.
        let restarted = format!("{header}<auth xmlns='s'>x</auth>{header}{sm}");
        assert_eq!(boundaries(&restarted).last(), Some(&restarted.len()));
    }

    #[test]
    fn span_stops_at_the_next_gap() {
        let mut t = Tracker::default();
        t.feed(b"<stream:stream xmlns='x'><message><body>hi");
        let rest = b"</body></message><presence/>";
        assert_eq!(t.span_to_boundary(rest), "</body></message>".len());
    }

    #[tokio::test]
    async fn writes_the_state_between_elements_once_bound() {
        use tokio::io::AsyncWriteExt;
        let csi = Csi::new(true);
        let mut io = csi.wrap(Vec::<u8>::new());
        io.write_all(b"<stream:stream xmlns='x'><message><body>hi")
            .await
            .unwrap();
        // Not bound yet: nothing is written.
        csi.set_active(false);
        io.write_all(b"</body></message>").await.unwrap();
        csi.bound(Some(true));
        assert!(csi.pending());
        io.write_all(b"<iq type='get'><ping xmlns='urn:xmpp:ping'/></iq>")
            .await
            .unwrap();
        io.flush().await.unwrap();
        assert_eq!(
            String::from_utf8(io.inner.clone()).unwrap(),
            "<stream:stream xmlns='x'><message><body>hi</body></message>\
             <inactive xmlns='urn:xmpp:csi:0'/><iq type='get'><ping xmlns='urn:xmpp:ping'/></iq>"
        );
        assert!(!csi.pending());
        // A server without CSI is never sent it.
        let csi = Csi::new(false);
        let mut io = csi.wrap(Vec::<u8>::new());
        csi.bound(Some(false));
        io.write_all(b"<stream:stream xmlns='x'><presence/>")
            .await
            .unwrap();
        io.flush().await.unwrap();
        assert!(!String::from_utf8(io.inner).unwrap().contains("csi"));
    }
}
