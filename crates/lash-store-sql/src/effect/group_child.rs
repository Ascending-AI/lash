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
//! # Lock order (ADR 0065 N2) is preserved, and this is what makes §3 atomic
//!
//! N2 is "child row before group row, without exception". The open path writes
//! every membership row and then the group row **in one transaction**, in that
//! order, so the rule is honoured and the group row's existence implies its
//! complete membership. That is exactly §3's "A persisted accepted group may
//! never exist without discoverable complete input": before the change the
//! group row was committed alone in its own transaction and the membership did
//! not exist at all.

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
                envelope_json, request_version, created_at_ms";

/// One accepted child as a reopen reads it back.
///
/// The full row minus `created_at_ms`, which is provisioning evidence rather
/// than reconstruction input: a reopen rebuilds a child from its envelope and
/// never from when it was accepted. Named rather than reusing
/// [`INSERT_COLUMNS`] because the two differ, and the authoring guide requires
/// a projection to be one declared constant character for character.
pub const MEMBERSHIP_COLUMNS: &str = "group_key, position, replay_key,
                    envelope_json, request_version";

crate::statements! {
    /// `runtime_effect_group_child` statements both backends issue verbatim.
    pub struct GroupChildStatements @ "effect_group_child" {
        /// Every accepted child of group `?1`, in child order.
        ///
        /// Ordered by `position` because a reopen rebuilds the group's child
        /// vector and rank is defined over that order; an unordered read would
        /// make the reconstruction depend on the backend's row order.
        select_membership = "SELECT group_key, position, replay_key,
                    envelope_json, request_version
             FROM runtime_effect_group_child
             WHERE group_key = ?1
             ORDER BY position";

        /// How many children group `?1` has accepted.
        ///
        /// A named projection of one column rather than a read of the whole
        /// membership: the reopen fence compares arity before it decodes any
        /// envelope, and `envelope_json` is an unbounded column this avoids
        /// decoding entirely.
        count_membership = "SELECT COUNT(*) FROM runtime_effect_group_child
             WHERE group_key = ?1";

        /// Delete every accepted child of session `?1`.
        delete_by_session = "DELETE FROM runtime_effect_group_child
             WHERE group_key IN (
                 SELECT group_key FROM runtime_effect_group WHERE session_id = ?1
             )";

        /// Delete every accepted child of scope `?1`.
        ///
        /// Reached through the group row for the same reason the table carries
        /// no `scope_id`: one owner of that fact. Group-atomic retirement
        /// (ADR 0065 N3) runs this in the transaction that deletes the groups.
        delete_by_scope = "DELETE FROM runtime_effect_group_child
             WHERE group_key IN (
                 SELECT group_key FROM runtime_effect_group WHERE scope_id = ?1
             )";
    }
}
