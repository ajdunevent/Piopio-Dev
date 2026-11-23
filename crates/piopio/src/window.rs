//! The main window: the conversation list beside the open chat.
//!
//! On narrow screens the split view collapses into one page at a time, so the
//! same window works on a phone.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use adw::{glib, gtk};
use futures_util::StreamExt;
use piopio_dbus::{AccountProxy, ConversationProxy, DaemonProxy, Dict, RosterProxy};
use zbus::zvariant::OwnedObjectPath;

use crate::chat::ChatPage;
use crate::client::{self, run};
use crate::dialogs;

/// One row in the conversation list.
struct Entry {
    account: String,
    proxy: ConversationProxy<'static>,
    row: adw::ActionRow,
    badge: gtk::Label,
    avatar: adw::Avatar,
    last_activity: i64,
    pinned: bool,
}

/// What the window shows about an account.
pub struct AccountInfo {
    pub path: OwnedObjectPath,
    pub jid: String,
    pub proxy: AccountProxy<'static>,
    pub roster: RosterProxy<'static>,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    sidebar_stack: gtk::Stack,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    entries: RefCell<HashMap<String, Entry>>,
    pub accounts: RefCell<Vec<Rc<AccountInfo>>>,
    open: RefCell<Option<(String, Rc<ChatPage>)>>,
    /// Shown when no conversation is open.
    placeholder: adw::NavigationPage,
}

impl Window {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        list.set_sort_func(sort_rows);

        let empty = adw::StatusPage::builder()
            .icon_name(piopio_dbus::APP_ID)
            .title("Welcome to Piopio")
            .description("Add your XMPP account to start chatting.")
            .build();
        let add_button = gtk::Button::builder()
            .label("Add Account")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .action_name("app.add-account")
            .build();
        empty.set_child(Some(&add_button));

        let no_chats = adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("No Chats Yet")
            .description("Start one with the + button.")
            .build();

