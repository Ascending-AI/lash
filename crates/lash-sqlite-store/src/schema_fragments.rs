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

/// The one session ingress (ADR 0101), carried by the durable core alone.
///
/// One row per admitted item, one per-session order under the database write
/// lock, two class-level lanes. The `delivery_*` columns hold the submitted
/// delivery, written once and never rewritten, and `submission_digest`
/// likewise. A claim's columns are set exactly on an `accepted` row, and a
/// tombstone carries its closed cause and no claim. The partial indexes keep
/// tombstones off the claim path.
pub(crate) const SESSION_INGRESS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS session_ingress (
    enqueue_seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id           TEXT NOT NULL UNIQUE,
    session_id        TEXT NOT NULL,
    lane              TEXT NOT NULL,
    kind              TEXT NOT NULL,
    source_key        TEXT,
    delivery_scope    TEXT NOT NULL,
    delivery_turn_id  TEXT,
    delivery_min_boundary TEXT,
    submission_digest TEXT NOT NULL,
    payload_json      TEXT NOT NULL,
    authority_json    TEXT,
    merge_key         TEXT,
    wake_process_id   TEXT,
    wake_sequence     INTEGER,
    state             TEXT NOT NULL,
    terminal_cause_json TEXT,
    enqueued_at_ms    INTEGER NOT NULL,
    terminal_at_ms    INTEGER,
    claim_id          TEXT,
    claim_token       TEXT,
    claim_admission_id TEXT,
    claim_fencing_token INTEGER NOT NULL DEFAULT 0,
    claim_drive_epoch INTEGER,
    claim_turn_id     TEXT,
    CONSTRAINT ck_session_ingress_kind CHECK (kind IN ('input', 'process_wake', 'session_command')),
    CONSTRAINT ck_session_ingress_lane CHECK ((kind = 'session_command' AND lane = 'command') OR (kind IN ('input', 'process_wake') AND lane = 'turn')),
    CONSTRAINT ck_session_ingress_state CHECK (state IN ('open', 'accepted', 'completed', 'cancelled')),
    CONSTRAINT ck_session_ingress_delivery CHECK ((delivery_scope = 'turn' AND delivery_turn_id IS NOT NULL AND delivery_min_boundary IN ('after_work', 'before_completion')) OR (delivery_scope IN ('any_boundary', 'next_turn') AND delivery_turn_id IS NULL AND delivery_min_boundary IS NULL)),
    CONSTRAINT ck_session_ingress_kind_delivery CHECK (kind = 'input' OR (kind = 'process_wake' AND delivery_scope = 'any_boundary') OR (kind = 'session_command' AND delivery_scope = 'next_turn')),
    CONSTRAINT ck_session_ingress_wake_source CHECK ((kind = 'process_wake' AND wake_process_id IS NOT NULL AND wake_sequence IS NOT NULL) OR (kind <> 'process_wake' AND wake_process_id IS NULL AND wake_sequence IS NULL)),
    CONSTRAINT ck_session_ingress_claim CHECK ((state = 'accepted' AND claim_id IS NOT NULL AND claim_token IS NOT NULL AND claim_admission_id IS NOT NULL AND claim_drive_epoch IS NOT NULL) OR (state <> 'accepted' AND claim_id IS NULL AND claim_token IS NULL AND claim_admission_id IS NULL AND claim_drive_epoch IS NULL AND claim_turn_id IS NULL)),
    CONSTRAINT ck_session_ingress_terminal CHECK ((state IN ('completed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL) OR (state IN ('open', 'accepted') AND terminal_cause_json IS NULL AND terminal_at_ms IS NULL)),
    UNIQUE (session_id, source_key)
);

CREATE INDEX IF NOT EXISTS idx_session_ingress_open
    ON session_ingress(session_id, lane, enqueue_seq) WHERE state IN ('open', 'accepted');

CREATE INDEX IF NOT EXISTS idx_session_ingress_addressed
    ON session_ingress(session_id, delivery_turn_id, enqueue_seq)
    WHERE delivery_turn_id IS NOT NULL AND state IN ('open', 'accepted');

CREATE INDEX IF NOT EXISTS idx_session_ingress_claim
    ON session_ingress(session_id, claim_id) WHERE claim_id IS NOT NULL;
";

/// The logical-root family (FIG-3600 S7), carried by the durable core alone.
///
/// `session_roots` holds one row per `(session, root)` a drive admitted work
/// under, with the root's terminal evidence once it has one: all four
/// `terminal_*` columns are set together, exactly once. `session_root_inputs`
/// binds each accepted input to the root that drives it. `control_intents`
/// records an operator's verb or a session's close; a `close_session` row
/// outlives its session as the deletion tombstone.
pub(crate) const SESSION_ROOTS_TABLES: &str = "
CREATE TABLE IF NOT EXISTS session_roots (
    session_id              TEXT NOT NULL,
    root                    TEXT NOT NULL,
    terminal_kind           TEXT,
    terminal_cause_json     TEXT,
    terminal_head_revision  INTEGER,
    terminal_at_ms          INTEGER,
    PRIMARY KEY (session_id, root),
    CONSTRAINT ck_session_roots_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL))
);

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
    attempts       INTEGER NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    engine_ref     TEXT
);

CREATE INDEX IF NOT EXISTS idx_control_intents_open
    ON control_intents(intent_id) WHERE state IN ('pending', 'failed_retryable');

CREATE INDEX IF NOT EXISTS idx_control_intents_session
    ON control_intents(session_id, kind);
";
