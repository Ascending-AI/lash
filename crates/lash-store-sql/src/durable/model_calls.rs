//! Retained sealed model calls, addressed like prompt snapshots.
pub const TABLE: &str = "model_call_records";
pub const PINS_TABLE: &str = "model_call_pins";
crate::statements! {
    pub struct ModelCallStatements @ "durable_model_call" {
        reserve = "INSERT INTO model_call_pins (session_id, run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call)
            VALUES (?1, ?2, ?3, ?4, ?5, FALSE, ?6, ?7, ?8)
            ON CONFLICT (session_id, run, call) DO UPDATE SET attempt = excluded.attempt
            WHERE model_call_pins.call_id = excluded.call_id AND model_call_pins.attempt < excluded.attempt";
        answered = "UPDATE model_call_pins SET answered = TRUE WHERE session_id = ?1 AND run = ?2 AND call = ?3";
        retire_pin = "DELETE FROM model_call_pins WHERE session_id = ?1 AND run = ?2 AND call = ?3";
        pins = "SELECT run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call FROM model_call_pins WHERE session_id = ?1 AND (run = ?2 OR (?2 LIKE 'turn:%' AND run = 'owned:' || SUBSTR(?2, 6))) ORDER BY run, call";
        delete_pins = "DELETE FROM model_call_pins WHERE session_id = ?1";
        append = "INSERT INTO model_call_records
            (session_id, run, call, call_id, record_ref, ended_at_ms, written_epoch)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (session_id, run, call) DO NOTHING";
        root = "SELECT record_ref FROM model_call_records WHERE session_id = ?1 AND run = ?2 AND call = ?3";
        read = "SELECT call_id, record_ref, ended_at_ms FROM model_call_records
            WHERE session_id = ?1 AND (run = ?2 OR (?2 LIKE 'turn:%' AND run = 'owned:' || SUBSTR(?2, 6))) ORDER BY run, call";
        delete_session = "DELETE FROM model_call_records WHERE session_id = ?1 RETURNING record_ref";
    }
}
