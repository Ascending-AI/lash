//! `runtime_effect_group`: one row per open effect group, carrying the
//! settlement-rank counter its children allocate from.

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_effect_group";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "group_key, scope_id, session_id, wake, loser_disposition,
                children, next_seq, created_at_ms";

/// The group as a caller reads it back.
///
/// The one projection over this table, and the full row minus `next_seq`:
/// the counter is never read, only bumped and returned by the bump, so a
/// reader that carried it would be reporting a number it must not act on.
pub const RECORD_COLUMNS: &str = "group_key, scope_id, session_id, wake, loser_disposition,
                    children, created_at_ms";

crate::statements! {
    /// `runtime_effect_group` statements both backends issue verbatim.
    pub struct GroupStatements @ "effect_group" {
        /// The durably recorded group row for `?1`.
        select_by_key = "SELECT group_key, scope_id, session_id, wake, loser_disposition,
                    children, created_at_ms
             FROM runtime_effect_group
             WHERE group_key = ?1";

        /// A single-row `UPDATE … SET next_seq = next_seq + 1` takes the row's
        /// lock, so there is no lost update under `READ COMMITTED` and none
        /// under `BEGIN IMMEDIATE`.
        bump_next_seq = "UPDATE runtime_effect_group
             SET next_seq = next_seq + 1
             WHERE group_key = ?1
             RETURNING next_seq";

        delete_by_session = "DELETE FROM runtime_effect_group WHERE session_id = ?1";

        delete_by_scope = "DELETE FROM runtime_effect_group WHERE scope_id = ?1";
    }
}
