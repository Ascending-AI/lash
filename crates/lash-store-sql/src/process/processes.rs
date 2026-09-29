//! `processes`: one row per registered process.
//!
//! The row is wide and its authoritative form is `record_json`; the other
//! columns are the indexed projection a non-terminal scan, a retention sweep or a
//! change feed filters on. No caller reads the whole row, so this module
//! declares the **named** projections that exist and nothing else.
//!
//! `status` carries domain vocabulary (`lash_core::ProcessStatus`). A
//! statement here names a lifecycle partition as a `{{term(column)}}` token
//! and never spells it.

/// The table's unprefixed name.
pub const TABLE: &str = "processes";

/// Every column, in insert order. The only statements that name all of them
/// are the two backends' registration inserts.
pub const INSERT_COLUMNS: &str = "process_id, start_key, originator_id,
                wake_session_id, identity_kind, identity_label, created_at_ms, updated_at_ms,
                last_event_sequence, change_seq, status, lifetime_scope_kind, lifetime_scope_id,
                lifetime, cancel_requested_at_ms, record_json, consumer_hold_key,
                consumer_hold_scope_kind, consumer_hold_scope_id";

/// What the change feed reports for a live row.
///
/// The feed unions live rows with tombstones, so both arms carry the same
/// three result columns: the sequence the caller resumes from, the kind that
/// says which arm produced the row, and the payload. The literal `'upsert'` is
/// the arm's name, not a lifecycle label.
pub const CHANGE_FEED_UPSERT_COLUMNS: &str = "change_seq, 'upsert' AS kind, record_json AS payload";

