//! The PostgreSQL half of the session-ingress family (ADR 0101).
//!
//! `lash-store-sql`'s `session_ingress` module owns the column lists and every
//! statement both backends issue verbatim; this module owns the two that fork
//! and renders both sets once, at startup.

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::session_ingress::SessionIngressStatements;

lash_store_sql::statements! {
    /// `session_ingress` statements only PostgreSQL issues.
    pub(crate) struct SessionIngressPostgresStatements @ "session_ingress" {
        /// The next `enqueue_seq`, drawn from the column's own sequence after
        /// the caller took the session history lock, so the value is
        /// per-session commit order: a later admission of the same session
        /// waits for the lock and draws a later value.
        ///
        /// `pg_get_serial_sequence` takes its relation as *text*, so the table
        /// name is spelled with the `lash_` prefix inside a string literal,
        /// which the renderer leaves alone.
        select_next_enqueue_seq = "SELECT nextval(pg_get_serial_sequence(
                 'lash_session_ingress',
                 'enqueue_seq'
             ))";

        /// Admit one row at the sequence value `?1` drew.
        insert = "INSERT INTO session_ingress (
                 enqueue_seq,
                 item_id, session_id, lane, kind, source_key,
                 delivery_scope, delivery_turn_id, delivery_min_boundary, submission_digest,
                 payload_json, authority_json, merge_key, wake_process_id, wake_sequence,
                 state, enqueued_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 'open', ?16)";
    }
}

/// Every session-ingress statement this store issues.
pub(crate) struct SessionIngressSql {
    /// Shared across both backends.
    pub(crate) shared: SessionIngressStatements,
    /// PostgreSQL only.
    pub(crate) postgres: SessionIngressPostgresStatements,
}

static SESSION_INGRESS_SQL: LazyLock<SessionIngressSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    SessionIngressSql {
        shared: SessionIngressStatements::render(dialect),
        postgres: SessionIngressPostgresStatements::render(dialect),
    }
});

/// The session-ingress statements, rendered once at first use.
pub(crate) fn session_ingress_sql() -> &'static SessionIngressSql {
    &SESSION_INGRESS_SQL
}

#[cfg(test)]
mod tests {
    /// FIG-3607 contract 5, over the DDL: no column of the session ingress
    /// names a runtime process registration.
    #[test]
    fn the_session_ingress_table_names_no_runtime_process_registration() {
        let schema = crate::PostgresStorage::schema_ddl();
        let start = schema
            .find("CREATE TABLE IF NOT EXISTS lash_session_ingress (")
            .expect("the schema declares the session ingress");
        let table = &schema[start..start + schema[start..].find("\n);").expect("table end")];
        for forbidden in ["process_incarnation", "process_ref"] {
            assert!(!table.contains(forbidden), "`{forbidden}` in {table}");
        }
    }
}
