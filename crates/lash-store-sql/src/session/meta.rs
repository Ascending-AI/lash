//! `session_meta`: one row per session identity, its recorded lineage and the
//! causal columns that say what created it.

/// The table's unprefixed name.
pub const TABLE: &str = "session_meta";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, created_at_ms, last_commit_at_ms, owning_process_id";

/// The stored relation, as `StoredRelation` decodes it positionally.
///
/// The full row minus `session_state_version`, `created_at_ms` and
/// `last_commit_at_ms`: the version marker is read on its own by the admission
/// path, and the two timestamps are catalog reporting rather than relation
/// identity. Both backends decode this projection by position, so its order is
/// load-bearing.
pub const RELATION_COLUMNS: &str = "session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id";

/// The SQLite catalog projection: [`RELATION_COLUMNS`] qualified by the
/// catalog's `meta` alias, with the two catalog timestamps, the joined head
/// revision and the deleted flag the union's other arm supplies as `1`.
///
/// It is its own list because the catalog reads columns the relation read does
/// not — a session's creation instant, its last commit and its head revision
/// are what a listing sorts and reports — and because every column is
/// alias-qualified against the `LEFT JOIN` beside it.
pub const CATALOG_COLUMNS_SQLITE: &str =
    "meta.session_id, meta.relation_kind, meta.parent_session_id,
                    meta.caused_by_kind,
                    meta.caused_by_session_id, meta.caused_by_turn_id,
                    meta.caused_by_effect_id, meta.caused_by_call_id,
                    meta.caused_by_process_id, meta.caused_by_process_event_sequence,
                    meta.caused_by_occurrence_id, meta.caused_by_subscription_id,
                    meta.caused_by_subscription_incarnation,
                    meta.caused_by_subscription_revision, meta.caused_by_node_id,
                    meta.source_session_id, meta.source_node_id,
                    meta.created_at_ms,
                    meta.last_commit_at_ms, COALESCE(head.head_revision, 0), 0 AS deleted";

/// What a session's permanent deletion evidence is copied from.
///
/// Five metadata columns plus the head revision the session reached, joined
/// from the head table. It is its own list because the deleted set records a
/// session's final shape rather than its relation: no causal column survives
/// deletion, and the head revision is not a `session_meta` column at all.
pub const DELETED_EVIDENCE_COLUMNS_SQLITE: &str =
    "meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                            COALESCE(head.head_revision, 0), meta.relation_kind,
                            meta.parent_session_id";

/// The PostgreSQL spelling of [`DELETED_EVIDENCE_COLUMNS_SQLITE`], which
/// differs only in the head table's alias, because the head table itself is
/// spelled differently (ADR 0098).
pub const DELETED_EVIDENCE_COLUMNS_POSTGRES: &str =
    "meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0), meta.relation_kind,
                    meta.parent_session_id";

