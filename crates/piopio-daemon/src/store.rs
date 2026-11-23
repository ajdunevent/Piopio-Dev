//! The daemon's encrypted database.
//!
//! One SQLCipher file holds accounts, credentials, contacts, conversations and
//! messages. Its key is generated on first run and kept in the user's keyring
//! (through the Secret portal inside Flatpak), so nothing readable ever touches
//! the disk. Every query runs on the blocking thread pool.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &[&str] = &[
    // Version 1
    r#"
    CREATE TABLE accounts (
        id          INTEGER PRIMARY KEY,
        jid         TEXT NOT NULL UNIQUE,
        password    TEXT NOT NULL,
        enabled     INTEGER NOT NULL DEFAULT 1,
        roster_ver  TEXT,
        mam_last_id TEXT,
        options     TEXT NOT NULL DEFAULT '{}'
    );
    CREATE TABLE contacts (
        account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        jid          TEXT NOT NULL,
        name         TEXT NOT NULL DEFAULT '',
        subscription TEXT NOT NULL DEFAULT 'none',
        ask          INTEGER NOT NULL DEFAULT 0,
        groups       TEXT NOT NULL DEFAULT '[]',
        PRIMARY KEY (account_id, jid)
    );
    CREATE TABLE conversations (
        id            INTEGER PRIMARY KEY,
        account_id    INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        peer          TEXT NOT NULL,
        kind          TEXT NOT NULL DEFAULT 'chat',
        pinned        INTEGER NOT NULL DEFAULT 0,
        muted         TEXT NOT NULL DEFAULT 'never',
        unread        INTEGER NOT NULL DEFAULT 0,
        last_activity INTEGER NOT NULL DEFAULT 0,
        encryption_wanted INTEGER NOT NULL DEFAULT 1,
        UNIQUE (account_id, peer, kind)
    );
    CREATE TABLE messages (
        id              INTEGER PRIMARY KEY,
        conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        origin_id       TEXT NOT NULL,
        server_id       TEXT,
        outgoing        INTEGER NOT NULL,
        sender          TEXT NOT NULL,
        body            TEXT NOT NULL,
        timestamp       INTEGER NOT NULL,
        state           TEXT NOT NULL,
        UNIQUE (conversation_id, origin_id, outgoing)
    );
    CREATE INDEX messages_by_time ON messages (conversation_id, timestamp, id);
    CREATE INDEX messages_by_server_id ON messages (server_id);
    "#,
    // Version 2: OMEMO
    r#"
    ALTER TABLE messages ADD COLUMN encryption TEXT NOT NULL DEFAULT 'none';
    CREATE TABLE omemo_own (
        account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
        state      TEXT NOT NULL
    );
    CREATE TABLE omemo_devices (
        account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        jid        TEXT NOT NULL,
        device_id  INTEGER NOT NULL,
        active     INTEGER NOT NULL DEFAULT 1,
        trust      TEXT NOT NULL DEFAULT 'undecided',
        identity   TEXT NOT NULL DEFAULT '',
        PRIMARY KEY (account_id, jid, device_id)
    );
    CREATE TABLE omemo_sessions (
        account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        jid        TEXT NOT NULL,
        device_id  INTEGER NOT NULL,
        state      TEXT NOT NULL,
        PRIMARY KEY (account_id, jid, device_id)
    );
    "#,
    // Version 3: corrections, retractions, replies and reactions
    r#"
    ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN retracted INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN reply_to TEXT NOT NULL DEFAULT '';
    ALTER TABLE messages ADD COLUMN reply_to_sender TEXT NOT NULL DEFAULT '';
    ALTER TABLE messages ADD COLUMN reply_preview TEXT NOT NULL DEFAULT '';
    CREATE TABLE reactions (
        message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        sender     TEXT NOT NULL,
        emojis     TEXT NOT NULL,
        PRIMARY KEY (message_id, sender)
    );
    "#,
    // Version 4: group chats
    r#"
    CREATE TABLE rooms (
        conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
        nick        TEXT NOT NULL,
        password    TEXT NOT NULL DEFAULT '',
        name        TEXT NOT NULL DEFAULT '',
        autojoin    INTEGER NOT NULL DEFAULT 1,
        mam_last_id TEXT
    );
    "#,
    // Version 5: shared files
    r#"
    CREATE TABLE attachments (
        message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
        url        TEXT NOT NULL,
        name       TEXT NOT NULL,
        mime       TEXT NOT NULL DEFAULT '',
        size       INTEGER,
        path       TEXT NOT NULL DEFAULT '',
        thumbnail  TEXT NOT NULL DEFAULT '',
        state      TEXT NOT NULL
    );
    "#,
    // Version 6: avatars
    r#"
    CREATE TABLE avatars (
        account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        jid        TEXT NOT NULL,
        hash       TEXT NOT NULL,
        path       TEXT NOT NULL,
        PRIMARY KEY (account_id, jid)
    );
    "#,
];

