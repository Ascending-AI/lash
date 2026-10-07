//! Tool-intent submission statements only SQLite issues.

lash_store_sql::statements! {
    /// `tool_intent_submissions` statements only SQLite issues.
    pub(crate) struct ToolIntentSubmissionSqliteStatements @ "tool_intent_submission" {
        /// Record submission `?1`, keeping an existing one under the same
        /// replay key: a replayed submission is the point of the ledger.
        insert_new = "INSERT OR IGNORE INTO tool_intent_submissions (
                 replay_key, owner, execution_scope_id, tool_call_id,
                 intent_index, payload_hash, submission_json, admitted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

        /// The durably deleted session owners in JSON array `?1` whose rows
        /// the retained-evidence lever reclaims: fence each, keeping an
        /// existing fence.
        fence_retired_owners = "INSERT OR IGNORE INTO tool_intent_retired_owners (owner)
             SELECT 'session:' || value FROM json_each(?1)";

        /// The session owners, not yet fenced, with a row admitted strictly
        /// before bound `?1`: the sweep's candidates, filtered against the
        /// durable-core session catalog before anything is fenced.
        select_reclaim_candidate_sessions = "SELECT DISTINCT substr(owner, 9)
             FROM tool_intent_submissions
             WHERE owner LIKE 'session:%'
               AND admitted_at_ms < ?1
               AND owner NOT IN (SELECT owner FROM tool_intent_retired_owners)
             ORDER BY 1";
    }
}
