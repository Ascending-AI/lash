//! `deleted_sessions`: permanent identity evidence for every session id this
//! deployment has ever deleted.
//!
//! The table is exempt from retention by design (FIG-754 / FIG-748): a session
//! id is used once (ADR 0049), and the row is what makes a re-bind of a
//! deleted id refusable forever.
//!
//! Every statement over this table forks. The two existence probes differ —
//! SQLite reads a row and asks whether one came back, PostgreSQL asks
//! `EXISTS(...)` and reads a boolean — and both inserts name the durable head
//! table, which the backends spell differently (ADR 0098).

/// The table's unprefixed name.
pub const TABLE: &str = "deleted_sessions";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, created_at_ms, last_commit_at_ms, head_revision, relation_kind, parent_session_id";

/// The deleted half of the session catalog's union.
///
/// A deleted session reports identity, timestamps and the head revision it
/// reached, and nothing else: every causal column is `NULL` because the
/// metadata row that carried it is gone, which is what the trailing `1` — the
/// `deleted` flag — tells the decoder.
pub const CATALOG_UNION_COLUMNS_SQLITE: &str = "session_id, COALESCE(relation_kind, 'root'),
                    parent_session_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    created_at_ms, last_commit_at_ms, head_revision, 1";

/// The PostgreSQL spelling of [`CATALOG_UNION_COLUMNS_SQLITE`]: the `deleted`
/// flag is a real boolean, and the two nullable timestamps are `COALESCE`d
/// because PostgreSQL's columns admit `NULL` where SQLite's do not.
pub const CATALOG_UNION_COLUMNS_POSTGRES: &str = "session_id, COALESCE(relation_kind, 'root'),
                    parent_session_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    COALESCE(created_at_ms, 0), last_commit_at_ms,
                    COALESCE(head_revision, 0), TRUE";
