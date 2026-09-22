//! `runtime_effect_replay`: one row per journaled runtime effect.
//!
//! The row is wide — it carries the effect envelope and its outcome — and no
//! caller ever wants all of it, so this module declares the **named**
//! projections that exist and nothing else. Each one is justified where it is
//! declared; the ownership gate refuses any other column list over this table,
//! which is how the "every call site picks its own columns" habit stays
//! deleted.
//!
//! Since FIG-3409 the row also carries the group child's §4/§5 commit
//! protocol: `commit_state` (`pending | committed | drained |
//! cancel_decided`, CHECK-constrained, NOT NULL) is the one durable
//! linearization point a final record and a cancel disposition CAS over, and
//! `commit_seq` is the position in the group's final-commit order the winning
//! CAS writes, allocated from the group row's `next_commit_seq`. The
//! membership table holds none of it — a CAS that had to reach a second row
//! to win could not run under the replay row's lock.
//!
//! The decoded row types are the effect-replay driver's port types
//! (`StoredEffectRow`, `UnsettledGroupChild`, `StoredGroupSettlement`): the
//! driver defines what a claim decision reads, so the type lives with the
//! driver and the column order that feeds it lives here.

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_effect_replay";

/// Every column, in insert order. The only statements that name all of them
/// are the two backends' inserts.
pub const INSERT_COLUMNS: &str = "scope_id, session_id, replay_key, envelope_hash,
            envelope_json, status, outcome_json, error_json, lease_owner_id,
            lease_token, lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
            commit_state, commit_seq, created_at_ms, updated_at_ms";

/// What a claim decision reads.
///
/// Narrow on purpose: the claim path runs under the row's write lock on both
/// backends, so it is the one read that must not carry anything it does not
/// use. It drops the lease owner and token because `decide_effect_claim` is
/// given the request's own fence and compares expiry, never identity. (SQLite
/// selected both until FIG-3380 and discarded them in the decoder.)
pub const CLAIM_COLUMNS: &str = "envelope_hash, envelope_json, status, outcome_json, error_json,
                lease_expires_at_ms, due_at_ms";

/// A group child's arbitration outcome and the group it belongs to.
///
/// `finalize`'s miss-path read: the fenced `UPDATE` returned nothing and the
/// port needs `commit_state` to tell a recorded `cancel_decided` — W17's
/// typed refusal — from any other miss, and `group_key` to say which group
/// decided it. Two columns because the read runs once per lost CAS and never
/// feeds a decoder that wants more.
pub const GROUP_COMMIT_COLUMNS: &str = "group_key, commit_state";

/// One group child's commit protocol state.
///
/// The §4/§5 arbitration read: `decide_cancel`, `discharge_child` and the
/// driver's `read_child_arbitration` need exactly the linearization point's
/// two halves — who won (`commit_state`) and where the winning final record
/// sits in the group's commit order (`commit_seq`). Nothing else on the wide
/// replay row is arbitration state.
pub const CHILD_COMMIT_COLUMNS: &str = "commit_state, commit_seq";

/// One group child's commit protocol state and its recorded drain input.
///
/// [`CHILD_COMMIT_COLUMNS`] plus `drain_input`: `commit_group_child`'s read
/// and the recovery question "what did the winning commit record" both need
/// the §4 row's sealed obligations — the drain input a boundary is handed is
/// always the committed row's own, never the caller's.
pub const CHILD_DRAIN_COLUMNS: &str = "commit_state, commit_seq, drain_input";

/// What a claim decision reads once the §4 columns joined the row.
///
/// [`CLAIM_COLUMNS`] plus `commit_state` and `drain_input`: the claim must
/// see the linearization point's winner (a `cancel_decided` parent fences
/// new admissions) and, when it reclaims a boundary-committed row, the drain
/// input that commit sealed. Still narrow: the lease owner and token stay
/// dropped for the same reason [`CLAIM_COLUMNS`] drops them.
pub const CLAIM_DRAIN_COLUMNS: &str = "envelope_hash, envelope_json, status, outcome_json,
            error_json, lease_expires_at_ms, due_at_ms, commit_state, drain_input";

/// The arbitration state of the replay row addressed by scope and replay
/// key, with its `group_key`.
///
/// [`CHILD_COMMIT_COLUMNS`] plus `group_key` so a caller holding a journal
/// address — a claim checking the §4 fence on a new admission's minting
/// parent, a host asking whether an emission is cancel-decided — can tell
/// "group child" from "ungrouped" and "no row" apart. PostgreSQL's locked
/// claim-fence variant reads the identical projection under `FOR UPDATE`.
pub const ARBITRATION_COLUMNS: &str = "commit_state, commit_seq, group_key";

