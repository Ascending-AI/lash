//! The platform's workspace store: users, channels, messages, the installed
//! app, and the event-delivery outbox.
//!
//! Everything here is plain product state. Nothing in this file knows that Lash
//! exists — that separation is the example's thesis.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::ids::Ts;

/// Idempotent schema, applied on every boot.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS workspace (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    team_id     TEXT NOT NULL,
    team_name   TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    handle        TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL,
    is_bot        INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS channels (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    topic       TEXT NOT NULL DEFAULT '',
    purpose     TEXT NOT NULL DEFAULT '',
    creator     TEXT NOT NULL,
    is_general  INTEGER NOT NULL DEFAULT 0,
    is_archived INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

-- `ts` is stored as epoch microseconds and is the message's identity within a
-- channel, hence the composite primary key. The wire form is rendered from it.
CREATE TABLE IF NOT EXISTS messages (
    channel_id      TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    ts              INTEGER NOT NULL,
    author_user_id  TEXT REFERENCES users(id),
    bot_id          TEXT,
    username        TEXT,
    subtype         TEXT,
    text            TEXT NOT NULL,
    thread_ts       INTEGER,
    metadata_json   TEXT,
    PRIMARY KEY (channel_id, ts)
);
CREATE INDEX IF NOT EXISTS idx_messages_thread
    ON messages(channel_id, thread_ts, ts);

-- A broadcast reply remains one threaded message. This relation only controls
-- its second projection into channel history; duplicating the message row would
-- give one Slack message two identities.
CREATE TABLE IF NOT EXISTS message_broadcasts (
    channel_id TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (channel_id, ts),
    FOREIGN KEY (channel_id, ts) REFERENCES messages(channel_id, ts) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS apps (
    id           TEXT PRIMARY KEY,
    bot_id       TEXT NOT NULL,
    bot_user_id  TEXT NOT NULL REFERENCES users(id),
    request_url  TEXT,
    verified_at  INTEGER,
    created_at   INTEGER NOT NULL
);

-- At-least-once delivery lives here rather than in memory so a platform restart
-- resumes undelivered events instead of dropping them. The bot's deduplication
-- story only means something against a sender that really does retry.
CREATE TABLE IF NOT EXISTS event_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id          TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    event_id        TEXT NOT NULL UNIQUE,
    payload_json    TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    delivered_at    INTEGER,
    abandoned_at    INTEGER,
    last_error      TEXT,
    -- Slack's `x-slack-retry-reason` reports why the *previous* attempt failed,
    -- so the reason has to outlive the attempt that produced it.
    last_reason     TEXT
);
CREATE INDEX IF NOT EXISTS idx_event_outbox_ready
    ON event_outbox(delivered_at, abandoned_at, next_attempt_at);
";

/// A workspace member. Bots are members too, exactly as in Slack.
#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    pub is_bot: bool,
    pub created_at: u64,
}

/// A channel.
#[derive(Clone, Debug)]
pub struct ChannelRow {
    pub id: String,
    pub name: String,
    pub topic: String,
    pub purpose: String,
    pub creator: String,
    pub is_general: bool,
    pub is_archived: bool,
    pub created_at: u64,
}

/// A message. `author` distinguishes the two Slack authorship shapes.
#[derive(Clone, Debug)]
pub struct MessageRow {
    pub channel_id: String,
    pub ts: Ts,
    pub author: Author,
    pub text: String,
    pub thread_ts: Option<Ts>,
    pub reply_broadcast: bool,
    pub metadata_json: Option<String>,
}

/// Who wrote a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    /// A human: the wire form carries `user`.
    User { user_id: String },
    /// An app: the wire form carries `bot_id`, `username` and
    /// `subtype: "bot_message"`.
    App { bot_id: String, username: String },
}

/// The installed app's identity and Events API registration.
#[derive(Clone, Debug)]
pub struct AppRow {
    pub id: String,
    pub bot_id: String,
    pub bot_user_id: String,
    pub request_url: Option<String>,
    pub verified_at: Option<u64>,
}

/// An outbox row awaiting (re)delivery.
#[derive(Clone, Debug)]
pub struct OutboxRow {
    pub id: i64,
    pub event_id: String,
    pub payload_json: String,
    /// Deliveries already attempted. Zero means this is the first delivery, so
    /// the attempt about to happen carries no retry headers.
    pub attempts: u32,
    /// Why the previous attempt failed, for `x-slack-retry-reason`.
    pub last_reason: Option<String>,
    pub request_url: String,
}

