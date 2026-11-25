//! One open conversation: its history and the box to write in.
//!
//! The send button is always there, attached to the box, so sending works
//! the same with an on-screen keyboard as with Enter; it turns red with an
//! open lock when messages would go out unencrypted. A message's menu (right click or long
//! press) replies, reacts, edits or deletes; Up in an empty box edits the
//! last message sent.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use adw::{gdk, glib, gtk};
use futures_util::StreamExt;
use piopio_dbus::{ConversationProxy, Dict, RoomProxy};
use zbus::zvariant::{OwnedValue, Value};

use crate::client::{self, run};
use crate::window::Window;

const PAGE: u32 = 50;
/// The reactions offered in a message's menu.
const QUICK_REACTIONS: &[&str] = &["👍", "❤️", "😂", "😮", "😢", "🙏"];

type MenuAction = Box<dyn Fn(&Rc<ChatPage>)>;

/// What the composer is doing besides writing a new message.
#[derive(Clone, Default, PartialEq)]
enum Mode {
    #[default]
    New,
    /// Replying to the message with this id.
    Reply(String),
    /// Correcting our message with this id.
    Edit(String),
}

pub struct ChatPage {
    pub page: adw::NavigationPage,
    proxy: ConversationProxy<'static>,
    window: std::rc::Weak<Window>,
    own_jid: String,
    send: gtk::Button,
    send_lock: gtk::Image,
    title: HeaderTitle,
    /// The account this conversation belongs to (its object path).
    account: String,
    /// Shown while the peer is blocked, with Unblock.
    blocked_banner: adw::Banner,
    composer: gtk::Widget,
    /// The pictures and name beside each message.
    /// Each message's picture and name, shown only on the first of a run.
    decorations: RefCell<HashMap<String, gtk::Label>>,
    /// What the peer's apps support; empty while unknown.
    peer_features: RefCell<Vec<String>>,
    attach: gtk::Button,
    /// Who is in a group chat, and leaving or rejoining it.
    people: gtk::MenuButton,
    is_room: Cell<bool>,
    /// The call button (hidden in group chats).
    call_button: RefCell<Option<gtk::MenuButton>>,
    /// Who we are here: our JID, or room@service/nick in a group chat.
    me: RefCell<String>,
    messages: gtk::Box,
    scroller: gtk::ScrolledWindow,
    entry: Composer,
    /// Shown above the box while replying or editing.
    mode_bar: gtk::Box,
    mode_label: gtk::Label,
    mode: RefCell<Mode>,
    /// Rows and their messages by message id, to update them in place.
    rows: RefCell<HashMap<String, (gtk::Box, Dict)>>,
    oldest: RefCell<String>,
    /// Ends "typing" after a pause.
    typing_timer: RefCell<Option<glib::SourceId>>,
    /// The subtitle when the peer isn't typing.
    subtitle: RefCell<String>,
    loading: Cell<bool>,
    /// The view is at the end and follows new messages (see stick_to_end).
    stuck: Cell<bool>,
    /// Set while the view moves itself, so that isn't taken as the user's.
    following: Cell<bool>,
    at_start: Cell<bool>,
}

impl ChatPage {
    pub fn new(
        window: &Rc<Window>,
        proxy: ConversationProxy<'static>,
        own_jid: String,
    ) -> Rc<Self> {
        let title = HeaderTitle::new();
        let people = gtk::MenuButton::builder()
            .icon_name("system-users-symbolic")
            .tooltip_text("People")
            .visible(false)
            .build();
        // Picture, name and address at the start, left-aligned.
        let header = adw::HeaderBar::builder().show_title(false).build();
        header.pack_start(&title.widget);
        let menu = gtk::gio::Menu::new();
        menu.append(Some("Conversation _Details…"), Some("chat.details"));
        menu.append(Some("_Delete Conversation…"), Some("chat.delete"));
        let more = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Conversation Menu")
            .menu_model(&menu)
            .build();
        header.pack_end(&more);
        // A Call menu (voice, video), shown once the peer's apps are known
        // to take calls; it offers only the kinds they support.
        let call = gtk::MenuButton::builder()
            .icon_name("call-start-symbolic")
            .tooltip_text("Call")
            .visible(false)
            .build();
        header.pack_end(&call);
        header.pack_end(&people);

        // A plain box, as in Sanctum's Assist: a ListBox laid its rows out
        // taller than it measured them, so the newest spilled past the end
        // of the scroll range.
        let messages = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::End)
            .css_classes(["chat-history"])
            .build();
        let clamp = adw::Clamp::builder()
            .maximum_size(800)
            .tightening_threshold(600)
            .child(&messages)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        // The history at its natural height: at its minimum, pictures
        // (which may shrink) got smaller as the box to write in grew,
        // instead of the history scrolling.
        if let Some(viewport) = scroller.child().and_downcast::<gtk::Viewport>() {
            viewport.set_vscroll_policy(gtk::ScrollablePolicy::Natural);
        }

        let mode_label = gtk::Label::builder()
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption"])
            .build();
        let mode_cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Cancel")
            .css_classes(["flat", "circular"])
            .build();
        let mode_bar = gtk::Box::builder()
            .spacing(6)
            .margin_start(12)
            .margin_end(6)
            .margin_top(4)
            .visible(false)
            .css_classes(["composer-mode"])
            .build();
        mode_bar.append(&mode_label);
        mode_bar.append(&mode_cancel);