/// Where a message stands. Outgoing: pending → sent → received → displayed,
/// or failed. Incoming messages are `received` until the user has seen them.
pub mod state {
    pub const PENDING: &str = "pending";
    pub const SENT: &str = "sent";
    pub const RECEIVED: &str = "received";
    pub const DISPLAYED: &str = "displayed";
    pub const FAILED: &str = "failed";
}

#[derive(Clone, Debug)]
pub struct AccountRow {
    pub id: i64,
    pub jid: String,
    pub password: String,
    pub enabled: bool,
    pub roster_ver: Option<String>,
    pub mam_last_id: Option<String>,
    /// Connection options as JSON (see `connect::Server`).
    pub options: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContactRow {
    pub jid: String,
    pub name: String,
    pub subscription: String,
    pub ask: bool,
    pub groups: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ConversationRow {
    pub id: i64,
    #[allow(dead_code)] // read for completeness; conversations are found by account already
    pub account_id: i64,
    pub peer: String,
    pub kind: String,
    pub pinned: bool,
    pub muted: String,
    pub unread: u32,
    pub last_activity: i64,
    pub encryption_wanted: bool,
}

#[derive(Clone, Debug)]
pub struct MessageRow {
    pub id: i64,
    pub origin_id: String,
    pub server_id: Option<String>,
    pub outgoing: bool,
    pub sender: String,
    pub body: String,
    /// Milliseconds since the Unix epoch.
    pub timestamp: i64,
    pub state: String,
    /// none, omemo, omemo-untrusted or omemo-failed (could not be decrypted).
    pub encryption: String,
    /// The body was corrected (XEP-0308).
    pub edited: bool,
    /// The sender retracted the message (XEP-0424); the body is gone.
    pub retracted: bool,
    /// The id of the message this one replies to (XEP-0461), or "".
    pub reply_to: String,
    pub reply_to_sender: String,
    /// The start of the replied-to message, for showing the quote.
    pub reply_preview: String,
    /// Reactions (XEP-0444): each sender's emojis.
    pub reactions: Vec<(String, Vec<String>)>,
    /// The file the message shares, if any.
    pub attachment: Option<AttachmentRow>,
}

/// A file shared in a message.
#[derive(Clone, Debug, PartialEq)]
pub struct AttachmentRow {
    pub message_id: i64,
    /// The link as shared (https:// or aesgcm://).
    pub url: String,
    pub name: String,
    pub mime: String,
    pub size: Option<i64>,
    /// Where the file is on disk, once downloaded (or sent from here).
    pub path: String,
    pub thumbnail: String,
    /// See `files::state`.
    pub state: String,
}

/// A message about to be stored. Duplicates (same conversation, origin id and
/// direction) are ignored, which makes Carbons, MAM and live delivery safe to
/// overlap.
#[derive(Clone, Debug, Default)]
pub struct NewMessage {
    pub origin_id: String,
    pub server_id: Option<String>,
    pub outgoing: bool,
    pub sender: String,
    pub body: String,
    pub timestamp: i64,
    pub state: &'static str,
    pub encryption: &'static str,
    pub reply_to: String,
    pub reply_to_sender: String,
    pub reply_preview: String,
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (or creates) the database in `dir`, unlocked with `key`.
    pub fn open(dir: &Path, key: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("piopio.db");
        let db = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        db.pragma_update(None, "key", key)?;
        // Fails here, not later, if the key is wrong or the file isn't ours.
        db.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .context("the database could not be unlocked with the key from the keyring")?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&db)?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
        })
    }

    async fn with<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut db = db.lock().expect("database lock poisoned");
            f(&mut db)
        })
        .await?
        .map_err(Into::into)
    }

    // Accounts

    pub async fn accounts(&self) -> anyhow::Result<Vec<AccountRow>> {
        self.with(|db| {
            let mut q = db.prepare(
                "SELECT id, jid, password, enabled, roster_ver, NULLIF(mam_last_id, ''), options
                 FROM accounts ORDER BY id",
            )?;
            q.query_map([], |r| {
                Ok(AccountRow {
                    id: r.get(0)?,
                    jid: r.get(1)?,
                    password: r.get(2)?,
                    enabled: r.get(3)?,
                    roster_ver: r.get(4)?,
                    mam_last_id: r.get(5)?,
                    options: r.get(6)?,
                })
            })?
            .collect()
        })
        .await
    }

    pub async fn add_account(
        &self,
        jid: String,
        password: String,
        options: String,
    ) -> anyhow::Result<i64> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO accounts (jid, password, options) VALUES (?1, ?2, ?3)",
                params![jid, password, options],
            )?;
            Ok(db.last_insert_rowid())
        })
        .await
    }

    pub async fn remove_account(&self, id: i64) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute("DELETE FROM accounts WHERE id = ?1", [id])
                .map(drop)
        })
        .await
    }

    pub async fn set_account_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE accounts SET enabled = ?2 WHERE id = ?1",
                params![id, enabled],
            )
            .map(drop)
        })
        .await
    }

    pub async fn set_password(&self, id: i64, password: String) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE accounts SET password = ?2 WHERE id = ?1",
                params![id, password],
            )
            .map(drop)
        })
        .await
    }

    pub async fn set_mam_last_id(&self, id: i64, last: String) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE accounts SET mam_last_id = ?2 WHERE id = ?1",
                params![id, last],
            )
            .map(drop)
        })
        .await
    }

    // Roster

    pub async fn contacts(&self, account: i64) -> anyhow::Result<Vec<ContactRow>> {
        self.with(move |db| {
            let mut q = db.prepare(
                "SELECT jid, name, subscription, ask, groups FROM contacts WHERE account_id = ?1 ORDER BY jid",
            )?;
            q.query_map([account], contact_from_row)?.collect()
        })
        .await
    }

    /// Replaces the whole roster (after a full roster fetch) and stores its version.
    pub async fn replace_roster(
        &self,
        account: i64,
        version: Option<String>,
        contacts: Vec<ContactRow>,
    ) -> anyhow::Result<()> {
        self.with(move |db| {
            let tx = db.transaction()?;
            tx.execute("DELETE FROM contacts WHERE account_id = ?1", [account])?;
            for c in &contacts {
                upsert_contact(&tx, account, c)?;
            }
            tx.execute(
                "UPDATE accounts SET roster_ver = ?2 WHERE id = ?1",
                params![account, version],
            )?;
            tx.commit()
        })
        .await
    }

    /// Applies one roster push. `None` removes the contact.
    pub async fn apply_roster_push(
        &self,
        account: i64,
        version: Option<String>,
        jid: String,
        contact: Option<ContactRow>,
    ) -> anyhow::Result<()> {
        self.with(move |db| {
            let tx = db.transaction()?;
            match contact {
                Some(c) => upsert_contact(&tx, account, &c)?,
                None => {
                    tx.execute(
                        "DELETE FROM contacts WHERE account_id = ?1 AND jid = ?2",
                        params![account, jid],
                    )?;
                }
            }
            if version.is_some() {
                tx.execute(
                    "UPDATE accounts SET roster_ver = ?2 WHERE id = ?1",
                    params![account, version],
                )?;
            }
            tx.commit()
        })
        .await
    }

    // Conversations

    pub async fn conversations(&self, account: i64) -> anyhow::Result<Vec<ConversationRow>> {
        self.with(move |db| {
            let mut q = db.prepare(&format!(
                "SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE account_id = ?1 ORDER BY id"
            ))?;
            q.query_map([account], conversation_from_row)?.collect()
        })
        .await
    }

    /// Finds the conversation with `peer`, creating it if needed.
    pub async fn conversation(
        &self,
        account: i64,
        peer: String,
        kind: &'static str,
    ) -> anyhow::Result<(ConversationRow, bool)> {
        self.with(move |db| {
            let created = db.execute(
                "INSERT OR IGNORE INTO conversations (account_id, peer, kind) VALUES (?1, ?2, ?3)",
                params![account, peer, kind],
            )? > 0;
            let row = db.query_row(
                &format!(
                    "SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE account_id = ?1 AND peer = ?2 AND kind = ?3"
                ),
                params![account, peer, kind],
                conversation_from_row,
            )?;
            Ok((row, created))
        })
        .await
    }

    pub async fn update_conversation(&self, row: ConversationRow) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE conversations SET pinned = ?2, muted = ?3, unread = ?4, last_activity = ?5,
                 encryption_wanted = ?6 WHERE id = ?1",
                params![
                    row.id,
                    row.pinned,
                    row.muted,
                    row.unread,
                    row.last_activity,
                    row.encryption_wanted
                ],
            )
            .map(drop)
        })
        .await
    }

    // Messages

    /// Stores a message. Returns it, or None if it was already stored.
    pub async fn insert_message(
        &self,
        conversation: i64,
        m: NewMessage,
    ) -> anyhow::Result<Option<MessageRow>> {
        self.with(move |db| {
            let inserted = db.execute(
                "INSERT OR IGNORE INTO messages
                 (conversation_id, origin_id, server_id, outgoing, sender, body, timestamp, state,
                  encryption, reply_to, reply_to_sender, reply_preview)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    conversation,
                    m.origin_id,
                    m.server_id,
                    m.outgoing,
                    m.sender,
                    m.body,
                    m.timestamp,
                    m.state,
                    m.encryption,
                    m.reply_to,
                    m.reply_to_sender,
                    m.reply_preview
                ],
            )?;
            if inserted == 0 {
                // Already known; remember the archive id if we only learn it now.
                if let Some(server_id) = &m.server_id {
                    db.execute(
                        "UPDATE messages SET server_id = ?4 WHERE conversation_id = ?1
                         AND origin_id = ?2 AND outgoing = ?3 AND server_id IS NULL",
                        params![conversation, m.origin_id, m.outgoing, server_id],
                    )?;
                }
                return Ok(None);
            }
            Ok(Some(MessageRow {
                id: db.last_insert_rowid(),
                origin_id: m.origin_id,
                server_id: m.server_id,
                outgoing: m.outgoing,
                sender: m.sender,
                body: m.body,
                timestamp: m.timestamp,
                state: m.state.to_owned(),
                encryption: m.encryption.to_owned(),
                edited: false,
                retracted: false,
                reply_to: m.reply_to,
                reply_to_sender: m.reply_to_sender,
                reply_preview: m.reply_preview,
                reactions: vec![],
                attachment: None,
            }))
        })
        .await
    }

    /// One page of history: up to `limit` messages older than `before` (a
    /// message id; 0 for the newest), returned oldest first.
    pub async fn messages(
        &self,
        conversation: i64,
        before: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<MessageRow>> {
        self.with(move |db| {
            let mut q = db.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1
                 AND (?2 = 0 OR (timestamp, id) < (SELECT timestamp, id FROM messages WHERE id = ?2))
                 ORDER BY timestamp DESC, id DESC LIMIT ?3"
            ))?;
            let mut rows: Vec<MessageRow> = q
                .query_map(params![conversation, before, limit], message_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            rows.reverse();
            for row in &mut rows {
                row.reactions = reactions_of(db, row.id)?;
                row.attachment = attachment_of(db, row.id)?;
            }
            Ok(rows)
        })
        .await
    }

    /// Updates an outgoing message's state, found by its origin id. Returns
    /// the updated row, or None if no such message exists. States only move
    /// forward: a late "sent" never overwrites "received".
    pub async fn advance_state(
        &self,
        conversation: i64,
        origin_id: String,
        new_state: &'static str,
    ) -> anyhow::Result<Option<MessageRow>> {
        self.with(move |db| {
            let row = db
                .query_row(
                    &format!(
                        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1
                         AND origin_id = ?2 AND outgoing = 1"
                    ),
                    params![conversation, origin_id],
                    message_from_row,
                )
                .optional()?;
            let Some(mut row) = row else { return Ok(None) };
            if rank(new_state) <= rank(&row.state) && new_state != state::FAILED {
                return Ok(None);
            }
            db.execute(
                "UPDATE messages SET state = ?2 WHERE id = ?1",
                params![row.id, new_state],
            )?;
            row.state = new_state.to_owned();
            Ok(Some(row))
        })
        .await
    }
}

