//! DDL fragments a SQLite database definition applies after its own schema:
//! table sets shared verbatim between independently versioned databases, and
//! the durable core's session ingress.
//!
//! A table declared here lives under every version counter of the databases
//! whose [`crate::schema::SqliteDatabase`] definition lists it, so a shape
//! edit is a single edit that reaches every carrier. Two copy-pasted copies
//! under two counters could diverge silently — an edit bumped under one
//! counter would leave the other database's stale table in place, and the
//! shared `await_event.rs` / `scope_fence.rs` SQL would then run against a
//! column set it no longer matches (S14-A1, FIG-3260).

/// The durable keyed promises of the effect-replay database, beside the
/// effects that resolve them. The effect-replay database is their only
/// carrier: FIG-3585 deleted the durable-core copy that store-delegated turn
/// control used.
pub(crate) const AWAIT_EVENT_TABLES: &str = "
CREATE TABLE IF NOT EXISTS await_event_meta (
    singleton       INTEGER PRIMARY KEY CONSTRAINT ck_await_event_meta_singleton CHECK (singleton = 1),
    signing_secret  BLOB NOT NULL
);

INSERT INTO await_event_meta (singleton, signing_secret)
VALUES (1, randomblob(32))
ON CONFLICT(singleton) DO NOTHING;

CREATE TABLE IF NOT EXISTS await_event_waits (
    key_id          TEXT PRIMARY KEY,
    scope_json      TEXT NOT NULL,
    wait_json       TEXT NOT NULL,
    session_id      TEXT,
    turn_control    INTEGER NOT NULL CONSTRAINT ck_await_event_waits_turn_control CHECK (turn_control IN (0, 1)),
    terminal_json   TEXT,
    created_at_ms   INTEGER NOT NULL,
    resolved_at_ms  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_await_event_waits_session
    ON await_event_waits(session_id);

-- Permanent by design: session ids cannot be reused, so revocation evidence
-- must remain after every retention-pruning pass.
CREATE TABLE IF NOT EXISTS await_event_revoked_sessions (
    session_id      TEXT PRIMARY KEY,
    revoked_at_ms   INTEGER NOT NULL
);
";

/// Retirement fence for effect scopes, shared by the process-registry and
/// effect-replay databases.
///
/// Permanent by design: process and runtime-operation ids are single-use, so a
/// retired scope's fence must outlive every retention pass and every restart.
/// Keyed by the scope's journal identity — the same key its effect rows carry.
/// In the registry file a registration deletes the fence and inserts the row
/// in one single-file commit; in the journal a retirement's fence insert is
/// its one commit point (FIG-2499, ADR 0049).
pub(crate) const SCOPE_RETIREMENT_TABLE: &str = "
CREATE TABLE IF NOT EXISTS effect_scope_retirements (
    scope_id        TEXT PRIMARY KEY,
    retired_at_ms   INTEGER NOT NULL,
    artifact_cleanup_completed INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_effect_scope_retirements_artifact_cleanup_completed CHECK (artifact_cleanup_completed IN (0, 1))
);
";

/// The session ingress's one per-session order (ADR 0101 §5, amended),
/// carried by the durable core alone.
///
/// Both admission tables, `pending_turn_inputs` and `queued_work_batches`,
/// draw their `enqueue_seq` from this counter under the database write lock,
/// so enqueue order across both is per-session commit order.
pub(crate) const SESSION_INGRESS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS session_ingress_sequence (
    session_id TEXT NOT NULL PRIMARY KEY,
    enqueue_seq INTEGER NOT NULL,
    CONSTRAINT ck_session_ingress_sequence_positive CHECK (enqueue_seq > 0)
);
";

/// The logical-root family (FIG-3600 S7), carried by the durable core alone.
///
/// `session_roots` holds one row per `(session, root)` a drive admitted work
/// under, with the exact result of an input root's claim, committed in the
/// claim's own transaction (FIG-3840), and the root's terminal evidence once
/// it has one: all four `terminal_*` columns are set together, exactly once. `session_root_inputs`
/// binds each accepted input to the root that drives it. `control_intents`
/// records an operator's verb or a session's close; a `close_session` row
/// outlives its session as the deletion tombstone.
pub(crate) const SESSION_ROOTS_TABLES: &str = "
CREATE TABLE IF NOT EXISTS session_roots (
    session_id              TEXT NOT NULL,
    root                    TEXT NOT NULL,
    claim_result_json       TEXT,
    terminal_kind           TEXT,
    terminal_cause_json     TEXT,
    terminal_head_revision  INTEGER,
    terminal_at_ms          INTEGER,
    obligation_id           TEXT,
    obligation_state        TEXT,
    obligation_attempts     INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms    INTEGER,
    obligation_claim_token  TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error   TEXT,
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_session_roots_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    PRIMARY KEY (session_id, root),
    CONSTRAINT ck_session_roots_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_session_roots_obligation_id
    ON session_roots(obligation_id);
CREATE INDEX IF NOT EXISTS idx_session_roots_obligation_due
    ON session_roots(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_session_roots_obligation_stalled
    ON session_roots(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE TABLE IF NOT EXISTS session_root_inputs (
    session_id  TEXT NOT NULL,
    input_id    TEXT NOT NULL,
    root        TEXT NOT NULL,
    PRIMARY KEY (session_id, input_id)
);

CREATE TABLE IF NOT EXISTS control_intents (
    intent_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id     TEXT NOT NULL,
    format         INTEGER NOT NULL,
    kind           TEXT NOT NULL CONSTRAINT ck_control_intents_kind CHECK (kind IN ('redrive', 'cancel', 'fork', 'close_session')),
    kind_json      TEXT NOT NULL,
    state          TEXT NOT NULL CONSTRAINT ck_control_intents_state CHECK (state IN ('pending', 'acknowledged', 'superseded', 'failed_retryable', 'failed')),
    state_json     TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    engine_ref     TEXT,
    obligation_id  TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_control_intents_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_control_intents_obligation_id
    ON control_intents(obligation_id);
CREATE INDEX IF NOT EXISTS idx_control_intents_obligation_due
    ON control_intents(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_control_intents_obligation_stalled
    ON control_intents(obligation_id)
    WHERE obligation_state = 'stalled';

CREATE INDEX IF NOT EXISTS idx_control_intents_session
    ON control_intents(session_id, kind);
";
