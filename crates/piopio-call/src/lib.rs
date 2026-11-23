//! Call media for Piopio: one WebRTC peer connection in GStreamer's
//! webrtcbin, with Opus audio (and VP8 video when asked).
//!
//! It lives in the UI process (a call only exists while someone is looking
//! at it). Signalling goes through the daemon, which turns the SDP and ICE
//! candidates this produces into Jingle and back.
//!
//! Events are delivered on GStreamer's threads; the caller moves them to
//! its main loop.

use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
pub use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;

/// Whether this side writes the offer or the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Offerer,
    Answerer,
}

/// Where media comes from and goes to, as GStreamer bin descriptions.
#[derive(Clone, Debug)]
pub struct Media {
    pub audio_source: String,
    pub audio_sink: String,
    pub video: bool,
    pub video_source: String,
    /// The element the other side's video goes to (named "video"); the
    /// UI takes the paintable from gtk4paintablesink.
    pub video_sink: String,
}

impl Default for Media {
    fn default() -> Self {
        Self {
            audio_source: "autoaudiosrc".into(),
            audio_sink: "autoaudiosink".into(),
            video: false,
            video_source: "autovideosrc".into(),
            video_sink: "gtk4paintablesink".into(),
        }
    }
}

/// Something the other side needs to hear about, or the call's progress.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Our offer or answer ("offer" or "answer", SDP text).
    LocalDescription { kind: String, sdp: String },
    /// One of our ICE candidates ("candidate:…" for media line `mline`).
    LocalCandidate { mline: u32, candidate: String },
    /// new, checking, connected, completed, failed, disconnected or closed.
    Connection(String),
    /// Audio from the other side is arriving.
    AudioFlowing,
    /// A video stream arrived; the paintable sink element to show it.
    RemoteVideo(gst::Element),
    /// Something went wrong (the text says what).
    Error(String),
}

type Handler = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Call {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    role: Role,
    media: Media,
    events: Handler,
    sending: Mutex<bool>,
}

/// Checks that the GStreamer elements calls need are installed.
pub fn check() -> anyhow::Result<()> {
    gst::init()?;
    for e in [
        "webrtcbin",
        "nicesrc",
        "dtlssrtpenc",
        "srtpenc",
        "opusenc",
        "opusdec",
        "rtpopuspay",
    ] {
        if gst::ElementFactory::find(e).is_none() {
            return Err(anyhow!("the GStreamer element {e} is missing"));
        }
    }
    Ok(())
}

impl Call {
    /// Sets up a peer connection. An offerer creates its offer right away;
    /// an answerer waits for [`Call::set_remote_description`].
    pub fn new(
        role: Role,
        media: Media,
        stun: Option<&str>,
        turn: &[String],
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        gst::init()?;
        let pipeline = gst::Pipeline::new();
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .build()
            .context("webrtcbin")?;
        if let Some(stun) = stun {
            webrtc.set_property("stun-server", stun);
        }
        for t in turn {
            let _: bool = webrtc.emit_by_name("add-turn-server", &[t]);
        }
        pipeline.add(&webrtc)?;
        let call = Arc::new(Self {
            pipeline,
            webrtc,
            role,
            media,
            events: Arc::new(events),
            sending: Mutex::new(false),
        });

        let weak = Arc::downgrade(&call);
        call.webrtc
            .connect("on-ice-candidate", false, move |values| {
                let mline = values[1].get::<u32>().ok()?;
                let candidate = values[2].get::<String>().ok()?;
                if let Some(call) = weak.upgrade()
                    && !candidate.is_empty()
                {
                    (call.events)(Event::LocalCandidate { mline, candidate });
                }
                None
            });
        let weak = Arc::downgrade(&call);
        call.webrtc
            .connect_notify(Some("ice-connection-state"), move |webrtc, _| {
                let state =
                    webrtc.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
                let name = match state {
                    gst_webrtc::WebRTCICEConnectionState::New => "new",
                    gst_webrtc::WebRTCICEConnectionState::Checking => "checking",
                    gst_webrtc::WebRTCICEConnectionState::Connected => "connected",
                    gst_webrtc::WebRTCICEConnectionState::Completed => "completed",
                    gst_webrtc::WebRTCICEConnectionState::Failed => "failed",
                    gst_webrtc::WebRTCICEConnectionState::Disconnected => "disconnected",
                    gst_webrtc::WebRTCICEConnectionState::Closed => "closed",
                    _ => "unknown",
                };
                if let Some(call) = weak.upgrade() {
                    (call.events)(Event::Connection(name.into()));
                }
            });
        let weak = Arc::downgrade(&call);
        call.webrtc.connect_pad_added(move |_, pad| {
            if let Some(call) = weak.upgrade()
                && pad.direction() == gst::PadDirection::Src
                && let Err(e) = call.receive(pad)
            {
                (call.events)(Event::Error(format!("{e:#}")));
            }
        });
        let bus = call.pipeline.bus().context("a pipeline has a bus")?;
        let weak = Arc::downgrade(&call);
        bus.set_sync_handler(move |_, msg| {
            if let gst::MessageView::Error(e) = msg.view()
                && let Some(call) = weak.upgrade()
            {
                (call.events)(Event::Error(e.error().to_string()));
            }
            gst::BusSyncReply::Drop
        });

        if role == Role::Offerer {
            call.add_senders(None)?;
            let weak = Arc::downgrade(&call);
            call.webrtc
                .connect("on-negotiation-needed", false, move |_| {
                    if let Some(call) = weak.upgrade() {
                        call.create("create-offer");
                    }
                    None
                });
        }
        call.pipeline.set_state(gst::State::Playing)?;
        Ok(call)
    }

