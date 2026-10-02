//! Neutral engine wait receipt statements.
pub const TABLE: &str = "wait_receipts";
crate::statements! {
    pub struct WaitReceiptStatements @ "wait_receipts" {
        insert_request = "INSERT INTO wait_receipts (wait_id, owner_key, session_id, started_at_ms, request_json) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (wait_id) DO NOTHING";
        select_request = "SELECT request_json FROM wait_receipts WHERE wait_id = ?1";
        resolve = "UPDATE wait_receipts SET resolution_json = ?2, resolved_at_ms = ?3 WHERE wait_id = ?1 AND resolution_json IS NULL";
        select_resolution = "SELECT resolution_json FROM wait_receipts WHERE wait_id = ?1";
        retire = "UPDATE wait_receipts SET retired_at_ms = ?2 WHERE owner_key = ?1 AND retired_at_ms IS NULL";
        reclaim = "DELETE FROM wait_receipts WHERE COALESCE(resolved_at_ms, started_at_ms) < ?1 AND (retired_at_ms < ?1 OR session_id IN (SELECT session_id FROM deleted_sessions))";
    }
}
