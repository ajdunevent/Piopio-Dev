//! The call window: ringing, answering, talking, hanging up.
//!
//! Media runs here (piopio-call); the daemon only signals. Events from
//! GStreamer's threads come to the main loop through a channel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::{glib, gtk};
use futures_util::StreamExt;
use piopio_call::{Event, Media, Role};
use piopio_dbus::CallProxy;
use zbus::zvariant::OwnedObjectPath;

use crate::client::{self, run};
use crate::window::Window;

/// What the daemon tells the window about a call.
enum Signal {
    State(String),
    Description(String, String),
    Candidate(u32, String),
}

pub struct CallView {
    dialog: adw::Dialog,
    proxy: CallProxy<'static>,
    status: gtk::Label,
    answer: gtk::Button,
    mute: gtk::ToggleButton,
    media: RefCell<Option<Arc<piopio_call::Call>>>,
    started: Cell<Option<std::time::Instant>>,
    ended: Cell<bool>,
}

/// Opens the view for the call at `path` (a new outgoing call, an
/// incoming one that is ringing, or one picked from a notification).
pub async fn open(window: &Rc<Window>, path: OwnedObjectPath) {
    let Ok(bus) = client::bus().await else { return };
    let loaded = run(async move {
        let proxy = CallProxy::builder(&bus).path(path)?.build().await?;
        let peer = proxy.peer().await?;
        let direction = proxy.direction().await?;
        let state = proxy.state().await?;
        Ok::<_, zbus::Error>((proxy, peer, direction, state))
    })
    .await;
    let Ok((proxy, peer, direction, state)) = loaded else {
        return;
    };
    if state == "ended" {
        return;
    }
    let view = CallView::new(
        window,
        proxy,
        &peer,
        direction == "incoming" && state == "ringing",
    );
    if direction == "outgoing" {
        view.start_media(Role::Offerer);
    }
}