    /// Our outgoing streams. `offer` is the remote offer, to reuse its
    /// payload types when answering.
    fn add_senders(&self, offer: Option<&gst_sdp::SDPMessage>) -> anyhow::Result<()> {
        let mut sending = self.sending.lock().expect("lock");
        if *sending {
            return Ok(());
        }
        *sending = true;
        let pt_for = |media: &str, codec: &str, default: u32| -> u32 {
            offer
                .and_then(|o| {
                    o.medias().find(|m| m.media() == Some(media)).and_then(|m| {
                        m.attributes().find_map(|a| {
                            let v = a.value()?;
                            (a.key() == "rtpmap" && v.to_uppercase().contains(codec))
                                .then(|| v.split(' ').next()?.parse().ok())
                                .flatten()
                        })
                    })
                })
                .unwrap_or(default)
        };
        let audio_pt = pt_for("audio", "OPUS", 111);
        let audio = gst::parse::bin_from_description(
            &format!(
                "{} ! audioconvert ! audioresample ! volume name=microphone ! queue ! opusenc ! \
                 rtpopuspay pt={audio_pt} ! \
                 capsfilter caps=application/x-rtp,media=audio,encoding-name=OPUS,payload={audio_pt}",
                self.media.audio_source
            ),
            true,
        )?;
        self.pipeline.add(&audio)?;
        audio.link(&self.webrtc)?;
        audio.sync_state_with_parent()?;
        let wants_video = match offer {
            Some(o) => o.medias().any(|m| m.media() == Some("video")),
            None => self.media.video,
        };
        if wants_video {
            let video_pt = pt_for("video", "VP8", 96);
            let video = gst::parse::bin_from_description(
                &format!(
                    "{} ! videoconvert ! queue ! vp8enc deadline=1 target-bitrate=800000 ! \
                     rtpvp8pay pt={video_pt} ! \
                     capsfilter caps=application/x-rtp,media=video,encoding-name=VP8,payload={video_pt}",
                    self.media.video_source
                ),
                true,
            )?;
            self.pipeline.add(&video)?;
            video.link(&self.webrtc)?;
            video.sync_state_with_parent()?;
        }
        Ok(())
    }

