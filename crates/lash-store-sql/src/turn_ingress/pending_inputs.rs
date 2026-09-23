//! `pending_turn_inputs`: one row per turn input submitted to a session.

/// The table's unprefixed name.
pub const TABLE: &str = "pending_turn_inputs";

/// Every column a reader decodes, in the order the row decoders expect.
///
/// Before FIG-3383 this list was hand-spelled at ten call sites across the two
/// backends and once more as a per-backend `PENDING_TURN_INPUT_COLUMNS`
/// constant. It is one list now, and a column added to it reaches every reader.
pub const COLUMNS: &str = "enqueue_seq, input_id, session_id, source_key, ingress_json,
     state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
     claim_owner_id, claim_owner_incarnation_id,
     claim_token, claim_session_lease_generation";

/// The columns an enqueue writes on SQLite, where `enqueue_seq` is the table's
/// `INTEGER PRIMARY KEY AUTOINCREMENT` and is never bound.
pub const INSERT_COLUMNS: &str = "input_id, session_id, source_key, ingress_json, state,
     input_json, submitted_ingress_json, submission_digest, enqueued_at_ms";

/// The columns an enqueue writes on PostgreSQL.
///
/// PostgreSQL draws `enqueue_seq` from the column's sequence with an explicit
/// `nextval` before the insert, because the upsert path has to know the value
/// it is about to write; SQLite lets the row allocate its own. The durable
/// column is identical, only who allocates it forks.
pub const INSERT_COLUMNS_WITH_SEQ: &str = "enqueue_seq, input_id, session_id, source_key,
     ingress_json, state, input_json, submitted_ingress_json, submission_digest,
     enqueued_at_ms";

/// The facts source-key replay consults (FIG-3544).
///
/// Narrow for the same reason [`SETTLEMENT_COLUMNS`] is: the replay verdict
/// compares only the admission-time digest, so deciding it never decodes the
/// unbounded `input_json`; the full row is read back only on a match.
pub const REPLAY_COLUMNS: &str = "input_id, submission_digest";

/// The facts provisioned-id adoption consults (FIG-3513).
///
/// Narrow for the same reason [`REPLAY_COLUMNS`] is: whether a re-run of one
/// acceptance adopts the row its id already names, or refuses a foreign one,
/// depends only on the holder's session and admission-time digest, so deciding
/// it never decodes the unbounded `input_json`; the full row is read back only
/// on a match.
pub const ADOPTION_COLUMNS: &str = "session_id, submission_digest";

/// The facts the settlement verdict
/// [`require_settleable_turn_input`](lash_core::store_backend_support::require_settleable_turn_input)
/// consults, and nothing else.
///
/// Narrow on purpose: this read runs once per settled input of every commit,
/// and `input_json` and `ingress_json` are unbounded caller payloads that no
/// part of the settlement decision looks at. Decoding them here would put the
/// size of a user's submission on the commit path.
pub const SETTLEMENT_COLUMNS: &str = "claim_id, claim_token, claim_session_lease_generation, state";

/// The facts an orphaned-active-turn scan consults.
///
/// Narrow for the same reason [`SETTLEMENT_COLUMNS`] is: the scan asks
/// [`orphaned_active_turn_input_is_repairable`](lash_core::store_backend_support::orphaned_active_turn_input_is_repairable)
/// about every active-turn row of the session and reports only the turn ids, so
/// it never needs the unbounded `input_json`. The repair that follows does, and
/// reads [`COLUMNS`].
pub const ORPHAN_SCAN_COLUMNS: &str = "state, ingress_json, claim_token,
     claim_session_lease_generation";

/// The claim identity a bound claim's rows share, which a cancel locks the
/// whole claim by (FIG-3589).
///
/// Narrow because the lock only has to name the claim; the rows it locks are
/// read afterwards through [`COLUMNS`].
pub const CLAIM_IDENTITY_COLUMNS: &str = "claim_id, claim_token";

/// The binding a cancel consults (FIG-3589).
///
/// Narrow because the cancel verdict needs only whether the row is bound, to
/// which turn, and which input that turn's receipt names; the row itself was
/// already read through [`COLUMNS`].
pub const BINDING_COLUMNS: &str = "claim_bound_turn_id, claim_bound_receipt_input_id";

