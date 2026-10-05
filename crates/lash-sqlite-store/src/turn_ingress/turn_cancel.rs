//! Backend-specific cancellation locks and conflict handling.

lash_store_sql::statements! {
    /// First-request acceptance under SQLite's write lock.
    pub(crate) struct CancelRequestSqliteStatements @ "turn_cancel_request" {
        insert_first = "INSERT OR IGNORE INTO turn_cancel_requests (
                 session_id, turn_id, request_id, origin, reason, disposition, mode, intent_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)";
    }
}

lash_store_sql::statements! {
    /// `turn_cancellation_bindings` statements only SQLite issues.
    pub(crate) struct CancellationBindingSqliteStatements @ "turn_cancellation_binding" {
        /// Admit binding `?2` for session `?1` at scope `?3`.
        ///
        /// No conflict clause: the absence was read under the same
        /// `BEGIN IMMEDIATE` lock, so a conflict is a defect and the
        /// constraint error is the right report. PostgreSQL cannot hold that
        /// pair atomic under READ COMMITTED and swallows the conflict instead,
        /// re-reading the row to compare it.
        insert_new = "INSERT INTO turn_cancellation_bindings (
                 session_id, binding_id, admitted_scope_json
             )
             VALUES (?1, ?2, ?3)";
    }
}

lash_store_sql::statements! {
    /// `turn_cancel_closure_authorizations` statements only SQLite issues.
    pub(crate) struct ClosureAuthorizationSqliteStatements @ "turn_cancel_closure_authorization" {
        /// Turn `?2` of session `?1`'s pinned closure.
        ///
        /// No lock suffix: every caller already holds the database write lock.
        select_by_turn = "SELECT authorization_json FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1 AND turn_id = ?2";
    }
}

lash_store_sql::statements! {
    /// `turn_cancel_retired_scopes` statements only SQLite issues.
    pub(crate) struct RetiredScopeSqliteStatements @ "turn_cancel_retired_scope" {
        /// Retire scope `?1`, keeping an existing retirement.
        insert_new = "INSERT OR IGNORE INTO turn_cancel_retired_scopes (scope_id) VALUES (?1)";
    }
}

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