    /// Plays what the other side sends: webrtcbin's RTP goes through a
    /// decodebin, whose decoded output goes to the sink.
    fn receive(&self, pad: &gst::Pad) -> anyhow::Result<()> {
        let caps = pad.current_caps().context("caps on the new pad")?;
        let video = caps
            .structure(0)
            .and_then(|s| s.get::<String>("media").ok())
            .is_some_and(|m| m == "video");
        let decode = gst::ElementFactory::make("decodebin").build()?;
        self.pipeline.add(&decode)?;
        decode.sync_state_with_parent()?;
        pad.link(&decode.static_pad("sink").context("decodebin's sink")?)?;

        let pipeline = self.pipeline.clone();
        let events = self.events.clone();
        let sink_description = if video {
            format!("videoconvert ! {} name=video", self.media.video_sink)
        } else {
            format!("audioconvert ! audioresample ! {}", self.media.audio_sink)
        };
        decode.connect_pad_added(move |_, decoded| {
            let result = (|| -> anyhow::Result<()> {
                let out = gst::parse::bin_from_description(&sink_description, true)?;
                pipeline.add(&out)?;
                out.sync_state_with_parent()?;
                decoded.link(&out.static_pad("sink").context("the output's sink")?)?;
                if video {
                    if let Some(v) = out.by_name("video") {
                        events(Event::RemoteVideo(v));
                    }
                } else {
                    let events = events.clone();
                    let told = std::sync::atomic::AtomicBool::new(false);
                    decoded.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                        if !told.swap(true, std::sync::atomic::Ordering::SeqCst) {
                            events(Event::AudioFlowing);
                        }
                        gst::PadProbeReturn::Ok
                    });
                }
                Ok(())
            })();
            if let Err(e) = result {
                events(Event::Error(format!("{e:#}")));
            }
        });
        Ok(())
    }

    /// Creates an offer or answer and tells the caller (as a LocalDescription).
    fn create(self: &Arc<Self>, what: &str) {
        let weak = Arc::downgrade(self);
        let kind = if what == "create-offer" {
            "offer"
        } else {
            "answer"
        };
        let promise = gst::Promise::with_change_func(move |reply| {
            let Some(call) = weak.upgrade() else { return };
            let description = reply
                .ok()
                .flatten()
                .and_then(|s| s.get::<gst_webrtc::WebRTCSessionDescription>(kind).ok());
            let Some(description) = description else {
                (call.events)(Event::Error(format!("no {kind} was created")));
                return;
            };
            call.webrtc.emit_by_name::<()>(
                "set-local-description",
                &[&description, &None::<gst::Promise>],
            );
            let sdp = description.sdp().as_text().unwrap_or_default();
            (call.events)(Event::LocalDescription {
                kind: kind.into(),
                sdp,
            });
        });
        self.webrtc
            .emit_by_name::<()>(what, &[&None::<gst::Structure>, &promise]);
    }

    /// The other side's offer (we then answer) or answer.
    pub fn set_remote_description(self: &Arc<Self>, kind: &str, sdp: &str) -> anyhow::Result<()> {
        let message = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
            .map_err(|_| anyhow!("the remote description isn't valid SDP"))?;
        let sdp_type = if kind == "offer" {
            gst_webrtc::WebRTCSDPType::Offer
        } else {
            gst_webrtc::WebRTCSDPType::Answer
        };
        if self.role == Role::Answerer && kind == "offer" {
            self.add_senders(Some(&message))?;
        }
        let description = gst_webrtc::WebRTCSessionDescription::new(sdp_type, message);
        let weak = Arc::downgrade(self);
        let answer = self.role == Role::Answerer && kind == "offer";
        let promise = gst::Promise::with_change_func(move |_| {
            if answer && let Some(call) = weak.upgrade() {
                call.create("create-answer");
            }
        });
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&description, &promise]);
        Ok(())
    }

    /// One of the other side's ICE candidates.
    pub fn add_remote_candidate(&self, mline: u32, candidate: &str) {
        let line = if candidate.starts_with("candidate:") || candidate.starts_with("a=") {
            candidate.trim_start_matches("a=").to_owned()
        } else {
            format!("candidate:{candidate}")
        };
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&mline, &line]);
    }

    /// Mutes or unmutes our microphone (sending silence, so the stream
    /// stays alive).
    pub fn set_muted(&self, muted: bool) {
        if let Some(volume) = self.pipeline.by_name("microphone") {
            volume.set_property("mute", muted);
        }
    }

    /// Ends the call and frees the pipeline.
    pub fn hangup(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
