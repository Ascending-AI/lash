//! Session tool request and completion receipts.
pub const TABLE: &str = "tool_call_receipts";
crate::statements! {
    pub struct ToolReceiptStatements @ "tool_receipts" {
        select_request = "SELECT request_json FROM tool_call_receipts WHERE request_key = ?1";
        insert_request = "INSERT INTO tool_call_receipts (request_key, session_id, payload_digest, requested_at_ms, request_json) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (request_key) DO NOTHING";
        complete = "UPDATE tool_call_receipts SET completion_json = ?2, completed_at_ms = ?3 WHERE request_key = ?1 AND completion_json IS NULL";
        select_completion = "SELECT completion_json FROM tool_call_receipts WHERE request_key = ?1";
        reclaim = "DELETE FROM tool_call_receipts WHERE COALESCE(completed_at_ms, requested_at_ms) < ?1 AND session_id IN (SELECT session_id FROM deleted_sessions)";
    }
}
