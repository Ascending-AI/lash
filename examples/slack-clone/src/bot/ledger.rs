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

// Serde supplies the core enum's wire names when it refuses an unknown tag.
// Capture that metadata so the SQL vocabulary has no second spelling table.
fn provider_kind_names() -> &'static [&'static str] {
    #[derive(Debug)]
    struct Vocabulary(&'static [&'static str]);
    impl std::fmt::Display for Vocabulary {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("provider kind vocabulary")
        }
    }
    impl std::error::Error for Vocabulary {}
    impl serde::de::Error for Vocabulary {
        fn custom<T: std::fmt::Display>(_: T) -> Self {
            Self(&[])
        }
        fn unknown_variant(_: &str, variants: &'static [&'static str]) -> Self {
            Self(variants)
        }
    }
    let names = match <ProviderFailureKind as serde::Deserialize<'_>>::deserialize(
        serde::de::value::StrDeserializer::<Vocabulary>::new(""),
    ) {
        Err(Vocabulary(names)) => names,
        Ok(_) => &[],
    };
    assert!(
        !names.is_empty(),
        "the core provider kind has a closed serde vocabulary"
    );
    names
}

/// Idempotent schema, applied on every boot.
pub static SCHEMA: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    fn names<T: Copy>(all: &[T], name: impl Fn(T) -> &'static str) -> String {
        all.iter()
            .copied()
            .map(|value| format!("'{}'", name(value)))
            .collect::<Vec<_>>()
            .join(", ")
    }
    let deferrals = names(DeferralReason::ALL, DeferralReason::as_str);
    let folds = names(FoldReason::ALL, FoldReason::as_str);
    let ignores = names(IgnoreReason::ALL, IgnoreReason::as_str);
    let stages = names(StageKind::ALL, StageKind::as_str);
    let provider_kinds = provider_kind_names()
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let checks = format!("CHECK (stage IN ({stages})), CHECK (
        (stage = 'accepted' AND reply_ts IS NULL AND (detail IS NULL OR detail IN ({deferrals}))) OR
        (stage = 'reply_pending' AND reply_ts IS NULL AND detail IS NOT NULL AND length(trim(detail)) > 0) OR
        (stage = 'folded' AND reply_ts IS NULL AND (detail IS NULL OR detail IN ({folds}))) OR
        (stage = 'replied' AND reply_ts IS NOT NULL AND length(reply_ts) > 0 AND detail IS NULL) OR
        (stage = 'provider_error' AND reply_ts IS NULL AND detail IS NULL) OR
        (stage = 'ignored' AND reply_ts IS NULL AND detail IS NOT NULL AND detail IN ({ignores}))
    ), CHECK (
        (stage = 'provider_error' AND provider_kind IS NOT NULL AND provider_kind IN ({provider_kinds}) AND provider_message IS NOT NULL
          AND provider_retryable IS NOT NULL AND provider_retryable IN (0, 1)) OR
        (stage != 'provider_error' AND provider_kind IS NULL AND provider_code IS NULL
          AND provider_message IS NULL AND provider_retryable IS NULL)
    )");
    "
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
    updated_at    INTEGER NOT NULL,
    provider_kind TEXT,
    provider_code TEXT,
    provider_message TEXT,
    provider_retryable INTEGER,
    __STAGE_CHECKS__
);
CREATE INDEX IF NOT EXISTS idx_handled_events_stage ON handled_events(stage);

-- Routing and Lash correlation are independent of event stage payloads.
CREATE TABLE IF NOT EXISTS event_routes (
    event_id     TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    thread_ts    TEXT,
    input_id     TEXT,
    fork_revision INTEGER
);
CREATE INDEX IF NOT EXISTS idx_event_routes_thread ON event_routes(thread_ts);
CREATE INDEX IF NOT EXISTS idx_event_routes_input ON event_routes(input_id);