/// The ordering key the pending-work comparison reads.
///
/// Narrow because the comparison is only ever between this pair and the queued
/// batches' identical pair; nothing decodes a row.
pub const ORDERING_COLUMNS: &str = "enqueued_at_ms, enqueue_seq";

crate::statements! {
    /// `pending_turn_inputs` statements both backends issue verbatim.
    ///
    /// Every statement that releases a claim spells the whole four-column
    /// claim identity plus the generation, because
    /// `ck_pending_turn_inputs_claim_identity_all_or_none` makes the
    /// all-or-none shape load-bearing;
    /// `every_release_statement_clears_the_whole_claim_identity` in this
    /// crate's suite holds them to it.
    pub struct PendingInputStatements @ "pending_turn_input" {
        select_by_id = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// The input session `?1` filed under source key `?2`.
        select_by_source_key = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2";

        /// Session `?1`'s undelivered inputs at `?2`, each with the aborted
        /// turn its claim is bound to, if any (FIG-3589), and the expiry of the
        /// session-execution lease its claim is pinned to, or NULL when no live
        /// lease holds that claim.
        ///
        /// The lease lookup is a correlated subquery rather than a second read
        /// because "is this claim live?" must be answered against the same
        /// snapshot the row came from (ADR 0029).
        list_undelivered = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, claim_bound_turn_id,
                    claim_bound_receipt_input_id,
                    (SELECT sel.lease_expires_at_ms
                     FROM session_execution_leases sel
                     WHERE pending_turn_inputs.claim_token IS NOT NULL
                       AND sel.session_id = ?1
                       AND sel.lease_token IS NOT NULL
                       AND sel.lease_expires_at_ms > ?2
                       AND sel.lease_fencing_token
                           = pending_turn_inputs.claim_session_lease_generation)
                        AS live_lease_expires_at_ms
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Settle input `?2` of session `?1` into state `?3`, releasing the
        /// whole claim identity.
        ///
        /// Also the drop disposition of an interrupted turn's repair, which is
        /// the same write: cancel the row and let go of the claim.
        cancel = "UPDATE pending_turn_inputs
             SET state = ?3,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0,
                 claim_bound_turn_id = NULL,
                 claim_bound_receipt_input_id = NULL
             WHERE session_id = ?1 AND input_id = ?2";

        /// Re-defer input `?2` of session `?1` to state `?3` under the
        /// next-turn ingress `?4`, releasing the whole claim identity.
        ///
        /// The ingress is rewritten, not preserved: a row pinned to a turn
        /// that is over must stop naming it, or the next claim scan pins it to
        /// the same dead turn (FIG-1573).
        ///
        /// Only the mutable `ingress_json` moves. `submitted_ingress_json` and
        /// `submission_digest` are written once at admission and never
        /// updated, so an identical source-key retry still matches after
        /// this rewrite (FIG-3544).
        defer_to_next_turn = "UPDATE pending_turn_inputs
             SET state = ?3,
                 ingress_json = ?4,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0,
                 claim_bound_turn_id = NULL,
                 claim_bound_receipt_input_id = NULL
             WHERE session_id = ?1 AND input_id = ?2";

        /// Claim input `?2` of session `?1` into state `?3` for claim `?4`,
        /// owner `?5`/`?6`, lease token `?7`, generation `?8`, fencing token
        /// `?9`, on behalf of the redrive of aborted turn `?10` (NULL for every
        /// other claim).
        ///
        /// The generation predicate stays on the statement as the backstop of
        /// the shared claimability verdict (FIG-3381): the verdict decides over
        /// the locked row, and a row count other than one is a disagreement
        /// between the two, not a lost race. The binding predicate is the
        /// backstop of the candidate scans' exclusion (FIG-3589): a row bound
        /// to an aborted turn is taken only by that turn's redrive, and the
        /// claim releases the binding. `claim_bound_turn_id = NULL` is never
        /// true, so an ordinary claim never matches a bound row.
        claim = "UPDATE pending_turn_inputs
             SET state = ?3,
                 claim_id = ?4,
                 claim_owner_id = ?5,
                 claim_owner_incarnation_id = ?6,
                 claim_token = ?7,
                 claim_fencing_token = ?9,
                 claim_session_lease_generation = ?8,
                 claim_bound_turn_id = NULL,
                 claim_bound_receipt_input_id = NULL
             WHERE session_id = ?1
               AND input_id = ?2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?8
               )
               AND (claim_bound_turn_id IS NULL OR claim_bound_turn_id = ?10)";

        /// Bind the next-turn rows claim `?2`/`?3` of session `?1` still
        /// holds to the aborted turn `?4`, whose receipt names input `?5`
        /// (FIG-3589).
        ///
        /// The claim identity is the predicate: a row another driver settled
        /// or reclaimed no longer carries it and is left alone. Only open
        /// next-turn rows bind, which is what
        /// `ck_pending_turn_inputs_bound_claim_is_next_turn` holds the table to.
        /// A row a pending queued run owns never binds: the run re-claims its
        /// frozen members on retry, so they lapse to the run as before.
        bind_claim = "UPDATE pending_turn_inputs
             SET claim_bound_turn_id = ?4,
                 claim_bound_receipt_input_id = ?5
             WHERE session_id = ?1
               AND claim_id = ?2
               AND claim_token = ?3
               AND {{deferred_next_turn_turn_input_state(state)}}
               AND NOT EXISTS (
                 SELECT 1 FROM queued_run_members m
                 JOIN queued_runs r ON r.session_id = m.session_id AND r.scope_id = m.scope_id
                 WHERE m.session_id = pending_turn_inputs.session_id
                   AND m.member_kind = 'input' AND m.member_id = pending_turn_inputs.input_id
                   AND r.status = 'pending'
               )";

        /// The binding input `?2` of session `?1` carries: the aborted turn
        /// and the receipt's input, both NULL for an unbound row (FIG-3589).
        binding_facts = "SELECT claim_bound_turn_id, claim_bound_receipt_input_id
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// Return the other rows of bound claim `?2`/`?3` on session `?1` to
        /// the next-turn queue, releasing the whole claim identity and the
        /// binding (FIG-3589).
        ///
        /// A cancel of one row of a bound claim leaves the aborted turn nothing
        /// its redrive could settle, so the rows the turn absorbed from earlier
        /// admissions are handed back for the next drain rather than stranded.
        /// Bound rows are next-turn rows, so the state stays
        /// `deferred_next_turn`, and an unbound claim is not touched.
        release_bound_claim = "UPDATE pending_turn_inputs
             SET claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0,
                 claim_bound_turn_id = NULL,
                 claim_bound_receipt_input_id = NULL
             WHERE session_id = ?1
               AND claim_id = ?2
               AND claim_token = ?3
               AND claim_bound_turn_id IS NOT NULL";

        /// Settle claimed input `?2` of session `?1` into `?3`, under claim
        /// `?4` and lease token `?5` (ADR 0069 §5).
        settle_claimed = "UPDATE pending_turn_inputs
             SET state = ?3,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0,
                 claim_bound_turn_id = NULL,
                 claim_bound_receipt_input_id = NULL
             WHERE session_id = ?1
               AND input_id = ?2
               AND claim_id = ?4
               AND claim_token = ?5";

        /// Settle unclaimed input `?2` of session `?1` into `?3`.
        ///
        /// The same write as [`settle_claimed`](Self::settle_claimed) with the
        /// other regime's predicate: the row must still be unclaimed and not
        /// already terminal. The terminal set is named, never spelled, so it
        /// cannot drift from
        /// [`unclaimed_turn_input_is_settleable`](lash_core::store_backend_support::unclaimed_turn_input_is_settleable).
        settle_unclaimed = "UPDATE pending_turn_inputs
             SET state = ?3,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0,
                 claim_bound_turn_id = NULL,
                 claim_bound_receipt_input_id = NULL
             WHERE session_id = ?1
               AND input_id = ?2
               AND claim_id IS NULL
               AND {{nonterminal_turn_input_state(state)}}";

        /// Reclaim session `?1`'s terminal input rows. Retention only.
        delete_terminal = "DELETE FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{terminal_turn_input_state(state)}}
               AND NOT EXISTS (
                 SELECT 1 FROM queued_run_members m
                 JOIN queued_runs r ON r.session_id = m.session_id AND r.scope_id = m.scope_id
                 WHERE m.session_id = pending_turn_inputs.session_id
                   AND m.member_kind = 'input' AND m.member_id = pending_turn_inputs.input_id
                   AND r.status = 'pending'
               )";

        delete_by_session = "DELETE FROM pending_turn_inputs WHERE session_id = ?1";
    }
}
