//! The cancellation and tool-intent statements only PostgreSQL issues.
//!
//! `turn_cancel_requests` forks wholesale: PostgreSQL stores the request as
//! typed columns with the affected-input receipts in
//! `lash_turn_cancel_affected_inputs`, SQLite as one `record_json` document.
//! ADR 0098 freezes both durable encodings, so every read and write of that
//! table is two statements. The rest fork on the row lock PostgreSQL must take
//! where SQLite holds the database write lock.

lash_store_sql::statements! {
    /// `turn_cancel_requests` statements only PostgreSQL issues.
    pub(crate) struct CancelRequestPostgresStatements @ "turn_cancel_request" {
        /// Record the first cancellation request for turn `?2` of session `?1`
        /// at revision 1.
        ///
        /// The caller has already read the absence under the row lock it holds
        /// on the turn, so there is no conflict clause: SQLite's counterpart
        /// keeps its `OR IGNORE` because it has no such lock to rely on.
        insert_first = "INSERT INTO turn_cancel_requests (
                 session_id, turn_id, request_id, origin, reason, disposition, mode,
                 intent_revision
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)";

        /// Turn `?2` of session `?1`'s request fields.
        select_request = "SELECT request_id, origin, reason, disposition, mode
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";

        /// [`select_request`](Self::select_request), locked for the caller's
        /// transaction: a record read that precedes a write serializes on this
        /// row.
        select_request_for_update = "SELECT request_id, origin, reason, disposition, mode
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2 FOR UPDATE";

        /// Turn `?2` of session `?1`'s intent snapshot: the request fields and
        /// the revision a closure compare-and-swap is taken against.
        select_request_with_revision = "SELECT request_id, origin, reason, disposition, mode,
                    intent_revision
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2";

        /// [`select_request_with_revision`](Self::select_request_with_revision),
        /// locked for the caller's transaction.
        select_request_with_revision_for_update = "SELECT request_id, origin, reason, disposition,
                    mode, intent_revision
             FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2 FOR UPDATE";

        /// Lock turn `?2` of session `?1`'s request, and report whether it is
        /// there at all.
        ///
        /// Concurrent appends to the affected-input receipts serialize on this
        /// row so their ordinals cannot collide; SQLite rewrites the one
        /// document under its write lock and needs no counterpart.
        lock_request = "SELECT 1 FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2 FOR UPDATE";

        /// The caller has already proved the observed intent is still current,
        /// so this is the winner's write, not a blind overwrite.
        upsert_record = "INSERT INTO turn_cancel_requests (
                 session_id, turn_id, request_id, origin, reason, disposition, mode,
                 intent_revision
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (session_id, turn_id) DO UPDATE SET
                 request_id = excluded.request_id,
                 origin = excluded.origin,
                 reason = excluded.reason,
                 disposition = excluded.disposition,
                 mode = excluded.mode,
                 intent_revision = excluded.intent_revision";
    }
}

lash_store_sql::statements! {
    /// `turn_cancellation_bindings` statements only PostgreSQL issues.
    pub(crate) struct CancellationBindingPostgresStatements @ "turn_cancellation_binding" {
        /// Admit binding `?2` for session `?1` at scope `?3`, keeping whatever
        /// is already admitted.
        ///
        /// PostgreSQL cannot hold "read the absence, then insert" atomic under
        /// READ COMMITTED, so the conflict clause is what detects a concurrent
        /// registrar and the caller re-reads the row to compare it. SQLite
        /// reads the absence under the same write lock it inserts under, so a
        /// conflict there is a defect and the constraint error is kept.
        insert_new = "INSERT INTO turn_cancellation_bindings (
                 session_id, binding_id, admitted_scope_json
             )
             VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING";

        /// Session `?1`'s admitted binding, locked for the caller's
        /// transaction: the validation that may insert one serializes here.
        select_by_session_for_update = "SELECT binding_id, admitted_scope_json
             FROM turn_cancellation_bindings
             WHERE session_id = ?1 FOR UPDATE";
    }
}

lash_store_sql::statements! {
    /// `turn_cancel_affected_inputs` statements. The table has no SQLite half:
    /// SQLite keeps the same dispositions as one `record_json` document on the
    /// cancel-request row, so both statements here are PostgreSQL-only by
    /// construction.
    pub(crate) struct CancelAffectedInputPostgresStatements @ "turn_cancel_affected_input" {
        /// Turn `?2` of session `?1`'s recorded dispositions, in the order the
        /// turn observed them.
        ///
        /// `ordinal` is the order and it is a total one — it is the third
        /// component of the primary key — so no tie needs breaking.
        select_by_turn = "SELECT input_id, input_json, disposition
             FROM turn_cancel_affected_inputs
             WHERE session_id = ?1 AND turn_id = ?2
             ORDER BY ordinal ASC";

        /// The next ordinal is computed inside the insert rather than read
        /// first: the caller already holds the cancel-request row's lock, and
        /// deriving it in one statement is what keeps the ordinal allocation
        /// and the append in a single round trip.
        append_at_next_ordinal = "INSERT INTO turn_cancel_affected_inputs (
                 session_id, turn_id, ordinal, input_id, disposition, input_json
             )
             VALUES (
                 ?1, ?2,
                 (SELECT COALESCE(MAX(ordinal) + 1, 0)
                    FROM turn_cancel_affected_inputs
                   WHERE session_id = ?1 AND turn_id = ?2),
                 ?3, ?4, ?5
             )";
    }
}

lash_store_sql::statements! {
    /// `turn_cancel_closure_authorizations` statements only PostgreSQL issues.
    pub(crate) struct ClosureAuthorizationPostgresStatements @ "turn_cancel_closure_authorization" {
        /// Turn `?2` of session `?1`'s pinned closure, locked for the caller's
        /// transaction.
        select_by_turn = "SELECT authorization_json FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1 AND turn_id = ?2 FOR UPDATE";
    }
}

lash_store_sql::statements! {
    /// `turn_cancel_retired_scopes` statements only PostgreSQL issues.
    pub(crate) struct RetiredScopePostgresStatements @ "turn_cancel_retired_scope" {
        /// Retire scope `?1`, keeping an existing retirement.
        insert_new = "INSERT INTO turn_cancel_retired_scopes (scope_id) VALUES (?1)
             ON CONFLICT DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// `tool_intent_submissions` statements only PostgreSQL issues.
    pub(crate) struct ToolIntentSubmissionPostgresStatements @ "tool_intent_submission" {
        /// Record submission `?1`, keeping an existing one under the same
        /// replay key: a replayed submission is the point of the ledger.
        insert_new = "INSERT INTO tool_intent_submissions (
                 replay_key, session_id, execution_scope_id, tool_call_id,
                 intent_index, kind, payload_hash, submission_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (replay_key) DO NOTHING";

        /// The submission recorded under replay key `?1`, locked for the
        /// caller's transaction: completing one is a read-then-write.
        select_by_replay_key_for_update = "SELECT submission_json FROM tool_intent_submissions
             WHERE replay_key = ?1 FOR UPDATE";
    }
}
