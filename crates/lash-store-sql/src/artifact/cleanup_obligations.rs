//! `artifact_cleanup_obligations`: the cleanup each ended or guarded
//! referrer owes (ADR 0113 §2.4, §2.5).
//!
//! One row per referrer, keyed by its stored pair, carrying ADR 0109's
//! obligation columns and the serialized `ArtifactCleanup` body. Unlike every
//! other ledger, `obligation_state` is never null — a row exists only while it
//! owes — and a delivered row is deleted rather than kept: the fences in
//! every artifact store are the permanent evidence. On SQLite the table
//! exists in the durable core and in the process registry, and the ledger
//! routes by the obligation id's `core:`/`registry:` prefix.
//!
//! The upsert rule of `arm_cleanup` (a guard inserts only when no row exists;
//! `Ended` replaces a guard; nothing replaces `Ended`) needs the stored plan,
//! which is JSON, so a backend reads the row with [`CleanupObligationStatements::select_by_referrer`]
//! and decides with `lash_core_store::store::CleanupUpsert` in one
//! transaction rather than in one statement.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_cleanup_obligations";

/// The columns a new row is inserted with, in insert order. The remaining
/// obligation columns keep their defaults: an inserted row is `due` with no
/// claim, no attempt and no error.
pub const INSERT_COLUMNS: &str = "referrer_kind, referrer_id, cleanup_json, obligation_id, obligation_state, obligation_due_at_ms";

/// The columns a row read by referrer or id returns, in order.
pub const CLEANUP_COLUMNS: &str =
    "obligation_id, obligation_state, referrer_kind, referrer_id, cleanup_json";

crate::statements! {
    /// `artifact_cleanup_obligations` record statements both backends issue
    /// verbatim.
    pub struct CleanupObligationStatements @ "artifact_cleanup" {
        /// Insert referrer `?1`/`?2`'s cleanup `?3` as obligation `?4`, due at
        /// `?5`, unless the referrer already has a row. The caller reads the
        /// affected-row count.
        insert_if_absent = "INSERT INTO artifact_cleanup_obligations
             (referrer_kind, referrer_id, cleanup_json, obligation_id, obligation_state, obligation_due_at_ms)
             VALUES (?1, ?2, ?3, ?4, 'due', ?5)
             ON CONFLICT (referrer_kind, referrer_id) DO NOTHING";

        /// Referrer `?1`/`?2`'s row: its obligation, state and body.
        select_by_referrer = "SELECT obligation_id, obligation_state, referrer_kind, referrer_id, cleanup_json
             FROM artifact_cleanup_obligations
             WHERE referrer_kind = ?1 AND referrer_id = ?2";

        /// Obligation `?1`'s row: its obligation, state and body.
        select_by_id = "SELECT obligation_id, obligation_state, referrer_kind, referrer_id, cleanup_json
             FROM artifact_cleanup_obligations
             WHERE obligation_id = ?1";

        /// Replace referrer `?1`/`?2`'s guard with its `Ended` body `?3`,
        /// re-armed due at `?4` whatever the guard's state: a relay holding
        /// the guard's claim loses it, and a stalled guard is new work.
        replace_guard_with_ended = "UPDATE artifact_cleanup_obligations
             SET cleanup_json = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE referrer_kind = ?1 AND referrer_id = ?2";

        /// Make referrer `?1`/`?2`'s `due` row due by `?3`: a nudge only ever
        /// shortens the wait. A claimed row is already being delivered, and a
        /// stalled one waits for its operator.
        nudge = "UPDATE artifact_cleanup_obligations
             SET obligation_due_at_ms = CASE
                     WHEN obligation_due_at_ms > ?3 THEN ?3 ELSE obligation_due_at_ms END
             WHERE referrer_kind = ?1 AND referrer_id = ?2 AND obligation_state = 'due'";

        /// Whether referrer `?1`/`?2` has a row at all.
        exists = "SELECT EXISTS (
                 SELECT 1 FROM artifact_cleanup_obligations
                 WHERE referrer_kind = ?1 AND referrer_id = ?2
             )";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str =
    "obligation_id, obligation_attempts, referrer_kind, referrer_id";

crate::statements! {
    /// `artifact_cleanup_obligations` obligation statements (ADR 0109, ADR
    /// 0113 §2.5). Both backends issue them verbatim; every settling write
    /// compares the state and, while claimed, the claim token.
    pub struct CleanupObligationLedgerStatements @ "artifact_cleanup" {
        /// Arm the row keyed `?1`, `?2` as obligation `?3`, due at `?4`, if
        /// it owes nothing. A row of this table always owes, so this arms
        /// nothing; it keeps the shared ledger's shape.
        obligation_arm = "UPDATE artifact_cleanup_obligations
             SET obligation_id = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE referrer_kind = ?1 AND referrer_id = ?2 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM artifact_cleanup_obligations
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE artifact_cleanup_obligations
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, referrer_kind, referrer_id";

        /// Claim obligation `?1` under token `?2` until `?3`: a `due` row
        /// whatever its backoff (a producer's own immediate attempt), or a
        /// claim `?2` already holds, its claimant re-deriving it after an
        /// interruption, which keeps its attempt count.
        obligation_claim = "UPDATE artifact_cleanup_obligations
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + CASE WHEN obligation_state = 'due' THEN 1 ELSE 0 END, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND (obligation_state = 'due'
                  OR (obligation_state = 'claimed' AND obligation_claim_token = ?2))
             RETURNING obligation_id, obligation_attempts, referrer_kind, referrer_id";

        /// Settle claim `?2` on obligation `?1` delivered: the row is
        /// deleted, since every store's fence is the permanent evidence
        /// (ADR 0113 §2.5). `?3`, the settle instant, is bound for the shared
        /// settle shape and is never null.
        obligation_settle_delivered = "DELETE FROM artifact_cleanup_obligations
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2 AND ?3 IS NOT NULL";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE artifact_cleanup_obligations
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Defer claim `?2` on obligation `?1`: not owed yet, so back to
        /// `due` at `?3` with its attempts reset (ADR 0113 §2.5). A guard
        /// that waits never exhausts its attempts.
        obligation_settle_defer = "UPDATE artifact_cleanup_obligations
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_attempts = 0, obligation_due_at_ms = ?3,
                 obligation_last_error = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE artifact_cleanup_obligations
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE artifact_cleanup_obligations
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, referrer_kind, referrer_id
             FROM artifact_cleanup_obligations
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM artifact_cleanup_obligations WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM artifact_cleanup_obligations WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for CleanupObligationLedgerStatements {
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
