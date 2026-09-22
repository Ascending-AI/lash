//! `runtime_effect_group_child`: one row per accepted child of an effect
//! group, carrying the request that reconstructs it (ADR 0099 §3, FIG-3408).
//!
//! # Why a table and not two more columns
//!
//! ADR 0065 fixes `runtime_effect_replay`'s growth at exactly two columns
//! (`group_key`, `settlement_seq`) and names a position column as a third copy
//! of a fact the caller already holds. That constraint is about the *replay*
//! table, and it stands. This is a different table for a different reason: an
//! **unclaimed** child has no replay row at all, and §3 requires the accepted
//! membership to be discoverable "including unclaimed children". A row that
//! must exist before any claim cannot live on the claim.
//!
//! # Retained input only — no arbitration columns
//!
//! The §4/§5 arbitration state is *not* here: it lives on the replay row as
//! `commit_state`/`commit_seq` (FIG-3409), because the CAS that decides a
//! child runs under the replay row's lock and must not reach for a second
//! row to win. The membership row is what the group accepted — a write-time
//! fact, immutable once written — which is why the group's actual
//! cardinality is `COUNT(*)` over this table rather than a column anywhere.
//!
//! `command_version` records which command encoding minted each retained
//! envelope and is checked at decode: a reopen or drain that cannot read the
//! version refuses rather than guessing.
//!
//! # Lock order is preserved
//!
//! The arbitration paths — `finalize`, `decide_cancel`, `discharge_child` —
//! touch a group's rows in one order: replay row, then group row. Taking the
//! child's replay row lock first is what makes a contestant for the commit
//! state wait before it ever reaches the group counter, and taking the
//! counter before the CAS is what lets the winning statement write its
//! allocated position atomically. The open path is the deliberate exception:
//! it writes every membership row and then the group row **in one
//! transaction** — the only ordering that makes the group row's existence
//! imply its complete membership (§3's "discoverable complete input") — and
//! it commits before any child of the group can claim, so it never holds
//! membership-row locks against an arbitration path.

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_effect_group_child";

/// Every column, in insert order.
///
/// Deliberately **no `scope_id`**. The group row already carries the journal
/// scope, every child of a group is admitted under it
/// (`RuntimeEffectGroup::validate_execution_scope` checks each child against
/// the group's admitted scope), and the retained envelope carries the child's
/// full `EffectAddress` anyway. A column here would be a third copy of one
/// fact, free to disagree at exactly the crash boundary this table exists to
/// survive. Scope-keyed deletes reach it through the group row instead.
pub const INSERT_COLUMNS: &str = "group_key, position, replay_key,
                envelope_json, command_version, created_at_ms";

/// One accepted child as a reopen reads it back.
///
/// The membership facts minus `created_at_ms` — provisioning evidence rather
/// than reconstruction input.
pub const MEMBERSHIP_COLUMNS: &str = "group_key, position, replay_key,
                    envelope_json, command_version";

/// What the group drain reads about a child that holds no settlement rank.
///
/// Anchored on this table — the membership row — and left-joined to the
/// group and replay rows: an unclaimed child has no replay row and is still
/// unsettled work, and a committed-but-undrained child is the pass's
/// recovery obligation rather than a torn row, so the projection carries
/// the membership row's `command_version` — checked at decode — and the
/// replay row's `commit_state`/`commit_seq`. `g.scope_id` is the group row's
/// scope — the membership row carries none — and `c.envelope_json` is the
/// retained accepted envelope: the raw child request, decodable to
/// `RuntimeEffectEnvelope`, which a claimed child's replay row holds in the
/// canonical `{json, hash}` form instead. For a never-claimed child the
/// membership copy is the only one.
pub const UNSETTLED_CHILD_COLUMNS: &str =
    "g.scope_id, c.position, c.replay_key, c.envelope_json, c.command_version,
                r.status, r.outcome_json, r.error_json, r.lease_expires_at_ms,
                r.commit_state, r.commit_seq";

crate::statements! {
    /// `runtime_effect_group_child` statements both backends issue verbatim.
    pub struct GroupChildStatements @ "effect_group_child" {
        /// Every accepted child of group `?1`, in child order.
        ///
        /// Ordered by `position` because a reopen rebuilds the group's child
        /// vector and rank is defined over that order; an unordered read would
        /// make the reconstruction depend on the backend's row order.
        select_membership = "SELECT group_key, position, replay_key,
                    envelope_json, command_version
             FROM runtime_effect_group_child
             WHERE group_key = ?1
             ORDER BY position";

        delete_by_session = "DELETE FROM runtime_effect_group_child
             WHERE group_key IN (
                 SELECT group_key FROM runtime_effect_group WHERE session_id = ?1
             )";

        /// Reached through the group row for the same reason the table carries
        /// no `scope_id`: one owner of that fact. Group-atomic retirement
        /// (ADR 0065 N3) runs this in the transaction that deletes the groups.
        delete_by_scope = "DELETE FROM runtime_effect_group_child
             WHERE group_key IN (
                 SELECT group_key FROM runtime_effect_group WHERE scope_id = ?1
             )";
    }
}
