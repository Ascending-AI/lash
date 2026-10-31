//! The retained tool-call directory. Journal compaction never deletes it.

pub const TABLE: &str = "tool_calls";

crate::statements! {
    pub struct ToolCallStatements @ "durable_tool_call" {
        admit = "INSERT INTO tool_calls
            (owner_key, call_id, execution_owner, run_seq, member, cancellable, session_id, turn_id, counted, admission_seq, admitted_count)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                COALESCE((SELECT admission_seq + 1 FROM tool_calls WHERE owner_key = ?1 ORDER BY admission_seq DESC LIMIT 1), 0),
                COALESCE((SELECT admitted_count FROM tool_calls WHERE owner_key = ?1 ORDER BY admission_seq DESC LIMIT 1), 0) + CASE WHEN ?9 THEN 1 ELSE 0 END)
            ON CONFLICT (owner_key, call_id) DO UPDATE SET call_id = excluded.call_id
            WHERE tool_calls.execution_owner = excluded.execution_owner
              AND tool_calls.run_seq = excluded.run_seq AND tool_calls.member = excluded.member
              AND tool_calls.cancellable = excluded.cancellable AND tool_calls.counted = excluded.counted";
        decide = "UPDATE tool_calls SET decision = ?3, decision_ref = ?4
            WHERE execution_owner = ?1 AND call_id = ?2
              AND ((decision IS NULL AND answer_outcome IS NULL) OR (decision = ?3 AND decision_ref = ?4))";
        answer = "UPDATE tool_calls SET answer_outcome = ?3, body_ref = ?4
            WHERE execution_owner = ?1 AND call_id = ?2
              AND (decision IS NULL OR (decision = 'cancel' AND ?5) OR (decision = 'final' AND NOT ?5))
              AND (answer_outcome IS NULL OR (answer_outcome = ?3
                AND (body_ref = ?4 OR (body_ref IS NULL AND ?4 IS NULL))))";
        complete = "UPDATE tool_calls SET decision = 'final', decision_ref = ?3,
                answer_outcome = ?4, body_ref = ?5
            WHERE execution_owner = ?1 AND call_id = ?2 AND NOT ?6
              AND ((decision IS NULL AND answer_outcome IS NULL)
                OR (decision = 'final' AND decision_ref = ?3
                  AND (answer_outcome IS NULL OR (answer_outcome = ?4
                    AND (body_ref = ?5 OR (body_ref IS NULL AND ?5 IS NULL))))))";
        decision_root = "SELECT decision_ref FROM tool_calls WHERE execution_owner = ?1 AND call_id = ?2";
        answer_root = "SELECT body_ref FROM tool_calls WHERE execution_owner = ?1 AND call_id = ?2";
        page = "SELECT call_id, execution_owner, run_seq, member, decision, decision_ref, answer_outcome, body_ref, admission_seq
            FROM tool_calls WHERE owner_key = ?1
              AND (admission_seq, call_id) > (?2, ?3)
            ORDER BY admission_seq, call_id LIMIT ?4";
        saved_reads = "SELECT call_id, execution_owner, run_seq, member, decision, decision_ref, answer_outcome, body_ref, admission_seq
            FROM tool_calls WHERE owner_key = ?1 AND answer_outcome = '\"Read\"'
              AND (admission_seq, call_id) > (?2, ?3)
            ORDER BY admission_seq, call_id LIMIT ?4";
        finish_reads = "UPDATE tool_calls SET answer_outcome = ?3
            WHERE session_id = ?1 AND turn_id = ?2 AND NOT cancellable AND answer_outcome IS NULL";
        delete_owner = "DELETE FROM tool_calls WHERE owner_key = ?1 RETURNING decision_ref, body_ref";
        delete_session = "DELETE FROM tool_calls WHERE session_id = ?1 RETURNING decision_ref, body_ref";
    }
}
