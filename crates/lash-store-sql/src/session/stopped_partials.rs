//! Sealed stopped turn values. Only a commit makes one readable.

pub const TABLE: &str = "stopped_partials";
pub const COLUMNS: &str = "session_id, turn_id, root, base, sealed_through, reason, recovered, digest, partial_json, body_bytes, sealed_at_ms, committed_at_ms";

crate::statements! {
    pub struct StoppedPartialStatements @ "stopped_partial" {
        select_turn = "SELECT root, base, sealed_through, reason, recovered, digest, partial_json, committed_at_ms
                  FROM stopped_partials WHERE session_id = ?1 AND turn_id = ?2";
        select_by_turn_or_root = "SELECT partial_json, committed_at_ms FROM stopped_partials
                  WHERE session_id = ?1 AND (turn_id = ?2 OR root = ?2)";
        insert = "INSERT INTO stopped_partials
                  (session_id, turn_id, root, base, sealed_through, reason, recovered, digest,
                   partial_json, body_bytes, sealed_at_ms, committed_at_ms)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)";
        commit = "UPDATE stopped_partials SET committed_at_ms = ?3
                  WHERE session_id = ?1 AND turn_id = ?2 AND committed_at_ms IS NULL";
        delete_session = "DELETE FROM stopped_partials WHERE session_id = ?1";
        delete_retained = "DELETE FROM stopped_partials AS partial
                  WHERE partial.committed_at_ms < ?1 AND partial.sealed_at_ms < ?1
                  AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                              WHERE deleted.session_id = partial.session_id)";
    }
}
