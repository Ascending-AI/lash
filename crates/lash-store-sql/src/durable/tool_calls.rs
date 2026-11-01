//! The retained tool-call directory. Journal compaction never deletes it.

pub const TABLE: &str = "tool_calls";

crate::statements! {
    lazy
    pub struct ToolCallStatements @ "durable_tool_call" {
        admit = "INSERT INTO tool_calls AS held
                (owner_key, call_id, execution_owner, run_seq, member, cancellable, session_id, turn_id, counted, admission_seq, admitted_count)
            SELECT i.owner, i.call, i.execution, i.run, i.member, i.cancellable, i.session, i.turn, i.counted,
                COALESCE(p.admission_seq + 1, 0), COALESCE(p.admitted_count, 0) + CASE WHEN i.counted THEN 1 ELSE 0 END
            FROM (SELECT ?1 AS owner, ?2 AS call, ?3 AS execution, ?4 AS run, ?5 AS member,
                    ?6 AS cancellable, ?7 AS session, ?8 AS turn, ?9 AS counted) AS i
            LEFT JOIN tool_calls AS p ON p.owner_key = i.owner AND p.admission_seq =
                (SELECT admission_seq FROM tool_calls WHERE owner_key = i.owner ORDER BY admission_seq DESC LIMIT 1)
            WHERE TRUE ON CONFLICT (owner_key, call_id) DO UPDATE SET call_id = excluded.call_id
            WHERE (held.execution_owner, held.run_seq, held.member, held.cancellable, held.counted)
                = (excluded.execution_owner, excluded.run_seq, excluded.member, excluded.cancellable, excluded.counted)";
        decide = "UPDATE tool_calls AS held SET decision = i.decision, decision_ref = i.fact
            FROM (SELECT ?1 AS owner, ?2 AS call, ?3 AS decision, COALESCE((SELECT id FROM blobs WHERE hash = ?4), CASE WHEN ?4 IS NOT NULL THEN -1 END) AS fact) AS i
            WHERE held.execution_owner = i.owner AND held.call_id = i.call
              AND ((held.decision IS NULL AND held.answer_outcome IS NULL)
                OR (held.decision, held.decision_ref) IS NOT DISTINCT FROM (i.decision, i.fact))";
        answer = "UPDATE tool_calls AS held SET answer_outcome = i.outcome, body_ref = i.body
            FROM (SELECT ?1 AS owner, ?2 AS call, ?3 AS outcome, COALESCE((SELECT id FROM blobs WHERE hash = ?4), CASE WHEN ?4 IS NOT NULL THEN -1 END) AS body, ?5 AS cancelled) AS i
            WHERE held.execution_owner = i.owner AND held.call_id = i.call
              AND (i.outcome <> '\"Read\"' OR NOT held.cancellable)
              AND (held.decision IS NULL OR held.decision = CASE WHEN i.cancelled THEN 'cancel' ELSE 'final' END)
              AND (held.answer_outcome IS NULL OR (held.answer_outcome, held.body_ref) IS NOT DISTINCT FROM (i.outcome, i.body))";
        complete = "UPDATE tool_calls AS held SET decision = 'final', decision_ref = i.fact, answer_outcome = i.outcome, body_ref = i.body
            FROM (SELECT ?1 AS owner, ?2 AS call, COALESCE((SELECT id FROM blobs WHERE hash = ?3), CASE WHEN ?3 IS NOT NULL THEN -1 END) AS fact, ?4 AS outcome, COALESCE((SELECT id FROM blobs WHERE hash = ?5), CASE WHEN ?5 IS NOT NULL THEN -1 END) AS body, ?6 AS cancelled) AS i
            WHERE held.execution_owner = i.owner AND held.call_id = i.call AND NOT i.cancelled
              AND (i.outcome <> '\"Read\"' OR NOT held.cancellable)
              AND ((held.decision IS NULL AND held.answer_outcome IS NULL)
                OR ((held.decision, held.decision_ref) IS NOT DISTINCT FROM ('final', i.fact)
                  AND (held.answer_outcome IS NULL OR (held.answer_outcome, held.body_ref) IS NOT DISTINCT FROM (i.outcome, i.body))))";
        decision_root = "SELECT (SELECT hash FROM blobs WHERE id = decision_ref) AS decision_ref FROM tool_calls WHERE execution_owner = ?1 AND call_id = ?2";
        answer_root = "SELECT (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref FROM tool_calls WHERE execution_owner = ?1 AND call_id = ?2";
        page = "SELECT call_id, execution_owner, run_seq, member, decision, (SELECT hash FROM blobs WHERE id = decision_ref) AS decision_ref, answer_outcome,
                (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref, admission_seq
            FROM tool_calls WHERE owner_key = ?1
              AND (admission_seq, call_id) > (?2, ?3)
            ORDER BY admission_seq, call_id LIMIT ?4";
        saved_reads = "SELECT call_id, execution_owner, run_seq, member, decision, (SELECT hash FROM blobs WHERE id = decision_ref) AS decision_ref, answer_outcome,
                (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref, admission_seq
            FROM tool_calls WHERE owner_key = ?1 AND NOT cancellable AND answer_outcome = '\"Read\"'
              AND (admission_seq, call_id) > (?2, ?3)
            ORDER BY admission_seq, call_id LIMIT ?4";
        finish_reads = "UPDATE tool_calls SET answer_outcome = ?2
            WHERE owner_key = ?1 AND NOT cancellable AND answer_outcome IS NULL";
        delete_owner = "DELETE FROM tool_calls WHERE owner_key = ?1 RETURNING (SELECT hash FROM blobs WHERE id = decision_ref) AS decision_ref, (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref";
        delete_session = "DELETE FROM tool_calls WHERE session_id = ?1 RETURNING (SELECT hash FROM blobs WHERE id = decision_ref) AS decision_ref, (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref";
    }
}
