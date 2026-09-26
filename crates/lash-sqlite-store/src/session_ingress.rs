//! The SQLite half of the session-ingress family (ADR 0101).
//!
//! `lash-store-sql`'s `session_ingress` module owns the column lists and every
//! statement both backends issue verbatim; this module owns the one statement
//! that forks and renders both sets once, at startup.

use std::sync::LazyLock;

use lash_store_sql::session_ingress::SessionIngressStatements;

use crate::schema_layout::Schema;

lash_store_sql::statements! {
    /// `session_ingress` statements only SQLite issues.
    pub(crate) struct SessionIngressSqliteStatements @ "session_ingress" {
        /// Admit at the sequence allocated under the database write lock.
        insert = "INSERT INTO session_ingress (enqueue_seq,
                 item_id, session_id, lane, kind, source_key,
                 delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                 payload_json, authority_json, merge_key, wake_process_id, wake_sequence,
                 state, enqueued_at_ms
             ) VALUES (?16, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'open', ?15)
             RETURNING enqueue_seq";
    }
}

/// Every session-ingress statement the session catalog issues.
pub(crate) struct SessionIngressSql {
    /// Shared across both backends.
    pub(crate) shared: SessionIngressStatements,
    /// SQLite only.
    pub(crate) sqlite: SessionIngressSqliteStatements,
}

static SESSION_INGRESS_SQL: LazyLock<SessionIngressSql> = LazyLock::new(|| {
    let dialect = Schema::Main.dialect();
    SessionIngressSql {
        shared: SessionIngressStatements::render(dialect),
        sqlite: SessionIngressSqliteStatements::render(dialect),
    }
});

/// The session catalog's session-ingress statements, rendered once at first
/// use.
pub(crate) fn session_ingress_sql() -> &'static SessionIngressSql {
    &SESSION_INGRESS_SQL
}

/// Every caller holds the database write lock for the entire allocation.
pub(crate) fn allocate_sequence(
    conn: &rusqlite::Connection,
    session_id: &lash_sansio::SessionId,
) -> Result<i64, crate::StoreError> {
    conn.query_row(
        session_ingress_sql().shared.allocate_sequence.sql(),
        rusqlite::params![session_id.as_str()],
        |row| row.get(0),
    )
    .map_err(crate::sqlite_error)
}

#[cfg(test)]
mod tests {
    /// FIG-3607 contract 5, over the DDL: no column of the session ingress
    /// names a runtime process registration.
    #[test]
    fn the_session_ingress_table_names_no_runtime_process_registration() {
        let schema = crate::schema_fragments::SESSION_INGRESS_TABLE;
        let start = schema
            .find("CREATE TABLE IF NOT EXISTS session_ingress (")
            .expect("the durable core declares the session ingress");
        let table = &schema[start..start + schema[start..].find("\n);").expect("table end")];
        for forbidden in ["process_incarnation", "process_ref"] {
            assert!(!table.contains(forbidden), "`{forbidden}` in {table}");
        }
    }
}
