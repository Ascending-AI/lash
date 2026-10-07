//! `turn_cancel_requests`: the cancel request one turn accepted.

pub const TABLE: &str = "turn_cancel_requests";

crate::statements! {
    /// `turn_cancel_requests` statements both backends issue verbatim.
    pub struct CancelRequestStatements @ "turn_cancel_request" {
        /// Delete session `?1`'s cancellation requests, on session deletion.
        delete_by_session = "DELETE FROM turn_cancel_requests WHERE session_id = ?1";

        /// Turn `?2` of session `?1`'s accepted cancel request.
        select_request = "SELECT request_id, origin, reason, disposition, mode
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";
    }
}
