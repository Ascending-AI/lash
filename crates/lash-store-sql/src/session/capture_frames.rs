//! Acknowledged capture frames and replay batch identities.

pub const TABLE: &str = "turn_capture_frames";
pub const COLUMNS: &str =
    "session_id, turn_id, sequence, base, invocation, attempt_epoch, batch_ordinal, frame_json";

crate::statements! {
    pub struct CaptureFrameStatements @ "capture_frame" {
        insert = "INSERT INTO turn_capture_frames
                  (session_id, turn_id, sequence, base, invocation, attempt_epoch, batch_ordinal, frame_json)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
        select_batch = "SELECT sequence, frame_json FROM turn_capture_frames
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3
                  AND attempt_epoch = ?4 AND batch_ordinal = ?5 ORDER BY sequence";
        select_inherited = "SELECT sequence, base, attempt_epoch, frame_json FROM turn_capture_frames
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3
                  ORDER BY sequence";
        select_all = "SELECT sequence, base, invocation, attempt_epoch, frame_json FROM turn_capture_frames
                  WHERE session_id = ?1 AND turn_id = ?2 ORDER BY sequence";
        delete_before_base = "DELETE FROM turn_capture_frames
                  WHERE session_id = ?1 AND turn_id = ?2 AND base < ?3";
        delete_turn = "DELETE FROM turn_capture_frames WHERE session_id = ?1 AND turn_id = ?2";
        delete_session = "DELETE FROM turn_capture_frames WHERE session_id = ?1";
    }
}
