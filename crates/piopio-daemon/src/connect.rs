//! Opening connections: where to connect, and the connector `StanzaStream`
//! calls whenever it needs a fresh stream.
//!
//! tokio-xmpp's `StanzaStream` handles resource binding, Stream Management and
//! resumption. The daemon supplies its own connector so that it, not the
//! library, decides what happens on failure: an authentication failure stops
//! retrying and is reported to the UI instead of looping forever.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use sasl::common::ChannelBinding;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, BufStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_xmpp::connect::starttls::starttls;
use tokio_xmpp::connect::tls_common::establish_tls_connection;
use tokio_xmpp::connect::{DnsConfig, ServerConnector};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::parsers::stream_features::StreamFeatures;
use tokio_xmpp::stanzastream::Connection;
use tokio_xmpp::xmlstream::{
    PendingFeaturesRecv, StreamHeader, Timeouts, XmppStream, initiate_stream,
};

use crate::csi::{Csi, CsiIo};

use crate::login::authenticate;

/// Where an account connects. Stored with the account as JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Server {
    /// Look up the server with DNS SRV records (the normal case).
    #[default]
    Srv,
    /// A fixed host and port, with STARTTLS.
    Host { host: String, port: u16 },
    /// Unencrypted TCP to a fixed address. Only for the test suite, and only
    /// accepted when PIOPIO_ALLOW_PLAINTEXT=1.
    TestPlaintext { address: String },
}

/// What the connector reports back to the session.
#[derive(Debug)]
pub enum Status {
    Connecting,
    /// A connection attempt failed; the connector retries after a delay.
    Retrying(String),
    /// The server rejected the credentials. No retry until the password
    /// changes or the account is re-enabled, which starts a new connection.
    AuthFailed(String),
}

pub type Connector = Box<dyn FnMut(Option<String>, oneshot::Sender<Connection>) + Send + 'static>;

/// Builds the connector `StanzaStream` calls whenever it needs a new stream.
pub fn connector(
    jid: BareJid,
    password: String,
    server: Server,
    status: mpsc::UnboundedSender<Status>,
    cancel: watch::Receiver<bool>,
    csi: Arc<Csi>,
) -> Connector {
    Box::new(move |_preferred_location, mut slot| {
        let (jid, password, server, status, mut cancel, csi) = (
            jid.clone(),
            password.clone(),
            server.clone(),
            status.clone(),
            cancel.clone(),
            csi.clone(),
        );
        tokio::spawn(async move {
            const MAX_DELAY: Duration = Duration::from_secs(60);
            let mut delay = Duration::from_secs(1);
            loop {
                let _ = status.send(Status::Connecting);
                let full = Jid::from(jid.clone());
                let attempt = tokio::select! {
                    _ = cancelled(&mut cancel) => return park(slot).await,
                    _ = slot.closed() => return,
                    attempt = attempt(&server, &full, &password, &csi) => attempt,
                };
                let wait = match attempt {
                    Ok(connection) => {
                        let _ = slot.send(connection);
                        return;
                    }
                    Err(e @ tokio_xmpp::Error::Auth(_)) => {
                        tracing::debug!(%jid, "login failed: {e:?}");
                        let _ = status.send(Status::AuthFailed(describe(&e, &jid)));
                        None
                    }
                    Err(e) => {
                        tracing::debug!(%jid, "connection attempt failed: {e:?}");
                        let _ = status.send(Status::Retrying(describe(&e, &jid)));
                        let wait = delay;
                        delay = (delay * 2).min(MAX_DELAY);
                        Some(wait)
                    }
                };
                tokio::select! {
                    _ = cancelled(&mut cancel) => return park(slot).await,
                    _ = slot.closed() => return,
                    _ = std::future::pending::<()>(), if wait.is_none() => {}
                    _ = tokio::time::sleep(wait.unwrap_or_default()), if wait.is_some() => {}
                }
            }
        });
    })
}

async fn attempt(
    server: &Server,
    jid: &Jid,
    password: &str,
    csi: &Arc<Csi>,
) -> Result<Connection, tokio_xmpp::Error> {
    let via = |how, dns| Via {
        how,
        dns,
        csi: csi.clone(),
    };
    match server {
        Server::Srv => {
            let domain = jid.domain().as_str();
            // Direct TLS (XEP-0368) where the server publishes it: TLS from
            // the first byte, often on port 443 where networks block 5222.
            // Anything but a refused login falls back to STARTTLS.
            if offers_direct_tls(domain).await {
                match login(
                    via(How::DirectTls, DnsConfig::srv_xmpps(domain)),
                    jid,
                    password,
                )
                .await
                {
                    Ok(connection) => return Ok(connection),
                    Err(e @ tokio_xmpp::Error::Auth(_)) => return Err(e),
                    Err(e) => {
                        tracing::debug!(%jid, "direct TLS failed, trying STARTTLS: {e:?}");
                    }
                }
            }
            let dns = DnsConfig::srv_default_client(domain);
            login(via(How::StartTls, dns), jid, password).await
        }
        Server::Host { host, port } => {
            let dns = DnsConfig::no_srv(host, *port);
            login(via(How::StartTls, dns), jid, password).await
        }
        Server::TestPlaintext { address } => {
            let dns = DnsConfig::addr(address);
            login(via(How::Plain, dns), jid, password).await
        }
    }
}

/// The connection's plaintext, whatever is under it.
trait RawIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> RawIo for T {}

#[derive(Clone, Copy, Debug)]
enum How {
    /// Only for the test suite.
    Plain,
    StartTls,
    DirectTls,
}

/// tokio-xmpp's three connectors in one, with the plaintext (above TLS)
/// passed through [`CsiIo`] so the daemon can write CSI nonzas.
#[derive(Clone, Debug)]
struct Via {
    how: How,
    dns: DnsConfig,
    csi: Arc<Csi>,
}

