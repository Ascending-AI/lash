//! `runtime_effect_group`: one row per open effect group, carrying the two
//! counters its children allocate from — `next_seq` for settlement rank at
//! discharge, `next_commit_seq` for final-commit order at the §4 point — the
//! write-time `expected_children`, and a `lifecycle` enum-per-phase column
//! (`live`/`closing`/`settled`, ADR 0099 §7, FIG-3410).

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_effect_group";

/// Every column, in insert order.
///
/// `lifecycle` is deliberately absent: the DDL default writes
/// `{"type":"live"}` at open, and the only writer afterward is the guarded
/// single-row CAS, which sets the column explicitly — an INSERT that named it
/// would only be able to repeat the default.
pub const INSERT_COLUMNS: &str = "group_key, scope_id, session_id, wake, loser_disposition,
                expected_children, next_seq, next_commit_seq, created_at_ms";

/// The group as a caller reads it back.
///
/// The one projection over this table: the full row minus both counters (each
/// is never read, only bumped and returned by its bump) — but *with* the
/// lifecycle, which is the durable closing fact §7's finalization and the
/// session-deletion pin read act on.
pub const RECORD_COLUMNS: &str = "group_key, scope_id, session_id, wake, loser_disposition,
                    expected_children, lifecycle, created_at_ms";

/// The pin a session retirement checks: key and lifecycle alone.
///
/// Session deletion refuses while any of the session's groups is accepted or
/// closing, and the refusal message names the offending group — the full row
/// would decode columns the check never reads.
pub const SESSION_PIN_COLUMNS: &str = "group_key, lifecycle";

crate::statements! {
    /// `runtime_effect_group` statements both backends issue verbatim.
    pub struct GroupStatements @ "effect_group" {
        /// The durably recorded group row for `?1`.
        select_by_key = "SELECT group_key, scope_id, session_id, wake, loser_disposition,
                    expected_children, lifecycle, created_at_ms
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

        /// Every group key recorded under `?1` (scope) in the closed range
        /// `[?2, ?3]`, in ascending byte order: the group half of the
        /// recorded-frontier read, whose replay half is
        /// `ReplayStatements::select_keys_in_range`. A group can be recorded
        /// before any of its children claims a replay row, so the frontier
        /// must see the group row itself.
        select_keys_in_range = "SELECT group_key FROM runtime_effect_group
             WHERE scope_id = ?1 AND group_key >= ?2 AND group_key <= ?3
             ORDER BY group_key";

        delete_by_session = "DELETE FROM runtime_effect_group WHERE session_id = ?1";

        delete_by_scope = "DELETE FROM runtime_effect_group WHERE scope_id = ?1";
    }
}