impl CallView {
    fn new(window: &Rc<Window>, proxy: CallProxy<'static>, peer: &str, ringing: bool) -> Rc<Self> {
        let avatar = adw::Avatar::new(96, Some(peer), true);
        let name = gtk::Label::builder()
            .label(peer)
            .css_classes(["title-1"])
            .wrap(true)
            .build();
        let status = gtk::Label::builder()
            .label(if ringing {
                "Incoming call"
            } else {
                "Calling…"
            })
            .css_classes(["dim-label"])
            .build();
        let mute = gtk::ToggleButton::builder()
            .icon_name("microphone-sensitivity-high-symbolic")
            .tooltip_text("Mute")
            .css_classes(["circular", "large-button"])
            .visible(!ringing)
            .build();
        let hangup = gtk::Button::builder()
            .icon_name("call-stop-symbolic")
            .tooltip_text(if ringing { "Decline" } else { "Hang Up" })
            .css_classes(["circular", "destructive-action", "large-button"])
            .build();
        let answer = gtk::Button::builder()
            .icon_name("call-start-symbolic")
            .tooltip_text("Answer")
            .css_classes(["circular", "suggested-action", "large-button"])
            .visible(ringing)
            .build();
        let buttons = gtk::Box::builder()
            .spacing(24)
            .halign(gtk::Align::Center)
            .build();
        buttons.append(&mute);
        buttons.append(&hangup);
        buttons.append(&answer);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(36)
            .margin_bottom(36)
            .margin_start(24)
            .margin_end(24)
            .build();
        content.append(&avatar);
        content.append(&name);
        content.append(&status);
        content.append(&gtk::Box::builder().height_request(24).build());
        content.append(&buttons);
        let dialog = adw::Dialog::builder()
            .title("Call")
            .content_width(360)
            .can_close(false)
            .child(&content)
            .build();

        let view = Rc::new(Self {
            dialog: dialog.clone(),
            proxy: proxy.clone(),
            status,
            answer: answer.clone(),
            mute: mute.clone(),
            media: RefCell::default(),
            started: Cell::new(None),
            ended: Cell::new(false),
        });

        let weak = Rc::downgrade(&view);
        hangup.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                let proxy = view.proxy.clone();
                glib::spawn_future_local(async move {
                    let _ = run(async move { proxy.hangup().await }).await;
                });
            }
        });
        let weak = Rc::downgrade(&view);
        answer.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                view.answer.set_visible(false);
                view.mute.set_visible(true);
                view.status.set_label("Connecting…");
                // Media first, so the offer finds us ready.
                view.start_media(Role::Answerer);
                let proxy = view.proxy.clone();
                glib::spawn_future_local(async move {
                    let _ = run(async move { proxy.accept().await }).await;
                });
            }
        });
        let weak = Rc::downgrade(&view);
        mute.connect_toggled(move |b| {
            if let Some(view) = weak.upgrade()
                && let Some(media) = view.media.borrow().as_ref()
            {
                media.set_muted(b.is_active());
                b.set_icon_name(if b.is_active() {
                    "microphone-disabled-symbolic"
                } else {
                    "microphone-sensitivity-high-symbolic"
                });
            }
        });

        view.follow();
        // The timer under the name, once connected.
        let weak = Rc::downgrade(&view);
        glib::timeout_add_seconds_local(1, move || {
            let Some(view) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if view.ended.get() {
                return glib::ControlFlow::Break;
            }
            if let Some(start) = view.started.get() {
                let s = start.elapsed().as_secs();
                view.status.set_label(&format!("{}:{:02}", s / 60, s % 60));
            }
            glib::ControlFlow::Continue
        });
        dialog.present(Some(&window.window));
        view
    }

    /// Follows the daemon: state changes, and the other side's SDP and
    /// candidates for the media.
    fn follow(self: &Rc<Self>) {
        let signals = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let state = proxy.receive_state_changed().await;
                let descriptions = proxy.receive_remote_description().await.ok()?;
                let candidates = proxy.receive_remote_candidate().await.ok()?;
                Some(futures_util::stream::select_all([
                    state
                        .filter_map(|c| async move { c.get().await.ok().map(Signal::State) })
                        .boxed(),
                    descriptions
                        .filter_map(|d| async move {
                            let a = d.args().ok()?;
                            Some(Signal::Description(
                                a.kind().to_string(),
                                a.description().to_string(),
                            ))
                        })
                        .boxed(),
                    candidates
                        .filter_map(|c| async move {
                            let a = c.args().ok()?;
                            Some(Signal::Candidate(*a.mline(), a.candidate().to_string()))
                        })
                        .boxed(),
                ]))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(signal) = signals.recv().await {
                let Some(view) = weak.upgrade() else { break };
                match signal {
                    Signal::State(s) => view.state_changed(&s).await,
                    Signal::Description(kind, sdp) => {
                        if let Some(media) = view.media.borrow().as_ref()
                            && let Err(e) = media.set_remote_description(&kind, &sdp)
                        {
                            view.status.set_label(&format!("{e:#}"));
                        }
                    }
                    Signal::Candidate(mline, c) => {
                        if let Some(media) = view.media.borrow().as_ref() {
                            media.add_remote_candidate(mline, &c);
                        }
                    }
                }
            }
        });
    }

    async fn state_changed(self: &Rc<Self>, state: &str) {
        match state {
            "connecting" => self.status.set_label("Connecting…"),
            "ended" => {
                self.ended.set(true);
                let proxy = self.proxy.clone();
                let reason = run(async move { proxy.end_reason().await.unwrap_or_default() }).await;
                self.status.set_label(&match reason.as_str() {
                    "" | "hung up" => "Call ended".to_owned(),
                    "declined" => "Declined".to_owned(),
                    "missed" => "Missed call".to_owned(),
                    "answered elsewhere" => "Answered on another device".to_owned(),
                    other => format!("Call ended: {other}"),
                });
                if let Some(media) = self.media.borrow_mut().take() {
                    media.hangup();
                }
                let dialog = self.dialog.clone();
                glib::timeout_add_seconds_local_once(2, move || {
                    dialog.set_can_close(true);
                    dialog.close();
                });
            }
            _ => {}
        }
    }

    /// Starts the media, wiring its events to the daemon.
    fn start_media(self: &Rc<Self>, role: Role) {
        let (tx, rx) = async_channel::unbounded::<Event>();
        let media = piopio_call::Call::new(role, Media::default(), None, &[], move |e| {
            let _ = tx.send_blocking(e);
        });
        let media = match media {
            Ok(m) => m,
            Err(e) => {
                self.status
                    .set_label(&format!("Calls don't work here: {e:#}"));
                let proxy = self.proxy.clone();
                glib::spawn_future_local(async move {
                    let _ = run(async move { proxy.hangup().await }).await;
                });
                return;
            }
        };
        *self.media.borrow_mut() = Some(media);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(view) = weak.upgrade() else { break };
                let proxy = view.proxy.clone();
                match event {
                    Event::LocalDescription { kind, sdp } => {
                        let _ = run(async move { proxy.set_local_description(&kind, &sdp).await })
                            .await;
                    }
                    Event::LocalCandidate { mline, candidate } => {
                        let _ =
                            run(async move { proxy.add_local_candidate(mline, &candidate).await })
                                .await;
                    }
                    Event::Connection(s) if s == "connected" || s == "completed" => {
                        if view.started.get().is_none() {
                            view.started.set(Some(std::time::Instant::now()));
                        }
                    }
                    Event::Connection(s) if s == "failed" => {
                        view.status.set_label("The connection failed");
                        let _ = run(async move { proxy.hangup().await }).await;
                    }
                    Event::Error(e) => {
                        view.status.set_label(&e);
                    }
                    _ => {}
                }
            }
        });
    }
}

/// Watches for incoming calls (new Call objects that ring).
pub fn is_call(interfaces: &[String]) -> bool {
    interfaces.iter().any(|i| i == "net.dunevent.Piopio.Call")
}
