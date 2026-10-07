//! `parent_end_plans`: one row per ended parent scope.
//!
//! Keyed by the scope rather than by a process row: a turn-scoped parent has
//! no process row at all, and a process-scoped parent's row may be pruned
//! before its children settle. The `(parent_kind, parent_id)` pair is the
//! scope's collision-free index projection — equality and keyset ordering
//! only, never parsed back. `parent_payload` is the versioned typed parent:
//! the authority a reader decodes, and the fact that still answers "which
//! scope ended" after the parent's own row is pruned.

/// The table's unprefixed name.
pub const TABLE: &str = "parent_end_plans";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "parent_kind, parent_id, parent_payload, ended_at_ms";

crate::statements! {
    /// `parent_end_plans` statements both backends issue verbatim.
    pub struct ParentEndPlanStatements @ "parent_end_plan" {
        /// Record that scope `?1` / `?2` ended at `?4`, keeping the first
        /// stamp if one is already recorded. `?3` is the scope's versioned
        /// typed payload.
        ///
        /// Shared conflict clause: both backends need it, because the terminal
        /// append that records the plan can be replayed.
        insert_if_absent = "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (parent_kind, parent_id) DO NOTHING";

        exists = "SELECT 1 FROM parent_end_plans WHERE parent_kind = ?1 AND parent_id = ?2";

        /// Scope `?1` / `?2`'s typed payload and end instant.
        select_stamps = "SELECT parent_payload, ended_at_ms
             FROM parent_end_plans
             WHERE parent_kind = ?1 AND parent_id = ?2";
    }
}
