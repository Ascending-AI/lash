//! `trigger_deliveries`: one row per subscription an occurrence fired at.
//!
//! The row freezes the subscription as it stood when the firing reserved the
//! delivery (`subscription_snapshot_json`), so a later edit to the
//! subscription cannot retroactively change what was delivered.
//!
//! `process_id` is the delivery's binding: `NULL` while the reservation's
//! start has not completed, and the minted id of the process its start key
//! registered once it has (ADR 0107). A bound delivery is never started
//! again.
//!
//! The row is also the delivery's `TriggerDelivery` obligation (ADR 0109,
//! ADR 0021's FIG-4090 amendment): the reserving insert arms it and the
//! binding update delivers it, so a reservation whose start a crash cut
//! short stays owed until the relay starts and binds it.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_deliveries";

/// Every column the reserving insert writes, in insert order.
pub const INSERT_COLUMNS: &str =
    "occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms,
                obligation_id, obligation_state, obligation_due_at_ms";

/// SQLite's spelling of the owning session of a frozen subscription snapshot.
///
/// Not a column list: an expression the retention sweep projects to enumerate
/// the sessions whose deliveries are still outstanding. It is named here for
/// the same reason
/// [`super::occurrences::RECLAMATION_SCOPE_COUNTS_SQLITE`] is — the comma
/// inside `json_extract`'s argument list makes it a multi-column projection as
/// far as the gate can tell, and naming it puts the two backends' spellings
/// where they can be compared.
pub const SESSION_OWNER_SCOPE_SQLITE: &str = "'session:' || json_extract(
                                    subscription_snapshot_json,
                                    '$.owner_scope.session_id'
                                )";

crate::statements! {
    /// `trigger_deliveries` statements both backends issue verbatim.
    pub struct DeliveryStatements @ "trigger_delivery" {
        /// Reserve the delivery of occurrence `?1` to subscription `?2`,
        /// freezing the subscription as `?5`. The reservation starts unbound
        /// and owes its start: its obligation `?7` is armed due at `?6`.
        insert = "INSERT INTO trigger_deliveries (
                occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms,
                obligation_id, obligation_state, obligation_due_at_ms
             )
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, 'due', ?6)";

        /// Bind delivery `?1`/`?2` to the process `?3` its start registered,
        /// which delivers its obligation at `?4` whatever state it was in: a
        /// relay's claim on it then settles `ClaimLost`. Matches only an
        /// unbound row or a row already bound to `?3`, so a caller reads zero
        /// affected rows as a conflicting binding; a repeated bind keeps the
        /// first delivery's settlement time.
        bind_process = "UPDATE trigger_deliveries
             SET process_id = ?3, obligation_state = 'delivered',
                 obligation_claim_token = NULL, obligation_due_at_ms = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = COALESCE(obligation_settled_at_ms, ?4)
             WHERE occurrence_id = ?1 AND subscription_id = ?2
               AND (process_id IS NULL OR process_id = ?3)";

        /// The reservations occurrence `?1` already holds, for an ingress that
        /// found the occurrence durable and is reporting it again.
        select_snapshots_by_occurrence = "SELECT process_id, created_at_ms, subscription_snapshot_json
             FROM trigger_deliveries
             WHERE occurrence_id = ?1";

        /// Every process a bound delivery started.
        select_distinct_process_ids = "SELECT DISTINCT process_id
             FROM trigger_deliveries
             WHERE process_id IS NOT NULL
             ORDER BY process_id ASC";

        /// Every bound delivery's identity, for a retention pass to compare
        /// against the process registry. An unbound reservation is not a
        /// candidate: its start still owes the delivery a process.
        select_retention_candidates = "SELECT occurrence_id, subscription_id, process_id
             FROM trigger_deliveries
             WHERE process_id IS NOT NULL
             ORDER BY occurrence_id ASC, subscription_id ASC";

        /// Every reservation in the store.
        ///
        /// The four listings below were one `format!` per backend over a
        /// `where_clause` argument. They are four statements now, one per
        /// caller, and this one carries no predicate at all — it used to say
        /// `WHERE 1 = 1` on SQLite and `WHERE TRUE` on PostgreSQL, which is
        /// the same absence spelled two ways.
        list_all = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// Every reservation occurrence `?1` caused.
        list_by_occurrence_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.occurrence_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// Every reservation subscription `?1` received.
        list_by_subscription_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.subscription_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// The reservation that started process `?1`.
        list_by_process_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.process_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";
    }
}

crate::statements! {
    /// `trigger_deliveries` obligation statements (ADR 0109): a reserved
    /// delivery owes its one process, started and bound. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token. The reserving insert arms the row and the
    /// binding update delivers it, so `arm` only serves the leader's repair.
    pub struct DeliveryObligationStatements @ "trigger_delivery" {
        /// Arm the row keyed `?1`, `?2` as obligation `?3`, due at `?4`, if it
        /// owes nothing and is still unbound.
        obligation_arm = "UPDATE trigger_deliveries
             SET obligation_id = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE occurrence_id = ?1 AND subscription_id = ?2
               AND obligation_state IS NULL AND process_id IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM trigger_deliveries
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE trigger_deliveries
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, occurrence_id, subscription_id";

        /// Claim obligation `?1` under token `?2` until `?3`: a `due` row
        /// whatever its backoff (a producer's own immediate attempt), or a
        /// claim `?2` already holds, its claimant re-deriving it after an
        /// interruption, which keeps its attempt count.
        obligation_claim = "UPDATE trigger_deliveries
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + CASE WHEN obligation_state = 'due' THEN 1 ELSE 0 END, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND (obligation_state = 'due'
                  OR (obligation_state = 'claimed' AND obligation_claim_token = ?2))
             RETURNING obligation_id, obligation_attempts, occurrence_id, subscription_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE trigger_deliveries
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE trigger_deliveries
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE trigger_deliveries
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE trigger_deliveries
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, occurrence_id, subscription_id
             FROM trigger_deliveries
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM trigger_deliveries WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM trigger_deliveries WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for DeliveryObligationStatements {
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
