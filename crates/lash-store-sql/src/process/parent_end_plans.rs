//! `parent_end_plans`: one row per ended parent scope.
//!
//! Keyed by the scope rather than by a process row: a turn-scoped parent has
//! no process row at all, and a process-scoped parent's row may be pruned
//! before its children settle. The `(parent_kind, parent_id)` pair is the
//! scope's collision-free index projection — equality and keyset ordering
//! only, never parsed back. `parent_payload` is the versioned typed parent:
//! the authority a reader decodes, and the fact that still answers "which
//! scope ended" after the parent's own row is pruned.

/// The table's unprefixed name.
pub const TABLE: &str = "parent_end_plans";

/// Every column, in insert order. `settled_at_ms` and the obligation
/// columns are absent: a plan is recorded unsettled and armed by the record.
pub const INSERT_COLUMNS: &str = "parent_kind, parent_id, parent_payload, ended_at_ms";

crate::statements! {
    /// `parent_end_plans` statements both backends issue verbatim.
    pub struct ParentEndPlanStatements @ "parent_end_plan" {
        /// Record that scope `?1` / `?2` ended at `?4`, keeping the first
        /// stamp if one is already recorded. `?3` is the scope's versioned
        /// typed payload.
        ///
        /// Shared conflict clause: both backends need it, because the terminal
        /// append that records the plan can be replayed.
        insert_if_absent = "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (parent_kind, parent_id) DO NOTHING";

        exists = "SELECT 1 FROM parent_end_plans WHERE parent_kind = ?1 AND parent_id = ?2";

        /// Scope `?1` / `?2`'s typed payload, two instants and obligation.
        select_stamps = "SELECT parent_payload, ended_at_ms, settled_at_ms, obligation_id, obligation_state
             FROM parent_end_plans
             WHERE parent_kind = ?1 AND parent_id = ?2";

        /// Settle scope `?1` / `?2` at `?3`, once.
        settle = "UPDATE parent_end_plans SET settled_at_ms = ?3
             WHERE parent_kind = ?1 AND parent_id = ?2 AND settled_at_ms IS NULL";

        /// The settle that applied the plan also delivers a `due` obligation
        /// the row owes (ADR 0109): its delivery is the application itself.
        /// A `claimed` row's claim owns its settle, and a `stalled` row keeps
        /// its stall for the operator.
        obligation_apply_delivered = "UPDATE parent_end_plans
             SET obligation_state = 'delivered', obligation_due_at_ms = NULL,
                 obligation_last_error = NULL, obligation_settled_at_ms = ?3
             WHERE parent_kind = ?1 AND parent_id = ?2 AND obligation_state = 'due'";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str =
    "obligation_id, obligation_attempts, parent_kind, parent_id";

crate::statements! {
    /// `parent_end_plans` obligation statements (ADR 0109): a closed scope's plan owes its children's cancels. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token.
    pub struct ParentEndPlanObligationStatements @ "parent_end_plan" {
        /// Arm the row keyed `?1`, `?2` as obligation `?3`, due at
        /// `?4`, if it owes nothing.
        obligation_arm = "UPDATE parent_end_plans
             SET obligation_id = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE parent_kind = ?1 AND parent_id = ?2 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM parent_end_plans
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE parent_end_plans
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, parent_kind, parent_id";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        obligation_claim = "UPDATE parent_end_plans
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'due'
             RETURNING obligation_id, obligation_attempts, parent_kind, parent_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE parent_end_plans
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE parent_end_plans
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE parent_end_plans
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE parent_end_plans
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, parent_kind, parent_id
             FROM parent_end_plans
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM parent_end_plans WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM parent_end_plans WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for ParentEndPlanObligationStatements {
    fn obligation_sql(&self) -> crate::obligation::ObligationSql<'_> {
        crate::obligation::ObligationSql {
            key_columns: 2,
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
            select_standing: &self.obligation_select_standing,
        }
    }
}

crate::statements! {
    /// `parent_end_plans` reads of a session's two-phase delete (ADR 0109 §4).
    pub struct ParentEndPlanCleanupStatements @ "parent_end_plan" {
        /// How many plans of the scopes one session owns owe children's
        /// cancels not yet delivered: due, claimed, or stalled. `?1` is the
        /// session scope's projection; `[?2, ?3)` and `[?4, ?5)` are the
        /// keyset ranges of its turns' and its queue drains' projections.
        count_undelivered_for_session = "SELECT COUNT(*) FROM parent_end_plans
             WHERE obligation_state IN ('due', 'claimed', 'stalled')
               AND ((parent_kind = 'session' AND parent_id = ?1)
                 OR (parent_kind = 'turn' AND parent_id >= ?2 AND parent_id < ?3)
                 OR (parent_kind = 'queue_drain' AND parent_id >= ?4 AND parent_id < ?5))";
    }
}