crate::statements! {
    /// `session_meta` statements both backends issue verbatim.
    pub struct SessionMetaStatements @ "session_meta" {
        /// Session `?1`'s drive epoch, the admission that last raised it, the
        /// start marker of the execution that sealed that admission, the
        /// control intent the session is closing under, and whether a cancel
        /// or fork still owes its engine half (pending, or failed and
        /// retryable). A verb the engine refused for good owes nothing more:
        /// it is surfaced on the intent, and the session drives on.
        select_drive_epoch = "SELECT drive_epoch, drive_admission_id, drive_root_start, closing_intent,
            EXISTS (SELECT 1 FROM control_intents WHERE control_intents.session_id = session_meta.session_id
                AND kind IN ('cancel', 'fork') AND state IN ('pending', 'failed_retryable'))
            FROM session_meta WHERE session_id = ?1";

        /// The seal's compare-and-set: raise session `?1`'s drive epoch from
        /// `?2` to `?3` under admission `?4`, sealed by the execution whose
        /// start marker is `?5`. Zero rows means the epoch moved.
        seal_drive_epoch = "UPDATE session_meta
             SET drive_epoch = ?3, drive_admission_id = ?4, drive_root_start = ?5
             WHERE session_id = ?1 AND drive_epoch = ?2";

        /// Close session `?1` under control intent `?2`: record the intent
        /// and raise the drive epoch under admission `?3`, so every fence an
        /// earlier admission sealed is stale (FIG-3600 S7); no execution
        /// sealed it, so it stores no start marker. A session already closing
        /// is left as it is: zero rows.
        begin_close = "UPDATE session_meta
             SET closing_intent = ?2, drive_epoch = drive_epoch + 1, drive_admission_id = ?3,
                 drive_root_start = NULL
             WHERE session_id = ?1 AND closing_intent IS NULL";

        /// The control intent session `?1` is closing under, if any.
        select_closing_intent = "SELECT closing_intent FROM session_meta WHERE session_id = ?1";

        /// Retain checkpoint `?2` (or nothing) as the base session `?1`'s
        /// latest turn was admitted on, replacing the previous admission's
        /// (FIG-3682). Maintenance keeps it as a checkpoint root, so a replay
        /// of that turn can still read the head it was admitted on.
        retain_admission_base = "UPDATE session_meta SET admission_base_checkpoint_ref = ?2 WHERE session_id = ?1";

        /// The recorded lineage of `?1`.
        select_lineage = "SELECT relation_kind, parent_session_id, source_session_id, source_node_id
             FROM session_meta WHERE session_id = ?1";

        /// The process that runs session `?1` as its own, recorded when that
        /// process's start created the session (FIG-3607 R1); NULL for every
        /// other session.
        select_owning_process = "SELECT owning_process_id FROM session_meta WHERE session_id = ?1";

        /// The durable session-state version marker of `?1`.
        select_state_version = "SELECT session_state_version FROM session_meta WHERE session_id = ?1";

        touch_last_commit = "UPDATE session_meta SET last_commit_at_ms = ?2 WHERE session_id = ?1";

        /// Stamp `?1`'s session-state version marker to `?2`.
        ///
        /// Test-only, and named here rather than spelled at the two probes
        /// that issue it: a conformance probe that writes a marker no
        /// production path writes is exactly the sort of statement that drifts
        /// between the backends unnoticed.
        set_state_version = "UPDATE session_meta SET session_state_version = ?2 WHERE session_id = ?1";

        delete_by_session = "DELETE FROM session_meta WHERE session_id = ?1";
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct MetaRootVerbStatements @ "session_meta" {
        raise_epoch = "UPDATE session_meta SET drive_epoch = drive_epoch + 1, drive_admission_id = ?2, drive_root_start = NULL WHERE session_id = ?1 AND closing_intent IS NULL";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str = "obligation_id, obligation_attempts, session_id";

crate::statements! {
    /// `session_meta` obligation statements (ADR 0109): a closing session owes its physical delete. Both backends issue
    /// them verbatim; every settling write compares the state and, while
    /// claimed, the claim token.
    pub struct SessionMetaObligationStatements @ "session_meta" {
        /// Arm the row keyed `?1` as obligation `?2`, due at
        /// `?3`, if it owes nothing.
        obligation_arm = "UPDATE session_meta
             SET obligation_id = ?2, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?3, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE session_id = ?1 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM session_meta
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE session_meta
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, session_id";

        /// Claim obligation `?1` under token `?2` until `?3`: a `due` row
        /// whatever its backoff (a producer's own immediate attempt), or a
        /// claim `?2` already holds, its claimant re-deriving it after an
        /// interruption, which keeps its attempt count.
        obligation_claim = "UPDATE session_meta
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + CASE WHEN obligation_state = 'due' THEN 1 ELSE 0 END, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND (obligation_state = 'due'
                  OR (obligation_state = 'claimed' AND obligation_claim_token = ?2))
             RETURNING obligation_id, obligation_attempts, session_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE session_meta
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE session_meta
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE session_meta
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE session_meta
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, session_id
             FROM session_meta
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM session_meta WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM session_meta WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for SessionMetaObligationStatements {
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
    /// `session_meta` reads of a session's two-phase delete (ADR 0109 §4).
    pub struct SessionMetaDeleteStatements @ "session_meta" {
        /// Session `?1`'s `SessionDelete` obligation, if its row carries one.
        delete_obligation = "SELECT obligation_id, obligation_state FROM session_meta
             WHERE session_id = ?1 AND obligation_id IS NOT NULL";

        /// How many sessions are closing: their close committed and their
        /// physical delete, which removes the row, has not run.
        count_closing = "SELECT COUNT(*) FROM session_meta WHERE closing_intent IS NOT NULL";
    }
}
