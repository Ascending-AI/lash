//! A finished process's history archive (FIG-7701): the neutral statements
//! that pack a process's run facts, tool calls and model calls into one
//! archive on its `processes` row, and read the archive back.
//!
//! The archive column is selected only here: every other read of a process
//! row names its columns and leaves it out.

crate::statements! {
    lazy
    /// The process history statements both backends issue verbatim.
    pub struct ProcessHistoryStatements @ "durable_process_history" {
        /// Process `?1`'s archive, when its history is packed.
        archive = "SELECT history_archive FROM processes WHERE process_id = ?1 AND history = 'archive'";

        /// Up to `?2` finished processes after `?1` whose history is not yet
        /// packed or kept, by id.
        due = "SELECT process_id FROM processes
             WHERE terminal_event_sequence IS NOT NULL AND history IS NULL AND process_id > ?1
             ORDER BY process_id LIMIT ?2";

        /// Whether process `?1` is finished with its history undecided.
        undecided = "SELECT 1 FROM processes
             WHERE process_id = ?1 AND terminal_event_sequence IS NOT NULL AND history IS NULL";

        /// Process `?1` keeps its history as rows.
        keep_rows = "UPDATE processes SET history = 'rows' WHERE process_id = ?1 AND history IS NULL";

        /// Process `?1`'s history is archive `?2`.
        pack = "UPDATE processes SET history = 'archive', history_archive = ?2
             WHERE process_id = ?1 AND history IS NULL";

        /// Up to `?2` of owner `?1`'s run facts, as the run-fact read orders them.
        facts = "SELECT run_seq, ordinal, kind, call_id, (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref, written_epoch
             FROM run_records WHERE owner_key = ?1
             ORDER BY run_seq, ordinal LIMIT ?2";

        /// Up to `?2` of owner `?1`'s tool calls, every column, as the call
        /// reads order them.
        calls = "SELECT owner_key, call_id, execution_owner, run_seq, member, admission_seq, counted,
                    admitted_count, cancellable, session_id, turn_id, request_id, origin, reason, decision,
                    (SELECT hash FROM blobs WHERE id = decision_ref) AS decision_ref, answer_outcome,
                    (SELECT hash FROM blobs WHERE id = body_ref) AS body_ref
             FROM tool_calls WHERE owner_key = ?1
             ORDER BY admission_seq, call_id LIMIT ?2";

        /// Before any row is fetched: whether owner `?1` holds a call with
        /// no outcome or session `?2` a model reservation with no record
        /// (1 when either does), and the bytes of the text columns of up to
        /// `?3` of each of owner `?1`'s run facts and tool calls and session
        /// `?2`'s model calls. The archive stores every one of those bytes,
        /// so the sum never exceeds the archive's size.
        preflight = "SELECT CASE WHEN EXISTS (SELECT 1 FROM tool_calls WHERE owner_key = ?1 AND answer_outcome IS NULL)
                    OR EXISTS (SELECT 1 FROM model_call_records WHERE session_id = ?2 AND record_ref IS NULL)
                  THEN 1 ELSE 0 END AS unsettled,
                (SELECT COALESCE(SUM(octet_length(kind) + COALESCE(octet_length(call_id), 0)), 0)
                   FROM (SELECT kind, call_id FROM run_records WHERE owner_key = ?1 LIMIT ?3) AS f)
                + (SELECT COALESCE(SUM(octet_length(owner_key) + octet_length(call_id) + octet_length(execution_owner)
                      + COALESCE(octet_length(session_id), 0) + COALESCE(octet_length(turn_id), 0)
                      + COALESCE(octet_length(request_id), 0) + COALESCE(octet_length(origin), 0)
                      + COALESCE(octet_length(reason), 0) + COALESCE(octet_length(decision), 0)
                      + COALESCE(octet_length(answer_outcome), 0)), 0)
                   FROM (SELECT owner_key, call_id, execution_owner, session_id, turn_id, request_id, origin,
                             reason, decision, answer_outcome
                         FROM tool_calls WHERE owner_key = ?1 LIMIT ?3) AS c)
                + (SELECT COALESCE(SUM(octet_length(run) + octet_length(call) + octet_length(call_id)
                      + COALESCE(octet_length(enclosing_owner), 0) + COALESCE(octet_length(enclosing_call), 0)), 0)
                   FROM (SELECT run, call, call_id, enclosing_owner, enclosing_call
                         FROM model_call_records WHERE session_id = ?2 LIMIT ?3) AS m) AS payload";

        /// Whether a call another owner admitted is executed by `?1`.
        foreign_calls = "SELECT 1 FROM tool_calls WHERE execution_owner = ?1 AND owner_key <> ?1 LIMIT 1";

        /// Up to `?2` of session `?1`'s model calls, every column, as the
        /// model-call reads order them.
        models = "SELECT run, call, call_id, attempt, answered, enclosing_owner, enclosing_run, enclosing_call,
                    (SELECT hash FROM blobs WHERE id = record_ref) AS record_ref, ended_at_ms, written_epoch
             FROM model_call_records WHERE session_id = ?1
             ORDER BY run, call LIMIT ?2";
    }
}
