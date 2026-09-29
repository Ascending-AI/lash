//! Physical turn capture counters and recovery evidence.

pub const TABLE: &str = "turn_capture_turns";
pub const COLUMNS: &str = "session_id, turn_id, root, base, next_sequence, recovered";

crate::statements! {
    pub struct CaptureTurnStatements @ "capture_turn" {
        insert = "INSERT INTO turn_capture_turns (session_id, turn_id, root, base, next_sequence, recovered)
                  VALUES (?1, ?2, ?3, 0, 1, 0) ON CONFLICT(session_id, turn_id) DO NOTHING";
        select = "SELECT root, base, next_sequence, recovered FROM turn_capture_turns
                  WHERE session_id = ?1 AND turn_id = ?2";
        select_by_root = "SELECT turn_id FROM turn_capture_turns
                  WHERE session_id = ?1 AND root = ?2 ORDER BY turn_id LIMIT 2";
        advance = "UPDATE turn_capture_turns SET base = ?3 WHERE session_id = ?1 AND turn_id = ?2 AND base = ?4";
        set_next_sequence = "UPDATE turn_capture_turns SET next_sequence = ?3
                  WHERE session_id = ?1 AND turn_id = ?2";
        mark_recovered = "UPDATE turn_capture_turns SET recovered = 1
                  WHERE session_id = ?1 AND turn_id = ?2";
        delete = "DELETE FROM turn_capture_turns WHERE session_id = ?1 AND turn_id = ?2";
        delete_session = "DELETE FROM turn_capture_turns WHERE session_id = ?1";
    }
}