impl ServerConnector for Via {
    type Stream = BufStream<CsiIo<Box<dyn RawIo>>>;

    async fn connect(
        &self,
        jid: &Jid,
        ns: &'static str,
        timeouts: Timeouts,
    ) -> Result<(PendingFeaturesRecv<Self::Stream>, ChannelBinding), tokio_xmpp::Error> {
        let domain = jid.domain().as_str();
        fn header<'a>(domain: &'a str, from: Option<Cow<'a, str>>) -> StreamHeader<'a> {
            StreamHeader {
                to: Some(Cow::Borrowed(domain)),
                from,
                id: None,
            }
        }
        let tcp = self.dns.resolve().await?;
        let (io, binding): (Box<dyn RawIo>, ChannelBinding) = match self.how {
            How::Plain => (Box::new(tcp), ChannelBinding::None),
            How::DirectTls => {
                let (tls, binding) = establish_tls_connection(tcp, domain).await?;
                (Box::new(tls), binding)
            }
            How::StartTls => {
                let plain =
                    initiate_stream(BufStream::new(tcp), ns, header(domain, None), timeouts)
                        .await?;
                let (features, plain): (StreamFeatures, XmppStream<BufStream<_>>) =
                    plain.recv_features().await?;
                if !features.can_starttls() {
                    return Err(tokio_xmpp::Error::Protocol(
                        tokio_xmpp::error::ProtocolError::NoTls,
                    ));
                }
                let (tls, binding) = starttls(plain, domain).await?;
                (Box::new(tls), binding)
            }
        };
        // Direct TLS names us in the header, as tokio-xmpp's connector does
        // (servers want it to offer SASL2).
        let from =
            matches!(self.how, How::DirectTls).then(|| Cow::Owned(jid.to_bare().to_string()));
        let stream = BufStream::new(self.csi.wrap(io));
        Ok((
            initiate_stream(stream, ns, header(domain, from), timeouts).await?,
            binding,
        ))
    }
}

/// Whether `domain` publishes `_xmpps-client._tcp` SRV records (a lone "."
/// target means it explicitly doesn't).
async fn offers_direct_tls(domain: &str) -> bool {
    let Ok(ascii) = idna_ascii(domain) else {
        return false;
    };
    if ascii.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    let lookup = async {
        let resolver = hickory_resolver::TokioResolver::builder_tokio()
            .ok()?
            .build()
            .ok()?;
        resolver
            .srv_lookup(format!("_xmpps-client._tcp.{ascii}."))
            .await
            .ok()
    };
    match tokio::time::timeout(Duration::from_secs(5), lookup).await {
        Ok(Some(records)) => records.answers().iter().any(|r| match &r.data {
            hickory_resolver::proto::rr::RData::SRV(srv) => !srv.target.is_root(),
            _ => false,
        }),
        _ => false,
    }
}

fn idna_ascii(domain: &str) -> Result<String, ()> {
    if domain.is_ascii() {
        Ok(domain.to_ascii_lowercase())
    } else {
        Err(())
    }
}

async fn login<C: ServerConnector>(
    server: C,
    jid: &Jid,
    password: &str,
) -> Result<Connection, tokio_xmpp::Error> {
    let (features, stream) = authenticate(server, jid, password, Timeouts::default()).await?;
    Ok(Connection {
        stream: stream.box_stream(),
        features,
        identity: jid.clone(),
    })
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    let _ = cancel.wait_for(|c| *c).await;
}

/// Holds on to the connection slot of a connection that is being stopped.
///
/// tokio-xmpp 6 panics if a connector drops its slot, and a `StanzaStream`
/// still waiting for a connection never notices that its frontend is gone.
/// Until that is fixed upstream, a connection stopped while offline leaves
/// this small task and the stream's worker parked; the daemon exiting frees them.
async fn park(slot: oneshot::Sender<Connection>) {
    let _slot = slot;
    std::future::pending::<()>().await;
}

/// Turns a connection error into a sentence the UI can show.
pub fn describe(error: &tokio_xmpp::Error, jid: &BareJid) -> String {
    use tokio_xmpp::Error as E;
    use tokio_xmpp::error::AuthError;
    use tokio_xmpp::parsers::sasl::DefinedCondition as Sasl;

    let domain = jid.domain();
    match error {
        E::DnsProto(_) | E::DnsNet(_) | E::Idna => format!("Couldn't find the server for {domain}"),
        E::Io(e) => format!("Network error: {e}"),
        E::Connection(e) => format!("Couldn't connect securely to {domain}: {e}"),
        E::Disconnected => "The server closed the connection".to_owned(),
        E::Protocol(e) => format!("The server sent something unexpected: {e}"),
        E::Auth(AuthError::Fail(Sasl::NotAuthorized)) => "Wrong address or password".to_owned(),
        E::Auth(AuthError::Fail(Sasl::AccountDisabled)) => {
            "This account is disabled on the server".to_owned()
        }
        E::Auth(AuthError::Fail(Sasl::CredentialsExpired)) => "The password has expired".to_owned(),
        E::Auth(AuthError::NoMechanism) => {
            "The server offers no login method Piopio supports".to_owned()
        }
        E::Auth(e) => format!("The server refused the login: {e}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_direct_tls_for_addresses_or_unknown_domains() {
        assert!(!offers_direct_tls("127.0.0.1").await);
        assert!(!offers_direct_tls("localhost").await);
        assert!(!offers_direct_tls("nonexistent.invalid").await);
    }

    /// Needs DNS: conversations.im publishes _xmpps-client records.
    #[tokio::test]
    #[ignore]
    async fn finds_direct_tls_records() {
        assert!(offers_direct_tls("conversations.im").await);
    }
}
