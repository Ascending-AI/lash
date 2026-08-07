//! The durable event ledger: the bot's idempotent-consumer record.
//!
//! Slack's Events API is at-least-once. The same `event_id` arrives again
//! whenever an acknowledgement is slow, lost, or the bot dies mid-handling, so a
//! bot without a durable record of what it has already done either replies twice
//! or drops work. Both failures are visible to humans in a chat channel, which
//! is why this ledger is part of the reference and not an afterthought.
//!
//! The design choice worth copying is that the ledger records a **stage**, not a
//! boolean. "Seen it" is not enough: a redelivery of an event the bot accepted
//! but never finished must *resume*, while a redelivery of an event the bot
//! finished must be dropped. One `handled` flag cannot tell those apart, and
//! guessing wrong loses a reply or duplicates one.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::store::SqliteHandle;

/// Idempotent schema, applied on every boot.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS handled_events (
    event_id      TEXT PRIMARY KEY,
    channel_id    TEXT NOT NULL,
    message_ts    TEXT NOT NULL,
    kind          TEXT NOT NULL,
    stage         TEXT NOT NULL,
    reply_ts      TEXT,
    detail        TEXT,
    deliveries    INTEGER NOT NULL DEFAULT 0,
    first_seen_at INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_handled_events_stage ON handled_events(stage);
";

/// How far the bot got with one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Recorded, nothing done yet. Not terminal: a redelivery resumes.
    Accepted,
    /// The turn is committed (or about to be) and a reply is owed. Not terminal.
    ReplyPending,
    /// Ambient context folded into the channel session. Terminal.
    Folded,
    /// Reply posted; `reply_ts` names it. Terminal.
    Replied,
    /// Deliberately not acted on. Terminal.
    Ignored,
}

impl Stage {
    /// Whether the event needs no further work.
    pub fn is_terminal(self) -> bool {
        matches!(self, Stage::Folded | Stage::Replied | Stage::Ignored)
    }

    /// Wire/storage name.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Accepted => "accepted",
            Stage::ReplyPending => "reply_pending",
            Stage::Folded => "folded",
            Stage::Replied => "replied",
            Stage::Ignored => "ignored",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "reply_pending" => Stage::ReplyPending,
            "folded" => Stage::Folded,
            "replied" => Stage::Replied,
            "ignored" => Stage::Ignored,
            // An unknown stage is treated as unfinished rather than done: the
            // recovery path is idempotent, so re-running is safe and silently
            // dropping work is not.
            _ => Stage::Accepted,
        }
    }
}

/// One ledger row.
#[derive(Clone, Debug)]
pub struct EventRecord {
    pub event_id: String,
    pub channel_id: String,
    pub message_ts: String,
    pub kind: String,
    pub stage: Stage,
    pub reply_ts: Option<String>,
    pub detail: Option<String>,
    /// How many times the platform has delivered this event. Greater than one
    /// is direct evidence the retry path ran.
    pub deliveries: u32,
}

/// The outcome of claiming an event for handling.
#[derive(Clone, Debug)]
pub enum Claim {
    /// First delivery: the caller owns the work.
    Fresh(EventRecord),
    /// Redelivered while unfinished: the caller resumes from `stage`.
    Resume(EventRecord),
    /// Redelivered after completion: the caller must do nothing.
    Settled(EventRecord),
}

impl Claim {
    /// The record, whatever the disposition.
    pub fn record(&self) -> &EventRecord {
        match self {
            Claim::Fresh(record) | Claim::Resume(record) | Claim::Settled(record) => record,
        }
    }
}

/// Durable store of handled events.
#[derive(Clone, Debug)]
pub struct EventLedger {
    database: SqliteHandle,
}

impl EventLedger {
    /// Wrap an already-open handle whose schema includes [`SCHEMA`].
    pub fn new(database: SqliteHandle) -> Self {
        Self { database }
    }

