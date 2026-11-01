//! Model-call identities, reservations and sealed records on one lifecycle row.
//!
//! An owner's reads name its runs as an `IN` list, so they seek the
//! `(session_id, run)` key instead of scanning every call the session
//! retained (FIG-6889).
pub const TABLE: &str = "model_call_records";
crate::statements! {
    lazy
    pub struct ModelCallStatements @ "durable_model_call" {
        reserve = "INSERT INTO model_call_records (session_id, run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call)
            VALUES (?1, ?2, ?3, ?4, ?5, FALSE, ?6, ?7, ?8)
            ON CONFLICT (session_id, run, call) DO UPDATE SET attempt = CASE WHEN model_call_records.attempt < excluded.attempt THEN excluded.attempt ELSE model_call_records.attempt END
            WHERE model_call_records.record_ref IS NULL AND model_call_records.call_id = excluded.call_id
              AND model_call_records.enclosing_owner IS NOT DISTINCT FROM excluded.enclosing_owner
              AND model_call_records.enclosing_run IS NOT DISTINCT FROM excluded.enclosing_run
              AND model_call_records.enclosing_call IS NOT DISTINCT FROM excluded.enclosing_call";
        answered = "UPDATE model_call_records SET answered = TRUE WHERE session_id = ?1 AND run = ?2 AND call = ?3 AND record_ref IS NULL";
        pins = "SELECT run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call FROM model_call_records WHERE session_id = ?1 AND record_ref IS NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END) ORDER BY run, call";
        pin_by_key = "SELECT run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call FROM model_call_records WHERE session_id = ?1 AND run = ?2 AND call = ?3 AND record_ref IS NULL";
        append = "INSERT INTO model_call_records
            (session_id, run, call, call_id, record_ref, ended_at_ms, written_epoch, answered)
            VALUES (?1, ?2, ?3, ?4, COALESCE((SELECT id FROM blobs WHERE hash = ?5), CASE WHEN ?5 IS NOT NULL THEN -1 END), ?6, ?7, TRUE)
            ON CONFLICT (session_id, run, call) DO UPDATE SET record_ref = excluded.record_ref,
                ended_at_ms = COALESCE(model_call_records.ended_at_ms, excluded.ended_at_ms),
                written_epoch = COALESCE(model_call_records.written_epoch, excluded.written_epoch), answered = TRUE
            WHERE model_call_records.call_id = excluded.call_id
              AND (model_call_records.record_ref IS NULL OR model_call_records.record_ref = excluded.record_ref)";
        root = "SELECT (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref FROM model_call_records WHERE session_id = ?1 AND run = ?2 AND call = ?3 AND record_ref IS NOT NULL";
        read = "SELECT call_id, (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref, ended_at_ms FROM model_call_records
            WHERE session_id = ?1 AND record_ref IS NOT NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END) ORDER BY run, call";
        ids = "SELECT call_id FROM model_call_records WHERE session_id = ?1 AND record_ref IS NOT NULL AND run IN (?2, CASE WHEN ?2 LIKE 'turn:%' THEN 'owned:' || SUBSTR(?2, 6) END) ORDER BY run, call";
        delete_session = "DELETE FROM model_call_records WHERE session_id = ?1 RETURNING (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref";
    }
}
