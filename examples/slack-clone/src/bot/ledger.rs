//! The durable event ledger: the bot's idempotent-consumer record.
//!
//! Slack's Events API is at-least-once. The same `event_id` arrives again
//! whenever an acknowledgement is slow, lost, or the bot dies mid-handling, so a
//! bot without a durable record of what it has already done either replies twice
//! or drops work. Both failures are visible to humans in a chat channel, which
//! is why this ledger is part of the reference and not an afterthought.
//!
//! Two design choices are worth copying.
//!
//! **A stage, not a boolean.** "Seen it" is not enough: a redelivery of an event
//! the bot accepted but never finished must *resume*, while a redelivery of an
//! event the bot finished must be dropped. One `handled` flag cannot tell those
//! apart, and guessing wrong loses a reply or duplicates one.
//!
//! **Every state transition is atomic.** [`EventLedger::claim`] transactionally
//! pairs its `INSERT … ON CONFLICT … RETURNING` with the additive route record,
//! and [`EventLedger::advance`] is a single compare-and-set `UPDATE`. Neither
//! depends on the caller holding a lock to be correct, which matters because the
//! thing being guarded against is concurrency the bot does not control.

use anyhow::Result;
use lash::provider::ProviderFailureKind;
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
    -- The exact text admitted to the channel session. Recorded so a recovery
    -- pass can replay the admission byte-for-byte instead of recomposing it:
    -- Lash keys queued-input idempotence on (source key, submitted content), so
    -- a recomposition that differed by even a display name would be rejected as
    -- a source-key conflict rather than deduplicated.
    input_text    TEXT,
    reply_ts      TEXT,
    detail        TEXT,
    deliveries    INTEGER NOT NULL DEFAULT 0,
    first_seen_at INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_handled_events_stage ON handled_events(stage);

-- Routing and Lash correlation are kept in an additive companion table so an
-- existing FIG-1008 ledger upgrades without rewriting its settled rows.
CREATE TABLE IF NOT EXISTS event_routes (
    event_id     TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    thread_ts    TEXT,
    input_id     TEXT,
    fork_node_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_event_routes_thread ON event_routes(thread_ts);
CREATE INDEX IF NOT EXISTS idx_event_routes_input ON event_routes(input_id);

-- A folded top-level message has not committed into the channel graph yet, so
-- its honest fork source is the graph boundary observed while that admission
-- held the channel lock. Keep that evidence separate from `fork_node_id`, which
-- continues to mean the later boundary produced by a committed turn.
CREATE TABLE IF NOT EXISTS event_admission_boundaries (
    event_id TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    node_id  TEXT NOT NULL
);

-- Provider failures are terminal operator evidence, not free-form ledger detail.
-- Keep them in an additive companion table so existing FIG-1008 rows upgrade
-- without rewriting their settled state.
CREATE TABLE IF NOT EXISTS event_provider_failures (
    event_id   TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    code       TEXT,
    message    TEXT NOT NULL,
    retryable  INTEGER NOT NULL
);
";

/// Columns every read projects, in the order [`read_row`] expects.
const BASE_COLUMNS: &str =
    "event_id, channel_id, message_ts, kind, stage, input_text, reply_ts, detail, deliveries";
const COLUMNS: &str = "handled_events.event_id, handled_events.channel_id, handled_events.message_ts, \
     handled_events.kind, handled_events.stage, handled_events.input_text, handled_events.reply_ts, \
     handled_events.detail, handled_events.deliveries, event_routes.thread_ts, event_routes.input_id, \
     event_routes.fork_node_id, event_admission_boundaries.node_id, \
     event_provider_failures.kind, event_provider_failures.code, \
     event_provider_failures.message, event_provider_failures.retryable";
const ROUTE_JOINS: &str = "LEFT JOIN event_routes USING(event_id) \
     LEFT JOIN event_admission_boundaries USING(event_id) \
     LEFT JOIN event_provider_failures USING(event_id)";

/// Event kind for a message that mentions the bot.
pub const KIND_APP_MENTION: &str = "app_mention";
/// Event kind for ordinary channel traffic.
pub const KIND_MESSAGE: &str = "message";

/// How far the bot got with one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Recorded, nothing done yet. Not terminal: a redelivery resumes.
    Accepted,
    /// The turn is committed and a reply is owed, with its text in `detail`.
    /// Not terminal.
    ReplyPending,
    /// Ambient context folded into the channel session. Terminal.
    Folded,
    /// Reply posted; `reply_ts` names it. Terminal.
    Replied,
    /// A turn reached a terminal provider failure. Terminal, with the typed
    /// failure in [`EventRecord::provider_failure`].
    ProviderError,
    /// Deliberately not acted on. Terminal.
    Ignored,
}