    /// Record a delivery and report whether this caller should do the work.
    ///
    /// The insert-or-bump is a single statement so two concurrent deliveries of
    /// the same event cannot both see "fresh": SQLite serializes them, and the
    /// loser observes the row the winner wrote. `deliveries` is bumped on every
    /// claim, including the first, so the count is delivery attempts and not
    /// "retries after the first".
    pub async fn claim(
        &self,
        event_id: String,
        channel_id: String,
        message_ts: String,
        kind: String,
    ) -> Result<Claim> {
        self.database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let existed = read(&transaction, &event_id)?;
                match existed {
                    None => {
                        transaction.execute(
                            "INSERT INTO handled_events
                                (event_id, channel_id, message_ts, kind, stage, deliveries,
                                 first_seen_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)",
                            params![
                                event_id,
                                channel_id,
                                message_ts,
                                kind,
                                Stage::Accepted.as_str(),
                                now_seconds(),
                            ],
                        )?;
                        let record = read(&transaction, &event_id)?
                            .ok_or_else(|| anyhow::anyhow!("ledger row vanished after insert"))?;
                        transaction.commit()?;
                        Ok(Claim::Fresh(record))
                    }
                    Some(_) => {
                        transaction.execute(
                            "UPDATE handled_events
                             SET deliveries = deliveries + 1, updated_at = ?2
                             WHERE event_id = ?1",
                            params![event_id, now_seconds()],
                        )?;
                        let record = read(&transaction, &event_id)?
                            .ok_or_else(|| anyhow::anyhow!("ledger row vanished during claim"))?;
                        transaction.commit()?;
                        if record.stage.is_terminal() {
                            Ok(Claim::Settled(record))
                        } else {
                            Ok(Claim::Resume(record))
                        }
                    }
                }
            })
            .await
    }

    /// Move an event to a new stage.
    pub async fn advance(
        &self,
        event_id: String,
        stage: Stage,
        reply_ts: Option<String>,
        detail: Option<String>,
    ) -> Result<()> {
        self.database
            .call(move |connection| {
                connection.execute(
                    "UPDATE handled_events
                     SET stage = ?2, reply_ts = COALESCE(?3, reply_ts),
                         detail = COALESCE(?4, detail), updated_at = ?5
                     WHERE event_id = ?1",
                    params![event_id, stage.as_str(), reply_ts, detail, now_seconds()],
                )?;
                Ok(())
            })
            .await
    }

    /// Read one row.
    pub async fn get(&self, event_id: String) -> Result<Option<EventRecord>> {
        self.database
            .call(move |connection| read(connection, &event_id))
            .await
    }

    /// Every event left unfinished by a previous process.
    ///
    /// Boot catch-up walks this list. Without it, an event accepted a
    /// millisecond before a crash is stuck forever: the platform's retries are
    /// bounded, and the ledger row makes every later redelivery look handled.
    pub async fn unfinished(&self) -> Result<Vec<EventRecord>> {
        self.database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT event_id, channel_id, message_ts, kind, stage, reply_ts, detail,
                            deliveries
                     FROM handled_events
                     WHERE stage IN ('accepted', 'reply_pending')
                     ORDER BY first_seen_at",
                )?;
                let rows = statement
                    .query_map([], read_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
    }
}

fn read(connection: &Connection, event_id: &str) -> Result<Option<EventRecord>> {
    Ok(connection
        .query_row(
            "SELECT event_id, channel_id, message_ts, kind, stage, reply_ts, detail, deliveries
             FROM handled_events WHERE event_id = ?1",
            params![event_id],
            read_row,
        )
        .optional()?)
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        event_id: row.get(0)?,
        channel_id: row.get(1)?,
        message_ts: row.get(2)?,
        kind: row.get(3)?,
        stage: Stage::parse(&row.get::<_, String>(4)?),
        reply_ts: row.get(5)?,
        detail: row.get(6)?,
        deliveries: row.get(7)?,
    })
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}
