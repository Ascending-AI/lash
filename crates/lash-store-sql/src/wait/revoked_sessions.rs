//! `await_event_revoked_sessions`: the permanent tombstone of a session whose
//! promises were revoked.

/// The table's unprefixed name.
pub const TABLE: &str = "await_event_revoked_sessions";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, revoked_at_ms";

crate::statements! {
    /// `await_event_revoked_sessions` statements both backends issue verbatim.
    pub struct RevokedSessionStatements @ "await_event_revoked_session" {
        /// Whether session `?1` has been revoked.
        exists = "SELECT EXISTS(
                 SELECT 1 FROM await_event_revoked_sessions WHERE session_id = ?1
             )";

        insert_ignore = "INSERT INTO await_event_revoked_sessions (session_id, revoked_at_ms)
             VALUES (?1, ?2)
             ON CONFLICT (session_id) DO NOTHING";
    }
}