        let entry = Composer::new();
        // A triangle pointing right with a padlock inside: closed (in the
        // button's colour, as if cut out) when encrypted, open and red when
        // not.
        let triangle = gtk::Image::builder()
            .icon_name("send-triangle-symbolic")
            .pixel_size(34)
            .build();
        let send_lock = gtk::Image::builder()
            .icon_name("changes-allow-symbolic")
            .pixel_size(11)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Center)
            // Placed for the most even room between the padlock's outline
            // and the triangle's edges: left of the icon's centre and a
            // touch high, since the lock is wide at the bottom and the
            // triangle narrows to the right.
            .margin_start(6)
            .margin_bottom(1)
            .css_classes(["send-lock", "open"])
            .build();
        let send_icon = gtk::Overlay::builder()
            .child(&triangle)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        send_icon.add_overlay(&send_lock);
        let send = gtk::Button::builder()
            .child(&send_icon)
            .tooltip_text("Send Unencrypted")
            .css_classes(["suggested-action", "send"])
            .build();
        let linked = gtk::Box::builder()
            .hexpand(true)
            .valign(gtk::Align::Center)
            .css_classes(["linked", "composer"])
            .build();
        linked.append(&entry.widget);
        linked.append(&send);
        let row = gtk::Box::builder()
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let attach = gtk::Button::builder()
            .icon_name("mail-attachment-symbolic")
            .tooltip_text("Share a File")
            .css_classes(["flat", "circular", "composer-button"])
            .valign(gtk::Align::Center)
            .build();
        row.append(&attach);
        row.append(&linked);
        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        composer.append(&mode_bar);
        composer.append(&row);
        let composer = adw::Clamp::builder()
            .maximum_size(800)
            .child(&composer)
            .build();

        let blocked_banner = adw::Banner::builder()
            .title("You blocked this contact. Their messages don’t reach you.")
            .button_label("_Unblock")
            .build();
        let view = adw::ToolbarView::builder()
            .top_bar_style(adw::ToolbarStyle::Raised)
            .build();
        view.add_top_bar(&header);
        view.add_top_bar(&blocked_banner);
        view.set_content(Some(&scroller));
        view.add_bottom_bar(&composer);
        view.set_bottom_bar_style(adw::ToolbarStyle::Raised);
        let page = adw::NavigationPage::new(&view, "Chat");

        let me = own_jid.clone();
        let account = proxy
            .inner()
            .path()
            .as_str()
            .rsplit_once("/conversation/")
            .map(|(a, _)| a.to_owned())
            .unwrap_or_default();
        let this = Rc::new(Self {
            page,
            proxy: proxy.clone(),
            window: Rc::downgrade(window),
            own_jid,
            send: send.clone(),
            send_lock: send_lock.clone(),
            title: title.clone(),
            account,
            blocked_banner: blocked_banner.clone(),
            composer: composer.clone().upcast(),
            decorations: RefCell::default(),
            peer_features: RefCell::default(),
            attach: attach.clone(),
            people,
            is_room: Cell::new(false),
            me: RefCell::new(me),
            call_button: RefCell::default(),
            messages,
            scroller,
            entry: entry.clone(),
            mode_bar,
            mode_label,
            mode: RefCell::default(),
            rows: RefCell::default(),
            oldest: RefCell::default(),
            typing_timer: RefCell::default(),
            subtitle: RefCell::default(),
            loading: Cell::new(false),
            stuck: Cell::new(true),
            following: Cell::new(false),
            at_start: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        entry.connect_changed(move |text| {
            if let Some(this) = weak.upgrade() {
                this.typed(!text.is_empty());
            }
        });
        let weak = Rc::downgrade(&this);
        let do_send = move || {
            if let Some(this) = weak.upgrade() {
                this.send();
            }
        };
        // Pasting a picture (raw image data, as from a screenshot) offers
        // to send it, as Dino does.
        let weak = Rc::downgrade(&this);
        entry.view.connect_paste_clipboard(move |view| {
            let clipboard = view.clipboard();
            if !clipboard
                .formats()
                .contains_type(gdk::Texture::static_type())
            {
                return;
            }
            view.stop_signal_emission_by_name("paste-clipboard");
            let weak = weak.clone();
            glib::spawn_future_local(async move {
                if let Ok(Some(texture)) = clipboard.read_texture_future().await
                    && let Some(this) = weak.upgrade()
                {
                    this.offer_image(texture);
                }
            });
        });
        // Files (or a picture, as from a browser) dropped on the
        // conversation offer to send them.
        let drop = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY);
        drop.set_types(&[gdk::FileList::static_type(), gdk::Texture::static_type()]);
        let weak = Rc::downgrade(&this);
        drop.connect_drop(move |_, value, _, _| {
            let Some(this) = weak.upgrade() else {
                return false;
            };
            // Only where sharing files is possible at all.
            if !this.attach.is_visible() || !this.attach.is_sensitive() {
                return false;
            }
            if let Ok(list) = value.get::<gdk::FileList>() {
                let paths: Vec<std::path::PathBuf> =
                    list.files().iter().filter_map(|f| f.path()).collect();
                if paths.is_empty() {
                    return false;
                }
                this.offer_files(paths);
                return true;
            }
            if let Ok(texture) = value.get::<gdk::Texture>() {
                this.offer_image(texture);
                return true;
            }
            false
        });
        this.page.add_controller(drop);
        entry.connect_activate({
            let do_send = do_send.clone();
            move || do_send()
        });
        send.connect_clicked(move |_| do_send());

        let weak = Rc::downgrade(&this);
        attach.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.pick_file();
            }
        });

        let weak = Rc::downgrade(&this);
        mode_cancel.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.set_mode(Mode::New);
            }
        });

        // Up in an empty box edits the last message we sent; Escape cancels.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Up
                    if this.entry.text().is_empty()
                        && this.peer_supports("urn:xmpp:message-correct:0") =>
                {
                    if let Some(id) = this.last_own_message() {
                        this.set_mode(Mode::Edit(id));
                        return glib::Propagation::Stop;
                    }
                }
                gdk::Key::Escape if *this.mode.borrow() != Mode::New => {
                    this.set_mode(Mode::New);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(keys);

        let weak = Rc::downgrade(&this);
        blocked_banner.connect_button_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.set_blocked(false);
            }
        });

        this.install_actions();
        let calls = gtk::gio::SimpleActionGroup::new();
        for (name, video) in [("voice", false), ("video", true)] {
            let action = gtk::gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&this);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    glib::spawn_future_local(async move { this.start_call(video).await });
                }
            });
            calls.add_action(&action);
        }
        this.page.insert_action_group("call", Some(&calls));
        this.call_button.replace(Some(call));

        // Load older messages when scrolled to the top.
        let weak = Rc::downgrade(&this);
        this.stick_to_end();
        this.scroller.connect_edge_reached(move |_, edge| {
            if edge == gtk::PositionType::Top
                && let Some(this) = weak.upgrade()
            {
                glib::spawn_future_local(async move { this.load_older().await });
            }
        });

        // Title, history, live updates.
        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            let Some(this) = weak.upgrade() else { return };
            let proxy = this.proxy.clone();
            if let Ok((name, peer, kind)) = run(async move {
                Ok::<_, zbus::Error>((
                    proxy.title().await?,
                    proxy.peer().await?,
                    proxy.kind().await?,
                ))
            })
            .await
            {
                title.set_title(&name);
                let subtitle = if name == peer { "" } else { &peer };
                title.set_subtitle(subtitle);
                *this.subtitle.borrow_mut() = subtitle.to_owned();
                this.page.set_title(&name);
                if kind == "room" {
                    this.is_room.set(true);
                    this.setup_room().await;
                }
            }
            this.follow_encryption();
            this.follow_server_features();
            if !this.is_room.get() {
                this.follow_typing();
                this.follow_blocked();
                this.follow_peer_features();
            }
            this.load_pictures().await;
            this.load_older().await;
            this.scroll_to_end();
            let proxy = this.proxy.clone();
            let _ = run(async move { proxy.mark_read().await }).await;
            this.entry.grab_focus();
            this.follow();
        });
        this
    }

    fn follow(self: &Rc<Self>) {
        let events = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let added = proxy.receive_message_added().await.ok()?;
                let changed = proxy.receive_message_changed().await.ok()?;
                Some(futures_util::stream::select(
                    added.filter_map(|s| async move {
                        s.args()
                            .ok()
                            .map(|a| (true, String::new(), a.message().clone()))
                    }),
                    changed.filter_map(|s| async move {
                        s.args()
                            .ok()
                            .map(|a| (false, a.id().to_string(), a.changes().clone()))
                    }),
                ))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((added, id, dict)) = events.recv().await {
                let Some(this) = weak.upgrade() else { break };
                if added {
                    // Followed by the view if it's at the end (see stick).
                    this.append(&dict);
                } else {
                    this.update(&id, dict);
                }
            }
        });
    }

    fn room_jid(&self) -> Result<String, ()> {
        self.proxy.cached_peer().ok().flatten().ok_or(())
    }

    /// The group chat parts: subject or state under the title, and the
    /// people popover with Leave or Join.
    async fn setup_room(self: &Rc<Self>) {
        let Ok(bus) = client::bus().await else { return };
        let path = self.proxy.inner().path().to_owned();
        let Ok(room) = run(async move { RoomProxy::builder(&bus).path(path)?.build().await }).await
        else {
            return;
        };
        self.people.set_visible(true);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let leave = gtk::Button::builder()
            .label("Leave")
            .css_classes(["destructive-action"])
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .width_request(240)
            .build();
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .propagate_natural_height(true)
                .max_content_height(360)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        let invite_entry = gtk::Entry::builder()
            .placeholder_text("Invite (name@example.org)")
            .hexpand(true)
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        let invite_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Invite")
            .build();
        let invite_row = gtk::Box::builder()
            .spacing(6)
            .css_classes(["linked"])
            .build();
        invite_row.append(&invite_entry);
        invite_row.append(&invite_button);
        content.append(&invite_row);
        {
            let (room, entry, window) = (room.clone(), invite_entry.clone(), self.window.clone());
            let do_invite = move || {
                let who = entry.text().trim().to_string();
                if !who.contains('@') {
                    return;
                }
                entry.set_text("");
                let (room, window) = (room.clone(), window.clone());
                glib::spawn_future_local(async move {
                    let target = who.clone();
                    let result = run(async move { room.invite(&target, "").await }).await;
                    if let Some(w) = window.upgrade() {
                        match result {
                            Ok(()) => w.toast(&format!("Invited {who}")),
                            Err(e) => w.toast(&client::explain(&e)),
                        }
                    }
                });
            };
            let again = do_invite.clone();
            invite_entry.connect_activate(move |_| again());
            invite_button.connect_clicked(move |_| do_invite());
        }
        content.append(&leave);
        self.people
            .set_popover(Some(&gtk::Popover::builder().child(&content).build()));

        let state: Rc<RefCell<String>> = Rc::default();
        {
            let (room, state, window) = (room.clone(), state.clone(), self.window.clone());
            leave.connect_clicked(move |_| {
                let (room, joined, window) =
                    (room.clone(), *state.borrow() == "joined", window.clone());
                glib::spawn_future_local(async move {
                    let result = run(async move {
                        if joined {
                            room.leave().await
                        } else {
                            room.join("", "").await
                        }
                    })
                    .await;
                    if let (Err(e), Some(w)) = (result, window.upgrade()) {
                        w.toast(&client::explain(&e));
                    }
                });
            });
        }

        // Anything changing: redraw the subtitle, the list and the button.
        let events = {
            let room = room.clone();
            client::forward(move || async move {
                let occupants = room.receive_occupants_changed().await.ok()?;
                let state = room.receive_state_changed().await;
                let subject = room.receive_subject_changed().await;
                Some(
                    futures_util::stream::once(async {}).chain(futures_util::stream::select(
                        occupants.map(|_| ()),
                        futures_util::stream::select(state.map(|_| ()), subject.map(|_| ())),
                    )),
                )
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while events.recv().await.is_ok() {
                let Some(this) = weak.upgrade() else { break };
                let r = room.clone();
                let Ok((now, subject, error, occupants, nick)) = run(async move {
                    Ok::<_, zbus::Error>((
                        r.state().await?,
                        r.subject().await?,
                        r.error().await?,
                        r.get_occupants().await?,
                        r.nick().await?,
                    ))
                })
                .await
                else {
                    continue;
                };
                if let Ok(room_jid) = this.room_jid() {
                    *this.me.borrow_mut() = format!("{room_jid}/{nick}");
                }
                this.title.set_subtitle(&match now.as_str() {
                    "joined" => subject.lines().next().unwrap_or("").to_owned(),
                    "joining" => "Joining…".to_owned(),
                    "error" => error,
                    _ => "You left this group chat".to_owned(),
                });
                leave.set_label(if now == "joined" { "Leave" } else { "Join" });
                if now == "joined" {
                    leave.add_css_class("destructive-action");
                    leave.remove_css_class("suggested-action");
                } else {
                    leave.add_css_class("suggested-action");
                    leave.remove_css_class("destructive-action");
                }
                this.people
                    .set_tooltip_text(Some(&format!("{} People", occupants.len())));
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                for o in &occupants {
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&text(o, "nick")).as_str())
                        .subtitle(glib::markup_escape_text(&text(o, "jid")).as_str())
                        .build();
                    let role = text(o, "affiliation");
                    if role == "owner" || role == "admin" {
                        row.add_suffix(
                            &gtk::Label::builder()
                                .label(if role == "owner" { "Owner" } else { "Admin" })
                                .css_classes(["caption", "dim-label"])
                                .build(),
                        );
                    }
                    list.append(&row);
                }
                *state.borrow_mut() = now;
            }
        });
    }

    /// Calls the peer: through the account that owns this conversation.
    async fn start_call(self: &Rc<Self>, video: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let path = self.proxy.inner().path().to_string();
        let account = path
            .rsplit_once("/conversation/")
            .map(|(a, _)| a.to_owned())
            .unwrap_or_default();
        let Some(info) = window
            .accounts
            .borrow()
            .iter()
            .find(|a| a.path.as_str() == account)
            .cloned()
        else {
            return;
        };
        let proxy = info.proxy.clone();
        let peer = self.proxy.clone();
        let started = run(async move {
            let jid = peer.peer().await?;
            proxy.start_call(&jid, video).await
        })
        .await;
        match started {
            Ok(path) => crate::calls::open(&window, path).await,
            Err(e) => window.toast(&format!("Couldn’t call: {}", client::explain(&e))),
        }
    }

    /// Whether the peer's apps support `feature`. Unknown (no answer yet,
    /// or a group chat) counts as yes, except for calls, which need it known.
    fn peer_supports(&self, feature: &str) -> bool {
        let features = self.peer_features.borrow();
        features.is_empty() || features.iter().any(|f| f == feature)
    }

    /// Call buttons for peers whose apps take Jingle calls (rung with
    /// XEP-0353), and whatever else depends on their features.
    fn follow_peer_features(self: &Rc<Self>) {
        let changes = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                // Subscribed before reading, so no change slips between.
                let later = proxy.receive_peer_features_changed().await;
                let first = proxy.peer_features().await.unwrap_or_default();
                Some(
                    futures_util::stream::once(async move { first })
                        .chain(later.filter_map(|c| async move { c.get().await.ok() })),
                )
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(features) = changes.recv().await {
                let Some(this) = weak.upgrade() else { break };
                let has = |f: &str| features.iter().any(|x| x == f);
                let rings = has("urn:xmpp:jingle-message:0") && has("urn:xmpp:jingle:1");
                let voice = rings && has("urn:xmpp:jingle:apps:rtp:audio");
                let video = rings && has("urn:xmpp:jingle:apps:rtp:video");
                if let Some(b) = this.call_button.borrow().as_ref() {
                    let menu = gtk::gio::Menu::new();
                    if voice {
                        menu.append(Some("_Voice Call"), Some("call.voice"));
                    }
                    if video {
                        menu.append(Some("V_ideo Call"), Some("call.video"));
                    }
                    b.set_menu_model(Some(&menu));
                    b.set_visible(voice || video);
                }
                *this.peer_features.borrow_mut() = features;
            }
        });
    }

    /// Hides Share a File when the server has no upload service.
    fn follow_server_features(self: &Rc<Self>) {
        let Some(account) = self.window.upgrade().and_then(|w| {
            w.accounts
                .borrow()
                .iter()
                .find(|a| a.path.as_str() == self.account)
                .map(|a| a.proxy.clone())
        }) else {
            return;
        };
        let changes = client::forward(move || async move {
            let later = account.receive_server_features_changed().await;
            let first = account.server_features().await.unwrap_or_default();
            Some(
                futures_util::stream::once(async move { first })
                    .chain(later.filter_map(|c| async move { c.get().await.ok() })),
            )
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(features) = changes.recv().await {
                let Some(this) = weak.upgrade() else { break };
                // Unknown (empty) until the server answers: keep it.
                this.attach.set_visible(
                    features.is_empty() || features.iter().any(|f| f == "urn:xmpp:http:upload:0"),
                );
            }
        });
    }

    /// Details and delete, for the conversation menu.
    fn install_actions(self: &Rc<Self>) {
        let group = gtk::gio::SimpleActionGroup::new();
        let details = gtk::gio::SimpleAction::new("details", None);
        let delete = gtk::gio::SimpleAction::new("delete", None);
        {
            let weak = Rc::downgrade(self);
            details.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade()
                    && let Some(window) = this.window.upgrade()
                {
                    crate::details::show(&window, &this);
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            delete.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.confirm_delete();
                }
            });
        }
        group.add_action(&details);
        group.add_action(&delete);
        self.page.insert_action_group("chat", Some(&group));
    }

    pub fn proxy(&self) -> &ConversationProxy<'static> {
        &self.proxy
    }

    pub fn window(&self) -> Option<std::rc::Weak<Window>> {
        Some(self.window.clone())
    }

    pub fn account_path(&self) -> &str {
        &self.account
    }

    /// Blocks or unblocks the peer, through the account.
    pub fn set_blocked(self: &Rc<Self>, blocked: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(account) = window
            .accounts
            .borrow()
            .iter()
            .find(|a| a.path.as_str() == self.account)
            .map(|a| a.proxy.clone())
        else {
            return;
        };
        let Ok(peer) = self.room_jid() else { return };
        let w = Rc::downgrade(&window);
        glib::spawn_future_local(async move {
            let done = run(async move {
                if blocked {
                    account.block(&peer).await
                } else {
                    account.unblock(&peer).await
                }
            })
            .await;
            if let Err(e) = done
                && let Some(w) = w.upgrade()
            {
                w.toast(&client::explain(&e));
            }
        });
    }

    /// Shows the banner and turns off the box while the peer is blocked.
    fn follow_blocked(self: &Rc<Self>) {
        let changes = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let later = proxy.receive_blocked_changed().await;
                let first = proxy.blocked().await.unwrap_or(false);
                Some(
                    futures_util::stream::once(async move { first })
                        .chain(later.filter_map(|c| async move { c.get().await.ok() })),
                )
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(blocked) = changes.recv().await {
                let Some(this) = weak.upgrade() else { break };
                this.blocked_banner.set_revealed(blocked);
                this.composer.set_sensitive(!blocked);
            }
        });
    }

    /// The peer's picture in the header, kept current with the name.
    async fn load_pictures(self: &Rc<Self>) {
        let proxy = self.proxy.clone();
        let picture = run(async move { proxy.avatar().await.unwrap_or_default() }).await;
        self.title
            .avatar
            .set_custom_image(texture(&picture).as_ref());
        let changes = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let picture = proxy.receive_avatar_changed().await;
                let title = proxy.receive_title_changed().await;
                Some(futures_util::stream::select(
                    picture.filter_map(|c| async move { c.get().await.ok().map(|p| (true, p)) }),
                    title.filter_map(|c| async move { c.get().await.ok().map(|t| (false, t)) }),
                ))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((is_picture, value)) = changes.recv().await {
                let Some(this) = weak.upgrade() else { break };
                if is_picture {
                    this.title.avatar.set_custom_image(texture(&value).as_ref());
                } else {
                    this.title.set_title(&value);
                    this.page.set_title(&value);
                }
            }
        });
    }

    pub fn confirm_delete(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let room = self.is_room.get();
        let ask = adw::AlertDialog::new(
            Some(if room {
                "Leave and Delete Group Chat?"
            } else {
                "Delete Conversation?"
            }),
            Some(if room {
                "You leave the group chat, it is removed from your other devices' bookmarks, and its history is deleted from this computer."
            } else {
                "Its history is deleted from this computer. It comes back if they write again."
            }),
        );
        ask.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
        ask.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        let proxy = self.proxy.clone();
        let w = Rc::downgrade(&window);
        ask.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let (proxy, w) = (proxy.clone(), w.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = run(async move { proxy.delete().await }).await
                    && let Some(w) = w.upgrade()
                {
                    w.toast(&client::explain(&e));
                }
            });
        });
        ask.present(Some(&window.window));
    }

    /// Tells the peer we're typing, and that we paused after 5 seconds.
    fn typed(self: &Rc<Self>, has_text: bool) {
        if self.is_room.get() {
            return;
        }
        if let Some(timer) = self.typing_timer.borrow_mut().take() {
            timer.remove();
        }
        self.chat_state(if has_text { "composing" } else { "active" });
        if has_text {
            let weak = Rc::downgrade(self);
            let id = glib::timeout_add_seconds_local_once(5, move || {
                if let Some(this) = weak.upgrade() {
                    this.typing_timer.borrow_mut().take();
                    this.chat_state("paused");
                }
            });
            *self.typing_timer.borrow_mut() = Some(id);
        }
    }

    fn chat_state(&self, state: &'static str) {
        let proxy = self.proxy.clone();
        glib::spawn_future_local(async move {
            let _ = run(async move { proxy.set_chat_state(state).await }).await;
        });
    }

    /// Shows "typing…" under the title while the peer types.
    fn follow_typing(self: &Rc<Self>) {
        let changes = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let later = proxy.receive_peer_state_changed().await;
                Some(later.filter_map(|c| async move { c.get().await.ok() }))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(state) = changes.recv().await {
                let Some(this) = weak.upgrade() else { break };
                if state == "composing" {
                    this.title.set_subtitle("typing…");
                } else {
                    this.title.set_subtitle(&this.subtitle.borrow());
                }
            }
        });
    }

    /// Keeps the send button and the composer's hint in step with
    /// Encryption.
    fn follow_encryption(self: &Rc<Self>) {
        let changes = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let later = proxy.receive_encryption_changed().await;
                let first = proxy.encryption().await.unwrap_or_default();
                Some(
                    futures_util::stream::once(async move { first })
                        .chain(later.filter_map(|c| async move { c.get().await.ok() })),
                )
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(encryption) = changes.recv().await {
                let Some(this) = weak.upgrade() else { break };
                let encrypted = encryption == "omemo" || encryption == "omemo2";
                if encrypted {
                    this.send_lock
                        .set_icon_name(Some("changes-prevent-symbolic"));
                    this.send_lock.set_css_classes(&["send-lock", "closed"]);
                    this.send.set_tooltip_text(Some("Send Encrypted"));
                    this.send.set_css_classes(&["suggested-action", "send"]);
                } else {
                    this.send_lock.set_icon_name(Some("changes-allow-symbolic"));
                    this.send_lock.set_css_classes(&["send-lock", "open"]);
                    this.send.set_tooltip_text(Some("Send Unencrypted"));
                    this.send.set_css_classes(&["suggested-action", "send"]);
                }
                this.entry.set_placeholder_text(Some(if encrypted {
                    "Encrypted Message"
                } else {
                    "Unencrypted Message"
                }));
            }
        });
    }

    async fn load_older(self: &Rc<Self>) {
        if self.loading.get() || self.at_start.get() {
            return;
        }
        self.loading.set(true);
        let before = self.oldest.borrow().clone();
        let proxy = self.proxy.clone();
        let page = run(async move { proxy.get_messages(&before, PAGE).await }).await;
        if let Ok(page) = page {
            if (page.len() as u32) < PAGE {
                self.at_start.set(true);
            }
            if let Some(first) = page.first() {
                *self.oldest.borrow_mut() = text(first, "id");
            }
            // Prepend, keeping the view where it was.
            let adj = self.scroller.vadjustment();
            let from_bottom = adj.upper() - adj.value();
            for m in page.iter().rev() {
                let row = self.row(m);
                self.messages.prepend(&row);
            }
            self.regroup();
            let scroller = self.scroller.clone();
            glib::idle_add_local_once(move || {
                let adj = scroller.vadjustment();
                adj.set_value(adj.upper() - from_bottom);
            });
        }
        self.loading.set(false);
    }

    fn append(self: &Rc<Self>, m: &Dict) {
        let row = self.row(m);
        self.messages.append(&row);
        self.regroup();
    }

    /// Shows the picture and name only on the first message of a run: the
    /// same sender, each within five minutes of the one before.
    fn regroup(&self) {
        const GAP_MS: i64 = 5 * 60 * 1000;
        let rows = self.rows.borrow();
        let decorations = self.decorations.borrow();
        let mut previous: Option<(String, i64)> = None;
        let mut child = self.messages.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            let id = widget.widget_name();
            let Some((_, m)) = rows.get(id.as_str()) else {
                continue;
            };
            let sender = if flag(m, "outgoing") {
                String::new()
            } else if self.is_room.get() {
                text(m, "sender")
            } else {
                // Any of the peer's devices.
                text(m, "sender").split('/').next().unwrap_or("").to_owned()
            };
            let at = m
                .get("timestamp")
                .and_then(|v| i64::try_from(v).ok())
                .unwrap_or(0);
            let first = previous
                .as_ref()
                .is_none_or(|(s, t)| *s != sender || at - t > GAP_MS);
            if let Some(label) = decorations.get(id.as_str()) {
                label.set_visible(first);
            }
            // Space above a run, none within it. Margins rather than CSS
            // padding: the list doesn't re-measure rows when a class
            // changes their padding, and they'd spill past their space.
            if let Some(line) = widget.first_child() {
                line.set_margin_top(if first { 6 } else { 0 });
            }
            previous = Some((sender, at));
        }
    }

    fn row(self: &Rc<Self>, m: &Dict) -> gtk::Box {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["message-row"])
            .name(text(m, "id"))
            .build();
        set_child(&row, &self.message(m));
        self.rows
            .borrow_mut()
            .insert(text(m, "id"), (row.clone(), m.clone()));
        row
    }

    /// Applies a MessageChanged: merges the changes and redraws the bubble.
    fn update(self: &Rc<Self>, id: &str, changes: Dict) {
        let found = self.rows.borrow().get(id).cloned();
        let Some((row, mut m)) = found else { return };
        for (k, v) in changes {
            m.insert(k, v);
        }
        set_child(&row, &self.message(&m));
        self.rows.borrow_mut().insert(id.to_owned(), (row, m));
        self.regroup();
    }

    /// A message with the sender's picture and name beside it: on the left
    /// for others, on the right for us.
    /// A message with its menu beside it. Group chats also name the
    /// sender (on the first message of a run); a chat with one person
    /// doesn't need to.
    fn message(self: &Rc<Self>, m: &Dict) -> gtk::Widget {
        let outgoing = flag(m, "outgoing");
        let column = self.bubble(m);
        if self.is_room.get() && !outgoing {
            let sender = text(m, "sender");
            let nick = sender
                .split_once('/')
                .map(|(_, n)| n.to_owned())
                .unwrap_or(sender);
            let label = gtk::Label::builder()
                .label(&nick)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["message-sender"])
                .build();
            if let Some(column) = column.downcast_ref::<gtk::Box>() {
                column.prepend(&label);
            }
            self.decorations.borrow_mut().insert(text(m, "id"), label);
        }
        let line = gtk::Box::builder()
            .spacing(6)
            .halign(if outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .css_classes(["message", if outgoing { "outgoing" } else { "incoming" }])
            .build();
        // The message's menu, on the side away from the sender.
        let dots = gtk::Button::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Message Menu")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular", "message-menu"])
            .visible(!flag(m, "retracted") && text(m, "encryption") != "omemo-failed")
            .build();
        let (weak, id) = (Rc::downgrade(self), text(m, "id"));
        dots.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                this.menu(b.upcast_ref(), &id, outgoing, None);
            }
        });
        if outgoing {
            line.append(&dots);
            line.append(&column);
        } else {
            line.append(&column);
            line.append(&dots);
        }
        line.upcast()
    }

    fn bubble(self: &Rc<Self>, m: &Dict) -> gtk::Widget {
        let id = text(m, "id");
        let outgoing = flag(m, "outgoing");
        let retracted = flag(m, "retracted");
        let encryption = text(m, "encryption");
        let undecryptable = encryption == "omemo-failed";

        let bubble = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["bubble", if outgoing { "outgoing" } else { "incoming" }])
            .build();

        let preview = text(m, "reply-preview");
        if !text(m, "reply-to").is_empty() && !retracted {
            let who = text(m, "reply-to-sender");
            let who = who
                .split_once('/')
                .map(|(_, n)| n.to_owned())
                .unwrap_or(who);
            let who = if who == self.own_jid {
                "You".to_owned()
            } else {
                who
            };
            let quote = gtk::Label::builder()
                .label(format!("{who}\n{preview}"))
                .xalign(0.0)
                .wrap(true)
                .lines(3)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["quote", "caption"])
                .build();
            bubble.append(&quote);
        }

        let (body_text, dimmed) = if retracted {
            ("This message was deleted".to_owned(), true)
        } else if undecryptable {
            (
                "This message was encrypted for other devices and can’t be read here.".to_owned(),
                true,
            )
        } else {
            (text(m, "body"), false)
        };
        let file_state = text(m, "file-state");
        let has_file = !file_state.is_empty() && !retracted;
        if has_file {
            bubble.append(&self.attachment(m));
        }
        // A file's message is only its link: don't show that too.
        if !(has_file && (body_text.is_empty() || body_text == text(m, "file-url"))) {
            let (label, use_markup) = if dimmed {
                (body_text, false)
            } else {
                (crate::styling::markup(&body_text), true)
            };
            let body = gtk::Label::builder()
                .label(label)
                .use_markup(use_markup)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .selectable(!dimmed)
                .xalign(0.0)
                .build();
            if dimmed {
                body.add_css_class("undecryptable");
            }
            bubble.append(&body);
        }

        // Under their messages on the left, under ours on the right.
        let meta = gtk::Box::builder()
            .spacing(4)
            .halign(if outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .build();
        if flag(m, "edited") && !retracted {
            meta.append(
                &gtk::Label::builder()
                    .label("edited")
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
        }
        if encryption == "omemo-untrusted" {
            meta.append(
                &gtk::Image::builder()
                    .icon_name("dialog-warning-symbolic")
                    .tooltip_text("Sent from a device you haven’t trusted")
                    .pixel_size(12)
                    .build(),
            );
        }
        // Not encrypted: an open red padlock on the outer side of the time.
        let unencrypted = (encryption == "none" && !retracted).then(|| {
            gtk::Image::builder()
                .icon_name("changes-allow-symbolic")
                .tooltip_text("Not encrypted")
                .pixel_size(12)
                .css_classes(["message-unencrypted"])
                .build()
        });
        if !outgoing && let Some(lock) = &unencrypted {
            meta.append(lock);
        }
        meta.append(
            &gtk::Label::builder()
                .label(time_of(m))
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        if outgoing && let Some(lock) = &unencrypted {
            // Ours: between the time and the checks.
            meta.append(lock);
        }
        if outgoing {
            let state = text(m, "state");
            // Dimmed until the server has it; then a check once delivered
            // and two once read.
            if state == "pending" {
                bubble.add_css_class("pending");
                bubble.set_tooltip_text(Some(state_name(&state)));
            }
            let icon = state_icon(&state);
            if !icon.is_empty() {
                meta.append(
                    &gtk::Image::builder()
                        .icon_name(icon)
                        .tooltip_text(state_name(&state))
                        .pixel_size(16)
                        .css_classes(["message-state"])
                        .build(),
                );
            }
        }
        bubble.append(&meta);

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .halign(if outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .css_classes([if outgoing { "outgoing" } else { "incoming" }])
            .build();
        column.append(&bubble);
        let all = reactions(m);
        if !all.is_empty() {
            column.append(&self.reaction_bar(&id, &all, outgoing));
        }

        if !retracted && !undecryptable {
            self.attach_menu(&bubble, &id, outgoing);
        }
        column.upcast()
    }

    /// A shared file: the picture's thumbnail, or a row with the file's
    /// name and a button to download or open it.
    fn attachment(self: &Rc<Self>, m: &Dict) -> gtk::Widget {
        let id = text(m, "id");
        let state = text(m, "file-state");
        let path = text(m, "file-path");
        let thumbnail = text(m, "file-thumbnail");
        let name = text(m, "file-name");
        if !thumbnail.is_empty() && !path.is_empty() && flag(m, "file-animated") {
            let p = path.clone();
            let player = crate::animation::player(&thumbnail, &path, move || open_file(&p));
            player.set_tooltip_text(Some(&name));
            return player;
        }
        if !thumbnail.is_empty() {
            let picture = gtk::Picture::for_filename(&thumbnail);
            picture.set_can_shrink(true);
            picture.set_content_fit(gtk::ContentFit::Cover);
            picture.set_size_request(240, 180);
            picture.add_css_class("attachment-picture");
            picture.set_tooltip_text(Some(&name));
            let click = gtk::GestureClick::new();
            let p = path.clone();
            click.connect_released(move |_, _, _, _| open_file(&p));
            picture.add_controller(click);
            return picture.upcast();
        }
        let row = gtk::Box::builder()
            .spacing(8)
            .css_classes(["attachment"])
            .build();
        let icon = if text(m, "file-mime").starts_with("image/") {
            "image-x-generic-symbolic"
        } else {
            "text-x-generic-symbolic"
        };
        row.append(&gtk::Image::builder().icon_name(icon).pixel_size(24).build());
        let size = m
            .get("file-size")
            .and_then(|v| i64::try_from(v).ok())
            .filter(|s| *s >= 0)
            .map(|s| glib::format_size(s as u64).to_string())
            .unwrap_or_default();
        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        labels.append(
            &gtk::Label::builder()
                .label(&name)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(28)
                .build(),
        );
        let detail = match state.as_str() {
            "uploading" => "Uploading…".to_owned(),
            "downloading" => "Downloading…".to_owned(),
            "failed" => "Couldn’t transfer the file".to_owned(),
            _ => size,
        };
        if !detail.is_empty() {
            labels.append(
                &gtk::Label::builder()
                    .label(detail)
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
        }
        row.append(&labels);
        match state.as_str() {
            "uploading" | "downloading" => row.append(&crate::spinner()),
            "done" if !path.is_empty() => {
                let open = gtk::Button::builder()
                    .icon_name("document-open-symbolic")
                    .tooltip_text("Open")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .build();
                open.connect_clicked(move |_| open_file(&path));
                row.append(&open);
            }
            "remote" | "failed" => {
                let get = gtk::Button::builder()
                    .icon_name("folder-download-symbolic")
                    .tooltip_text("Download")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .build();
                let (proxy, window) = (self.proxy.clone(), self.window.clone());
                get.connect_clicked(move |_| {
                    let (proxy, id, window) = (proxy.clone(), id.clone(), window.clone());
                    glib::spawn_future_local(async move {
                        let result = run(async move { proxy.download_file(&id).await }).await;
                        if let (Err(e), Some(w)) = (result, window.upgrade()) {
                            w.toast(&client::explain(&e));
                        }
                    });
                });
                row.append(&get);
            }
            _ => {}
        }
        row.upcast()
    }

    /// Asks for a file and shares it.
    /// Shows a pasted picture and sends it (as PNG) if wanted.
    fn offer_image(self: &Rc<Self>, texture: gdk::Texture) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let picture = gtk::Picture::builder()
            .paintable(&texture)
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .height_request(220)
            .css_classes(["attachment-picture"])
            .build();
        let ask = adw::AlertDialog::new(Some("Send Image?"), None);
        ask.set_extra_child(Some(&picture));
        ask.add_responses(&[("cancel", "_Cancel"), ("send", "_Send")]);
        ask.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        ask.set_default_response(Some("send"));
        let (proxy, weak) = (self.proxy.clone(), self.window.clone());
        ask.connect_response(None, move |_, response| {
            if response != "send" {
                return;
            }
            let png = texture.save_to_png_bytes();
            let (proxy, weak) = (proxy.clone(), weak.clone());
            glib::spawn_future_local(async move {
                let sent = run(async move {
                    // Written to a private temporary file for the daemon to
                    // read, and removed once it has.
                    let path = glib::tmp_dir().join(format!(
                        "piopio-pasted-{}-{}.png",
                        std::process::id(),
                        glib::monotonic_time()
                    ));
                    std::fs::write(&path, &png)
                        .map_err(|e| zbus::Error::Failure(format!("can't keep the image: {e}")))?;
                    let file = std::fs::File::open(&path);
                    let _ = std::fs::remove_file(&path);
                    let file = file
                        .map_err(|e| zbus::Error::Failure(format!("can't read the image: {e}")))?;
                    let name = format!(
                        "image-{}.png",
                        glib::DateTime::now_local()
                            .ok()
                            .and_then(|d| d.format("%Y%m%d-%H%M%S").ok())
                            .map(|s| s.to_string())
                            .unwrap_or_default()
                    );
                    proxy
                        .send_file(zbus::zvariant::Fd::from(&file), &name, HashMap::new())
                        .await
                })
                .await;
                if let (Err(e), Some(w)) = (sent, weak.upgrade()) {
                    w.toast(&format!("Not sent: {}", client::explain(&e)));
                }
            });
        });
        ask.present(Some(&window.window));
    }

    fn pick_file(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Share a File")
            .modal(true)
            .build();
        let (proxy, weak) = (self.proxy.clone(), self.window.clone());
        dialog.open(
            Some(&window.window),
            gtk::gio::Cancellable::NONE,
            move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                send_path(proxy.clone(), weak.clone(), path);
            },
        );
    }

    /// Asks before sending dropped files, showing a single picture.
    fn offer_files(self: &Rc<Self>, paths: Vec<std::path::PathBuf>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let name_of = |p: &std::path::Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into())
        };
        let (heading, body) = match paths.as_slice() {
            [one] => (format!("Send “{}”?", name_of(one)), None),
            many => (
                format!("Send {} Files?", many.len()),
                Some(
                    many.iter()
                        .map(|p| name_of(p))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            ),
        };
        let ask = adw::AlertDialog::new(Some(&heading), body.as_deref());
        if let [one] = paths.as_slice()
            && let Ok(texture) = gdk::Texture::from_filename(one)
        {
            let picture = gtk::Picture::for_paintable(&texture);
            picture.set_can_shrink(true);
            picture.set_size_request(-1, 200);
            ask.set_extra_child(Some(&picture));
        }
        ask.add_responses(&[("cancel", "_Cancel"), ("send", "_Send")]);
        ask.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        ask.set_default_response(Some("send"));
        let (proxy, weak) = (self.proxy.clone(), self.window.clone());
        ask.connect_response(None, move |_, response| {
            if response == "send" {
                for path in &paths {
                    send_path(proxy.clone(), weak.clone(), path.clone());
                }
            }
        });
        ask.present(Some(&window.window));
    }

    /// Reaction chips under a message; tapping one adds or removes ours.
    fn reaction_bar(
        self: &Rc<Self>,
        id: &str,
        all: &HashMap<String, Vec<String>>,
        outgoing: bool,
    ) -> gtk::Box {
        // Count each emoji, in a stable order.
        let mut counts: Vec<(String, usize, bool)> = Vec::new();
        let mut senders: Vec<&String> = all.keys().collect();
        senders.sort();
        for sender in senders {
            for emoji in &all[sender] {
                let mine = **sender == *self.me.borrow();
                match counts.iter_mut().find(|(e, _, _)| e == emoji) {
                    Some(entry) => {
                        entry.1 += 1;
                        entry.2 |= mine;
                    }
                    None => counts.push((emoji.clone(), 1, mine)),
                }
            }
        }
        let bar = gtk::Box::builder()
            .spacing(4)
            .halign(if outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .css_classes(["reactions"])
            .build();
        for (emoji, count, mine) in counts {
            let chip = gtk::ToggleButton::builder()
                .label(format!("{emoji} {count}"))
                .active(mine)
                .tooltip_text(if mine {
                    "Remove your reaction"
                } else {
                    "React with this too"
                })
                .css_classes(["reaction"])
                .build();
            let weak = Rc::downgrade(self);
            let id = id.to_owned();
            chip.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.toggle_reaction(&id, &emoji);
                }
            });
            bar.append(&chip);
        }
        bar
    }

    fn attach_menu(self: &Rc<Self>, bubble: &gtk::Box, id: &str, outgoing: bool) {
        let open = {
            let weak = Rc::downgrade(self);
            let bubble = bubble.clone();
            let id = id.to_owned();
            move |x: f64, y: f64| {
                if let Some(this) = weak.upgrade() {
                    this.menu(bubble.upcast_ref(), &id, outgoing, Some((x, y)));
                }
            }
        };
        // Captured before the text's own menu (the body is selectable).
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let open2 = open.clone();
        click.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            open2(x, y);
        });
        bubble.add_controller(click);
        let press = gtk::GestureLongPress::builder()
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        press.connect_pressed(move |g, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            open(x, y);
        });
        bubble.add_controller(press);
    }

    /// The message's menu: reactions, Reply and Copy, and for our own
    /// messages Edit and Delete for Everyone. Points at `at` within
    /// `parent`, or at all of it.
    fn menu(
        self: &Rc<Self>,
        parent: &gtk::Widget,
        id: &str,
        outgoing: bool,
        at: Option<(f64, f64)>,
    ) {
        let popover = gtk::Popover::new();
        if let Some((x, y)) = at {
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        }
        popover.set_parent(parent);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();

        let can_react = self.peer_supports("urn:xmpp:reactions:0");
        let can_edit = self.peer_supports("urn:xmpp:message-correct:0");
        let can_retract = self.peer_supports("urn:xmpp:message-retract:1")
            || self.peer_supports("urn:xmpp:message-retract:0");
        let emojis = gtk::Box::builder().spacing(2).visible(can_react).build();
        for emoji in QUICK_REACTIONS {
            let b = gtk::Button::builder()
                .label(*emoji)
                .tooltip_text("React")
                .css_classes(["flat", "circular"])
                .build();
            let (weak, id, p) = (Rc::downgrade(self), id.to_owned(), popover.clone());
            b.connect_clicked(move |_| {
                p.popdown();
                if let Some(this) = weak.upgrade() {
                    this.toggle_reaction(&id, emoji);
                }
            });
            emojis.append(&b);
        }
        // Any other emoji, from GTK's picker.
        let more = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("More Reactions")
            .css_classes(["flat", "circular"])
            .build();
        {
            let (weak, id, p) = (Rc::downgrade(self), id.to_owned(), popover.clone());
            more.connect_clicked(move |b| {
                let chooser = gtk::EmojiChooser::new();
                chooser.set_parent(b);
                let (weak, id, p) = (weak.clone(), id.clone(), p.clone());
                chooser.connect_emoji_picked(move |chooser, emoji| {
                    chooser.popdown();
                    p.popdown();
                    if let Some(this) = weak.upgrade() {
                        this.toggle_reaction(&id, emoji);
                    }
                });
                chooser.connect_closed(|c| {
                    let c = c.clone();
                    glib::idle_add_local_once(move || c.unparent());
                });
                chooser.popup();
            });
        }
        emojis.append(&more);
        content.append(&emojis);

        let item = |label: &str, action: MenuAction| {
            let text = gtk::Label::builder().label(label).xalign(0.0).build();
            let b = gtk::Button::builder()
                .child(&text)
                .css_classes(["flat"])
                .build();
            let (weak, p) = (Rc::downgrade(self), popover.clone());
            b.connect_clicked(move |_| {
                p.popdown();
                if let Some(this) = weak.upgrade() {
                    action(&this);
                }
            });
            b
        };
        let reply_id = id.to_owned();
        content.append(&item(
            "Reply",
            Box::new(move |this| this.set_mode(Mode::Reply(reply_id.clone()))),
        ));
        let message = self.rows.borrow().get(id).map(|(_, m)| m.clone());
        let file = message.as_ref().and_then(|m| {
            let path = text(m, "file-path");
            (text(m, "file-state") == "done" && !path.is_empty()).then(|| {
                (
                    path,
                    text(m, "file-name"),
                    text(m, "file-mime").starts_with("image/"),
                )
            })
        });
        let has_file = message
            .as_ref()
            .is_some_and(|m| !text(m, "file-state").is_empty());
        if let Some((path, name, image)) = file {
            if image {
                let path = path.clone();
                content.append(&item(
                    "Copy Image",
                    Box::new(move |this| this.copy_image(&path)),
                ));
            }
            content.append(&item(
                "Save…",
                Box::new(move |this| this.save_file(&path, &name)),
            ));
        }
        // A file's message is its link: nothing to copy as text.
        if !has_file {
            let copy_id = id.to_owned();
            content.append(&item(
                "Copy Text",
                Box::new(move |this| {
                    if let Some((_, m)) = this.rows.borrow().get(&copy_id) {
                        this.entry.clipboard().set_text(&text(m, "body"));
                    }
                }),
            ));
        }
        if outgoing && can_edit {
            let edit_id = id.to_owned();
            content.append(&item(
                "Edit",
                Box::new(move |this| this.set_mode(Mode::Edit(edit_id.clone()))),
            ));
        }
        if outgoing && can_retract {
            let delete_id = id.to_owned();
            content.append(&item(
                "Delete for Everyone",
                Box::new(move |this| this.confirm_retract(delete_id.clone())),
            ));
        }
        popover.set_child(Some(&content));
        // Focus back to the box to write in: left to itself, it lands on
        // the message's text and selects all of it.
        let weak = Rc::downgrade(self);
        popover.connect_closed(move |p| {
            let p = p.clone();
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                p.unparent();
                if let Some(this) = weak.upgrade() {
                    this.entry.grab_focus();
                }
            });
        });
        popover.popup();
    }

    /// Puts a picture itself on the clipboard (the first frame, if it
    /// moves).
    fn copy_image(self: &Rc<Self>, path: &str) {
        match gdk::Texture::from_filename(path) {
            Ok(texture) => self.entry.clipboard().set_texture(&texture),
            Err(e) => {
                if let Some(w) = self.window.upgrade() {
                    w.toast(&format!("Couldn’t copy the picture: {}", e.message()));
                }
            }
        }
    }

    /// Asks where to save a received or sent file, and copies it there.
    fn save_file(self: &Rc<Self>, path: &str, name: &str) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Save File")
            .modal(true)
            .initial_name(name)
            .build();
        let source = gtk::gio::File::for_path(path);
        let w = Rc::downgrade(&window);
        dialog.save(
            Some(&window.window),
            gtk::gio::Cancellable::NONE,
            move |chosen| {
                let Ok(dest) = chosen else { return };
                glib::spawn_future_local(async move {
                    let copied = source
                        .copy_future(
                            &dest,
                            gtk::gio::FileCopyFlags::OVERWRITE,
                            glib::Priority::DEFAULT,
                        )
                        .0
                        .await;
                    if let Some(w) = w.upgrade() {
                        match copied {
                            Ok(()) => w.toast("Saved"),
                            Err(e) => w.toast(&format!("Couldn’t save the file: {}", e.message())),
                        }
                    }
                });
            },
        );
    }

    fn set_mode(self: &Rc<Self>, mode: Mode) {
        let message = |id: &str| self.rows.borrow().get(id).map(|(_, m)| m.clone());
        match &mode {
            Mode::New => {
                if matches!(*self.mode.borrow(), Mode::Edit(_)) {
                    self.entry.set_text("");
                }
                self.mode_bar.set_visible(false);
            }
            Mode::Reply(id) => {
                let Some(m) = message(id) else { return };
                let first = text(&m, "body").lines().next().unwrap_or("").to_owned();
                self.mode_label.set_label(&format!("Replying to “{first}”"));
                self.mode_bar.set_visible(true);
            }
            Mode::Edit(id) => {
                let Some(m) = message(id) else { return };
                self.entry.set_text(&text(&m, "body"));
                self.mode_label.set_label("Editing message");
                self.mode_bar.set_visible(true);
            }
        }
        *self.mode.borrow_mut() = mode;
        self.entry.grab_focus();
    }

    /// The newest message we sent that can still be edited.
    fn last_own_message(&self) -> Option<String> {
        let rows = self.rows.borrow();
        rows.values()
            .filter(|(_, m)| flag(m, "outgoing") && !flag(m, "retracted"))
            .max_by_key(|(_, m)| {
                (
                    m.get("timestamp")
                        .and_then(|v| i64::try_from(v).ok())
                        .unwrap_or(0),
                    text(m, "id").parse::<i64>().unwrap_or(0),
                )
            })
            .map(|(_, m)| text(m, "id"))
    }

    fn toggle_reaction(self: &Rc<Self>, id: &str, emoji: &str) {
        let Some((_, m)) = self.rows.borrow().get(id).cloned() else {
            return;
        };
        let me = self.me.borrow().clone();
        let mut mine = reactions(&m).remove(&me).unwrap_or_default();
        if let Some(i) = mine.iter().position(|e| e == emoji) {
            mine.remove(i);
        } else {
            mine.push(emoji.to_owned());
        }
        let (proxy, id) = (self.proxy.clone(), id.to_owned());
        let window = self.window.clone();
        glib::spawn_future_local(async move {
            let result = run(async move {
                let refs: Vec<&str> = mine.iter().map(String::as_str).collect();
                proxy.react(&id, &refs).await
            })
            .await;
            if let (Err(e), Some(w)) = (result, window.upgrade()) {
                w.toast(&format!("Reaction not sent: {}", client::explain(&e)));
            }
        });
    }

    fn confirm_retract(self: &Rc<Self>, id: String) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let ask = adw::AlertDialog::new(
            Some("Delete for Everyone?"),
            Some(
                "The message is removed here and for everyone whose app supports it. Others may already have read it.",
            ),
        );
        ask.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
        ask.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        let proxy = self.proxy.clone();
        let w = Rc::downgrade(&window);
        ask.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let (proxy, id, w) = (proxy.clone(), id.clone(), w.clone());
            glib::spawn_future_local(async move {
                let result = run(async move { proxy.retract(&id).await }).await;
                if let (Err(e), Some(w)) = (result, w.upgrade()) {
                    w.toast(&format!("Not deleted: {}", client::explain(&e)));
                }
            });
        });
        ask.present(Some(&window.window));
    }

    /// Keeps the view at the end while it's there: whenever the history
    /// grows (a message from either side, a picture loading, an edit) or
    /// the view shrinks, it follows. Scrolling up lets go; scrolling back
    /// down to the end holds on again.
    fn stick_to_end(self: &Rc<Self>) {
        let adj = self.scroller.vadjustment();
        // The size the view had when the user last scrolled: a value change
        // that comes with a new size is layout, not the user.
        let size = Rc::new(Cell::new((adj.upper(), adj.page_size())));
        let follow = {
            let weak = Rc::downgrade(self);
            let size = size.clone();
            move |adj: &gtk::Adjustment| {
                let Some(this) = weak.upgrade() else { return };
                size.set((adj.upper(), adj.page_size()));
                // Only once layout has settled (as Sanctum's Assist does): a
                // value set during layout is ignored by the view, and setting
                // the same value again later doesn't move it.
                if this.stuck.get() && !this.loading.get() {
                    this.scroll_to_end();
                }
            }
        };
        let weak = Rc::downgrade(self);
        let (f, size2) = (follow.clone(), size.clone());
        adj.connect_value_changed(move |adj| {
            let Some(this) = weak.upgrade() else { return };
            if this.following.get() {
                return;
            }
            if size2.get() != (adj.upper(), adj.page_size()) {
                f(adj);
            } else {
                this.stuck.set(this.near_end());
            }
        });
        adj.connect_changed(move |a| follow(a));
    }

    fn near_end(&self) -> bool {
        let adj = self.scroller.vadjustment();
        adj.upper() - adj.page_size() - adj.value() < 80.0
    }

    fn scroll_to_end(self: &Rc<Self>) {
        self.stuck.set(true);
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else { return };
            let adj = this.scroller.vadjustment();
            this.following.set(true);
            adj.set_value(adj.upper() - adj.page_size());
            this.following.set(false);
        });
    }

    fn send(self: &Rc<Self>) {
        let body = self.entry.text().to_string();
        if body.trim().is_empty() {
            return;
        }
        let mode = self.mode.borrow().clone();
        self.entry.set_text("");
        *self.mode.borrow_mut() = Mode::New;
        self.mode_bar.set_visible(false);
        // Keep typing after tapping Send, as with Enter.
        self.entry.grab_focus();
        let proxy = self.proxy.clone();
        let window = self.window.clone();
        let entry = self.entry.clone();
        glib::spawn_future_local(async move {
            let text = body.clone();
            let sent = run(async move {
                match mode {
                    Mode::Edit(id) => proxy.correct(&id, &text).await,
                    Mode::Reply(id) => {
                        let options = HashMap::from([("reply-to", Value::from(id.as_str()))]);
                        proxy.send(&text, options).await.map(|_| ())
                    }
                    Mode::New => proxy.send(&text, HashMap::new()).await.map(|_| ()),
                }
            })
            .await;
            if let Err(e) = sent {
                if let Some(window) = window.upgrade() {
                    window.toast(&format!("Not sent: {}", client::explain(&e)));
                }
                // Give the text back rather than lose it.
                if entry.text().is_empty() {
                    entry.set_text(&body);
                }
            }
        });
        self.scroll_to_end();
    }
}

