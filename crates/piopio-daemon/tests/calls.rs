//! A call between two daemons on the local test server, with real media:
//! the test plays both UIs, running piopio-call (webrtcbin) on each side
//! and passing SDP and candidates through the daemons, which carry them
//! over XMPP as Jingle Message Initiation and a Jingle RTP session.
//!
//!   tests/server/run.sh cargo test -p piopio-daemon --test calls

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestDaemon, test_server, wait_for};
use futures_util::StreamExt;
use piopio_call::{Call, Event, Media, Role};
use piopio_dbus::CallProxy;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;

fn media() -> Media {
    Media {
        audio_source: "audiotestsrc is-live=true wave=sine".into(),
        audio_sink: "fakesink sync=false".into(),
        ..Default::default()
    }
}

async fn call_proxy(d: &TestDaemon, path: zbus::zvariant::OwnedObjectPath) -> CallProxy<'static> {
    CallProxy::builder(&d.connection)
        .path(path)
        .unwrap()
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap()
}

/// Plays the UI's part: media events go to the daemon, and the daemon's
/// signals go to the media. Returns when audio flows (true) or it ends.
async fn run_side(proxy: CallProxy<'static>, role: Role) -> bool {
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let call: Arc<Call> = Call::new(role, media(), None, &[], move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let mut descriptions = proxy.receive_remote_description().await.unwrap();
    let mut candidates = proxy.receive_remote_candidate().await.unwrap();
    let mut audio = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            Some(e) = rx.recv() => match e {
                Event::LocalDescription { kind, sdp } => {
                    proxy.set_local_description(&kind, &sdp).await.unwrap();
                }
                Event::LocalCandidate { mline, candidate } => {
                    let _ = proxy.add_local_candidate(mline, &candidate).await;
                }
                Event::AudioFlowing => {
                    audio = true;
                    break;
                }
                Event::Error(e) => panic!("media: {e}"),
                _ => {}
            },
            Some(d) = descriptions.next() => {
                let a = d.args().unwrap();
                call.set_remote_description(a.kind(), a.description()).unwrap();
            }
            Some(c) = candidates.next() => {
                let a = c.args().unwrap();
                call.add_remote_candidate(*a.mline(), a.candidate());
            }
        }
    }
    // Keep the media alive briefly so the other side's audio flows too.
    tokio::time::sleep(Duration::from_secs(2)).await;
    call.hangup();
    audio
}

#[tokio::test]
async fn audio_call() {
    if test_server().is_none() || piopio_call::check().is_err() {
        eprintln!("PIOPIO_TEST_SERVER or GStreamer WebRTC missing; skipping");
        return;
    }
    let erin = TestDaemon::start().await.unwrap();
    let frank = TestDaemon::start().await.unwrap();
    let (_, erin_account) = erin.add_local("erin").await;
    let (_, _) = frank.add_local("frank").await;

    // Frank's UI waits for a call object to appear.
    let manager = zbus::fdo::ObjectManagerProxy::builder(&frank.connection)
        .destination(piopio_dbus::DAEMON_BUS_NAME)
        .unwrap()
        .path(piopio_dbus::ROOT_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut added = manager.receive_interfaces_added().await.unwrap();

    let erin_call = erin_account
        .start_call("frank@localhost", false)
        .await
        .unwrap();
    let erin_call = call_proxy(&erin, erin_call).await;
    assert_eq!(erin_call.state().await.unwrap(), "calling");

    let frank_path = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let s = added.next().await.unwrap();
            let a = s.args().unwrap();
            if a.interfaces_and_properties()
                .keys()
                .any(|k| k.as_str() == "net.dunevent.Piopio.Call")
            {
                return a.object_path().to_owned();
            }
        }
    })
    .await
    .expect("Frank's phone rings");
    let frank_call = call_proxy(&frank, frank_path.into()).await;
    assert_eq!(frank_call.state().await.unwrap(), "ringing");
    assert_eq!(frank_call.peer().await.unwrap(), "erin@localhost");
    assert_eq!(frank_call.direction().await.unwrap(), "incoming");

    let frank_side = tokio::spawn(run_side(frank_call.clone(), Role::Answerer));
    frank_call.accept().await.unwrap();
    let erin_side = tokio::spawn(run_side(erin_call.clone(), Role::Offerer));
    let (erin_audio, frank_audio) = (erin_side.await.unwrap(), frank_side.await.unwrap());
    assert!(erin_audio, "Erin hears Frank");
    assert!(frank_audio, "Frank hears Erin");
    assert_eq!(erin_call.state().await.unwrap(), "active");

    erin_call.hangup().await.unwrap();
    wait_for(20, "Frank's side to end", || async {
        (frank_call.state().await.unwrap_or_else(|_| "ended".into()) == "ended").then_some(())
    })
    .await;
}