        let sidebar_stack = gtk::Stack::new();
        let loading = crate::spinner();
        loading.set_halign(gtk::Align::Center);
        loading.set_valign(gtk::Align::Center);
        loading.set_size_request(32, 32);
        sidebar_stack.add_named(&loading, Some("loading"));
        sidebar_stack.add_named(&empty, Some("welcome"));
        sidebar_stack.add_named(&no_chats, Some("empty"));
        sidebar_stack.add_named(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&list)
                .vexpand(true)
                .build(),
            Some("list"),
        );
        sidebar_stack.set_visible_child_name("loading");

        let menu = gtk::gio::Menu::new();
        menu.append(Some("_Join Group Chat…"), Some("app.join-room"));
        menu.append(Some("_Accounts"), Some("app.accounts"));
        menu.append(Some("Add _Account…"), Some("app.add-account"));
        menu.append(Some("_About Piopio"), Some("app.about"));
        let header = adw::HeaderBar::new();
        header.pack_start(
            &gtk::Button::builder()
                .icon_name("list-add-symbolic")
                .tooltip_text("New Chat")
                .action_name("app.new-chat")
                .build(),
        );
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .tooltip_text("Main Menu")
                .menu_model(&menu)
                .primary(true)
                .build(),
        );

        let banner = adw::Banner::new("");
        banner.set_revealed(false);

        let sidebar = adw::ToolbarView::new();
        sidebar.add_top_bar(&header);
        sidebar.add_top_bar(&banner);
        sidebar.set_content(Some(&sidebar_stack));

        let placeholder = adw::ToolbarView::new();
        placeholder.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
        placeholder.set_content(Some(
            &adw::StatusPage::builder()
                .icon_name("chat-message-new-symbolic")
                .title("No Chat Selected")
                .build(),
        ));

        let placeholder = adw::NavigationPage::new(&placeholder, "Chat");
        let split = adw::NavigationSplitView::builder()
            .sidebar(&adw::NavigationPage::new(&sidebar, "Piopio"))
            .content(&placeholder)
            .min_sidebar_width(260.0)
            .build();

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Piopio")
            .default_width(900)
            .default_height(640)
            .width_request(360)
            .height_request(294)
            .content(&toasts)
            .build();
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            600.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);

        let this = Rc::new(Self {
            window,
            split,
            list,
            sidebar_stack,
            banner,
            toasts,
            entries: RefCell::new(HashMap::new()),
            accounts: RefCell::new(Vec::new()),
            open: RefCell::new(None),
            placeholder,
        });

        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                let path = row.widget_name().to_string();
                this.open_conversation(&path);
            }
        });

        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            if let Some(this) = weak.upgrade() {
                this.load().await;
            }
        });
        this
    }

    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    /// Connects to the daemon (starting it if needed) and fills the window.
    async fn load(self: &Rc<Self>) {
        let bus = match client::bus().await {
            Ok(bus) => bus,
            Err(e) => return self.unreachable(&e),
        };
        let paths = {
            let bus = bus.clone();
            run(async move { DaemonProxy::new(&bus).await?.list_accounts().await }).await
        };
        let paths = match paths {
            Ok(paths) => paths,
            Err(e) => return self.unreachable(&e),
        };
        for path in paths {
            self.add_account_info(path).await;
        }
        self.watch_objects(bus.clone());
        self.load_conversations(&bus).await;
        self.update_sidebar();
    }

    fn unreachable(&self, error: &zbus::Error) {
        let page = adw::StatusPage::builder()
            .icon_name("network-offline-symbolic")
            .title("Can't Reach the Piopio Service")
            .description(glib::markup_escape_text(&client::explain(error)).as_str())
            .build();
        self.sidebar_stack.add_named(&page, Some("unreachable"));
        self.sidebar_stack.set_visible_child_name("unreachable");
    }

    /// Learns about an account and follows its connection state.
    pub async fn add_account_info(self: &Rc<Self>, path: OwnedObjectPath) {
        if self.accounts.borrow().iter().any(|a| a.path == path) {
            return;
        }
        let Ok(bus) = client::bus().await else { return };
        let made = {
            let (bus, path) = (bus.clone(), path.clone());
            run(async move {
                let proxy = AccountProxy::builder(&bus)
                    .path(path.clone())?
                    .build()
                    .await?;
                let roster = RosterProxy::builder(&bus).path(path)?.build().await?;
                let jid = proxy.jid().await?;
                Ok::<_, zbus::Error>((proxy, roster, jid))
            })
            .await
        };
        let Ok((proxy, roster, jid)) = made else {
            return;
        };
        let info = Rc::new(AccountInfo {
            path,
            jid,
            proxy: proxy.clone(),
            roster: roster.clone(),
        });
        self.accounts.borrow_mut().push(info.clone());

        // Invitations to group chats → a toast with Join.
        let invitations = {
            let proxy = proxy.clone();
            client::forward(move || async move {
                let s = proxy.receive_room_invitation().await.ok()?;
                Some(s.filter_map(|i| async move {
                    let a = i.args().ok()?;
                    Some((
                        a.room().to_string(),
                        a.from().to_string(),
                        a.password().to_string(),
                    ))
                }))
            })
        };
        let weak = Rc::downgrade(self);
        let account = proxy.clone();
        glib::spawn_future_local(async move {
            while let Ok((room, from, password)) = invitations.recv().await {
                let Some(this) = weak.upgrade() else { break };
                let toast = adw::Toast::builder()
                    .title(
                        glib::markup_escape_text(&format!("{from} invited you to {room}")).as_str(),
                    )
                    .button_label("Join")
                    .timeout(0)
                    .build();
                let (account, w) = (account.clone(), Rc::downgrade(&this));
                toast.connect_button_clicked(move |_| {
                    let (account, room, password, w) =
                        (account.clone(), room.clone(), password.clone(), w.clone());
                    glib::spawn_future_local(async move {
                        let result = run(async move {
                            let mut options = HashMap::new();
                            if !password.is_empty() {
                                options.insert("password", zbus::zvariant::Value::from(password));
                            }
                            account.join_room(&room, "", options).await
                        })
                        .await;
                        let Some(w) = w.upgrade() else { return };
                        match result {
                            Ok(path) => {
                                w.add_conversation(path.clone()).await;
                                w.open_conversation(path.as_str());
                            }
                            Err(e) => w.toast(&client::explain(&e)),
                        }
                    });
                });
                this.toasts.add_toast(toast);
            }
        });

        // Connection state → banner.
        let changes = {
            let proxy = proxy.clone();
            client::forward(move || async move {
                let state = proxy.receive_state_changed().await;
                let error = proxy.receive_error_changed().await;
                Some(futures_util::stream::select(
                    state.map(|_| ()),
                    error.map(|_| ()),
                ))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let Some(this) = weak.upgrade() {
                this.update_banner().await;
            }
            while changes.recv().await.is_ok() {
                let Some(this) = weak.upgrade() else { break };
                this.update_banner().await;
            }
        });

        // Subscription requests → a question.
        let requests = {
            let roster = roster.clone();
            client::forward(move || async move { roster.receive_subscription_request().await.ok() })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(signal) = requests.recv().await {
                let Some(this) = weak.upgrade() else { break };
                if let Ok(args) = signal.args() {
                    dialogs::subscription_request(&this, &info, args.jid().to_string());
                }
            }
        });
        self.update_sidebar();
    }

    async fn update_banner(&self) {
        let accounts: Vec<Rc<AccountInfo>> = self.accounts.borrow().clone();
        let mut problems = Vec::new();
        for account in accounts {
            let proxy = account.proxy.clone();
            let status = run(async move {
                let enabled = proxy.enabled().await.unwrap_or(false);
                let state = proxy.state().await.unwrap_or_default();
                let error = proxy.error().await.unwrap_or_default();
                (enabled, state, error)
            })
            .await;
            match status {
                (false, _, _) => {}
                (true, state, _) if state == "connected" => {}
                (true, state, error) if state == "error" => {
                    problems.push(format!("{}: {error}", account.jid));
                }
                (true, _, error) if !error.is_empty() => {
                    problems.push(format!("{}: {error}", account.jid));
                }
                (true, _, _) => problems.push(format!("Connecting {}…", account.jid)),
            }
        }
        if problems.is_empty() {
            self.banner.set_revealed(false);
        } else {
            self.banner
                .set_title(&glib::markup_escape_text(&problems.join("\n")));
            self.banner.set_revealed(true);
        }
    }

    fn watch_objects(self: &Rc<Self>, bus: zbus::Connection) {
        let added = {
            let bus = bus.clone();
            client::forward(move || async move {
                let manager = zbus::fdo::ObjectManagerProxy::builder(&bus)
                    .destination(piopio_dbus::DAEMON_BUS_NAME)
                    .ok()?
                    .path(piopio_dbus::ROOT_PATH)
                    .ok()?
                    .build()
                    .await
                    .ok()?;
                let added = manager.receive_interfaces_added().await.ok()?;
                let removed = manager.receive_interfaces_removed().await.ok()?;
                Some(futures_util::stream::select(
                    added.filter_map(|s| async move {
                        let args = s.args().ok()?;
                        let interfaces: Vec<String> = args
                            .interfaces_and_properties()
                            .keys()
                            .map(|k| k.to_string())
                            .collect();
                        Some((true, args.object_path().to_owned().into(), interfaces))
                    }),
                    removed.filter_map(|s| async move {
                        let args = s.args().ok()?;
                        let interfaces: Vec<String> =
                            args.interfaces().iter().map(|k| k.to_string()).collect();
                        Some((false, args.object_path().to_owned().into(), interfaces))
                    }),
                ))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((is_added, path, interfaces)) = added.recv().await {
                let Some(this) = weak.upgrade() else { break };
                let path: OwnedObjectPath = path;
                let has = |name: &str| interfaces.iter().any(|i| i == name);
                if is_added && has("net.dunevent.Piopio.Conversation") {
                    this.add_conversation(path).await;
                } else if is_added && has("net.dunevent.Piopio.Account") {
                    this.add_account_info(path).await;
                } else if !is_added && has("net.dunevent.Piopio.Conversation") {
                    this.remove_conversation(path.as_str());
                } else if !is_added && has("net.dunevent.Piopio.Account") {
                    this.accounts.borrow_mut().retain(|a| a.path != path);
                    this.update_sidebar();
                    this.update_banner().await;
                }
            }
        });
    }

    async fn load_conversations(self: &Rc<Self>, bus: &zbus::Connection) {
        let bus = bus.clone();
        let paths = run(async move {
            let manager = zbus::fdo::ObjectManagerProxy::builder(&bus)
                .destination(piopio_dbus::DAEMON_BUS_NAME)?
                .path(piopio_dbus::ROOT_PATH)?
                .build()
                .await?;
            let objects = manager.get_managed_objects().await?;
            Ok::<_, zbus::Error>(
                objects
                    .into_iter()
                    .filter(|(_, ifaces)| {
                        ifaces
                            .keys()
                            .any(|k| k.as_str() == "net.dunevent.Piopio.Conversation")
                    })
                    .map(|(p, _)| p)
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .unwrap_or_default();
        for path in paths {
            self.add_conversation(path).await;
        }
    }

    /// Adds a row for a conversation and keeps it current.
    pub async fn add_conversation(self: &Rc<Self>, path: OwnedObjectPath) {
        let key = path.as_str().to_owned();
        if self.entries.borrow().contains_key(&key) {
            return;
        }
        let Ok(bus) = client::bus().await else { return };
        let loaded = {
            let path = path.clone();
            run(async move {
                let proxy = ConversationProxy::builder(&bus).path(path)?.build().await?;
                let title = proxy.title().await?;
                let unread = proxy.unread().await?;
                let last = proxy.last_activity().await?;
                let pinned = proxy.pinned().await?;
                let newest = proxy.get_messages("", 1).await?.into_iter().next();
                let picture = proxy.avatar().await.unwrap_or_default();
                Ok::<_, zbus::Error>((proxy, title, unread, last, pinned, newest, picture))
            })
            .await
        };
        let Ok((proxy, title, unread, last, pinned, newest, picture)) = loaded else {
            return;
        };
        if self.entries.borrow().contains_key(&key) {
            return;
        }
        let badge = gtk::Label::builder()
            .css_classes(["unread-badge"])
            .valign(gtk::Align::Center)
            .build();
        let avatar = adw::Avatar::new(36, Some(&title), true);
        set_avatar_image(&avatar, &picture);
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&title).as_str())
            .subtitle_lines(1)
            .activatable(true)
            .name(key.as_str())
            .build();
        row.add_prefix(&avatar);
        row.add_suffix(&badge);
        set_unread(&badge, &row, unread);
        if let Some(m) = &newest {
            row.set_subtitle(&glib::markup_escape_text(&preview(m)));
        }
        let account = key.split("/conversation/").next().unwrap_or("").to_owned();
        self.list.append(&row);
        self.entries.borrow_mut().insert(
            key.clone(),
            Entry {
                account,
                proxy: proxy.clone(),
                row: row.clone(),
                badge,
                avatar,
                last_activity: last,
                pinned,
            },
        );
        row.set_widget_name(&key);
        store_sort_keys(&row, last, pinned);
        self.list.invalidate_sort();
        self.update_sidebar();

        // Follow new messages, unread count and title.
        let events = {
            let proxy = proxy.clone();
            client::forward(move || async move {
                let added = proxy.receive_message_added().await.ok()?;
                let unread = proxy.receive_unread_changed().await;
                let title = proxy.receive_title_changed().await;
                let last = proxy.receive_last_activity_changed().await;
                let picture = proxy.receive_avatar_changed().await;
                let pinned = proxy.receive_pinned_changed().await;
                Some(futures_util::stream::select_all([
                    pinned
                        .filter_map(|c| async move { c.get().await.ok().map(RowEvent::Pinned) })
                        .boxed(),
                    picture
                        .filter_map(|c| async move { c.get().await.ok().map(RowEvent::Avatar) })
                        .boxed(),
                    added
                        .filter_map(|s| async move {
                            s.args()
                                .ok()
                                .map(|a| RowEvent::Message(a.message().clone()))
                        })
                        .boxed(),
                    unread
                        .filter_map(|c| async move { c.get().await.ok().map(RowEvent::Unread) })
                        .boxed(),
                    title
                        .filter_map(|c| async move { c.get().await.ok().map(RowEvent::Title) })
                        .boxed(),
                    last.filter_map(|c| async move { c.get().await.ok().map(RowEvent::Activity) })
                        .boxed(),
                ]))
            })
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(this) = weak.upgrade() else { break };
                this.row_event(&key, event);
            }
        });
    }

    fn row_event(&self, key: &str, event: RowEvent) {
        let mut entries = self.entries.borrow_mut();
        let Some(entry) = entries.get_mut(key) else {
            return;
        };
        match event {
            RowEvent::Message(m) => {
                entry
                    .row
                    .set_subtitle(&glib::markup_escape_text(&preview(&m)));
            }
            RowEvent::Unread(n) => {
                let open = self.open.borrow().as_ref().is_some_and(|(k, _)| k == key);
                if open && n > 0 && self.window.is_active() {
                    // Reading it right now.
                    let proxy = entry.proxy.clone();
                    glib::spawn_future_local(async move {
                        let _ = run(async move { proxy.mark_read().await }).await;
                    });
                }
                set_unread(&entry.badge, &entry.row, n);
            }
            RowEvent::Title(t) => {
                entry.row.set_title(&glib::markup_escape_text(&t));
                entry.avatar.set_text(Some(&t));
            }
            RowEvent::Avatar(path) => set_avatar_image(&entry.avatar, &path),
            RowEvent::Pinned(p) => {
                entry.pinned = p;
                store_sort_keys(&entry.row, entry.last_activity, p);
                drop(entries);
                self.list.invalidate_sort();
            }
            RowEvent::Activity(t) => {
                entry.last_activity = t;
                store_sort_keys(&entry.row, t, entry.pinned);
                drop(entries);
                self.list.invalidate_sort();
            }
        }
    }

    fn remove_conversation(&self, key: &str) {
        if let Some(entry) = self.entries.borrow_mut().remove(key) {
            self.list.remove(&entry.row);
        }
        let was_open = self.open.borrow().as_ref().is_some_and(|(k, _)| k == key);
        if was_open {
            self.open.borrow_mut().take();
            self.split.set_content(Some(&self.placeholder));
            self.split.set_show_content(false);
        }
        self.update_sidebar();
    }

    fn update_sidebar(&self) {
        let name = if self.accounts.borrow().is_empty() {
            "welcome"
        } else if self.entries.borrow().is_empty() {
            "empty"
        } else {
            "list"
        };
        if self.sidebar_stack.visible_child_name().as_deref() != Some("unreachable") {
            self.sidebar_stack.set_visible_child_name(name);
        }
    }

    /// Shows a conversation (from the list, a notification or a new chat).
    pub fn open_conversation(self: &Rc<Self>, path: &str) {
        let Some((proxy, account)) = self
            .entries
            .borrow()
            .get(path)
            .map(|e| (e.proxy.clone(), e.account.clone()))
        else {
            // It may not be listed yet (just created); try again shortly.
            let (weak, path) = (Rc::downgrade(self), path.to_owned());
            glib::spawn_future_local(async move {
                let Some(this) = weak.upgrade() else { return };
                if let Ok(p) = OwnedObjectPath::try_from(path.clone()) {
                    this.add_conversation(p).await;
                }
                if this.entries.borrow().contains_key(&path) {
                    this.open_conversation(&path);
                }
            });
            return;
        };
        let jid = self
            .accounts
            .borrow()
            .iter()
            .find(|a| a.path.as_str() == account)
            .map(|a| a.jid.clone())
            .unwrap_or_default();
        let page = ChatPage::new(self, proxy, jid);
        self.split.set_content(Some(&page.page));
        self.split.set_show_content(true);
        if let Some(entry) = self.entries.borrow().get(path) {
            self.list.select_row(Some(&entry.row));
        }
        *self.open.borrow_mut() = Some((path.to_owned(), page));
    }
}

