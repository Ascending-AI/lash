//! `pins`: one row per target a host asked a session to retain.
//!
//! A target is an input, a turn (a logical run) or a head revision. A pin is
//! a statement about a name, not about a state: it can be written before its
//! target exists, while the target runs, or after it ended, and writing it
//! never reads or moves the session's head. It resolves to a revision by
//! query, through rows that already exist (`session_run_inputs`, then
//! `session_runs.terminal_head_revision`), and the only reader of that
//! resolution is the retained-revisions relation
//! ([`super::revisions::SessionRevisionStatements::prune_unretained`]).
//! Pins are deleted with their session.

/// The table's unprefixed name.
pub const TABLE: &str = "pins";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, target_kind, target_id";

crate::statements! {
    /// `pins` statements both backends issue verbatim.
    pub struct PinStatements @ "pin" {
        /// Pin target `?2`/`?3` of session `?1`. Pinning it again changes
        /// nothing.
        insert = "INSERT INTO pins (session_id, target_kind, target_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, target_kind, target_id) DO NOTHING";

        /// Release the pin on target `?2`/`?3` of session `?1`.
        delete = "DELETE FROM pins
             WHERE session_id = ?1 AND target_kind = ?2 AND target_id = ?3";

        /// Every pin of session `?1`, in target order.
        select_by_session = "SELECT target_kind, target_id FROM pins
             WHERE session_id = ?1
             ORDER BY target_kind, target_id";

        /// Every pin of session `?1`: its deletion.
        delete_by_session = "DELETE FROM pins WHERE session_id = ?1";
    }
}
