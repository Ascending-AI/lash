//! `runtime_effect_replay`: one row per journaled runtime effect.
//!
//! The row is wide — it carries the effect envelope and its outcome — and no
//! caller ever wants all of it, so this module declares the three **named**
//! projections that exist and nothing else. Each one is justified where it is
//! declared; the ownership gate refuses any other column list over this table,
//! which is how the "every call site picks its own columns" habit stays
//! deleted.
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
            created_at_ms, updated_at_ms";

/// What a claim decision reads.
///
/// Narrow on purpose: the claim path runs under the row's write lock on both
/// backends, so it is the one read that must not carry anything it does not
/// use. It drops the lease owner and token because `decide_effect_claim` is
/// given the request's own fence and compares expiry, never identity. (SQLite
/// selected both until FIG-3380 and discarded them in the decoder.)
pub const CLAIM_COLUMNS: &str = "envelope_hash, envelope_json, status, outcome_json, error_json,
                lease_expires_at_ms, due_at_ms";

/// What the group drain reads about a child that holds no settlement rank.
///
/// Kept separate from [`CLAIM_COLUMNS`] because it carries `scope_id` and
/// `replay_key` — the drain reads many rows and has to say which — and drops
/// the envelope hash, which only a fence comparison needs.
pub const UNSETTLED_CHILD_COLUMNS: &str =
    "scope_id, replay_key, envelope_json, status, outcome_json, error_json, lease_expires_at_ms";

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
        set_settlement_seq = "UPDATE runtime_effect_replay
             SET settlement_seq = ?3
             WHERE scope_id = ?1 AND replay_key = ?2";

        /// The children of group `?1` that hold no rank: the complement of
        /// [`ReplayStatements::select_settlement_by_rank`]'s filter, served by
        /// the partial index whose predicate is exactly this one.
        select_unsettled_children = "SELECT scope_id, replay_key, envelope_json, status, outcome_json, error_json, lease_expires_at_ms
             FROM runtime_effect_replay
             WHERE group_key = ?1 AND settlement_seq IS NULL
             ORDER BY replay_key";

        /// The `?2`-th (zero-based offset) settled member of group `?1`.
        select_settlement_by_rank = "SELECT settlement_seq, replay_key, status, outcome_json, error_json
             FROM runtime_effect_replay
             WHERE group_key = ?1 AND settlement_seq IS NOT NULL
             ORDER BY settlement_seq
             LIMIT 1 OFFSET ?2";

        delete_by_session = "DELETE FROM runtime_effect_replay WHERE session_id = ?1";

        delete_by_scope = "DELETE FROM runtime_effect_replay WHERE scope_id = ?1";
    }
}
