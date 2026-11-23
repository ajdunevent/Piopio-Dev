//! One open conversation: its history and the box to write in.
//!
//! The send button is always there, so sending works the same with an
//! on-screen keyboard as with Enter.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use futures_util::StreamExt;
use piopio_dbus::{ConversationProxy, Dict};

use crate::client::{self, run};
use crate::window::Window;

const PAGE: u32 = 50;

pub struct ChatPage {
    pub page: adw::NavigationPage,
    proxy: ConversationProxy<'static>,
    messages: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    /// Bubble state labels by message id, to update ticks in place.
    states: RefCell<HashMap<String, gtk::Label>>,
    oldest: RefCell<String>,
    loading: Cell<bool>,
    at_start: Cell<bool>,
}

impl ChatPage {
    pub fn new(
        window: &Rc<Window>,
        proxy: ConversationProxy<'static>,
        _own_jid: String,
    ) -> Rc<Self> {
        let title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();

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

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Message")
            .input_hints(gtk::InputHints::SPELLCHECK | gtk::InputHints::EMOJI)
            .build();
        let send = gtk::Button::builder()
            .icon_name("mail-send-symbolic")
            .tooltip_text("Send")
            .css_classes(["circular", "suggested-action"])
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        let composer = gtk::Box::builder()
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        composer.append(&entry);
        composer.append(&send);
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

        let this = Rc::new(Self {
            page,
            proxy: proxy.clone(),
            messages,
            scroller,
            entry: entry.clone(),
            states: RefCell::new(HashMap::new()),
            oldest: RefCell::new(String::new()),
            loading: Cell::new(false),
            at_start: Cell::new(false),
        });

        entry.connect_changed({
            let send = send.clone();
            move |e| send.set_sensitive(!e.text().trim().is_empty())
        });
        let weak = Rc::downgrade(&this);
        let window_weak = Rc::downgrade(window);
        let do_send = move || {
            if let (Some(this), Some(window)) = (weak.upgrade(), window_weak.upgrade()) {
                this.send(&window);
            }
        };
        entry.connect_activate({
            let do_send = do_send.clone();
            move |_| do_send()
        });
        send.connect_clicked(move |_| do_send());

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
            if let Ok((name, peer)) = run(async move {
                Ok::<_, zbus::Error>((proxy.title().await?, proxy.peer().await?))
            })
            .await
            {
                title.set_title(&name);
                title.set_subtitle(if name == peer { "" } else { &peer });
                this.page.set_title(&name);
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
                } else if let Some(label) = this.states.borrow().get(&id)
                    && let Some(state) = dict.get("state")
                    && let Ok(state) = <&str>::try_from(state)
                {
                    label.set_label(tick(state));
                    label.set_tooltip_text(Some(state_name(state)));
                }
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
                let row = self.bubble(m);
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

    fn append(&self, m: &Dict) {
        let row = self.bubble(m);
        self.messages.append(&row);
    }

    fn bubble(&self, m: &Dict) -> gtk::ListBoxRow {
        let outgoing = m
            .get("outgoing")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        let body = gtk::Label::builder()
            .label(text(m, "body"))
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .xalign(0.0)
            .build();
        let time = gtk::Label::builder()
            .label(time_of(m))
            .css_classes(["caption", "dim-label"])
            .build();
        let meta = gtk::Box::builder()
            .spacing(4)
            .halign(gtk::Align::End)
            .build();
        meta.append(&time);
        if outgoing {
            let state = text(m, "state");
            let label = gtk::Label::builder()
                .label(tick(&state))
                .tooltip_text(state_name(&state))
                .css_classes(["caption", "message-state"])
                .build();
            meta.append(&label);
            self.states.borrow_mut().insert(text(m, "id"), label);
        }
        let bubble = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["bubble", if outgoing { "outgoing" } else { "incoming" }])
            .halign(if outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .build();
        bubble.append(&body);
        bubble.append(&meta);
        gtk::ListBoxRow::builder()
            .child(&bubble)
            .activatable(false)
            .selectable(false)
            .build()
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

    fn send(self: &Rc<Self>, window: &Rc<Window>) {
        let body = self.entry.text().to_string();
        if body.trim().is_empty() {
            return;
        }
        self.entry.set_text("");
        // Keep typing after tapping Send, as with Enter.
        self.entry.grab_focus();
        let proxy = self.proxy.clone();
        let window = window.clone();
        let entry = self.entry.clone();
        glib::spawn_future_local(async move {
            let text = body.clone();
            let sent =
                run(async move { proxy.send(&text, HashMap::new()).await.map(|_| ()) }).await;
            if let Err(e) = sent {
                window.toast(&format!("Not sent: {}", client::explain(&e)));
                // Give the text back rather than lose it.
                if entry.text().is_empty() {
                    entry.set_text(&body);
                }
            }
        });
        self.scroll_to_end();
    }
}

fn text(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
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
