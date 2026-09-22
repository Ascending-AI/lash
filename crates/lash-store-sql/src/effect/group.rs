//! `runtime_effect_group`: one row per open effect group, carrying the two
//! counters its children allocate from — `next_seq` for settlement rank at
//! discharge, `next_commit_seq` for final-commit order at the §4 point — the
//! write-time `expected_children`, and a `lifecycle` enum-per-phase column
//! (`live` today; `closing`/`settled` are FIG-3410's writes on this column,
//! not new columns).

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_effect_group";

/// Every column, in insert order.
///
/// `lifecycle` is deliberately absent: the DDL default writes
/// `{"type":"live"}` at open, and no code path reads or writes it before
/// FIG-3410 — the column is the reservation, declared now so the closing and
/// settled phases are new values, not a migration.
pub const INSERT_COLUMNS: &str = "group_key, scope_id, session_id, wake, loser_disposition,
                expected_children, next_seq, next_commit_seq, created_at_ms";

/// The group as a caller reads it back.
///
/// The one projection over this table, and the full row minus both counters
/// and the lifecycle: each counter is never read, only bumped and returned by
/// its bump, and the lifecycle has no consumer until FIG-3410 — a reader that
/// carried either would be reporting a fact it must not act on.
pub const RECORD_COLUMNS: &str = "group_key, scope_id, session_id, wake, loser_disposition,
                    expected_children, created_at_ms";

crate::statements! {
    /// `runtime_effect_group` statements both backends issue verbatim.
    pub struct GroupStatements @ "effect_group" {
        /// The durably recorded group row for `?1`.
        select_by_key = "SELECT group_key, scope_id, session_id, wake, loser_disposition,
                    expected_children, created_at_ms
             FROM runtime_effect_group
             WHERE group_key = ?1";

        /// A single-row `UPDATE … SET next_seq = next_seq + 1` takes the row's
        /// lock, so there is no lost update under `READ COMMITTED` and none
        /// under `BEGIN IMMEDIATE`.
        bump_next_seq = "UPDATE runtime_effect_group
             SET next_seq = next_seq + 1
             WHERE group_key = ?1
             RETURNING next_seq";

        /// Allocate the next final-commit position in group `?1` and report
        /// it — the §4 twin of [`bump_next_seq`](Self::bump_next_seq).
        ///
        /// Bumped ahead of the child's commit-state CAS, in the shared lock
        /// order, so the position the winning CAS writes was allocated under
        /// this transaction's group-row lock; a losing CAS rolls the bump
        /// back with the rest.
        bump_next_commit_seq = "UPDATE runtime_effect_group
             SET next_commit_seq = next_commit_seq + 1
             WHERE group_key = ?1
             RETURNING next_commit_seq";

        delete_by_session = "DELETE FROM runtime_effect_group WHERE session_id = ?1";

        delete_by_scope = "DELETE FROM runtime_effect_group WHERE scope_id = ?1";
    }
}