/// Insert the singleton workspace row if absent and return its team id.
pub fn ensure_workspace(connection: &Connection, team_id: &str, team_name: &str) -> Result<String> {
    connection.execute(
        "INSERT OR IGNORE INTO workspace (id, team_id, team_name, created_at)
         VALUES (1, ?1, ?2, ?3)",
        params![team_id, team_name, Ts::now().epoch_seconds()],
    )?;
    let stored: String =
        connection.query_row("SELECT team_id FROM workspace WHERE id = 1", [], |row| {
            row.get(0)
        })?;
    Ok(stored)
}

/// The workspace's team name, for `auth.test`.
pub fn team_name(connection: &Connection) -> Result<String> {
    Ok(
        connection.query_row("SELECT team_name FROM workspace WHERE id = 1", [], |row| {
            row.get(0)
        })?,
    )
}

/// Insert a user, or return the existing one with the same handle.
///
/// Handle collision resolving to the existing row is what makes the UI's
/// name-picker identity work across browser reloads without any auth.
pub fn upsert_user(
    connection: &Connection,
    id: &str,
    handle: &str,
    display_name: &str,
    is_bot: bool,
) -> Result<UserRow> {
    connection.execute(
        "INSERT OR IGNORE INTO users (id, handle, display_name, is_bot, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, handle, display_name, is_bot, Ts::now().epoch_seconds()],
    )?;
    user_by_handle(connection, handle)?
        .ok_or_else(|| anyhow::anyhow!("user `{handle}` vanished immediately after insert"))
}

/// Look a user up by handle.
pub fn user_by_handle(connection: &Connection, handle: &str) -> Result<Option<UserRow>> {
    Ok(connection
        .query_row(
            "SELECT id, handle, display_name, is_bot, created_at
             FROM users WHERE handle = ?1",
            params![handle],
            read_user,
        )
        .optional()?)
}

/// Look a user up by id.
pub fn user_by_id(connection: &Connection, id: &str) -> Result<Option<UserRow>> {
    Ok(connection
        .query_row(
            "SELECT id, handle, display_name, is_bot, created_at
             FROM users WHERE id = ?1",
            params![id],
            read_user,
        )
        .optional()?)
}

