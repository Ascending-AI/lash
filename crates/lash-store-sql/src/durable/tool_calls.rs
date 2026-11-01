//! The retained tool-call directory. Journal compaction never deletes it.

pub const TABLE: &str = "tool_calls";

crate::statements! {
    lazy
    pub struct ToolCallStatements @ "durable_tool_call" {
        admit = "WITH incoming AS (
                SELECT ?1 AS owner, ?2 AS call, ?3 AS execution, ?4 AS run, ?5 AS member,
                    ?6 AS cancellable, ?7 AS session, ?8 AS turn, ?9 AS counted
            ), prior AS MATERIALIZED (
                SELECT admission_seq, admitted_count FROM tool_calls
                WHERE owner_key = (SELECT owner FROM incoming) ORDER BY admission_seq DESC LIMIT 1
            )
            INSERT INTO tool_calls AS held
                (owner_key, call_id, execution_owner, run_seq, member, cancellable, session_id, turn_id, counted, admission_seq, admitted_count)
            SELECT i.owner, i.call, i.execution, i.run, i.member, i.cancellable, i.session, i.turn, i.counted,
                COALESCE(p.admission_seq + 1, 0), COALESCE(p.admitted_count, 0) + CASE WHEN i.counted THEN 1 ELSE 0 END
            FROM incoming AS i LEFT JOIN prior AS p ON TRUE WHERE TRUE
            ON CONFLICT (owner_key, call_id) DO UPDATE SET call_id = excluded.call_id
            WHERE (held.execution_owner, held.run_seq, held.member, held.cancellable, held.counted)
                = (excluded.execution_owner, excluded.run_seq, excluded.member, excluded.cancellable, excluded.counted)";
        decide = "WITH incoming AS (SELECT ?1 AS owner, ?2 AS call, ?3 AS decision, ?4 AS fact)
            UPDATE tool_calls AS held SET decision = i.decision, decision_ref = i.fact
            FROM incoming AS i WHERE held.execution_owner = i.owner AND held.call_id = i.call
              AND ((held.decision IS NULL AND held.answer_outcome IS NULL)
                OR (held.decision, held.decision_ref) IS NOT DISTINCT FROM (i.decision, i.fact))";
        answer = "WITH incoming AS (SELECT ?1 AS owner, ?2 AS call, ?3 AS outcome, ?4 AS body, ?5 AS cancelled)
            UPDATE tool_calls AS held SET answer_outcome = i.outcome, body_ref = i.body
            FROM incoming AS i WHERE held.execution_owner = i.owner AND held.call_id = i.call
              AND (held.decision IS NULL OR held.decision = CASE WHEN i.cancelled THEN 'cancel' ELSE 'final' END)
              AND (held.answer_outcome IS NULL OR (held.answer_outcome, held.body_ref) IS NOT DISTINCT FROM (i.outcome, i.body))";
        complete = "WITH incoming AS (SELECT ?1 AS owner, ?2 AS call, ?3 AS fact, ?4 AS outcome, ?5 AS body, ?6 AS cancelled)
            UPDATE tool_calls AS held SET decision = 'final', decision_ref = i.fact, answer_outcome = i.outcome, body_ref = i.body
            FROM incoming AS i WHERE held.execution_owner = i.owner AND held.call_id = i.call AND NOT i.cancelled
              AND ((held.decision IS NULL AND held.answer_outcome IS NULL)
                OR ((held.decision, held.decision_ref) IS NOT DISTINCT FROM ('final', i.fact)
                  AND (held.answer_outcome IS NULL OR (held.answer_outcome, held.body_ref) IS NOT DISTINCT FROM (i.outcome, i.body))))";
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
