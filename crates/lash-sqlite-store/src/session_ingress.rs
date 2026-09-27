//! The SQLite half of the session-ingress family: the one per-session
//! sequence both admission tables draw `enqueue_seq` from (ADR 0101 §5,
//! amended).
//!
//! `lash-store-sql`'s `session_ingress` module owns the statements; this
//! module renders them once, at startup.

use std::sync::LazyLock;

use lash_store_sql::session_ingress::SessionIngressStatements;

use crate::schema_layout::Schema;

static SESSION_INGRESS_SQL: LazyLock<SessionIngressStatements> =
    LazyLock::new(|| SessionIngressStatements::render(Schema::Main.dialect()));

/// The session catalog's session-ingress statements, rendered once at first
/// use.
pub(crate) fn session_ingress_sql() -> &'static SessionIngressStatements {
    &SESSION_INGRESS_SQL
}

/// Every caller holds the database write lock for the entire allocation.
pub(crate) fn allocate_sequence(
    conn: &rusqlite::Connection,
    session_id: &lash_sansio::SessionId,
) -> Result<i64, crate::StoreError> {
    conn.query_row(
        session_ingress_sql().allocate_sequence.sql(),
        rusqlite::params![session_id.as_str()],
        |row| row.get(0),
    )
    .map_err(crate::sqlite_error)
}