crate::statements! {
    /// `processes` statements both backends issue verbatim.
    pub struct ProcessStatements @ "process" {
        /// The stored record for `?1`.
        select_record_json_by_id = "SELECT record_json FROM processes WHERE process_id = ?1";

        /// The retained process registered under start key `?1`, if any: the
        /// idempotency half of a registration (ADR 0107).
        select_record_json_by_start_key = "SELECT record_json FROM processes WHERE start_key = ?1";

        exists_by_id = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)";

        /// The session `?1`'s wakes are delivered to, if any.
        select_wake_session_id = "SELECT wake_session_id FROM processes WHERE process_id = ?1";

        /// Retarget `?1`'s wake session to `?2`.
        set_wake_session_id = "UPDATE processes SET wake_session_id = ?2 WHERE process_id = ?1";

        clear_wake_session_for_session = "UPDATE processes SET wake_session_id = NULL WHERE wake_session_id = ?1";

        /// Release the consumer hold `?2` holds on `?1` (ADR 0116 §3.6): a
        /// no-op when the row carries another hold or none.
        release_consumer_hold = "UPDATE processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL
             WHERE process_id = ?1 AND consumer_hold_key = ?2";

        /// Release every consumer hold owned by the scope `(?1, ?2)`: the
        /// scope's close ends every wait its calls still hold.
        release_consumer_holds_owned_by = "UPDATE processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL
             WHERE consumer_hold_scope_kind = ?1 AND consumer_hold_scope_id = ?2";

        /// The identity columns are absent because none of them is mutable.
        /// `?8`/`?9` are the parked projection: the live park's `since_ms`
        /// and reason code, both `NULL` while the process is not parked.
        /// `?10` is the retired executable generation a `retired_generation`
        /// park names, `NULL` for any other park (FIG-3571). `?11` is the
        /// build generation of the parked checkpoint, `NULL` when the park's
        /// writer records none (FIG-3795).
        update_mutable_columns = "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, status = ?4,
                 last_event_sequence = ?5, cancel_requested_at_ms = ?6, record_json = ?7,
                 parked_since_ms = ?8, parked_reason_code = ?9, park_executable_generation = ?10,
                 park_build_generation = ?11
             WHERE process_id = ?1";

        /// Record the build generation of the build that admitted the
        /// process's current segment (FIG-3795 S2): written in the same
        /// transaction as the segment's start stamp — `first_started` for
        /// segment 0, the retained handover's start marker past it.
        set_segment_generation = "UPDATE processes SET segment_generation = ?2
             WHERE process_id = ?1";

        /// The live processes whose current segment build generation `?1`
        /// admitted (FIG-3799), over the live-generation partial index.
        count_live_by_segment_generation = "SELECT COUNT(*) FROM processes
             WHERE segment_generation = ?1 AND {{live_process_status(status)}}";

        /// The live processes of segment generation `?1` strictly after key
        /// `?2` (`''` from the start), at most `?3`, in key order: the page
        /// the drain's hand-over wakes (FIG-3799).
        list_live_by_segment_generation = "SELECT process_id FROM processes
             WHERE segment_generation = ?1 AND {{live_process_status(status)}}
               AND process_id > ?2
             ORDER BY process_id
             LIMIT ?3";

        /// The parked processes whose parked checkpoint build generation `?1`
        /// wrote (FIG-3799), over the park build-generation index.
        count_parked_by_build_generation = "SELECT COUNT(*) FROM processes
             WHERE park_build_generation = ?1 AND parked_since_ms IS NOT NULL";

        /// Live process parks per reason code, with each code's oldest
        /// `since_ms`, over the parked projection's partial index.
        summarize_parked = "SELECT parked_reason_code, COUNT(*), MIN(parked_since_ms)
             FROM processes
             WHERE parked_since_ms IS NOT NULL
             GROUP BY parked_reason_code";

        /// Live retired-generation process parks grouped by the generation
        /// their start recorded (FIG-3571): read off the projected, indexed
        /// `park_executable_generation` column, never the record.
        count_retired_parks_by_executable_generation = "SELECT park_executable_generation, COUNT(*)
             FROM processes
             WHERE park_executable_generation IS NOT NULL
             GROUP BY park_executable_generation";

        /// Every live process, whole. The unpaged read behind the in-memory
        /// registry rebuild; the paged non-terminal scans are backend-only because
        /// SQLite pins them to a partial index by name.
        collect_non_terminal_records = "SELECT record_json FROM processes
                         WHERE {{live_process_status(status)}}
                         ORDER BY process_id ASC";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str = "obligation_id, obligation_attempts, process_id";

crate::statements! {
    /// `processes` obligation statements (ADR 0109): a terminal process owes its terminal publication. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token.
    pub struct ProcessObligationStatements @ "process" {
        /// Arm the row keyed `?1` as obligation `?2`, due at
        /// `?3`, if it owes nothing.
        obligation_arm = "UPDATE processes
             SET obligation_id = ?2, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?3, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE process_id = ?1 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM processes
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE processes
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, process_id";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        obligation_claim = "UPDATE processes
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'due'
             RETURNING obligation_id, obligation_attempts, process_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE processes
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE processes
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE processes
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE processes
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, process_id
             FROM processes
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM processes WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM processes WHERE obligation_id = ?1";

        /// Settle process `?1`'s terminal publication delivered at `?2`,
        /// whatever claim holds it: the engine published the terminal itself
        /// (ADR 0109 §1.4, a delivery that settles its own row). A relay
        /// holding a claim then settles `ClaimLost`.
        obligation_settle_published = "UPDATE processes
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = NULL,
                 obligation_last_error = NULL, obligation_settled_at_ms = ?2
             WHERE process_id = ?1 AND obligation_state IN ('due', 'claimed', 'stalled')";

        /// Process `?1`'s terminal publication obligation: its id and state,
        /// or no row while it owes none.
        obligation_select_by_process = "SELECT obligation_id, obligation_state FROM processes
             WHERE process_id = ?1 AND obligation_id IS NOT NULL";
    }
}

impl crate::obligation::ObligationStatementSet for ProcessObligationStatements {
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
            select_standing: &self.obligation_select_standing,
        }
    }
}

