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
/// `session_runs` holds one row per `(session, run)` admitted work ran
/// under, with the exact result of the run's admission (`admission_json`),
/// committed in the admission's own transaction (FIG-3840, FIG-3927), and the run's terminal evidence once
/// it has one: all four `terminal_*` columns are set together, exactly once, and
/// `terminal_kind` is the kind its cause derives (`RunTerminalCause::kind`),
/// kept beside it only for the indexes and checked against it. `session_run_inputs`
/// binds each accepted input to the run that executes it. `control_intents`
/// records a session's close; a `close_session` row outlives its session as
/// the deletion tombstone.
pub(crate) const SESSION_RUNS_TABLES: &str = "
CREATE TABLE IF NOT EXISTS session_runs (
    session_id              TEXT NOT NULL,
    run                    TEXT NOT NULL,
    admission_json          TEXT,
    terminal_kind           TEXT,
    terminal_cause_json     TEXT,
    terminal_head_revision  INTEGER,
    terminal_at_ms          INTEGER,
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_session_runs_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL)),
    CONSTRAINT ck_session_runs_terminal_kind CHECK (terminal_kind IS NULL OR terminal_kind = CASE json_extract(terminal_cause_json, '$.cause')
        WHEN 'committed' THEN CASE
            WHEN json_type(terminal_cause_json, '$.outcome.finished') IS NOT NULL THEN 'answered'
            WHEN json_type(terminal_cause_json, '$.outcome.agent_frame_switch') IS NOT NULL THEN 'answered'
            WHEN json_type(terminal_cause_json, '$.outcome.stopped.cancelled') IS NOT NULL THEN 'cancelled'
            ELSE 'failed' END
        WHEN 'refused' THEN 'failed'
        WHEN 'commands_applied' THEN 'answered'
        WHEN 'cancelled' THEN 'cancelled'
        WHEN 'operator_cancelled' THEN 'cancelled'
        WHEN 'forked' THEN 'cancelled'
        WHEN 'session_deleted' THEN 'cancelled'
        ELSE '' END)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_session_runs_unfinished
    ON session_runs(session_id)
    WHERE admission_json IS NOT NULL AND terminal_kind IS NULL;
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
    kind           TEXT NOT NULL CONSTRAINT ck_control_intents_kind CHECK (kind IN ('close_session')),
    kind_json      TEXT NOT NULL,
    state          TEXT NOT NULL CONSTRAINT ck_control_intents_state CHECK (state IN ('pending', 'acknowledged')),
    state_json     TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL
);


CREATE INDEX IF NOT EXISTS idx_control_intents_session
    ON control_intents(session_id, kind);
";