/// [`ARBITRATION_COLUMNS`] plus `drain_input`.
///
/// PostgreSQL's locked claim-fence variant of the same read: the claim that
/// must wait out an in-flight `decide_cancel` also answers the §4 boundary's
/// read-back of the committed row's sealed drain input, so the locked
/// projection carries it too.
pub const ARBITRATION_DRAIN_COLUMNS: &str = "commit_state, commit_seq, group_key, drain_input";

/// What a settled group member reports to a caller consuming ranks.
///
/// This is the narrowest read of the three and the one that most needs to be:
/// it is served once per rank a caller consumes, and `envelope_json` — the
/// effect payload, unbounded in size — is exactly what the caller already has.
pub const SETTLEMENT_COLUMNS: &str = "settlement_seq, replay_key, status, outcome_json, error_json";

crate::statements! {
    /// `runtime_effect_replay` statements both backends issue verbatim.
    pub struct ReplayStatements @ "effect_replay" {
        /// Whether a replay row exists for `?1` (scope) / `?2` (replay key),
        /// without reading any of it.
        exists_by_key = "SELECT EXISTS(
                 SELECT 1 FROM runtime_effect_replay
                 WHERE scope_id = ?1 AND replay_key = ?2
             )";

        /// Take an expired lease over: `?1` scope, `?2` replay key, `?3`
        /// owner, `?4` lease token, `?5` expiry, `?6` due-at, `?7` now.
        ///
        /// Unfenced by design on both backends: the decision to take over was
        /// made from the row read under its write lock in the same
        /// transaction, and `decide_effect_claim` owns it.
        take_over_lease = "UPDATE runtime_effect_replay
             SET lease_owner_id = ?3,
                 lease_token = ?4,
                 lease_expires_at_ms = ?5,
                 due_at_ms = ?6,
                 updated_at_ms = ?7
             WHERE scope_id = ?1 AND replay_key = ?2";

        /// Stamp the settlement rank `?3` allocated for `?1` / `?2`.
        ///
        /// `decide_cancel`'s rank write: a cancel-decided child is rankable
        /// immediately, so its rank lands in the same transaction as the
        /// `cancel_decided` CAS but stays a separate statement — the rank was
        /// allocated by the group-row bump that ran between them.
        set_settlement_seq = "UPDATE runtime_effect_replay
             SET settlement_seq = ?3
             WHERE scope_id = ?1 AND replay_key = ?2";

        /// The group `?1` / `?2` (scope, replay key) belongs to and its
        /// commit state, if the row exists.
        ///
        /// `finalize`'s miss-path read: the fenced `UPDATE` returned nothing
        /// and the port needs to know whether a recorded `cancel_decided`
        /// explains it — turning a bare miss into W17's typed refusal.
        select_group_commit_state = "SELECT group_key, commit_state FROM runtime_effect_replay
             WHERE scope_id = ?1 AND replay_key = ?2";

        /// One group child's commit protocol state: `?1` group, `?2` replay
        /// key — replay keys are unique within a group, so the pair names
        /// exactly one row.
        ///
        /// Read inside the writer's own transaction by `decide_cancel` and
        /// `discharge_child`, and standalone by the driver's
        /// `read_child_arbitration`; the row's `UPDATE` lock is what
        /// serializes the writers, and the read itself never needs to take it.
        select_child_commit_state = "SELECT commit_state, commit_seq FROM runtime_effect_replay
             WHERE group_key = ?1 AND replay_key = ?2";

        /// The commit protocol state of the replay row `?1`/`?2` (scope,
        /// replay key), with its `group_key` so a caller can tell "group
        /// child" from "ungrouped" and "no row" apart.
        ///
        /// Anchored on the journal address because the callers — a claim
        /// checking the §4 fence on a new admission's minting parent, or a
        /// host asking whether the emission that minted an intent is
        /// cancel-decided — hold the minting effect's replay identity, not
        /// its group. `group_key IS NULL` answers "not a group child".
        select_arbitration_by_key = "SELECT commit_state, commit_seq, group_key FROM runtime_effect_replay
             WHERE scope_id = ?1 AND replay_key = ?2";

        /// Win the §4 point for a final record: `?1` scope, `?2` replay key,
        /// `?3` the group the row belongs to, `?4` the commit position
        /// allocated from `next_commit_seq`.
        ///
        /// Guarded on `commit_state = 'pending'` so a rowcount of zero is the
        /// loss — a cancel disposition got there first — rather than a silent
        /// overwrite of the winner. The statement runs under the replay row
        /// lock `finalize`'s fenced write already holds, which is what makes
        /// the CAS the one durable linearization point.
        commit_child = "UPDATE runtime_effect_replay
             SET commit_state = 'committed', commit_seq = ?4
             WHERE scope_id = ?1 AND replay_key = ?2 AND group_key = ?3
               AND commit_state = 'pending'";

        /// The ungrouped twin of [`commit_child`](Self::commit_child): `?1`
        /// scope, `?2` replay key.
        ///
        /// A row with no `group_key` has no arbitration to contest and no
        /// commit position to allocate, but `committed` is still the honest
        /// state once its final record holds — `pending` means "the §4 point
        /// is open", which a terminal row's is not.
        commit_ungrouped = "UPDATE runtime_effect_replay
             SET commit_state = 'committed'
             WHERE scope_id = ?1 AND replay_key = ?2 AND commit_state = 'pending'";

        /// Win the §4 point at the child's final-attempt boundary: `?1`
        /// scope, `?2` replay key, `?3` the group the row belongs to, `?4`
        /// the commit position allocated from `next_commit_seq`, `?5` the
        /// serialized drain input, `?6` the lease owner the boundary was
        /// built under, `?7` now.
        ///
        /// [`commit_child`](Self::commit_child) plus the drain input and the
        /// lease guard. The boundary commit persists the arbitration
        /// decision, the drain order, and the obligations together: after it
        /// lands, a crash before discharge leaves everything a recovery pass
        /// needs to finish the drain rather than re-execute the attempt.
        /// The lease guard matches owner identity alone — the boundary may
        /// run after the stamped expiry while the row is still this owner's,
        /// and only reclamation under a different owner is the fence loss
        /// the guard exists to refuse.
        commit_child_final = "UPDATE runtime_effect_replay
             SET commit_state = 'committed', commit_seq = ?4, drain_input = ?5,
                 updated_at_ms = ?7
             WHERE scope_id = ?1 AND replay_key = ?2 AND group_key = ?3
               AND commit_state = 'pending'
               AND lease_owner_id = ?6";

        /// One group child's commit protocol state and recorded drain input:
        /// `?1` group, `?2` replay key.
        ///
        /// `commit_group_child`'s read and the recovery question "what did the
        /// winning commit record" share it: the drain input a boundary is
        /// handed is always the committed row's own, never the caller's.
        select_child_drain = "SELECT commit_state, commit_seq, drain_input FROM runtime_effect_replay
             WHERE group_key = ?1 AND replay_key = ?2";

        /// Win the §4 point for a cancel disposition: `?1` scope, `?2`
        /// replay key, `?3` the group the row belongs to. Same guard, same
        /// meaning of a miss.
        cancel_commit_state = "UPDATE runtime_effect_replay
             SET commit_state = 'cancel_decided'
             WHERE scope_id = ?1 AND replay_key = ?2 AND group_key = ?3
               AND commit_state = 'pending'";

        /// Seat the rank and finish the §5 discharge in one write: `?1`
        /// scope, `?2` replay key, `?3` the group the row belongs to, `?4`
        /// the rank allocated from `next_seq`.
        ///
        /// Guarded on `commit_state = 'committed'` so a repeated discharge is
        /// a miss rather than a second write — which is what makes the
        /// port's `AlreadyDischarged` arm a read of durable fact instead of a
        /// guess — and so rank and drain state land atomically: a recovered
        /// reader never sees a rankable child that is not drained nor a
        /// drained child without a rank. `status <> 'in_progress'` is the
        /// second half of that guard: a committed row still mid-drain owes a
        /// terminal, and a rank must not seat ahead of it.
        settle_drained = "UPDATE runtime_effect_replay
             SET commit_state = 'drained', settlement_seq = ?4
             WHERE scope_id = ?1 AND replay_key = ?2 AND group_key = ?3
               AND commit_state = 'committed' AND status <> 'in_progress'";

        /// [`settle_drained`](Self::settle_drained) plus the drained terminal:
        /// `?1` scope, `?2` replay key, `?3` the group the row belongs to,
        /// `?4` the rank, `?5` status, `?6` outcome, `?7` error, `?8` now.
        ///
        /// A boundary-committed row holds no terminal — its commit wrote the
        /// decision and the drain input, not an outcome — so its discharge is
        /// the write that seats the projected record, clears the lease, and
        /// marks it `drained`, all in the same CAS the rank rides on.
        settle_drained_final = "UPDATE runtime_effect_replay
             SET commit_state = 'drained', settlement_seq = ?4,
                 status = ?5, outcome_json = ?6, error_json = ?7,
                 lease_owner_id = NULL, lease_token = NULL,
                 lease_expires_at_ms = 0, due_at_ms = NULL, updated_at_ms = ?8
             WHERE scope_id = ?1 AND replay_key = ?2 AND group_key = ?3
               AND commit_state = 'committed'";

        /// Whether any sibling replay row of group `?1` holds `commit_state =
        /// 'committed'` with `commit_seq` below `?2`: the §5 barrier an
        /// intent drain waits behind so drains are admitted in final-commit
        /// order.
        has_undrained_lower_commit = "SELECT EXISTS(
                 SELECT 1 FROM runtime_effect_replay
                 WHERE group_key = ?1 AND commit_state = 'committed'
                   AND commit_seq < ?2
             )";

        /// Write the cancelled terminal into the child's replay row, whether
        /// or not the row exists yet: `?1` scope, `?2` session, `?3` replay
        /// key, `?4` envelope hash, `?5` envelope, `?6` error payload, `?7`
        /// group, `?8` now.
        ///
        /// `decide_cancel`'s speculative half of the §4 point. One upsert
        /// rather than an update-or-insert pair because the row can appear
        /// between two statements: an unclaimed child gets the inserted
        /// terminal, a claimed one gets the conflict update, and either way
        /// the write is guarded on `in_progress` — a row that went terminal
        /// under us is a miss the caller re-classifies from the commit
        /// state, never a silent overwrite of a terminal it did not write. A
        /// rowcount of zero is exactly that miss.
        write_cancelled = "INSERT INTO runtime_effect_replay (
                scope_id, session_id, replay_key, envelope_hash,
                envelope_json, status, outcome_json, error_json, lease_owner_id,
                lease_token, lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
                commit_state, commit_seq, created_at_ms, updated_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, 'failed', NULL, ?6, NULL, NULL, 0, NULL, ?7, NULL, 'pending', NULL, ?8, ?8)
             ON CONFLICT (scope_id, replay_key) DO UPDATE
             SET status = 'failed', outcome_json = NULL, error_json = ?6,
                 lease_owner_id = NULL, lease_token = NULL,
                 lease_expires_at_ms = 0, due_at_ms = NULL, updated_at_ms = ?8
             WHERE runtime_effect_replay.status = 'in_progress'";

        /// Take the child's replay-row lock ahead of its §5 discharge: `?1`
        /// scope, `?2` replay key, `?3` the group the caller believes the row
        /// belongs to, `?4` now.
        ///
        /// Exists to take the lock in the contract's row order — replay,
        /// then group — so the counter writes that follow it serialize
        /// behind one owner. The timestamp it writes is real: a discharged
        /// row is an updated row, and the barrier paths that abandon the
        /// discharge roll the touch back with the rest of the transaction.
        touch_for_discharge = "UPDATE runtime_effect_replay
             SET updated_at_ms = ?4
             WHERE scope_id = ?1 AND replay_key = ?2 AND group_key = ?3";

        /// The children of group `?1` that hold no rank: the complement of
        /// [`ReplayStatements::select_settlement_by_rank`]'s filter, anchored
        /// on the retained membership and left-joined to the replay row so an
        /// accepted-but-never-claimed child is reported too — a drain pass is
        /// exactly who must see it.
        ///
        /// Committed-but-undrained children lead the order — they are the
        /// pass's cheap obligations — in commit order; the still-executing
        /// and never-claimed rest follow in position order. `drained` and
        /// `cancel_decided` children hold ranks and are not in the set.
        select_unsettled_children = "SELECT g.scope_id, c.position, c.replay_key, c.envelope_json, c.command_version, r.status, r.outcome_json, r.error_json, r.lease_expires_at_ms, r.commit_state, r.commit_seq
             FROM runtime_effect_group_child c
             JOIN runtime_effect_group g ON g.group_key = c.group_key
             LEFT JOIN runtime_effect_replay r
               ON r.scope_id = g.scope_id AND r.replay_key = c.replay_key
             WHERE c.group_key = ?1 AND r.settlement_seq IS NULL
             ORDER BY r.commit_seq IS NULL, r.commit_seq, c.position";

        /// The `?2`-th (zero-based offset) settled member of group `?1`.
        select_settlement_by_rank = "SELECT settlement_seq, replay_key, status, outcome_json, error_json
             FROM runtime_effect_replay
             WHERE group_key = ?1 AND settlement_seq IS NOT NULL
             ORDER BY settlement_seq
             LIMIT 1 OFFSET ?2";

        /// The rank `?1` / `?2` (scope, replay key) was seated at, if any.
        ///
        /// The idempotency read-back of the arbitration paths: an
        /// `AlreadyDecided` cancel and an `AlreadyDischarged` drain answer
        /// with the rank the first writer allocated, which is durable state
        /// rather than a remembered return value.
        select_settlement_seq = "SELECT settlement_seq FROM runtime_effect_replay
             WHERE scope_id = ?1 AND replay_key = ?2";

        delete_by_session = "DELETE FROM runtime_effect_replay WHERE session_id = ?1";

        delete_by_scope = "DELETE FROM runtime_effect_replay WHERE scope_id = ?1";
    }
}
