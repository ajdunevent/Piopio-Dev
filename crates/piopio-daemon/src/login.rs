//! Opens an authenticated (but not yet bound) client stream.
//!
//! Resource binding and Stream Management are left to tokio-xmpp's
//! `StanzaStream`. This is the place where SASL2, Bind 2 and FAST will go.

use std::borrow::Cow;

use sasl::common::Credentials;
use tokio_xmpp::connect::ServerConnector;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::parsers::{ns, stream_features::StreamFeatures};
use tokio_xmpp::xmlstream::{StreamHeader, Timeouts, XmppStream};

pub async fn authenticate<C: ServerConnector>(
    server: C,
    jid: &Jid,
    password: &str,
    timeouts: Timeouts,
) -> Result<(StreamFeatures, XmppStream<C::Stream>), tokio_xmpp::Error> {
    let username = jid.node().map(|n| n.as_str()).unwrap_or_default();

    let (pending, channel_binding) = server.connect(jid, ns::JABBER_CLIENT, timeouts).await?;
    let (features, stream): (StreamFeatures, XmppStream<C::Stream>) =
        pending.recv_features().await?;

    let credentials = Credentials::default()
        .with_username(username)
        .with_password(password)
        .with_channel_binding(channel_binding);
    let stream = tokio_xmpp::client_login(stream, features.sasl_mechanisms, credentials).await?;

    let stream = stream
        .send_header(StreamHeader {
            to: Some(Cow::Borrowed(jid.domain().as_str())),
            from: None,
            id: None,
        })
        .await?;
    Ok(stream.recv_features().await?)
}
