//! Tool-intent submission statements only PostgreSQL issues.

lash_store_sql::statements! {
    /// `tool_intent_submissions` statements only PostgreSQL issues.
    pub(crate) struct ToolIntentSubmissionPostgresStatements @ "tool_intent_submission" {
        /// Record submission `?1`, keeping an existing one under the same
        /// replay key: a replayed submission is the point of the ledger.
        insert_new = "INSERT INTO tool_intent_submissions (
                 replay_key, owner, execution_scope_id, tool_call_id,
                 intent_index, payload_hash, submission_json, admitted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (replay_key) DO NOTHING";

        /// Fence every durably deleted session owner with a row admitted
        /// strictly before bound `?1`, keeping an existing fence:
        /// `deleted_sessions` is in this database, so the proof is a join.
        fence_retired_owners = "INSERT INTO tool_intent_retired_owners (owner)
             SELECT DISTINCT submissions.owner
             FROM tool_intent_submissions AS submissions
             JOIN deleted_sessions AS deleted
               ON submissions.owner = 'session:' || deleted.session_id
             WHERE submissions.admitted_at_ms < ?1
             ON CONFLICT (owner) DO NOTHING";

        /// The submission recorded under replay key `?1`, locked for the
        /// caller's transaction: completing one is a read-then-write.
        select_by_replay_key_for_update = "SELECT submission_json FROM tool_intent_submissions
             WHERE replay_key = ?1 FOR UPDATE";
    }
}