-- A folded top-level message has not committed into the channel graph yet, so
-- its honest fork source is the channel's head revision observed while that
-- admission held the channel lock. Keep that evidence separate from
-- `fork_revision`, which continues to mean the later revision a committed turn
-- published.
CREATE TABLE IF NOT EXISTS event_admission_boundaries (
    event_id TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    revision INTEGER NOT NULL
);

-- A folded message waits here until a mention binds it into an atomic batch.
CREATE TABLE IF NOT EXISTS event_folds (
    event_id         TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    mention_event_id TEXT NOT NULL REFERENCES handled_events(event_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_event_folds_mention ON event_folds(mention_event_id);

-- The context and mention admitted together. Frozen on first use for retries.
CREATE TABLE IF NOT EXISTS mention_sends (
    event_id  TEXT PRIMARY KEY REFERENCES handled_events(event_id) ON DELETE CASCADE,
    inputs_json TEXT NOT NULL
);

"
    .replace("__STAGE_CHECKS__", &checks)
});

/// Columns every read projects, in the order [`read_row`] expects.
const BASE_COLUMNS: &str = "event_id, channel_id, message_ts, kind, stage, input_text, reply_ts, detail, deliveries, provider_kind, provider_code, provider_message, provider_retryable";
const COLUMNS: &str = "handled_events.event_id, handled_events.channel_id, handled_events.message_ts, handled_events.kind, handled_events.stage, handled_events.input_text, handled_events.reply_ts, handled_events.detail, handled_events.deliveries, handled_events.provider_kind, handled_events.provider_code, handled_events.provider_message, handled_events.provider_retryable, event_routes.thread_ts, event_routes.input_id, event_routes.fork_revision, event_admission_boundaries.revision";
const ROUTE_JOINS: &str =
    "LEFT JOIN event_routes USING(event_id) LEFT JOIN event_admission_boundaries USING(event_id)";

/// Event kind for a message that mentions the bot.
pub const KIND_APP_MENTION: &str = "app_mention";
/// Event kind for ordinary channel traffic.
pub const KIND_MESSAGE: &str = "message";

macro_rules! reasons {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            pub fn parse(raw: &str) -> Option<Self> { match raw { $($wire => Some(Self::$variant)),+, _ => None } }
        }
    };
}

reasons!(DeferralReason {
    ThreadRootNotProcessed => "thread_root_not_processed",
    ThreadRootNotAvailable => "thread_root_not_available",
});
reasons!(FoldReason { EmptyModelReply => "empty_model_reply" });
reasons!(IgnoreReason {
    AppAuthoredMessage => "app_authored_message",
    NoAuthor => "no_author",
    SupersededByAppMention => "superseded_by_app_mention",
    AdmissionTextUnavailable => "admission_text_unavailable",
    ThreadSessionRetired => "thread_session_retired",
    ReplyLostAfterCommit => "reply_lost_after_commit",
});
reasons!(StageKind {
    Accepted => "accepted",
    ReplyPending => "reply_pending",
    Folded => "folded",
    Replied => "replied",
    ProviderError => "provider_error",
    Ignored => "ignored",
});

/// A stage owns exactly the payload that recovery may use at that stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Accepted {
        deferral: Option<DeferralReason>,
    },
    ReplyPending {
        reply: Box<lash::transcript::TranscriptRowRecord>,
    },
    Folded {
        reason: Option<FoldReason>,
    },
    Replied {
        reply_ts: String,
    },
    ProviderError(ProviderFailure),
    Ignored {
        reason: IgnoreReason,
    },
}