crate::statements! {
    /// `processes` start-obligation statements (ADR 0109): a registered process owes its first engine run. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token.
    pub struct ProcessStartObligationStatements @ "process" {
        /// Arm the row keyed `?1` as obligation `?2`, due at
        /// `?3`, if it owes nothing.
        start_obligation_arm = "UPDATE processes
             SET start_obligation_id = ?2, start_obligation_state = 'due', start_obligation_attempts = 0,
                 start_obligation_due_at_ms = ?3, start_obligation_claim_token = NULL,
                 start_obligation_stall_reason = NULL, start_obligation_last_error = NULL,
                 start_obligation_settled_at_ms = NULL
             WHERE process_id = ?1 AND start_obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        start_obligation_select_due = "SELECT start_obligation_id FROM processes
             WHERE start_obligation_state IN ('due', 'claimed') AND start_obligation_due_at_ms <= ?1
             ORDER BY start_obligation_due_at_ms, start_obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        start_obligation_claim_due_row = "UPDATE processes
             SET start_obligation_state = 'claimed', start_obligation_claim_token = ?2,
                 start_obligation_attempts = start_obligation_attempts + 1, start_obligation_due_at_ms = ?3
             WHERE start_obligation_id = ?1 AND start_obligation_state IN ('due', 'claimed')
               AND start_obligation_due_at_ms <= ?4
             RETURNING start_obligation_id, start_obligation_attempts, process_id";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        start_obligation_claim = "UPDATE processes
             SET start_obligation_state = 'claimed', start_obligation_claim_token = ?2,
                 start_obligation_attempts = start_obligation_attempts + 1, start_obligation_due_at_ms = ?3
             WHERE start_obligation_id = ?1 AND start_obligation_state = 'due'
             RETURNING start_obligation_id, start_obligation_attempts, process_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        start_obligation_settle_delivered = "UPDATE processes
             SET start_obligation_state = 'delivered', start_obligation_claim_token = NULL,
                 start_obligation_due_at_ms = NULL, start_obligation_last_error = NULL,
                 start_obligation_settled_at_ms = ?3
             WHERE start_obligation_id = ?1 AND start_obligation_state = 'claimed'
               AND start_obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        start_obligation_settle_retry = "UPDATE processes
             SET start_obligation_state = 'due', start_obligation_claim_token = NULL,
                 start_obligation_due_at_ms = ?3, start_obligation_last_error = ?4
             WHERE start_obligation_id = ?1 AND start_obligation_state = 'claimed'
               AND start_obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        start_obligation_settle_stall = "UPDATE processes
             SET start_obligation_state = 'stalled', start_obligation_claim_token = NULL,
                 start_obligation_due_at_ms = NULL, start_obligation_stall_reason = ?3,
                 start_obligation_last_error = ?4, start_obligation_settled_at_ms = ?5
             WHERE start_obligation_id = ?1 AND start_obligation_state = 'claimed'
               AND start_obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        start_obligation_rearm = "UPDATE processes
             SET start_obligation_state = 'due', start_obligation_attempts = 0, start_obligation_due_at_ms = ?2,
                 start_obligation_stall_reason = NULL, start_obligation_settled_at_ms = NULL
             WHERE start_obligation_id = ?1 AND start_obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        start_obligation_select_stalled = "SELECT start_obligation_id, start_obligation_attempts, start_obligation_stall_reason, start_obligation_last_error, start_obligation_settled_at_ms, process_id
             FROM processes
             WHERE start_obligation_state = 'stalled' AND start_obligation_id > ?1
             ORDER BY start_obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        start_obligation_count_stalled = "SELECT COUNT(*) FROM processes WHERE start_obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        start_obligation_select_standing = "SELECT start_obligation_state, start_obligation_attempts FROM processes WHERE start_obligation_id = ?1";

    }
}

impl crate::obligation::ObligationStatementSet for ProcessStartObligationStatements {
    fn obligation_sql(&self) -> crate::obligation::ObligationSql<'_> {
        crate::obligation::ObligationSql {
            key_columns: 1,
            arm: &self.start_obligation_arm,
            select_due: &self.start_obligation_select_due,
            claim_due_row: &self.start_obligation_claim_due_row,
            claim: &self.start_obligation_claim,
            settle_delivered: &self.start_obligation_settle_delivered,
            settle_retry: &self.start_obligation_settle_retry,
            settle_stall: &self.start_obligation_settle_stall,
            rearm: &self.start_obligation_rearm,
            select_stalled: &self.start_obligation_select_stalled,
            count_stalled: &self.start_obligation_count_stalled,
            select_standing: &self.start_obligation_select_standing,
        }
    }
}