enum RowEvent {
    Message(Dict),
    Unread(u32),
    Title(String),
    Activity(i64),
    Avatar(String),
    Pinned(bool),
}

/// Shows a picture file in an avatar (or the initials when there's none).
pub fn set_avatar_image(avatar: &adw::Avatar, path: &str) {
    let texture = (!path.is_empty())
        .then(|| gtk::gdk::Texture::from_filename(path).ok())
        .flatten();
    avatar.set_custom_image(texture.as_ref());
}

fn text(dict: &Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn preview(m: &Dict) -> String {
    let body = if m
        .get("retracted")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
    {
        "Message deleted".to_owned()
    } else if text(m, "encryption") == "omemo-failed" {
        "Encrypted message".to_owned()
    } else {
        text(m, "body")
    };
    let outgoing = m
        .get("outgoing")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);
    let first_line = body.lines().next().unwrap_or("").to_owned();
    if outgoing {
        format!("You: {first_line}")
    } else {
        first_line
    }
}

fn set_unread(badge: &gtk::Label, row: &adw::ActionRow, unread: u32) {
    badge.set_visible(unread > 0);
    badge.set_label(&unread.to_string());
    if unread > 0 {
        row.add_css_class("unread");
    } else {
        row.remove_css_class("unread");
    }
}

// Rows sort pinned first, then by last activity, newest first. The keys
// live on the row itself so the sort function needs no window access.
fn store_sort_keys(row: &adw::ActionRow, last_activity: i64, pinned: bool) {
    unsafe {
        row.set_data("piopio-last", last_activity);
        row.set_data("piopio-pinned", pinned);
    }
}

fn sort_keys(row: &gtk::ListBoxRow) -> (bool, i64) {
    unsafe {
        let last = row
            .data::<i64>("piopio-last")
            .map(|p| *p.as_ref())
            .unwrap_or(0);
        let pinned = row
            .data::<bool>("piopio-pinned")
            .map(|p| *p.as_ref())
            .unwrap_or(false);
        (pinned, last)
    }
}

fn sort_rows(a: &gtk::ListBoxRow, b: &gtk::ListBoxRow) -> gtk::Ordering {
    let (pa, la) = sort_keys(a);
    let (pb, lb) = sort_keys(b);
    (pb, lb).cmp(&(pa, la)).into()
}
