//! The cancellation and tool-intent statements only SQLite issues.
//!
//! `turn_cancel_requests` forks wholesale: SQLite stores the request as one
//! `record_json` document beside its revision, PostgreSQL as typed columns with
//! the affected-input receipts in a second table. ADR 0098 freezes both durable
//! encodings, so every read and write of that table is two statements. The rest
//! fork only on how each backend spells "keep what is already there".

lash_store_sql::statements! {
    /// `turn_cancel_requests` statements only SQLite issues.
    pub(crate) struct CancelRequestSqliteStatements @ "turn_cancel_request" {
        /// Record the first cancellation request for turn `?2` of session
        /// `?1`, at revision 1, keeping any request already there.
        ///
        /// The first policy acceptor is immutable, so the conflict is not an
        /// error: the caller reads the stored request back and reports it.
        insert_first = "INSERT OR IGNORE INTO turn_cancel_requests (
                 session_id, turn_id, record_json, intent_revision
             )
             VALUES (?1, ?2, ?3, 1)";

        /// The stored request document for turn `?2` of session `?1`.
        select_record = "SELECT record_json FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";

        /// The stored request document and its revision: the intent snapshot a
        /// closure compare-and-swap is taken against.
        select_record_with_revision = "SELECT record_json, intent_revision
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";

        /// Rewrite the stored request document of turn `?2` of session `?1`.
        ///
        /// SQLite appends an affected-input receipt by rewriting the document
        /// it lives in; PostgreSQL inserts a row into
        /// `lash_turn_cancel_affected_inputs` instead.
        update_record = "UPDATE turn_cancel_requests
             SET record_json = ?3
             WHERE session_id = ?1 AND turn_id = ?2";

        /// Record turn `?2` of session `?1`'s request `?3` at revision `?4`,
        /// replacing whatever is there.
        ///
        /// The caller has already proved the observed intent is still current,
        /// so this is the winner's write, not a blind overwrite.
        upsert_record = "INSERT INTO turn_cancel_requests (
                 session_id, turn_id, record_json, intent_revision
             )
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id, turn_id) DO UPDATE SET
                 record_json = excluded.record_json,
                 intent_revision = excluded.intent_revision";
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
                 replay_key, session_id, execution_scope_id, tool_call_id,
                 intent_index, kind, payload_hash, submission_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
    }
}
