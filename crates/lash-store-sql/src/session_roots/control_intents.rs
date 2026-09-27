//! `control_intents`: an operator's verb on a parked root, or a session's
//! close, as a versioned record (ADR 0104 O4). The store half of the intent
//! commits in the transaction that inserts the row; `state` tracks the engine
//! half. A `close_session` row outlives its session: it is the deletion
//! tombstone a deleted session's roots answer from.

/// The table's unprefixed name.
pub const TABLE: &str = "control_intents";

/// Every column a row decoder reads, in the order both backends index.
pub const ROW_COLUMNS: &str =
    "intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref";

/// What recording an intent writes: [`ROW_COLUMNS`] minus the allocated
/// `intent_id`, plus the `kind`/`state` tags beside their JSON bodies so a
/// reader filters on the tag without decoding.
pub const INSERT_COLUMNS: &str =
    "session_id, format, kind, kind_json, state, state_json, attempts, created_at_ms, engine_ref";

crate::statements! {
    /// `control_intents` statements both backends issue verbatim.
    pub struct ControlIntentStatements @ "control_intent" {
        /// Open intents (pending, or failed and retryable) after id `?1`, in
        /// id order, at most `?2`.
        select_open_after = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE intent_id > ?1 AND state IN ('pending', 'failed_retryable')
             ORDER BY intent_id
             LIMIT ?2";

        /// Session `?1`'s `close_session` intent: its deletion tombstone.
        select_close_session = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE session_id = ?1 AND kind = 'close_session'
             ORDER BY intent_id
             LIMIT 1";

        /// Intent `?1`.
        select_by_id = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE intent_id = ?1";

        /// Session `?1`'s open verbs (pending, or failed and retryable), in
        /// id order: what its close supersedes.
        select_open_verbs_by_session = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE session_id = ?1 AND kind <> 'close_session'
               AND state IN ('pending', 'failed_retryable')
             ORDER BY intent_id";

        /// Record a new intent of session `?1` (format `?2`, kind `?3` with
        /// JSON `?4`, state `?5` with JSON `?6`, instant `?7`, engine handle
        /// `?8`), answering its allocated id.
        insert = "INSERT INTO control_intents
                 (session_id, format, kind, kind_json, state, state_json, attempts, created_at_ms, engine_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)
             RETURNING intent_id";

        /// Move intent `?1` to state `?2` (JSON `?3`) at attempt count `?4`,
        /// if it is still at state JSON `?5` and attempt count `?6`: a
        /// compare-and-set, so zero rows means another writer moved it first.
        update_state = "UPDATE control_intents
             SET state = ?2, state_json = ?3, attempts = ?4
             WHERE intent_id = ?1 AND state_json = ?5 AND attempts = ?6";

        /// Every intent of session `?1` but its `close_session` tombstone:
        /// the part of its deletion that forgets the verbs.
        delete_verbs_by_session = "DELETE FROM control_intents
             WHERE session_id = ?1 AND kind <> 'close_session'";
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct ControlVerbStatements @ "control_intent" {
        set_kind = "UPDATE control_intents SET kind_json = ?2 WHERE intent_id = ?1";
        intents = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
            FROM control_intents WHERE intent_id > ?1 ORDER BY intent_id LIMIT ?2";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str = "obligation_id, obligation_attempts, intent_id";

/// A stalled obligation as an operator lists it: [`OBLIGATION_CLAIM_COLUMNS`]
/// with the stall's reason, last error and instant before the key.
pub const OBLIGATION_STALLED_COLUMNS: &str = "obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, intent_id";

crate::statements! {
    /// `control_intents` obligation statements (ADR 0109): an intent owes its engine half and its follow-on drive. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token.
    pub struct ControlIntentObligationStatements @ "control_intent" {
        /// Arm the row keyed `?1` as obligation `?2`, due at
        /// `?3`, if it owes nothing.
        obligation_arm = "UPDATE control_intents
             SET obligation_id = ?2, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?3, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE intent_id = ?1 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM control_intents
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE control_intents
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, intent_id";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        obligation_claim = "UPDATE control_intents
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'due'
             RETURNING obligation_id, obligation_attempts, intent_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE control_intents
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE control_intents
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE control_intents
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE control_intents
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, intent_id
             FROM control_intents
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM control_intents WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state.
        obligation_select_state = "SELECT obligation_state FROM control_intents WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for ControlIntentObligationStatements {
    fn obligation_sql(&self) -> crate::obligation::ObligationSql<'_> {
        crate::obligation::ObligationSql {
            key_columns: 1,
            arm: &self.obligation_arm,
            select_due: &self.obligation_select_due,
            claim_due_row: &self.obligation_claim_due_row,
            claim: &self.obligation_claim,
            settle_delivered: &self.obligation_settle_delivered,
            settle_retry: &self.obligation_settle_retry,
            settle_stall: &self.obligation_settle_stall,
            rearm: &self.obligation_rearm,
            select_stalled: &self.obligation_select_stalled,
            count_stalled: &self.obligation_count_stalled,
            select_state: &self.obligation_select_state,
        }
    }
}
