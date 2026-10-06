//! The session's closing state: the neutral statements of the `session_close` domain (I0, FIG-5194).
//!
//! Owned by L6b (FIG-5176): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).

/// The table's unprefixed name: one row per closing or closed session. Once
/// its last step is the tombstone, the row is the session's tombstone.
pub const TABLE: &str = "session_close";

/// The unprefixed name of the table of a session's scopes whose cascade
/// still has `Until` children to mark.
pub const ENDING_TABLE: &str = "session_scope_ends";

crate::statements! {
    /// `session_close` statements both backends issue verbatim.
    pub struct SessionCloseStatements @ "durable_session_close" {
        /// Begin session `?1`'s close at `?2`, written at epoch `?3`. No
        /// change when it is already closing.
        begin = "INSERT INTO session_close (session_id, done_step, begun_at_ms, written_epoch)
             VALUES (?1, NULL, ?2, ?3)
             ON CONFLICT (session_id) DO NOTHING";

        /// Session `?1`'s close row: its last step done, when it began and
        /// the epoch that last wrote it.
        row = "SELECT done_step, begun_at_ms, written_epoch FROM session_close
             WHERE session_id = ?1";

        /// Record step `?2` as session `?1`'s last step done, at epoch `?3`.
        record_step = "UPDATE session_close SET done_step = ?2, written_epoch = ?3
             WHERE session_id = ?1";

        /// Record scope `?2` of session `?1` as ending at `?3`, at epoch `?4`.
        /// No change when it is already ending.
        scope_ending = "INSERT INTO session_scope_ends
                 (session_id, scope_key, begun_at_ms, written_epoch)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id, scope_key) DO NOTHING";

        /// Clear scope `?2` of session `?1`: its cascade is done.
        scope_ended = "DELETE FROM session_scope_ends WHERE session_id = ?1 AND scope_key = ?2";

        /// Session `?1`'s ending scopes, oldest first.
        ending_scopes = "SELECT scope_key FROM session_scope_ends WHERE session_id = ?1
             ORDER BY begun_at_ms, scope_key";
    }
}