/// Opens a downloaded file with the app the user chose for its type.
fn open_file(path: &str) {
    if path.is_empty() {
        return;
    }
    let file = gtk::gio::File::for_path(path);
    gtk::FileLauncher::new(Some(&file)).launch(
        None::<&gtk::Window>,
        gtk::gio::Cancellable::NONE,
        |_| {},
    );
}

fn text(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn flag(dict: &Dict, key: &str) -> bool {
    dict.get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

fn reactions(dict: &Dict) -> HashMap<String, Vec<String>> {
    dict.get("reactions")
        .and_then(|v: &OwnedValue| {
            HashMap::<String, Vec<String>>::try_from(v.try_clone().ok()?).ok()
        })
        .unwrap_or_default()
}

/// The chat's title in the header: the peer's picture with their name and
/// address beside it, left-aligned.
#[derive(Clone)]
struct HeaderTitle {
    widget: gtk::Box,
    avatar: adw::Avatar,
    title: gtk::Label,
    subtitle: gtk::Label,
}

impl HeaderTitle {
    fn new() -> Self {
        let avatar = adw::Avatar::new(32, None, true);
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title"])
            .build();
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .css_classes(["subtitle"])
            .build();
        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        labels.append(&title);
        labels.append(&subtitle);
        let widget = gtk::Box::builder()
            .spacing(10)
            .margin_start(4)
            .css_classes(["chat-title"])
            .build();
        widget.append(&avatar);
        widget.append(&labels);
        Self {
            widget,
            avatar,
            title,
            subtitle,
        }
    }

    fn set_title(&self, title: &str) {
        self.title.set_label(title);
        self.avatar.set_text(Some(title));
    }

    fn set_subtitle(&self, subtitle: &str) {
        self.subtitle.set_label(subtitle);
        self.subtitle.set_visible(!subtitle.is_empty());
    }
}

/// The box to write in: grows with what's written (up to a few lines, then
/// scrolls), Enter sends and Shift+Enter starts a new line.
#[derive(Clone)]
struct Composer {
    widget: gtk::Widget,
    view: gtk::TextView,
    placeholder: gtk::Label,
}

impl Composer {
    const MAX_HEIGHT: i32 = 180;

    fn new() -> Self {
        let view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .input_hints(gtk::InputHints::SPELLCHECK | gtk::InputHints::EMOJI)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .css_classes(["composer-text"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .max_content_height(Self::MAX_HEIGHT)
            .hexpand(true)
            .child(&view)
            .build();
        let placeholder = gtk::Label::builder()
            .label("Message")
            .xalign(0.0)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(11)
            .margin_top(8)
            .can_target(false)
            .css_classes(["dim-label"])
            .build();
        let overlay = gtk::Overlay::builder()
            .child(&scroller)
            .hexpand(true)
            .css_classes(["composer-box"])
            .build();
        overlay.add_overlay(&placeholder);
        let composer = Self {
            widget: overlay.upcast(),
            view,
            placeholder,
        };
        let placeholder = composer.placeholder.clone();
        // No scrollbar until the text outgrows the box: even unused, it
        // makes the box taller than one line.
        let (view, s) = (composer.view.clone(), scroller.clone());
        composer.view.buffer().connect_changed(move |b| {
            placeholder.set_visible(b.char_count() == 0);
            let (view, s) = (view.clone(), s.clone());
            glib::idle_add_local_once(move || {
                let (_, natural, _, _) = view.measure(gtk::Orientation::Vertical, view.width());
                s.set_vscrollbar_policy(if natural > Self::MAX_HEIGHT {
                    gtk::PolicyType::Automatic
                } else {
                    gtk::PolicyType::External
                });
            });
        });
        composer
    }

    fn text(&self) -> String {
        let buffer = self.view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// Replaces the text, leaving the cursor at its end.
    fn set_text(&self, text: &str) {
        let buffer = self.view.buffer();
        buffer.set_text(text);
        buffer.place_cursor(&buffer.end_iter());
    }

    fn set_placeholder_text(&self, text: Option<&str>) {
        self.placeholder.set_label(text.unwrap_or(""));
    }

    fn grab_focus(&self) {
        self.view.grab_focus();
    }

    fn clipboard(&self) -> gdk::Clipboard {
        self.view.clipboard()
    }

    fn connect_changed(&self, f: impl Fn(&str) + 'static) {
        let this = self.clone();
        self.view.buffer().connect_changed(move |_| f(&this.text()));
    }

    /// Enter (without Shift) sends.
    fn connect_activate(&self, f: impl Fn() + 'static) {
        let keys = gtk::EventControllerKey::builder()
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        keys.connect_key_pressed(move |_, key, _, state| {
            let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            if enter && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                f();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.view.add_controller(keys);
    }

    fn add_controller(&self, controller: impl IsA<gtk::EventController>) {
        self.view.add_controller(controller);
    }
}

/// Replaces a message row's only child.
fn set_child(row: &gtk::Box, child: &gtk::Widget) {
    while let Some(old) = row.first_child() {
        row.remove(&old);
    }
    row.append(child);
}

fn time_of(m: &Dict) -> String {
    let ms = m
        .get("timestamp")
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or(0);
    crate::timefmt::message(ms)
}

/// A picture file as a texture for an avatar, or None.
fn texture(path: &str) -> Option<gdk::Texture> {
    (!path.is_empty())
        .then(|| gdk::Texture::from_filename(path).ok())
        .flatten()
}

/// Delivered: one check; read: two, overlapping (Dino's). Sending shows
/// as a dimmed bubble instead, and sent to the server needs no mark.
fn state_icon(state: &str) -> &'static str {
    match state {
        "received" => "message-delivered-symbolic",
        "displayed" => "message-read-symbolic",
        "failed" => "dialog-warning-symbolic",
        _ => "",
    }
}

fn state_name(state: &str) -> &'static str {
    match state {
        "pending" => "Sending",
        "sent" => "Sent to the server",
        "received" => "Delivered",
        "displayed" => "Read",
        "failed" => "Not sent",
        _ => "",
    }
}

/// Shares a file from disk in the conversation.
fn send_path(
    proxy: ConversationProxy<'static>,
    weak: std::rc::Weak<Window>,
    path: std::path::PathBuf,
) {
    let name = path
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    glib::spawn_future_local(async move {
        let sent = run(async move {
            let f = std::fs::File::open(&path)
                .map_err(|e| zbus::Error::Failure(format!("can't read the file: {e}")))?;
            proxy
                .send_file(zbus::zvariant::Fd::from(&f), &name, HashMap::new())
                .await
        })
        .await;
        if let (Err(e), Some(w)) = (sent, weak.upgrade()) {
            w.toast(&format!("Not sent: {}", client::explain(&e)));
        }
    });
}
