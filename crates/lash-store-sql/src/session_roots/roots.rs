//! `session_roots`: one row per `(session, root)` a drive admitted work
//! under, holding the root's terminal evidence once it has one. The row lives
//! until its session is deleted.

/// The table's unprefixed name.
pub const TABLE: &str = "session_roots";

/// The terminal evidence columns, in the order both backends decode them.
pub const TERMINAL_COLUMNS: &str =
    "terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms";

/// The key columns alone: opening a root writes its identity and nothing
/// else, so its terminal columns stay NULL until an end writes them.
pub const KEY_COLUMNS: &str = "session_id, root";

crate::statements! {
    /// `session_roots` statements both backends issue verbatim.
    pub struct SessionRootStatements @ "session_root" {
        /// Open root `?2` of session `?1` if it has no row yet.
        insert_open = "INSERT INTO session_roots (session_id, root) VALUES (?1, ?2)
             ON CONFLICT (session_id, root) DO NOTHING";

        /// The recorded result of root `?2`'s claim (NULL until its claim
        /// transaction commits), while the claim's head input `?3` is still
        /// undelivered. Once the head settles, is cancelled or is pruned,
        /// there is nothing left to replay and no row comes back.
        select_claim_result = "SELECT claim_result_json FROM session_roots
             WHERE session_id = ?1 AND root = ?2
               AND EXISTS (
                   SELECT 1 FROM pending_turn_inputs
                   WHERE session_id = ?1
                     AND input_id = ?3
                     AND {{nonterminal_turn_input_state(state)}}
               )";

        /// Record the claim after opening its root, in the claim transaction.
        write_claim_result = "UPDATE session_roots SET claim_result_json = ?3
             WHERE session_id = ?1 AND root = ?2 AND claim_result_json IS NULL";

        /// The terminal evidence of root `?2` of session `?1`: all four
        /// columns NULL while the root has none.
        select_terminal = "SELECT terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms
             FROM session_roots
             WHERE session_id = ?1 AND root = ?2";

        /// Write root `?2`'s terminal evidence (kind `?3`, cause `?4`, head
        /// revision `?5`, instant `?6`) unless it already has one: the
        /// caller decided the write against the stored evidence in the same
        /// transaction, and a zero row count means another writer won.
        write_terminal = "UPDATE session_roots
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6
             WHERE session_id = ?1 AND root = ?2 AND terminal_kind IS NULL";

        /// The roots of session `?1` without terminal evidence, in root
        /// order: what its close ends.
        select_open_roots = "SELECT root FROM session_roots
             WHERE session_id = ?1 AND terminal_kind IS NULL
             ORDER BY root";

        /// Every root of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_roots WHERE session_id = ?1";
    }
}

/// Rendered statements used by a parked-root control transaction.
pub struct RootVerbStatements {
    pub bound_inputs: crate::Rendered,
    pub rebind: crate::Rendered,
    pub unbind: crate::Rendered,
    pub set_kind: crate::Rendered,
    pub raise_epoch: crate::Rendered,
    pub input: crate::Rendered,
    pub release_inputs: crate::Rendered,
    pub release_batches: crate::Rendered,
    pub delete_batch_items: crate::Rendered,
    pub delete_batch: crate::Rendered,
    pub sessions: crate::Rendered,
    pub intents: crate::Rendered,
}

impl RootVerbStatements {
    /// Render each statement through its table owner.
    #[must_use]
    pub fn render(dialect: crate::Dialect) -> Self {
        let group0 = crate::session_roots::root_inputs::RootInputVerbStatements::render(dialect);
        let group1 = crate::session_roots::control_intents::ControlVerbStatements::render(dialect);
        let group2 = crate::session::meta::MetaRootVerbStatements::render(dialect);
        let group3 =
            crate::turn_ingress::pending_inputs::PendingRootVerbStatements::render(dialect);
        let group4 = crate::turn_ingress::queued_batches::BatchRootVerbStatements::render(dialect);
        let group5 = crate::turn_ingress::queued_items::ItemRootVerbStatements::render(dialect);
        Self {
            bound_inputs: group0.bound_inputs,
            rebind: group0.rebind,
            unbind: group0.unbind,
            set_kind: group1.set_kind,
            intents: group1.intents,
            raise_epoch: group2.raise_epoch,
            sessions: group2.sessions,
            input: group3.input,
            release_inputs: group3.release_inputs,
            release_batches: group4.release_batches,
            delete_batch: group4.delete_batch,
            delete_batch_items: group5.delete_batch_items,
        }
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str = "obligation_id, obligation_attempts, session_id, root";

/// A stalled obligation as an operator lists it: [`OBLIGATION_CLAIM_COLUMNS`]
/// with the stall's reason, last error and instant before the key.
pub const OBLIGATION_STALLED_COLUMNS: &str = "obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, session_id, root";

crate::statements! {
    /// `session_roots` obligation statements (ADR 0109): a terminal root owes its scope close. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token.
    pub struct SessionRootObligationStatements @ "session_root" {
        /// Arm the row keyed `?1`, `?2` as obligation `?3`, due at
        /// `?4`, if it owes nothing.
        obligation_arm = "UPDATE session_roots
             SET obligation_id = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE session_id = ?1 AND root = ?2 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM session_roots
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE session_roots
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, session_id, root";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        obligation_claim = "UPDATE session_roots
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'due'
             RETURNING obligation_id, obligation_attempts, session_id, root";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE session_roots
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE session_roots
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE session_roots
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE session_roots
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, session_id, root
             FROM session_roots
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM session_roots WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state.
        obligation_select_state = "SELECT obligation_state FROM session_roots WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for SessionRootObligationStatements {
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
            select_state: &self.obligation_select_state,
        }
    }
}

crate::statements! {
    /// `session_roots` reads of a session's two-phase delete (ADR 0109 §4).
    pub struct SessionRootCleanupStatements @ "session_root" {
        /// How many of session `?1`'s roots owe a scope close not yet
        /// delivered: due, claimed, or stalled.
        count_undelivered_scope_close = "SELECT COUNT(*) FROM session_roots
             WHERE session_id = ?1 AND obligation_state IN ('due', 'claimed', 'stalled')";
    }
}