/// A group chat we take part in (or took part in).
#[derive(Clone, Debug, PartialEq)]
pub struct RoomRow {
    pub conversation_id: i64,
    pub nick: String,
    pub password: String,
    pub name: String,
    pub autojoin: bool,
    /// The newest archive id fetched from the room's archive.
    pub mam_last_id: Option<String>,
}

impl Store {
    pub async fn room(&self, conversation: i64) -> anyhow::Result<Option<RoomRow>> {
        self.with(move |db| {
            db.query_row(
                "SELECT conversation_id, nick, password, name, autojoin, mam_last_id
                 FROM rooms WHERE conversation_id = ?1",
                [conversation],
                room_from_row,
            )
            .optional()
        })
        .await
    }

    pub async fn set_room(&self, room: RoomRow) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO rooms (conversation_id, nick, password, name, autojoin, mam_last_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (conversation_id) DO UPDATE SET nick = excluded.nick,
                   password = excluded.password, name = excluded.name,
                   autojoin = excluded.autojoin, mam_last_id = excluded.mam_last_id",
                params![
                    room.conversation_id,
                    room.nick,
                    room.password,
                    room.name,
                    room.autojoin,
                    room.mam_last_id
                ],
            )
            .map(drop)
        })
        .await
    }
}

fn room_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RoomRow> {
    Ok(RoomRow {
        conversation_id: r.get(0)?,
        nick: r.get(1)?,
        password: r.get(2)?,
        name: r.get(3)?,
        autojoin: r.get(4)?,
        mam_last_id: r.get(5)?,
    })
}