impl Stage {
    pub fn kind(&self) -> StageKind {
        match self {
            Self::Accepted { .. } => StageKind::Accepted,
            Self::ReplyPending { .. } => StageKind::ReplyPending,
            Self::Folded { .. } => StageKind::Folded,
            Self::Replied { .. } => StageKind::Replied,
            Self::ProviderError(_) => StageKind::ProviderError,
            Self::Ignored { .. } => StageKind::Ignored,
        }
    }
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Accepted { .. } | Self::ReplyPending { .. } => false,
            Self::Folded { .. }
            | Self::Replied { .. }
            | Self::ProviderError(_)
            | Self::Ignored { .. } => true,
        }
    }
    pub fn as_str(&self) -> &'static str {
        self.kind().as_str()
    }
    pub fn detail(&self) -> Result<Option<String>> {
        Ok(match self {
            Self::Accepted { deferral } => deferral.map(|reason| reason.as_str().to_owned()),
            Self::ReplyPending { reply } => Some(serde_json::to_string(reply)?),
            Self::Folded { reason } => reason.map(|reason| reason.as_str().to_owned()),
            Self::Ignored { reason } => Some(reason.as_str().to_owned()),
            Self::Replied { .. } | Self::ProviderError(_) => None,
        })
    }
    pub fn reply_ts(&self) -> Option<&str> {
        match self {
            Self::Replied { reply_ts } => Some(reply_ts),
            _ => None,
        }
    }
    pub fn provider_failure(&self) -> Option<&ProviderFailure> {
        match self {
            Self::ProviderError(failure) => Some(failure),
            _ => None,
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
    /// How many times the platform has delivered this event. Greater than one
    /// is direct evidence the retry path ran.
    pub deliveries: u32,
    /// Thread parent, or `None` for top-level channel traffic.
    pub thread_ts: Option<String>,
    /// Durable Lash admission identity of the send that carried this event:
    /// a mention's own, or the mention an ambient message was folded into.
    pub input_id: Option<String>,
    /// Pinned head revision of the turn that committed this input, once it
    /// has: the state a thread rooted here forks.
    pub fork_revision: Option<u64>,
    /// Pinned channel head revision captured while a folded top-level message
    /// held the channel lock. Used while no committed turn carries the
    /// message yet.
    pub admission_revision: Option<u64>,
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

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MentionInputs {
    pub context: String,
    pub mention: String,
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
    #[expect(
        clippy::expect_used,
        reason = "read re-fetches the row this same transaction just inserted, so it is Some"
    )]
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
                        StageKind::Accepted.as_str(),
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

    /// Bind the route's waiting ambient rows and freeze its context and mention.
    /// The context starts with `inherited` on the first mention in a thread,
    /// followed by the bound ambient lines in order.
    ///
    /// The first call stores both members and every later call returns
    /// them unchanged, so a retry sends the same bytes: an ambient message that
    /// arrives after the binding waits for the route's next mention.
    pub async fn bind_mention_send(
        &self,
        mention_event_id: String,
        mention_text: String,
        inherited: String,
    ) -> Result<MentionInputs> {
        self.database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                if let Some(inputs) = transaction
                    .query_row(
                        "SELECT inputs_json FROM mention_sends WHERE event_id = ?1",
                        params![mention_event_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    return Ok(serde_json::from_str(&inputs)?);
                }
                let (channel_id, message_ts, thread_ts): (String, String, Option<String>) =
                    transaction.query_row(
                        "SELECT handled_events.channel_id, handled_events.message_ts,
                                event_routes.thread_ts
                         FROM handled_events LEFT JOIN event_routes USING(event_id)
                         WHERE handled_events.event_id = ?1",
                        params![mention_event_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                let first_on_route: bool = transaction.query_row(
                    "SELECT NOT EXISTS (
                         SELECT 1 FROM mention_sends
                         JOIN handled_events USING(event_id)
                         LEFT JOIN event_routes USING(event_id)
                         WHERE handled_events.channel_id = ?1
                           AND event_routes.thread_ts IS ?2)",
                    params![channel_id, thread_ts],
                    |row| row.get(0),
                )?;
                let ambient = {
                    let mut statement = transaction.prepare(
                        "SELECT handled_events.event_id, handled_events.input_text
                         FROM handled_events LEFT JOIN event_routes USING(event_id)
                         WHERE handled_events.channel_id = ?1
                           AND event_routes.thread_ts IS ?2
                           AND handled_events.kind = ?3
                           AND handled_events.stage = ?4
                           AND handled_events.input_text IS NOT NULL
                           AND handled_events.message_ts < ?5
                           AND handled_events.event_id NOT IN (SELECT event_id FROM event_folds)
                         ORDER BY handled_events.message_ts, handled_events.first_seen_at",
                    )?;
                    statement
                        .query_map(
                            params![
                                channel_id,
                                thread_ts,
                                KIND_MESSAGE,
                                StageKind::Folded.as_str(),
                                message_ts
                            ],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                let mut text = String::new();
                if first_on_route {
                    text.push_str(&inherited);
                }
                for (event_id, line) in &ambient {
                    transaction.execute(
                        "INSERT INTO event_folds (event_id, mention_event_id) VALUES (?1, ?2)",
                        params![event_id, mention_event_id],
                    )?;
                    text.push_str(line);
                    text.push('\n');
                }
                let inputs = MentionInputs {
                    context: text,
                    mention: mention_text,
                };
                transaction.execute(
                    "INSERT INTO mention_sends (event_id, inputs_json) VALUES (?1, ?2)",
                    params![mention_event_id, serde_json::to_string(&inputs)?],
                )?;
                transaction.commit()?;
                Ok(inputs)
            })
            .await
    }

    /// The route's ambient text still waiting for a mention, oldest first.
    pub async fn unfolded_context(
        &self,
        channel_id: String,
        thread_ts: Option<String>,
    ) -> Result<Vec<String>> {
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT handled_events.input_text
                     FROM handled_events LEFT JOIN event_routes USING(event_id)
                     WHERE handled_events.channel_id = ?1
                       AND event_routes.thread_ts IS ?2
                       AND handled_events.kind = ?3
                       AND handled_events.stage = ?4
                       AND handled_events.input_text IS NOT NULL
                       AND handled_events.event_id NOT IN (SELECT event_id FROM event_folds)
                     ORDER BY handled_events.message_ts, handled_events.first_seen_at",
                )?;
                Ok(statement
                    .query_map(
                        params![
                            channel_id,
                            thread_ts,
                            KIND_MESSAGE,
                            StageKind::Folded.as_str()
                        ],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Record each batch member's identity on the Slack events it carries.
    pub async fn record_mention_inputs(
        &self,
        mention_event_id: String,
        mention_input_id: lash::InputId,
        context_input_id: Option<lash::InputId>,
    ) -> Result<()> {
        self.database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                transaction.execute(
                    "UPDATE event_routes SET input_id = COALESCE(input_id, ?2)
                     WHERE event_id = ?1",
                    params![mention_event_id, mention_input_id.as_str()],
                )?;
                if let Some(input_id) = context_input_id {
                    transaction.execute(
                        "UPDATE event_routes SET input_id = COALESCE(input_id, ?2)
                         WHERE event_id IN (SELECT event_id FROM event_folds
                                            WHERE mention_event_id = ?1)",
                        params![mention_event_id, input_id.as_str()],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn record_admission_revision(&self, event_id: String, revision: u64) -> Result<()> {
        self.database
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO event_admission_boundaries (event_id, revision)
                     VALUES (?1, ?2)
                     ON CONFLICT(event_id) DO NOTHING",
                    params![event_id, revision],
                )?;
                Ok(())
            })
            .await
    }

    /// Associate every admission committed by a turn with the pinned revision
    /// that turn published.
    pub async fn record_fork_revision_for_inputs(
        &self,
        input_ids: Vec<String>,
        fork_revision: u64,
    ) -> Result<()> {
        self.database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                for input_id in input_ids {
                    transaction.execute(
                        "UPDATE event_routes SET fork_revision = COALESCE(fork_revision, ?2)
                         WHERE input_id = ?1",
                        params![input_id, fork_revision],
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
    pub async fn advance(&self, event_id: String, from: StageKind, to: Stage) -> Result<bool> {
        self.database.call(move |connection| {
            let failure = to.provider_failure();
            let updated = connection.execute(
                "UPDATE handled_events SET stage = ?3, reply_ts = ?4, detail = ?5,
                 provider_kind = ?6, provider_code = ?7, provider_message = ?8, provider_retryable = ?9,
                 updated_at = ?10 WHERE event_id = ?1 AND stage = ?2",
                params![event_id, from.as_str(), to.as_str(), to.reply_ts(), to.detail()?,
                    failure.map(|failure| failure.kind.code()), failure.and_then(|failure| failure.code.as_deref()),
                    failure.map(|failure| failure.message.as_str()), failure.map(|failure| i64::from(failure.retryable)), now_seconds()],
            )?;
            Ok(updated == 1)
        }).await
    }

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

fn decode_stage(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stage> {
    fn corrupt(message: impl Into<String>) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message.into(),
            )),
        )
    }
    let raw: String = row.get(4)?;
    let kind =
        StageKind::parse(&raw).ok_or_else(|| corrupt(format!("unknown ledger stage {raw}")))?;
    let reply_ts: Option<String> = row.get(6)?;
    let detail: Option<String> = row.get(7)?;
    let provider_kind: Option<String> = row.get(9)?;
    let provider_code: Option<String> = row.get(10)?;
    let provider_message: Option<String> = row.get(11)?;
    let provider_retryable: Option<i64> = row.get(12)?;
    let has_failure = provider_kind.is_some()
        || provider_code.is_some()
        || provider_message.is_some()
        || provider_retryable.is_some();
    if kind != StageKind::ProviderError && has_failure {
        return Err(corrupt("provider data on another stage"));
    }
    if kind != StageKind::Replied && reply_ts.is_some() {
        return Err(corrupt("reply timestamp on another stage"));
    }
    let stage = match kind {
        StageKind::Accepted => Stage::Accepted {
            deferral: detail
                .as_deref()
                .map(|raw| {
                    DeferralReason::parse(raw).ok_or_else(|| corrupt("unknown deferral reason"))
                })
                .transpose()?,
        },
        StageKind::ReplyPending => Stage::ReplyPending {
            reply: serde_json::from_str::<Box<lash::transcript::TranscriptRowRecord>>(
                &detail.ok_or_else(|| corrupt("missing reply debt"))?,
            )
            .map_err(|error| corrupt(format!("invalid committed reply debt: {error}")))?,
        },
        StageKind::Folded => Stage::Folded {
            reason: detail
                .as_deref()
                .map(|raw| FoldReason::parse(raw).ok_or_else(|| corrupt("unknown fold reason")))
                .transpose()?,
        },
        StageKind::Replied => {
            if detail.is_some() {
                return Err(corrupt("stale reply debt"));
            }
            Stage::Replied {
                reply_ts: reply_ts
                    .filter(|ts| !ts.is_empty())
                    .ok_or_else(|| corrupt("missing reply timestamp"))?,
            }
        }
        StageKind::ProviderError => {
            if detail.is_some() {
                return Err(corrupt("detail on provider failure"));
            }
            let kind = provider_kind.ok_or_else(|| corrupt("missing provider kind"))?;
            let kind =
                serde_json::from_value(serde_json::Value::String(kind)).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        9,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            let retryable = match provider_retryable {
                Some(0) => false,
                Some(1) => true,
                _ => return Err(corrupt("missing or invalid provider retryability")),
            };
            Stage::ProviderError(ProviderFailure {
                kind,
                code: provider_code,
                message: provider_message.ok_or_else(|| corrupt("missing provider message"))?,
                retryable,
            })
        }
        StageKind::Ignored => Stage::Ignored {
            reason: detail
                .as_deref()
                .and_then(IgnoreReason::parse)
                .ok_or_else(|| corrupt("missing or unknown ignore reason"))?,
        },
    };
    Ok(stage)
}

fn read_base_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        event_id: row.get(0)?,
        channel_id: row.get(1)?,
        message_ts: row.get(2)?,
        kind: row.get(3)?,
        stage: decode_stage(row)?,
        input_text: row.get(5)?,
        deliveries: row.get(8)?,
        thread_ts: None,
        input_id: None,
        fork_revision: None,
        admission_revision: None,
    })
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let mut record = read_base_row(row)?;
    record.thread_ts = row.get(13)?;
    record.input_id = row.get(14)?;
    record.fork_revision = row.get(15)?;
    record.admission_revision = row.get(16)?;
    Ok(record)
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn reply_fixture(text: &str) -> lash::transcript::TranscriptRowRecord {
    serde_json::from_value(serde_json::json!({
        "row_id":"debt-fixture", "kind":"assistant_reply", "timestamp":"2026-10-03T00:00:00Z", "suppressed":null,
        "provenance":{"turn_id":"fixture-turn", "input_id":null, "plugin_id":null, "is_turn_reply":true},
        "content":{"text":text, "reasoning":[], "attachments":[], "language":null, "code":null, "output":null, "success":null, "error":null, "tools":[], "tools_omitted":0}
    })).expect("reply debt fixture")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_provider_failure_kinds_are_rejected() {
        let connection = Connection::open_in_memory().expect("database");
        connection.execute_batch(&SCHEMA).expect("schema");
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .expect("inject corruption");
        connection.execute("INSERT INTO handled_events (event_id, channel_id, message_ts, kind, stage, provider_kind, provider_message, provider_retryable, first_seen_at, updated_at) VALUES ('bad', 'C1', '1', 'message', 'provider_error', 'quoat', 'refusal', 0, 0, 0)", []).expect("corrupt failure");
        assert!(
            read(&connection, "bad").is_err(),
            "unknown failure kind became a valid provider failure"
        );
    }

    #[test]
    fn unknown_stages_are_rejected_instead_of_resumed() {
        let connection = Connection::open_in_memory().expect("database");
        connection.execute_batch(&SCHEMA).expect("schema");
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .expect("inject corruption");
        connection.execute("INSERT INTO handled_events (event_id, channel_id, message_ts, kind, stage, first_seen_at, updated_at) VALUES ('bad', 'C1', '1', 'message', 'acceptde', 0, 0)", []).expect("corrupt row");
        assert!(
            read(&connection, "bad").is_err(),
            "unknown stages must fail decoding"
        );
    }

    #[test]
    fn sql_rejects_stage_payloads_that_belong_to_another_stage() {
        let connection = Connection::open_in_memory().expect("database");
        connection.execute_batch(&SCHEMA).expect("schema");
        for (stage, reply_ts, detail) in [
            ("acceptde", None, None),
            ("accepted", Some("1.2"), None),
            ("accepted", None, Some("owed reply")),
            ("reply_pending", None, None),
            ("reply_pending", Some("1.2"), Some("owed reply")),
            ("folded", None, Some("thread_root_not_processed")),
            ("replied", None, None),
            ("replied", Some("1.2"), Some("owed reply")),
            ("ignored", None, None),
            ("provider_error", None, None),
        ] {
            assert!(connection.execute("INSERT INTO handled_events (event_id, channel_id, message_ts, kind, stage, reply_ts, detail, first_seen_at, updated_at) VALUES (?1, 'C1', '1', 'message', ?1, ?2, ?3, 0, 0)", params![stage, reply_ts, detail]).is_err(), "invalid {stage} payload was accepted");
        }
        for (stage, provider_kind, provider_message, provider_retryable) in [
            ("accepted", Some("quota"), Some("refusal"), Some(0)),
            ("provider_error", None, Some("refusal"), Some(0)),
            ("provider_error", Some("quota"), None, Some(0)),
            ("provider_error", Some("quota"), Some("refusal"), None),
            ("provider_error", Some("quota"), Some("refusal"), Some(2)),
            ("provider_error", Some("quoota"), Some("refusal"), Some(0)),
        ] {
            assert!(connection.execute("INSERT INTO handled_events (event_id, channel_id, message_ts, kind, stage, provider_kind, provider_message, provider_retryable, first_seen_at, updated_at) VALUES ('provider', 'C1', '1', 'message', ?1, ?2, ?3, ?4, 0, 0)", params![stage, provider_kind, provider_message, provider_retryable]).is_err(), "invalid provider payload on {stage}");
        }
        for kind in provider_kind_names() {
            assert!(connection.execute("INSERT INTO handled_events (event_id, channel_id, message_ts, kind, stage, provider_kind, provider_message, provider_retryable, first_seen_at, updated_at) VALUES (?1, 'C1', '1', 'message', 'provider_error', ?1, 'refusal', 0, 0, 0)", [kind]).is_ok(), "valid core provider kind {kind} was refused");
        }
    }

    #[tokio::test]
    async fn folding_a_deferred_event_removes_its_deferral() {
        let (_scratch, ledger) = ledger().await;
        claim(&ledger, "deferred").await;
        ledger
            .advance(
                "deferred".into(),
                StageKind::Accepted,
                Stage::Accepted {
                    deferral: Some(DeferralReason::ThreadRootNotProcessed),
                },
            )
            .await
            .expect("defer");
        ledger
            .advance(
                "deferred".into(),
                StageKind::Accepted,
                Stage::Folded { reason: None },
            )
            .await
            .expect("fold");
        let row = ledger
            .get("deferred".into())
            .await
            .expect("get")
            .expect("row");
        assert!(
            row.stage
                .detail()
                .expect("serialize stage detail")
                .is_none(),
            "folded event retained a stale deferral"
        );
    }

    async fn ledger() -> (tempfile::TempDir, EventLedger) {
        let scratch = tempfile::tempdir().expect("tempdir");
        let database =
            SqliteHandle::open(&scratch.path().join("events.db"), &SCHEMA).expect("open ledger");
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
        assert_eq!(second.record().stage.kind(), StageKind::Accepted);
    }

    #[tokio::test]
    async fn a_terminal_row_claims_as_settled() {
        let (_scratch, ledger) = ledger().await;
        claim(&ledger, "Ev1").await;
        assert!(
            ledger
                .advance(
                    "Ev1".to_string(),
                    StageKind::Accepted,
                    Stage::Folded { reason: None }
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
                    StageKind::Accepted,
                    Stage::Replied {
                        reply_ts: "1.2".to_string()
                    }
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
                    StageKind::Accepted,
                    Stage::ReplyPending {
                        reply: Box::new(reply_fixture("stale text"))
                    }
                )
                .await
                .expect("advance")
        );
        let record = ledger
            .get("Ev1".to_string())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(record.stage.kind(), StageKind::Replied);
        assert_eq!(record.stage.reply_ts(), Some("1.2"));
        assert_eq!(
            record.stage.detail().expect("serialize stage detail"),
            None,
            "the stale detail must not have landed"
        );
    }

    #[tokio::test]
    async fn advance_can_clear_existing_detail() {
        let (_scratch, ledger) = ledger().await;
        claim(&ledger, "Ev1").await;
        ledger
            .advance(
                "Ev1".to_string(),
                StageKind::Accepted,
                Stage::ReplyPending {
                    reply: Box::new(reply_fixture("owed reply")),
                },
            )
            .await
            .expect("record reply debt");
        ledger
            .advance(
                "Ev1".to_string(),
                StageKind::ReplyPending,
                Stage::Replied {
                    reply_ts: "1.2".to_string(),
                },
            )
            .await
            .expect("clear reply debt");
        let record = ledger
            .get("Ev1".to_string())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            record.stage.detail().expect("serialize stage detail"),
            None,
            "settled reply debt must be cleared"
        );
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
                StageKind::Accepted,
                Stage::Replied {
                    reply_ts: "1.2".to_owned(),
                },
            )
            .await
            .expect("advance");
        ledger
            .advance(
                "Ev3".to_string(),
                StageKind::Accepted,
                Stage::ReplyPending {
                    reply: Box::new(reply_fixture("owed")),
                },
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
