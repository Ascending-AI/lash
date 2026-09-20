//! `process_observers`: which sessions observe which process incarnation.
//!
//! Three columns, all of them key. There is no projection of this table wider
//! than its insert: every read either asks whether a row exists or lists the
//! session ids under one process.

/// The table's unprefixed name.
pub const TABLE: &str = "process_observers";

/// Every column, in insert order. The whole row is the key.
pub const INSERT_COLUMNS: &str = "session_id, process_id, process_incarnation";

crate::statements! {
    /// `process_observers` statements both backends issue verbatim.
    pub struct ObserverStatements @ "process_observer" {
        /// Record that session `?1` observes incarnation `?2` / `?3`, where
        /// the caller has already established the row is absent.
        ///
        /// Conflict-free by construction on this path only; the idempotent
        /// spellings are each backend's own, because a duplicate is an
        /// `INSERT OR IGNORE` on SQLite and an `ON CONFLICT DO NOTHING` on
        /// PostgreSQL.
        insert = "INSERT INTO process_observers (session_id, process_id, process_incarnation)
                             VALUES (?1, ?2, ?3)";

        /// Drop session `?1`'s observation of incarnation `?2` / `?3`.
        delete = "DELETE FROM process_observers
                             WHERE session_id = ?1 AND process_id = ?2 AND process_incarnation = ?3";

        /// Drop every observation session `?1` holds: the session is going
        /// away.
        delete_by_session = "DELETE FROM process_observers WHERE session_id = ?1";
    }
}