/// What we know about one OMEMO device of a contact (or of ourselves).
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRow {
    pub jid: String,
    pub device_id: u32,
    pub active: bool,
    /// undecided, blind, trusted or untrusted.
    pub trust: String,
    /// The identity key in hex, once known.
    pub identity: String,
}

impl Store {
    /// Whether a message is already stored (to skip decrypting it again).
    pub async fn has_message(
        &self,
        conversation: i64,
        origin_id: String,
        outgoing: bool,
    ) -> anyhow::Result<bool> {
        self.with(move |db| {
            db.query_row(
                "SELECT 1 FROM messages WHERE conversation_id = ?1 AND origin_id = ?2 AND outgoing = ?3",
                params![conversation, origin_id, outgoing],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
        })
        .await
    }

    /// Finds a message of a conversation by the id others refer to it with:
    /// its origin/message id or its archive id.
    pub async fn find_message(
        &self,
        conversation: i64,
        id: String,
    ) -> anyhow::Result<Option<MessageRow>> {
        self.with(move |db| {
            let row = db
                .query_row(
                    &format!(
                        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1
                         AND (origin_id = ?2 OR server_id = ?2) ORDER BY id LIMIT 1"
                    ),
                    params![conversation, id],
                    message_from_row,
                )
                .optional()?;
            let Some(mut row) = row else { return Ok(None) };
            row.reactions = reactions_of(db, row.id)?;
            row.attachment = attachment_of(db, row.id)?;
            Ok(Some(row))
        })
        .await
    }

    /// A message of a conversation by its row id.
    pub async fn message_by_id(
        &self,
        conversation: i64,
        id: i64,
    ) -> anyhow::Result<Option<MessageRow>> {
        self.with(move |db| {
            let row = db
                .query_row(
                    &format!(
                        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1 AND id = ?2"
                    ),
                    params![conversation, id],
                    message_from_row,
                )
                .optional()?;
            let Some(mut row) = row else { return Ok(None) };
            row.reactions = reactions_of(db, row.id)?;
                row.attachment = attachment_of(db, row.id)?;
            Ok(Some(row))
        })
        .await
    }

    /// A JID's avatar: (hash, path); empty strings once removed.
    pub async fn avatar(
        &self,
        account: i64,
        jid: String,
    ) -> anyhow::Result<Option<(String, String)>> {
        self.with(move |db| {
            db.query_row(
                "SELECT hash, path FROM avatars WHERE account_id = ?1 AND jid = ?2",
                params![account, jid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
        .await
    }

    pub async fn set_avatar(
        &self,
        account: i64,
        jid: String,
        hash: String,
        path: String,
    ) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO avatars (account_id, jid, hash, path) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (account_id, jid) DO UPDATE SET hash = excluded.hash, path = excluded.path",
                params![account, jid, hash, path],
            )
            .map(drop)
        })
        .await
    }

    pub async fn set_attachment(&self, a: AttachmentRow) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO attachments (message_id, url, name, mime, size, path, thumbnail, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (message_id) DO UPDATE SET url = excluded.url, name = excluded.name,
                   mime = excluded.mime, size = excluded.size, path = excluded.path,
                   thumbnail = excluded.thumbnail, state = excluded.state",
                params![a.message_id, a.url, a.name, a.mime, a.size, a.path, a.thumbnail, a.state],
            )
            .map(drop)
        })
        .await
    }

    /// Sets a message's body without marking it edited (a file's link,
    /// once uploaded).
    pub async fn set_body(&self, id: i64, body: String) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE messages SET body = ?2 WHERE id = ?1",
                params![id, body],
            )
            .map(drop)
        })
        .await
    }

    /// Replaces a message's body (a correction).
    pub async fn correct_message(&self, id: i64, body: String) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE messages SET body = ?2, edited = 1 WHERE id = ?1 AND retracted = 0",
                params![id, body],
            )
            .map(drop)
        })
        .await
    }

    /// Forgets a retracted message's content (body, reactions, quote).
    pub async fn retract_message(&self, id: i64) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "UPDATE messages SET body = '', reply_preview = '', retracted = 1 WHERE id = ?1",
                [id],
            )?;
            db.execute("DELETE FROM reactions WHERE message_id = ?1", [id])
                .map(drop)
        })
        .await
    }

    /// Sets one sender's reactions to a message (empty removes them) and
    /// returns everyone's reactions to it.
    pub async fn set_reactions(
        &self,
        id: i64,
        sender: String,
        emojis: Vec<String>,
    ) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        self.with(move |db| {
            if emojis.is_empty() {
                db.execute(
                    "DELETE FROM reactions WHERE message_id = ?1 AND sender = ?2",
                    params![id, sender],
                )?;
            } else {
                db.execute(
                    "INSERT INTO reactions (message_id, sender, emojis) VALUES (?1, ?2, ?3)
                     ON CONFLICT (message_id, sender) DO UPDATE SET emojis = excluded.emojis",
                    params![
                        id,
                        sender,
                        serde_json::to_string(&emojis).unwrap_or_default()
                    ],
                )?;
            }
            reactions_of(db, id)
        })
        .await
    }

    /// Records how a pending outgoing message was finally sent.
    pub async fn set_message_encryption(
        &self,
        conversation: i64,
        origin_id: String,
        encryption: &'static str,
    ) -> anyhow::Result<bool> {
        self.with(move |db| {
            db.execute(
                "UPDATE messages SET encryption = ?3
                 WHERE conversation_id = ?1 AND origin_id = ?2 AND outgoing = 1",
                params![conversation, origin_id, encryption],
            )
            .map(|n| n > 0)
        })
        .await
    }

    pub async fn omemo_own(&self, account: i64) -> anyhow::Result<Option<String>> {
        self.with(move |db| {
            db.query_row(
                "SELECT state FROM omemo_own WHERE account_id = ?1",
                [account],
                |r| r.get(0),
            )
            .optional()
        })
        .await
    }

    pub async fn set_omemo_own(&self, account: i64, state: String) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO omemo_own (account_id, state) VALUES (?1, ?2)
                 ON CONFLICT (account_id) DO UPDATE SET state = excluded.state",
                params![account, state],
            )
            .map(drop)
        })
        .await
    }

    pub async fn devices(&self, account: i64, jid: String) -> anyhow::Result<Vec<DeviceRow>> {
        self.with(move |db| {
            let mut q = db.prepare(
                "SELECT jid, device_id, active, trust, identity FROM omemo_devices
                 WHERE account_id = ?1 AND jid = ?2 ORDER BY device_id",
            )?;
            q.query_map(params![account, jid], |r| {
                Ok(DeviceRow {
                    jid: r.get(0)?,
                    device_id: r.get(1)?,
                    active: r.get(2)?,
                    trust: r.get(3)?,
                    identity: r.get(4)?,
                })
            })?
            .collect()
        })
        .await
    }

    /// Records a published device list: listed devices become active (new
    /// ones are added), devices no longer listed become inactive.
    pub async fn set_device_list(
        &self,
        account: i64,
        jid: String,
        ids: Vec<u32>,
    ) -> anyhow::Result<()> {
        self.with(move |db| {
            let tx = db.transaction()?;
            tx.execute(
                "UPDATE omemo_devices SET active = 0 WHERE account_id = ?1 AND jid = ?2",
                params![account, jid],
            )?;
            for id in ids {
                tx.execute(
                    "INSERT INTO omemo_devices (account_id, jid, device_id) VALUES (?1, ?2, ?3)
                     ON CONFLICT (account_id, jid, device_id) DO UPDATE SET active = 1",
                    params![account, jid, id],
                )?;
            }
            tx.commit()
        })
        .await
    }

    /// Updates one device's identity key and trust (inserting it if needed).
    pub async fn set_device(
        &self,
        account: i64,
        jid: String,
        device_id: u32,
        identity: String,
        trust: String,
    ) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO omemo_devices (account_id, jid, device_id, active, trust, identity)
                 VALUES (?1, ?2, ?3, 0, ?5, ?4)
                 ON CONFLICT (account_id, jid, device_id) DO UPDATE SET
                   identity = excluded.identity, trust = excluded.trust",
                params![account, jid, device_id, identity, trust],
            )
            .map(drop)
        })
        .await
    }

    pub async fn omemo_session(
        &self,
        account: i64,
        jid: String,
        device_id: u32,
    ) -> anyhow::Result<Option<String>> {
        self.with(move |db| {
            db.query_row(
                "SELECT state FROM omemo_sessions WHERE account_id = ?1 AND jid = ?2 AND device_id = ?3",
                params![account, jid, device_id],
                |r| r.get(0),
            )
            .optional()
        })
        .await
    }

    pub async fn set_omemo_session(
        &self,
        account: i64,
        jid: String,
        device_id: u32,
        state: String,
    ) -> anyhow::Result<()> {
        self.with(move |db| {
            db.execute(
                "INSERT INTO omemo_sessions (account_id, jid, device_id, state) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (account_id, jid, device_id) DO UPDATE SET state = excluded.state",
                params![account, jid, device_id, state],
            )
            .map(drop)
        })
        .await
    }
}

