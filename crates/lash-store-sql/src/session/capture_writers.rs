//! Epoch leases for each effect invocation.

pub const TABLE: &str = "turn_capture_writers";
pub const COLUMNS: &str = "session_id, turn_id, invocation, attempt_epoch, state";

crate::statements! {
    pub struct CaptureWriterStatements @ "capture_writer" {
        select_latest = "SELECT attempt_epoch, state FROM turn_capture_writers
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3
                  ORDER BY attempt_epoch DESC LIMIT 1";
        select_epoch = "SELECT state FROM turn_capture_writers
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3 AND attempt_epoch = ?4";
        insert = "INSERT INTO turn_capture_writers
                  (session_id, turn_id, invocation, attempt_epoch, state)
                  VALUES (?1, ?2, ?3, ?4, ?5)";
        fence_invocation = "UPDATE turn_capture_writers SET state = 'fenced'
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3 AND state = 'live'";
        retract = "UPDATE turn_capture_writers SET state = 'retracted'
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3
                  AND attempt_epoch = ?4 AND state = 'live'";
        retract_fenced = "UPDATE turn_capture_writers SET state = 'retracted'
                  WHERE session_id = ?1 AND turn_id = ?2 AND invocation = ?3
                  AND attempt_epoch = ?4 AND state = 'fenced'";
        fence_turn = "UPDATE turn_capture_writers SET state = 'fenced'
                  WHERE session_id = ?1 AND turn_id = ?2 AND state = 'live'";
        select_retracted = "SELECT invocation, attempt_epoch FROM turn_capture_writers
                  WHERE session_id = ?1 AND turn_id = ?2 AND state = 'retracted'";
        delete_turn = "DELETE FROM turn_capture_writers WHERE session_id = ?1 AND turn_id = ?2";
        delete_session = "DELETE FROM turn_capture_writers WHERE session_id = ?1";
    }
}
