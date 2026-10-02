//! `turn_cancel_requests`: the relational cancellation intent for one turn.

pub const TABLE: &str = "turn_cancel_requests";
pub const INSERT_COLUMNS: &str =
    "session_id, turn_id, request_id, origin, reason, disposition, mode, intent_revision";
pub const REQUEST_COLUMNS: &str = "request_id, origin, reason, disposition, mode";

crate::statements! {
    /// `turn_cancel_requests` statements both backends issue verbatim.
    pub struct CancelRequestStatements @ "turn_cancel_request" {
        /// Delete session `?1`'s cancellation requests, on session deletion.
        delete_by_session = "DELETE FROM turn_cancel_requests WHERE session_id = ?1";

        /// Advance turn `?2` of session `?1` to intent revision `?3`.
        ///
        /// The revision is the closure compare-and-swap's version: the first
        /// policy acceptor is immutable, and a stronger same-policy request
        /// advances only this column.
        advance_intent_revision = "UPDATE turn_cancel_requests
             SET intent_revision = ?3
             WHERE session_id = ?1 AND turn_id = ?2";
        select_request = "SELECT request_id, origin, reason, disposition, mode
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";
        select_request_with_revision = "SELECT request_id, origin, reason, disposition, mode,
                    intent_revision
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";
        upsert_record = "INSERT INTO turn_cancel_requests (
                 session_id, turn_id, request_id, origin, reason, disposition, mode,
                 intent_revision
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (session_id, turn_id) DO UPDATE SET
                 request_id = excluded.request_id,
                 origin = excluded.origin,
                 reason = excluded.reason,
                 disposition = excluded.disposition,
                 mode = excluded.mode,
                 intent_revision = excluded.intent_revision";
    }
}