fn rank(state: &str) -> u8 {
    match state {
        state::FAILED => 0,
        state::PENDING => 1,
        state::SENT => 2,
        state::RECEIVED => 3,
        state::DISPLAYED => 4,
        _ => 0,
    }
}

const CONVERSATION_COLUMNS: &str =
    "id, account_id, peer, kind, pinned, muted, unread, last_activity, encryption_wanted";
const MESSAGE_COLUMNS: &str = "id, origin_id, server_id, outgoing, sender, body, timestamp, state, \
     encryption, edited, retracted, reply_to, reply_to_sender, reply_preview";

fn attachment_of(db: &Connection, message: i64) -> rusqlite::Result<Option<AttachmentRow>> {
    db.prepare_cached(
        "SELECT message_id, url, name, mime, size, path, thumbnail, state
         FROM attachments WHERE message_id = ?1",
    )?
    .query_row([message], |r| {
        Ok(AttachmentRow {
            message_id: r.get(0)?,
            url: r.get(1)?,
            name: r.get(2)?,
            mime: r.get(3)?,
            size: r.get(4)?,
            path: r.get(5)?,
            thumbnail: r.get(6)?,
            state: r.get(7)?,
        })
    })
    .optional()
}

fn reactions_of(db: &Connection, message: i64) -> rusqlite::Result<Vec<(String, Vec<String>)>> {
    let mut q = db.prepare_cached(
        "SELECT sender, emojis FROM reactions WHERE message_id = ?1 ORDER BY sender",
    )?;
    q.query_map([message], |r| {
        let emojis: String = r.get(1)?;
        Ok((r.get(0)?, serde_json::from_str(&emojis).unwrap_or_default()))
    })?
    .collect()
}

