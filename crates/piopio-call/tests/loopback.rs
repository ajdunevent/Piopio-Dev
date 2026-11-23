//! Two peer connections in one process, wired to each other directly:
//! offer, answer and candidates cross over, ICE connects on the loopback
//! interface, and Opus audio arrives on both sides.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use piopio_call::{Call, Event, Media, Role};

fn test_media() -> Media {
    Media {
        audio_source: "audiotestsrc is-live=true wave=sine".into(),
        audio_sink: "fakesink sync=false".into(),
        ..Default::default()
    }
}

#[test]
fn two_peers_talk() {
    if piopio_call::check().is_err() {
        eprintln!("GStreamer WebRTC elements missing; skipping");
        return;
    }
    let (a_tx, a_rx) = mpsc::channel::<Event>();
    let (b_tx, b_rx) = mpsc::channel::<Event>();
    let a_tx = std::sync::Mutex::new(a_tx);
    let b_tx = std::sync::Mutex::new(b_tx);
    let alice = Call::new(Role::Offerer, test_media(), None, &[], move |e| {
        let _ = a_tx.lock().unwrap().send(e);
    })
    .unwrap();
    let bob = Call::new(Role::Answerer, test_media(), None, &[], move |e| {
        let _ = b_tx.lock().unwrap().send(e);
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut alice_audio, mut bob_audio) = (false, false);
    let (mut alice_connected, mut bob_connected) = (false, false);
    while Instant::now() < deadline
        && !(alice_audio && bob_audio && alice_connected && bob_connected)
    {
        while let Ok(e) = a_rx.try_recv() {
            match e {
                Event::LocalDescription { kind, sdp } => {
                    assert_eq!(kind, "offer");
                    bob.set_remote_description("offer", &sdp).unwrap();
                }
                Event::LocalCandidate { mline, candidate } => {
                    bob.add_remote_candidate(mline, &candidate)
                }
                Event::Connection(s) if s == "connected" || s == "completed" => {
                    alice_connected = true
                }
                Event::AudioFlowing => alice_audio = true,
                Event::Error(e) => panic!("alice: {e}"),
                _ => {}
            }
        }
        while let Ok(e) = b_rx.try_recv() {
            match e {
                Event::LocalDescription { kind, sdp } => {
                    assert_eq!(kind, "answer");
                    alice.set_remote_description("answer", &sdp).unwrap();
                }
                Event::LocalCandidate { mline, candidate } => {
                    alice.add_remote_candidate(mline, &candidate)
                }
                Event::Connection(s) if s == "connected" || s == "completed" => {
                    bob_connected = true
                }
                Event::AudioFlowing => bob_audio = true,
                Event::Error(e) => panic!("bob: {e}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(alice_connected && bob_connected, "ICE connected");
    assert!(alice_audio && bob_audio, "audio both ways");
    alice.set_muted(true);
    alice.hangup();
    bob.hangup();
}
