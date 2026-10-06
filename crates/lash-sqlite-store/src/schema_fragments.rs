//! DDL fragments provisioning applies after the schema bodies
//! ([`crate::schema::FRAGMENTS`]): the session ingress and the logical runs.

/// The session ingress's one per-session order (ADR 0101 §5, amended).
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

/// The logical-run family (FIG-3600 S7), carried by the durable core alone.
///
/// `session_runs` holds one row per `(session, run)` a shift admitted work
/// under, with the executor the seal of its admission recorded
/// (`executor_json`, written in the seal's transaction, FIG-4814), the exact
/// result of the run's admission (`admission_json`),
/// committed in the admission's own transaction (FIG-3840, FIG-3927), and the run's terminal evidence once
/// it has one: all four `terminal_*` columns are set together, exactly once. `session_run_inputs`
/// binds each accepted input to the run that executes it. `control_intents`
/// records an operator's verb or a session's close; a `close_session` row
/// outlives its session as the deletion tombstone.
pub(crate) const SESSION_RUNS_TABLES: &str = "
CREATE TABLE IF NOT EXISTS session_shift_admissions (
    session_id TEXT NOT NULL,
    admission TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    PRIMARY KEY (session_id, admission)
);

CREATE TABLE IF NOT EXISTS session_runs (
    session_id              TEXT NOT NULL,
    run                    TEXT NOT NULL,
    executor_json           TEXT,
    admission_json          TEXT,
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
    obligation_last_error_code TEXT CONSTRAINT ck_session_runs_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_session_runs_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_session_runs_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL))
);

-- The obligation columns' indexes (ADR 0109 §1.1): the id, the due read
-- and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_session_runs_obligation_id
    ON session_runs(obligation_id);
CREATE UNIQUE INDEX IF NOT EXISTS ux_session_runs_unfinished
    ON session_runs(session_id)
    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL;
CREATE INDEX IF NOT EXISTS idx_session_runs_obligation_due
    ON session_runs(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_session_runs_obligation_stalled
    ON session_runs(obligation_id)
    WHERE obligation_state = 'stalled';
CREATE INDEX IF NOT EXISTS idx_session_runs_open
    ON session_runs(session_id, run)
    WHERE terminal_kind IS NULL;

CREATE TABLE IF NOT EXISTS session_run_inputs (
    session_id  TEXT NOT NULL,
    input_id    TEXT NOT NULL,
    run        TEXT NOT NULL,
    PRIMARY KEY (session_id, input_id)
);

CREATE TABLE IF NOT EXISTS control_intents (
    intent_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id     TEXT NOT NULL,
    format         INTEGER NOT NULL,
    kind           TEXT NOT NULL CONSTRAINT ck_control_intents_kind CHECK (kind IN ('redrive', 'cancel', 'fork', 'close_session')),
    kind_json      TEXT NOT NULL,
    state          TEXT NOT NULL CONSTRAINT ck_control_intents_state CHECK (state IN ('pending', 'acknowledged', 'superseded', 'refused')),
    state_json     TEXT NOT NULL,
    engine_half_owed INTEGER NOT NULL GENERATED ALWAYS AS ((state = 'pending' AND obligation_state IN ('due', 'claimed')) IS TRUE) VIRTUAL,
    created_at_ms  INTEGER NOT NULL,
    engine_ref     TEXT,
    obligation_id  TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_last_error_code TEXT CONSTRAINT ck_control_intents_obligation_error_code CHECK ((obligation_last_error IS NULL) = (obligation_last_error_code IS NULL)),
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_control_intents_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE)
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