fn conversation_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationRow> {
    Ok(ConversationRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        peer: r.get(2)?,
        kind: r.get(3)?,
        pinned: r.get(4)?,
        muted: r.get(5)?,
        unread: r.get(6)?,
        last_activity: r.get(7)?,
        encryption_wanted: r.get(8)?,
    })
}

fn message_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: r.get(0)?,
        origin_id: r.get(1)?,
        server_id: r.get(2)?,
        outgoing: r.get(3)?,
        sender: r.get(4)?,
        body: r.get(5)?,
        timestamp: r.get(6)?,
        state: r.get(7)?,
        encryption: r.get(8)?,
        edited: r.get(9)?,
        retracted: r.get(10)?,
        reply_to: r.get(11)?,
        reply_to_sender: r.get(12)?,
        reply_preview: r.get(13)?,
        reactions: vec![],
        attachment: None,
    })
}

fn contact_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ContactRow> {
    let groups: String = r.get(4)?;
    Ok(ContactRow {
        jid: r.get(0)?,
        name: r.get(1)?,
        subscription: r.get(2)?,
        ask: r.get(3)?,
        groups: serde_json::from_str(&groups).unwrap_or_default(),
    })
}

fn upsert_contact(db: &Connection, account: i64, c: &ContactRow) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO contacts (account_id, jid, name, subscription, ask, groups)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (account_id, jid) DO UPDATE SET
           name = excluded.name, subscription = excluded.subscription,
           ask = excluded.ask, groups = excluded.groups",
        params![
            account,
            c.jid,
            c.name,
            c.subscription,
            c.ask,
            serde_json::to_string(&c.groups).unwrap_or_else(|_| "[]".into())
        ],
    )
    .map(drop)
}