impl Stage {
    /// Whether the event needs no further work.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Stage::Folded | Stage::Replied | Stage::ProviderError | Stage::Ignored
        )
    }

    /// Wire/storage name.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Accepted => "accepted",
            Stage::ReplyPending => "reply_pending",
            Stage::Folded => "folded",
            Stage::Replied => "replied",
            Stage::ProviderError => "provider_error",
            Stage::Ignored => "ignored",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "reply_pending" => Stage::ReplyPending,
            "folded" => Stage::Folded,
            "replied" => Stage::Replied,
            "provider_error" => Stage::ProviderError,
            "ignored" => Stage::Ignored,
            // An unknown stage is treated as unfinished rather than done: the
            // recovery path is idempotent, so re-running is safe and silently
            // dropping work is not.
            _ => Stage::Accepted,
        }
    }
}

/// Typed provider failure retained by the operator-facing event ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    pub kind: ProviderFailureKind,
    pub code: Option<String>,
    pub message: String,
    pub retryable: bool,
}

/// One ledger row.
#[derive(Clone, Debug)]
pub struct EventRecord {
    pub event_id: String,
    pub channel_id: String,
    pub message_ts: String,
    pub kind: String,
    pub stage: Stage,
    /// The text admitted to the channel session, when one was admitted.
    pub input_text: Option<String>,
    pub reply_ts: Option<String>,
    pub detail: Option<String>,
    /// How many times the platform has delivered this event. Greater than one
    /// is direct evidence the retry path ran.
    pub deliveries: u32,
    /// Thread parent, or `None` for top-level channel traffic.
    pub thread_ts: Option<String>,
    /// Durable Lash admission identity returned by `enqueue`.
    pub input_id: Option<String>,
    /// Retained turn boundary that includes this input, when it has committed.
    pub fork_node_id: Option<String>,
    /// Retained channel boundary captured while a folded top-level admission held
    /// the channel lock. Used only while the root is still queued.
    pub admission_node_id: Option<String>,
    /// Typed provider failure that terminalized this event, when any.
    pub provider_failure: Option<ProviderFailure>,
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
    /// One transaction pairs the `INSERT … ON CONFLICT(event_id) DO UPDATE …
    /// RETURNING` admission with its thread route. It needs no caller-held lock:
    /// two concurrent deliveries of the same event are serialized by SQLite,
    /// and the loser sees the row the winner wrote.
    ///
    /// `deliveries` is bumped on every claim including the first, so the value is
    /// delivery attempts and not "retries after the first" — and `deliveries == 1`
    /// is exactly the condition for "this caller inserted the row".
    pub async fn claim(
        &self,
        event_id: String,
        channel_id: String,
        message_ts: String,
        kind: String,
        input_text: Option<String>,
        thread_ts: Option<String>,
    ) -> Result<Claim> {
        let record = self
            .database
            .call(move |connection| {
                let now = now_seconds();
                let transaction = connection.transaction()?;
                let record = transaction.query_row(
                    &format!(
                        "INSERT INTO handled_events
                            (event_id, channel_id, message_ts, kind, stage, input_text,
                             deliveries, first_seen_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)
                         ON CONFLICT(event_id) DO UPDATE SET
                            deliveries = deliveries + 1,
                            -- Keep the first admission's text: it is what Lash
                            -- already holds under the source key.
                            input_text = COALESCE(handled_events.input_text, excluded.input_text),
                            updated_at = ?7
                         RETURNING {BASE_COLUMNS}"
                    ),
                    params![
                        event_id,
                        channel_id,
                        message_ts,
                        kind,
                        Stage::Accepted.as_str(),
                        input_text,
                        now,
                    ],
                    read_base_row,
                )?;
                transaction.execute(
                    "INSERT INTO event_routes (event_id, thread_ts)
                     VALUES (?1, ?2)
                     ON CONFLICT(event_id) DO UPDATE SET
                         thread_ts = COALESCE(event_routes.thread_ts, excluded.thread_ts)",
                    params![record.event_id, thread_ts],
                )?;
                let record = read(&transaction, &record.event_id)?.expect("claimed row exists");
                transaction.commit()?;
                Ok(record)
            })
            .await?;
        Ok(if record.deliveries <= 1 {
            Claim::Fresh(record)
        } else if record.stage.is_terminal() {
            Claim::Settled(record)
        } else {
            Claim::Resume(record)
        })
    }