/// Every member, ordered by id so pagination is stable.
pub fn list_users(connection: &Connection) -> Result<Vec<UserRow>> {
    let mut statement = connection
        .prepare("SELECT id, handle, display_name, is_bot, created_at FROM users ORDER BY id")?;
    let rows = statement
        .query_map([], read_user)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn read_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: row.get(0)?,
        handle: row.get(1)?,
        display_name: row.get(2)?,
        is_bot: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// Insert a channel, or return the existing one with the same name.
pub fn upsert_channel(
    connection: &Connection,
    id: &str,
    name: &str,
    creator: &str,
    is_general: bool,
) -> Result<ChannelRow> {
    connection.execute(
        "INSERT OR IGNORE INTO channels (id, name, creator, is_general, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, name, creator, is_general, Ts::now().epoch_seconds()],
    )?;
    channel_by_name(connection, name)?
        .ok_or_else(|| anyhow::anyhow!("channel `{name}` vanished immediately after insert"))
}

/// Look a channel up by name (no leading `#`).
pub fn channel_by_name(connection: &Connection, name: &str) -> Result<Option<ChannelRow>> {
    Ok(connection
        .query_row(
            "SELECT id, name, topic, purpose, creator, is_general, is_archived, created_at
             FROM channels WHERE name = ?1",
            params![name],
            read_channel,
        )
        .optional()?)
}

/// Look a channel up by id.
pub fn channel_by_id(connection: &Connection, id: &str) -> Result<Option<ChannelRow>> {
    Ok(connection
        .query_row(
            "SELECT id, name, topic, purpose, creator, is_general, is_archived, created_at
             FROM channels WHERE id = ?1",
            params![id],
            read_channel,
        )
        .optional()?)
}

/// Every channel, ordered by id so pagination is stable.
pub fn list_channels(connection: &Connection, exclude_archived: bool) -> Result<Vec<ChannelRow>> {
    let mut statement = connection.prepare(
        "SELECT id, name, topic, purpose, creator, is_general, is_archived, created_at
         FROM channels
         WHERE (?1 = 0 OR is_archived = 0)
         ORDER BY id",
    )?;
    let rows = statement
        .query_map(params![exclude_archived], read_channel)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Number of members in the workspace, reported as every channel's
/// `num_members`: the platform has no per-channel membership.
pub fn member_count(connection: &Connection) -> Result<u32> {
    Ok(connection.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?)
}

fn read_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelRow> {
    Ok(ChannelRow {
        id: row.get(0)?,
        name: row.get(1)?,
        topic: row.get(2)?,
        purpose: row.get(3)?,
        creator: row.get(4)?,
        is_general: row.get(5)?,
        is_archived: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Append a message, minting the `ts` that becomes its identity.
///
/// The mint is `max(now, newest_ts + 1)`, so `ts` is unique and strictly
/// increasing per channel even when two posts land in the same microsecond.
/// Slack guarantees exactly this, and clients that treat `ts` as an ordering key
/// depend on it.
///
/// Takes a `Transaction` rather than a `Connection` on purpose: the caller must
/// commit the message and the Events API rows it implies together, or a crash
/// between the two commits loses the event forever. The type signature is what
/// keeps that invariant from being forgotten.
pub fn append_message(
    transaction: &rusqlite::Transaction<'_>,
    channel_id: &str,
    author: Author,
    text: &str,
    thread_ts: Option<Ts>,
    reply_broadcast: bool,
    metadata_json: Option<&str>,
) -> Result<MessageRow> {
    if channel_by_id(transaction, channel_id)?.is_none() {
        bail!("channel_not_found");
    }
    if let Some(parent) = thread_ts
        && !message_exists(transaction, channel_id, parent)?
    {
        bail!("thread_not_found");
    }
    let newest: Option<i64> = transaction.query_row(
        "SELECT MAX(ts) FROM messages WHERE channel_id = ?1",
        params![channel_id],
        |row| row.get(0),
    )?;
    let ts = match newest {
        Some(newest) => {
            Ts::from_micros((newest as u64).max(Ts::now().micros().saturating_sub(1))).next()
        }
        None => Ts::now(),
    };
    let (author_user_id, bot_id, username, subtype) = match &author {
        Author::User { user_id } => (Some(user_id.clone()), None, None, None),
        Author::App { bot_id, username } => (
            None,
            Some(bot_id.clone()),
            Some(username.clone()),
            Some("bot_message".to_string()),
        ),
    };
    transaction.execute(
        "INSERT INTO messages
            (channel_id, ts, author_user_id, bot_id, username, subtype, text, thread_ts, metadata_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            channel_id,
            ts.micros() as i64,
            author_user_id,
            bot_id,
            username,
            subtype,
            text,
            thread_ts.map(|parent| parent.micros() as i64),
            metadata_json,
        ],
    )?;
    if reply_broadcast && thread_ts.is_some() {
        transaction.execute(
            "INSERT INTO message_broadcasts (channel_id, ts) VALUES (?1, ?2)",
            params![channel_id, ts.micros() as i64],
        )?;
    }
    Ok(MessageRow {
        channel_id: channel_id.to_string(),
        ts,
        author,
        text: text.to_string(),
        thread_ts,
        reply_broadcast: reply_broadcast && thread_ts.is_some(),
        metadata_json: metadata_json.map(str::to_string),
    })
}

fn message_exists(connection: &Connection, channel_id: &str, ts: Ts) -> Result<bool> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM messages WHERE channel_id = ?1 AND ts = ?2",
            params![channel_id, ts.micros() as i64],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// A half-open `ts` window for a history query.
#[derive(Clone, Copy, Debug, Default)]
pub struct TsWindow {
    /// Exclusive lower bound unless `inclusive`.
    pub oldest: Option<Ts>,
    /// Exclusive upper bound unless `inclusive`.
    pub latest: Option<Ts>,
    pub inclusive: bool,
}

/// Newest-first channel history, as `conversations.history` returns it.
///
/// Only top-level messages: Slack's `conversations.history` hides thread
/// replies, which is why `conversations.replies` exists at all.
pub fn channel_history(
    connection: &Connection,
    channel_id: &str,
    window: TsWindow,
    limit: usize,
) -> Result<Vec<MessageRow>> {
    let (lower, upper) = window.bounds();
    let mut statement = connection.prepare(
        "SELECT channel_id, ts, author_user_id, bot_id, username, text, thread_ts, metadata_json,
                EXISTS (SELECT 1 FROM message_broadcasts
                        WHERE message_broadcasts.channel_id = messages.channel_id
                          AND message_broadcasts.ts = messages.ts)
         FROM messages
         WHERE channel_id = ?1
           AND (thread_ts IS NULL OR EXISTS (
               SELECT 1 FROM message_broadcasts
               WHERE message_broadcasts.channel_id = messages.channel_id
                 AND message_broadcasts.ts = messages.ts
           ))
           AND ts > ?2 AND ts < ?3
         ORDER BY ts DESC
         LIMIT ?4",
    )?;
    let rows = statement
        .query_map(
            params![channel_id, lower, upper, limit as i64],
            read_message,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Oldest-first thread replies, as `conversations.replies` returns them
/// (the parent is prepended by the caller).
pub fn thread_replies(
    connection: &Connection,
    channel_id: &str,
    parent: Ts,
    window: TsWindow,
    limit: usize,
) -> Result<Vec<MessageRow>> {
    let (lower, upper) = window.bounds();
    let mut statement = connection.prepare(
        "SELECT channel_id, ts, author_user_id, bot_id, username, text, thread_ts, metadata_json,
                EXISTS (SELECT 1 FROM message_broadcasts
                        WHERE message_broadcasts.channel_id = messages.channel_id
                          AND message_broadcasts.ts = messages.ts)
         FROM messages
         WHERE channel_id = ?1
           AND thread_ts = ?2
           AND ts > ?3 AND ts < ?4
         ORDER BY ts ASC
         LIMIT ?5",
    )?;
    let rows = statement
        .query_map(
            params![
                channel_id,
                parent.micros() as i64,
                lower,
                upper,
                limit as i64
            ],
            read_message,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Thread-parent statistics for `conversations.replies`.
pub fn thread_summary(
    connection: &Connection,
    channel_id: &str,
    parent: Ts,
) -> Result<ThreadSummary> {
    let mut statement = connection.prepare(
        "SELECT COUNT(*), COUNT(DISTINCT COALESCE(author_user_id, bot_id)), MAX(ts)
         FROM messages WHERE channel_id = ?1 AND thread_ts = ?2",
    )?;
    let summary = statement.query_row(params![channel_id, parent.micros() as i64], |row| {
        let latest: Option<i64> = row.get(2)?;
        Ok(ThreadSummary {
            reply_count: row.get(0)?,
            reply_users_count: row.get(1)?,
            latest_reply: latest.map(|ts| Ts::from_micros(ts as u64)),
        })
    })?;
    Ok(summary)
}

/// Reply statistics for one thread parent.
#[derive(Clone, Copy, Debug)]
pub struct ThreadSummary {
    pub reply_count: u32,
    pub reply_users_count: u32,
    pub latest_reply: Option<Ts>,
}

/// Fetch one message by identity.
pub fn message_by_ts(
    connection: &Connection,
    channel_id: &str,
    ts: Ts,
) -> Result<Option<MessageRow>> {
    Ok(connection
        .query_row(
            "SELECT channel_id, ts, author_user_id, bot_id, username, text, thread_ts, metadata_json,
                    EXISTS (SELECT 1 FROM message_broadcasts
                            WHERE message_broadcasts.channel_id = messages.channel_id
                              AND message_broadcasts.ts = messages.ts)
             FROM messages WHERE channel_id = ?1 AND ts = ?2",
            params![channel_id, ts.micros() as i64],
            read_message,
        )
        .optional()?)
}

impl TsWindow {
    /// SQL bounds. Exclusive comparison is the query's shape, so `inclusive`
    /// widens each bound by one microsecond rather than switching operators.
    fn bounds(self) -> (i64, i64) {
        let widen = u64::from(self.inclusive);
        let lower = self
            .oldest
            .map(|ts| ts.micros().saturating_sub(widen))
            .unwrap_or(0);
        let upper = self
            .latest
            .map(|ts| ts.micros().saturating_add(widen))
            .unwrap_or(u64::MAX >> 1);
        (lower as i64, upper as i64)
    }
}

fn read_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    let author_user_id: Option<String> = row.get(2)?;
    let bot_id: Option<String> = row.get(3)?;
    let username: Option<String> = row.get(4)?;
    let thread_ts: Option<i64> = row.get(6)?;
    let author = match (author_user_id, bot_id) {
        (Some(user_id), _) => Author::User { user_id },
        (None, Some(bot_id)) => Author::App {
            bot_id,
            username: username.unwrap_or_else(|| "app".to_string()),
        },
        (None, None) => Author::App {
            bot_id: "B000000000".to_string(),
            username: "unknown".to_string(),
        },
    };
    Ok(MessageRow {
        channel_id: row.get(0)?,
        ts: Ts::from_micros(row.get::<_, i64>(1)? as u64),
        author,
        text: row.get(5)?,
        thread_ts: thread_ts.map(|ts| Ts::from_micros(ts as u64)),
        reply_broadcast: row.get(8)?,
        metadata_json: row.get(7)?,
    })
}