fn migrate(db: &Connection) -> anyhow::Result<()> {
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let version = usize::try_from(version).unwrap_or(0);
    if version > SCHEMA.len() {
        bail!(
            "the database was written by a newer Piopio (schema {version}); this build knows {}",
            SCHEMA.len()
        );
    }
    for (i, step) in SCHEMA.iter().enumerate().skip(version) {
        db.execute_batch(&format!(
            "BEGIN; {step}; PRAGMA user_version = {}; COMMIT;",
            i + 1
        ))
        .with_context(|| format!("migrating the database to schema {}", i + 1))?;
    }
    Ok(())
}

/// Where the database lives: `$PIOPIO_DATA_DIR`, or `$XDG_DATA_HOME/piopio`
/// (inside Flatpak that is the app's own data directory).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PIOPIO_DATA_DIR") {
        return dir.into();
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("piopio")
}

/// Gets the database key from the keyring, creating it on first run.
///
/// `$PIOPIO_DB_KEY` overrides the keyring; tests use it because CI has none.
pub async fn database_key() -> anyhow::Result<String> {
    if let Ok(key) = std::env::var("PIOPIO_DB_KEY") {
        return Ok(key);
    }
    let keyring = oo7::Keyring::new()
        .await
        .context("could not open the keyring (Secret portal or Secret Service)")?;
    keyring.unlock().await.ok();
    let attributes = [
        ("application", piopio_dbus::APP_ID),
        ("purpose", "database-key"),
    ];
    let items = keyring.search_items(&attributes).await?;
    if let Some(item) = items.first() {
        let secret = item.secret().await?;
        return String::from_utf8(secret.to_vec()).context("the stored key is not text");
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let key: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    keyring
        .create_item("Piopio database key", &attributes, key.as_bytes(), true)
        .await?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "test-key").unwrap();
        (dir, store)
    }

    fn msg(id: &str, ts: i64) -> NewMessage {
        NewMessage {
            origin_id: id.into(),
            server_id: None,
            outgoing: true,
            sender: "me@example.org".into(),
            body: format!("body {id}"),
            timestamp: ts,
            state: state::PENDING,
            encryption: "none",
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn wrong_key_is_refused() {
        let (dir, store) = open();
        store
            .add_account("a@example.org".into(), "pw".into(), "{}".into())
            .await
            .unwrap();
        drop(store);
        assert!(Store::open(dir.path(), "another-key").is_err());
        let again = Store::open(dir.path(), "test-key").unwrap();
        assert_eq!(again.accounts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn file_is_not_plain_sqlite() {
        let (dir, store) = open();
        store
            .add_account("secret@example.org".into(), "hunter2".into(), "{}".into())
            .await
            .unwrap();
        drop(store);
        let bytes = std::fs::read(dir.path().join("piopio.db")).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"));
        let wal = std::fs::read(dir.path().join("piopio.db-wal")).unwrap_or_default();
        for file in [&bytes, &wal] {
            assert!(!file.windows(7).any(|w| w == b"hunter2"));
        }
    }

    #[tokio::test]
    async fn messages_dedupe_page_and_advance() {
        let (_dir, store) = open();
        let account = store
            .add_account("a@example.org".into(), "pw".into(), "{}".into())
            .await
            .unwrap();
        let (conv, created) = store
            .conversation(account, "b@example.org".into(), "chat")
            .await
            .unwrap();
        assert!(created);
        let (_, created) = store
            .conversation(account, "b@example.org".into(), "chat")
            .await
            .unwrap();
        assert!(!created);

        for i in 0..5 {
            assert!(
                store
                    .insert_message(conv.id, msg(&format!("m{i}"), 1000 + i))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            store
                .insert_message(conv.id, msg("m2", 1002))
                .await
                .unwrap()
                .is_none()
        );

        let newest = store.messages(conv.id, 0, 2).await.unwrap();
        assert_eq!(
            newest
                .iter()
                .map(|m| m.origin_id.as_str())
                .collect::<Vec<_>>(),
            ["m3", "m4"]
        );
        let older = store.messages(conv.id, newest[0].id, 10).await.unwrap();
        assert_eq!(older.len(), 3);
        assert_eq!(older[0].origin_id, "m0");

        assert!(
            store
                .advance_state(conv.id, "m1".into(), state::RECEIVED)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .advance_state(conv.id, "m1".into(), state::SENT)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn roster_replace_and_push() {
        let (_dir, store) = open();
        let account = store
            .add_account("a@example.org".into(), "pw".into(), "{}".into())
            .await
            .unwrap();
        let c = |jid: &str| ContactRow {
            jid: jid.into(),
            name: String::new(),
            subscription: "both".into(),
            ask: false,
            groups: vec!["Friends".into()],
        };
        store
            .replace_roster(account, Some("v1".into()), vec![c("b@x"), c("c@x")])
            .await
            .unwrap();
        store
            .apply_roster_push(account, Some("v2".into()), "b@x".into(), None)
            .await
            .unwrap();
        let contacts = store.contacts(account).await.unwrap();
        assert_eq!(contacts, vec![c("c@x")]);
        assert_eq!(
            store.accounts().await.unwrap()[0].roster_ver.as_deref(),
            Some("v2")
        );
    }
}