    /// Record the exact Lash admission identity after an idempotent enqueue.
    pub async fn record_input_id(&self, event_id: String, input_id: String) -> Result<()> {
        self.database
            .call(move |connection| {
                connection.execute(
                    "UPDATE event_routes SET input_id = COALESCE(input_id, ?2)
                     WHERE event_id = ?1",
                    params![event_id, input_id],
                )?;
                Ok(())
            })
            .await
    }

    /// Record the exact channel boundary preceding a folded top-level admission.
    pub async fn record_admission_node(&self, event_id: String, node_id: String) -> Result<()> {
        self.database
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO event_admission_boundaries (event_id, node_id)
                     VALUES (?1, ?2)
                     ON CONFLICT(event_id) DO NOTHING",
                    params![event_id, node_id],
                )?;
                Ok(())
            })
            .await
    }

    /// Associate every admission committed by a turn with its retained boundary.
    pub async fn record_fork_node_for_inputs(
        &self,
        input_ids: Vec<String>,
        fork_node_id: String,
    ) -> Result<()> {
        self.database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                for input_id in input_ids {
                    transaction.execute(
                        "UPDATE event_routes SET fork_node_id = COALESCE(fork_node_id, ?2)
                         WHERE input_id = ?1",
                        params![input_id, fork_node_id],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    /// Top-level admissions at or before a thread root, oldest first.
    pub async fn channel_context_through(
        &self,
        channel_id: String,
        message_ts: String,
    ) -> Result<Vec<EventRecord>> {
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(&format!(
                    "SELECT {COLUMNS} FROM handled_events
                     {ROUTE_JOINS}
                     WHERE handled_events.channel_id = ?1 AND handled_events.message_ts <= ?2
                       AND event_routes.thread_ts IS NULL AND handled_events.input_text IS NOT NULL
                     ORDER BY handled_events.message_ts, handled_events.first_seen_at"
                ))?;
                Ok(statement
                    .query_map(params![channel_id, message_ts], read_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// The bot admission that corresponds to a top-level Slack message.
    pub async fn channel_message(
        &self,
        channel_id: String,
        message_ts: String,
    ) -> Result<Option<EventRecord>> {
        self.database
            .call(move |connection| {
                Ok(connection
                    .query_row(
                        &format!(
                            "SELECT {COLUMNS} FROM handled_events
                             {ROUTE_JOINS}
                             WHERE handled_events.channel_id = ?1 AND handled_events.message_ts = ?2
                               AND event_routes.thread_ts IS NULL
                               AND handled_events.input_text IS NOT NULL
                             ORDER BY CASE handled_events.kind WHEN 'app_mention' THEN 0 ELSE 1 END
                             LIMIT 1"
                        ),
                        params![channel_id, message_ts],
                        read_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Any top-level ledger row for a Slack message, including one the bot
    /// deliberately ignored and therefore never admitted.
    ///
    /// Thread routing uses this only after [`Self::channel_message`] found no
    /// admissible root. A terminal ignored row can then prove that waiting for
    /// an admission is pointless, while no row at all may still mean delivery is
    /// racing and remains worth a bounded wait.
    pub async fn top_level_event(
        &self,
        channel_id: String,
        message_ts: String,
    ) -> Result<Option<EventRecord>> {
        self.database
            .call(move |connection| {
                Ok(connection
                    .query_row(
                        &format!(
                            "SELECT {COLUMNS} FROM handled_events
                             {ROUTE_JOINS}
                             WHERE handled_events.channel_id = ?1 AND handled_events.message_ts = ?2
                               AND event_routes.thread_ts IS NULL
                             ORDER BY CASE handled_events.kind WHEN 'app_mention' THEN 0 ELSE 1 END
                             LIMIT 1"
                        ),
                        params![channel_id, message_ts],
                        read_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Move an event from `from` to `to`, if it is still at `from`.
    ///
    /// The compare-and-set is the point: without it a stale handler — a task from
    /// a previous boot, or a redelivery racing a recovery pass — could regress a
    /// `replied` row back to `reply_pending` and cause the duplicate reply this
    /// whole module exists to prevent. Returns `false` when the row had already
    /// moved on, which callers treat as "somebody else finished this".
    pub async fn advance(
        &self,
        event_id: String,
        from: Stage,
        to: Stage,
        reply_ts: Option<String>,
        detail: Option<String>,
    ) -> Result<bool> {
        self.database
            .call(move |connection| {
                let updated = connection.execute(
                    "UPDATE handled_events
                     SET stage = ?3, reply_ts = COALESCE(?4, reply_ts),
                         detail = COALESCE(?5, detail), updated_at = ?6
                     WHERE event_id = ?1 AND stage = ?2",
                    params![
                        event_id,
                        from.as_str(),
                        to.as_str(),
                        reply_ts,
                        detail,
                        now_seconds(),
                    ],
                )?;
                Ok(updated == 1)
            })
            .await
    }

    /// Terminalize an event with its typed provider failure atomically.
    pub async fn advance_provider_error(
        &self,
        event_id: String,
        from: Stage,
        failure: ProviderFailure,
    ) -> Result<bool> {
        self.database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let updated = transaction.execute(
                    "UPDATE handled_events
                     SET stage = ?3, updated_at = ?4
                     WHERE event_id = ?1 AND stage = ?2",
                    params![
                        event_id,
                        from.as_str(),
                        Stage::ProviderError.as_str(),
                        now_seconds()
                    ],
                )?;
                if updated == 1 {
                    transaction.execute(
                        "INSERT INTO event_provider_failures
                            (event_id, kind, code, message, retryable)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(event_id) DO UPDATE SET
                            kind = excluded.kind,
                            code = excluded.code,
                            message = excluded.message,
                            retryable = excluded.retryable",
                        params![
                            event_id,
                            failure.kind.code(),
                            failure.code,
                            failure.message,
                            failure.retryable as i64,
                        ],
                    )?;
                }
                transaction.commit()?;
                Ok(updated == 1)
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
                let mut statement = connection.prepare(&format!(
                    "SELECT {COLUMNS} FROM handled_events
                     {ROUTE_JOINS}
                     WHERE stage IN ('accepted', 'reply_pending')
                     ORDER BY event_routes.thread_ts IS NOT NULL,
                              first_seen_at, message_ts"
                ))?;
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
            &format!(
                "SELECT {COLUMNS} FROM handled_events
                 {ROUTE_JOINS}
                 WHERE handled_events.event_id = ?1"
            ),
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
        input_text: row.get(5)?,
        reply_ts: row.get(6)?,
        detail: row.get(7)?,
        deliveries: row.get(8)?,
        thread_ts: row.get(9)?,
        input_id: row.get(10)?,
        fork_node_id: row.get(11)?,
        admission_node_id: row.get(12)?,
        provider_failure: provider_failure_from_row(row, 13)?,
    })
}

fn read_base_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        event_id: row.get(0)?,
        channel_id: row.get(1)?,
        message_ts: row.get(2)?,
        kind: row.get(3)?,
        stage: Stage::parse(&row.get::<_, String>(4)?),
        input_text: row.get(5)?,
        reply_ts: row.get(6)?,
        detail: row.get(7)?,
        deliveries: row.get(8)?,
        thread_ts: None,
        input_id: None,
        fork_node_id: None,
        admission_node_id: None,
        provider_failure: None,
    })
}

fn provider_failure_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<ProviderFailure>> {
    let kind: Option<String> = row.get(offset)?;
    let code: Option<String> = row.get(offset + 1)?;
    let message: Option<String> = row.get(offset + 2)?;
    let retryable: Option<i64> = row.get(offset + 3)?;
    Ok(match (kind, message, retryable) {
        (Some(kind), Some(message), Some(retryable)) => Some(ProviderFailure {
            kind: parse_provider_failure_kind(&kind),
            code,
            message,
            retryable: retryable != 0,
        }),
        _ => None,
    })
}

fn parse_provider_failure_kind(raw: &str) -> ProviderFailureKind {
    match raw {
        "transport" => ProviderFailureKind::Transport,
        "timeout" => ProviderFailureKind::Timeout,
        "http" => ProviderFailureKind::Http,
        "stream" => ProviderFailureKind::Stream,
        "auth" => ProviderFailureKind::Auth,
        "validation" => ProviderFailureKind::Validation,
        "quota" => ProviderFailureKind::Quota,
        "unsupported" => ProviderFailureKind::Unsupported,
        _ => ProviderFailureKind::Unknown,
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ledger() -> (tempfile::TempDir, EventLedger) {
        let scratch = tempfile::tempdir().expect("tempdir");
        let database =
            SqliteHandle::open(&scratch.path().join("events.db"), SCHEMA).expect("open ledger");
        (scratch, EventLedger::new(database))
    }

    async fn claim(ledger: &EventLedger, event_id: &str) -> Claim {
        ledger
            .claim(
                event_id.to_string(),
                "C1".to_string(),
                "1.000001".to_string(),
                KIND_APP_MENTION.to_string(),
                Some("ada: hello".to_string()),
                None,
            )
            .await
            .expect("claim")
    }

    #[tokio::test]
    async fn the_first_claim_is_fresh_and_later_claims_only_count_deliveries() {
        let (_scratch, ledger) = ledger().await;
        assert!(matches!(claim(&ledger, "Ev1").await, Claim::Fresh(_)));
        let second = claim(&ledger, "Ev1").await;
        assert!(matches!(second, Claim::Resume(_)));
        assert_eq!(second.record().deliveries, 2);
        assert_eq!(second.record().stage, Stage::Accepted);
    }

    #[tokio::test]
    async fn a_terminal_row_claims_as_settled() {
        let (_scratch, ledger) = ledger().await;
        claim(&ledger, "Ev1").await;
        assert!(
            ledger
                .advance(
                    "Ev1".to_string(),
                    Stage::Accepted,
                    Stage::Folded,
                    None,
                    None
                )
                .await
                .expect("advance")
        );
        assert!(matches!(claim(&ledger, "Ev1").await, Claim::Settled(_)));
    }

    #[tokio::test]
    async fn advance_is_a_no_op_when_the_row_has_already_moved_on() {
        let (_scratch, ledger) = ledger().await;
        claim(&ledger, "Ev1").await;
        assert!(
            ledger
                .advance(
                    "Ev1".to_string(),
                    Stage::Accepted,
                    Stage::Replied,
                    Some("1.2".to_string()),
                    None,
                )
                .await
                .expect("advance")
        );
        // A stale handler still believing the row is `accepted` must not be able
        // to regress it — that is how a duplicate reply gets posted.
        assert!(
            !ledger
                .advance(
                    "Ev1".to_string(),
                    Stage::Accepted,
                    Stage::ReplyPending,
                    None,
                    Some("stale text".to_string()),
                )
                .await
                .expect("advance")
        );
        let record = ledger
            .get("Ev1".to_string())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(record.stage, Stage::Replied);
        assert_eq!(record.reply_ts.as_deref(), Some("1.2"));
        assert_eq!(record.detail, None, "the stale detail must not have landed");
    }

    #[tokio::test]
    async fn unfinished_lists_only_the_resumable_stages() {
        let (_scratch, ledger) = ledger().await;
        for event_id in ["Ev1", "Ev2", "Ev3"] {
            claim(&ledger, event_id).await;
        }
        ledger
            .advance(
                "Ev2".to_string(),
                Stage::Accepted,
                Stage::Replied,
                None,
                None,
            )
            .await
            .expect("advance");
        ledger
            .advance(
                "Ev3".to_string(),
                Stage::Accepted,
                Stage::ReplyPending,
                None,
                Some("owed".to_string()),
            )
            .await
            .expect("advance");
        let unfinished: Vec<String> = ledger
            .unfinished()
            .await
            .expect("unfinished")
            .into_iter()
            .map(|record| record.event_id)
            .collect();
        assert_eq!(unfinished, ["Ev1", "Ev3"]);
    }
}
