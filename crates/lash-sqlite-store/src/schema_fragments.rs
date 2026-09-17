//! DDL fragments shared verbatim between independently versioned SQLite
//! databases.
//!
//! A table declared here lives under every version counter of the databases
//! whose [`crate::schema::SqliteDatabase`] definition lists it, so a shape
//! edit is a single edit that reaches every carrier. Two copy-pasted copies
//! under two counters could diverge silently — an edit bumped under one
//! counter would leave the other database's stale table in place, and the
//! shared `await_event.rs` / `scope_fence.rs` SQL would then run against a
//! column set it no longer matches (S14-A1, FIG-3260).

/// Cancellation-only durable promises for Native sessions, shared by the
/// durable-core and effect-replay databases.
///
/// In durable core they let a reopened session recover the same authority
/// without migrating unrelated Native effects into the effect journal; in the
/// effect journal they sit beside the effects that resolve them.
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
