//! One open conversation: its history and the box to write in.
//!
//! The send button is always there, so sending works the same with an
//! on-screen keyboard as with Enter. A message's menu (right click or long
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
    lock: gtk::Button,
    title: adw::WindowTitle,
    /// Who is in a group chat, and leaving or rejoining it.
    people: gtk::MenuButton,
    is_room: Cell<bool>,
    /// Who we are here: our JID, or room@service/nick in a group chat.
    me: RefCell<String>,
    messages: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    /// Shown above the box while replying or editing.
    mode_bar: gtk::Box,
    mode_label: gtk::Label,
    mode: RefCell<Mode>,
    /// Rows and their messages by message id, to update them in place.
    rows: RefCell<HashMap<String, (gtk::ListBoxRow, Dict)>>,
    oldest: RefCell<String>,
    /// Ends "typing" after a pause.
    typing_timer: RefCell<Option<glib::SourceId>>,
    /// The subtitle when the peer isn't typing.
    subtitle: RefCell<String>,
    loading: Cell<bool>,
    at_start: Cell<bool>,
}

impl ChatPage {
    pub fn new(
        window: &Rc<Window>,
        proxy: ConversationProxy<'static>,
        own_jid: String,
    ) -> Rc<Self> {
        let title = adw::WindowTitle::new("", "");
        let lock = gtk::Button::builder()
            .icon_name("channel-insecure-symbolic")
            .tooltip_text("Not Encrypted")
            .build();
        let people = gtk::MenuButton::builder()
            .icon_name("system-users-symbolic")
            .tooltip_text("People")
            .visible(false)
            .build();
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_end(&lock);
        header.pack_end(&people);

        let messages = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["chat-history"])
            .valign(gtk::Align::End)
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

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Unencrypted Message")
            .input_hints(gtk::InputHints::SPELLCHECK | gtk::InputHints::EMOJI)
            .build();
        let send = gtk::Button::builder()
            .icon_name("mail-send-symbolic")
            .tooltip_text("Send")
            .css_classes(["circular", "suggested-action"])
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
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
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();
        row.append(&attach);
        row.append(&entry);
        row.append(&send);
        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        composer.append(&mode_bar);
        composer.append(&row);
        let composer = adw::Clamp::builder()
            .maximum_size(800)
            .child(&composer)
            .build();

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&scroller));
        view.add_bottom_bar(&composer);
        view.set_bottom_bar_style(adw::ToolbarStyle::Raised);
        let page = adw::NavigationPage::new(&view, "Chat");

        let me = own_jid.clone();
        let this = Rc::new(Self {
            page,
            proxy: proxy.clone(),
            window: Rc::downgrade(window),
            own_jid,
            lock: lock.clone(),
            title: title.clone(),
            people,
            is_room: Cell::new(false),
            me: RefCell::new(me),
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
            at_start: Cell::new(false),
        });

        entry.connect_changed({
            let send = send.clone();
            move |e| send.set_sensitive(!e.text().trim().is_empty())
        });
        let weak = Rc::downgrade(&this);
        entry.connect_changed(move |e| {
            if let Some(this) = weak.upgrade() {
                this.typed(!e.text().is_empty());
            }
        });
        let weak = Rc::downgrade(&this);
        let do_send = move || {
            if let Some(this) = weak.upgrade() {
                this.send();
            }
        };
        entry.connect_activate({
            let do_send = do_send.clone();
            move |_| do_send()
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
                gdk::Key::Up if this.entry.text().is_empty() => {
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
        lock.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade()
                && let Some(window) = this.window.upgrade()
            {
                crate::encryption::show(&window, this.proxy.clone(), this.own_jid.clone());
            }
        });

        // Load older messages when scrolled to the top.
        let weak = Rc::downgrade(&this);
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
                    // The lock still shows whether the room is encrypted,
                    // but there is no per-person dialog behind it.
                    this.lock.set_sensitive(false);
                    this.setup_room().await;
                }
            }
            this.follow_encryption();
            if !this.is_room.get() {
                this.follow_typing();
            }
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
                    let stick = this.near_end();
                    this.append(&dict);
                    if stick {
                        this.scroll_to_end();
                    }
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

    /// Keeps the lock and the composer's hint in step with Encryption.
    fn follow_encryption(self: &Rc<Self>) {
        let changes = {
            let proxy = self.proxy.clone();
            client::forward(move || async move {
                let first = proxy.encryption().await.unwrap_or_default();
                let later = proxy.receive_encryption_changed().await;
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
                let encrypted = encryption == "omemo";
                this.lock.set_icon_name(if encrypted {
                    "channel-secure-symbolic"
                } else {
                    "channel-insecure-symbolic"
                });
                this.lock.set_tooltip_text(Some(if encrypted {
                    "Encrypted"
                } else {
                    "Not Encrypted"
                }));
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
    }

    fn row(self: &Rc<Self>, m: &Dict) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .build();
        row.set_child(Some(&self.bubble(m)));
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
        row.set_child(Some(&self.bubble(&m)));
        self.rows.borrow_mut().insert(id.to_owned(), (row, m));
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

        if self.is_room.get() && !outgoing {
            let sender = text(m, "sender");
            let nick = sender.split_once('/').map(|(_, n)| n).unwrap_or(&sender);
            bubble.append(
                &gtk::Label::builder()
                    .label(nick)
                    .xalign(0.0)
                    .css_classes(["sender"])
                    .build(),
            );
        }

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

        let meta = gtk::Box::builder()
            .spacing(4)
            .halign(gtk::Align::End)
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
        meta.append(
            &gtk::Label::builder()
                .label(time_of(m))
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        if outgoing {
            let state = text(m, "state");
            meta.append(
                &gtk::Label::builder()
                    .label(tick(&state))
                    .tooltip_text(state_name(&state))
                    .css_classes(["caption", "message-state"])
                    .build(),
            );
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
                let name = file
                    .basename()
                    .map(|b| b.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                let (proxy, weak) = (proxy.clone(), weak.clone());
                glib::spawn_future_local(async move {
                    let sent = run(async move {
                        let f = std::fs::File::open(&path).map_err(|e| {
                            zbus::Error::Failure(format!("can't read the file: {e}"))
                        })?;
                        proxy
                            .send_file(zbus::zvariant::Fd::from(&f), &name, HashMap::new())
                            .await
                    })
                    .await;
                    if let (Err(e), Some(w)) = (sent, weak.upgrade()) {
                        w.toast(&format!("Not sent: {}", client::explain(&e)));
                    }
                });
            },
        );
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
                    this.menu(&bubble, &id, outgoing, x, y);
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

    fn menu(self: &Rc<Self>, bubble: &gtk::Box, id: &str, outgoing: bool, x: f64, y: f64) {
        let popover = gtk::Popover::builder()
            .pointing_to(&gdk::Rectangle::new(x as i32, y as i32, 1, 1))
            .build();
        popover.set_parent(bubble);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();

        let emojis = gtk::Box::builder().spacing(2).build();
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
        let copy_id = id.to_owned();
        content.append(&item(
            "Copy Text",
            Box::new(move |this| {
                if let Some((_, m)) = this.rows.borrow().get(&copy_id) {
                    this.entry.clipboard().set_text(&text(m, "body"));
                }
            }),
        ));
        if outgoing {
            let edit_id = id.to_owned();
            content.append(&item(
                "Edit",
                Box::new(move |this| this.set_mode(Mode::Edit(edit_id.clone()))),
            ));
            let delete_id = id.to_owned();
            content.append(&item(
                "Delete for Everyone",
                Box::new(move |this| this.confirm_retract(delete_id.clone())),
            ));
        }
        popover.set_child(Some(&content));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
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
                self.entry.set_position(-1);
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

    fn near_end(&self) -> bool {
        let adj = self.scroller.vadjustment();
        adj.upper() - adj.page_size() - adj.value() < 80.0
    }

    fn scroll_to_end(&self) {
        let scroller = self.scroller.clone();
        glib::idle_add_local_once(move || {
            let adj = scroller.vadjustment();
            adj.set_value(adj.upper());
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

fn time_of(m: &Dict) -> String {
    let ms = m
        .get("timestamp")
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or(0);
    let Ok(when) = glib::DateTime::from_unix_local(ms / 1000) else {
        return String::new();
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };
    let format = if when.ymd() == now.ymd() {
        "%H:%M"
    } else if when.year() == now.year() {
        "%e %b %H:%M"
    } else {
        "%e %b %Y"
    };
    when.format(format)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn tick(state: &str) -> &'static str {
    match state {
        "pending" => "◷",
        "sent" => "✓",
        "received" => "✓✓",
        "displayed" => "✓✓",
        "failed" => "!",
        _ => "",
    }
}

fn state_name(state: &str) -> &'static str {
    match state {
        "pending" => "Sending",
        "sent" => "Sent",
        "received" => "Delivered",
        "displayed" => "Read",
        "failed" => "Not sent",
        _ => "",
    }
}
