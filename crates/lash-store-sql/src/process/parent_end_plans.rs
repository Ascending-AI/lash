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

/// Every column, in insert order. `settled_at_ms` is absent: a plan is
/// recorded unsettled and settled later.
pub const INSERT_COLUMNS: &str = "parent_kind, parent_id, parent_payload, ended_at_ms";

/// A pending plan as the sweep reads it.
///
/// Carries the typed payload because the projection columns are never parsed
/// back, and both instants because the sweep decides from the pair: the row
/// is work while `settled_at_ms` is null, and `ended_at_ms` is the order it
/// is worked in.
pub const PLAN_COLUMNS: &str = "parent_kind, parent_id, parent_payload, ended_at_ms, settled_at_ms";

/// [`PLAN_COLUMNS`] without the key, for the read that names the scope it is
/// asking about.
pub const STAMP_COLUMNS: &str = "parent_payload, ended_at_ms, settled_at_ms";

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

        /// The oldest `?1` plans that are still unsettled.
        list_pending = "SELECT parent_kind, parent_id, parent_payload, ended_at_ms, settled_at_ms
             FROM parent_end_plans
             WHERE settled_at_ms IS NULL
             ORDER BY ended_at_ms, parent_kind, parent_id
             LIMIT ?1";

        /// Scope `?1` / `?2`'s typed payload and two instants.
        select_stamps = "SELECT parent_payload, ended_at_ms, settled_at_ms FROM parent_end_plans
             WHERE parent_kind = ?1 AND parent_id = ?2";

        /// Settle scope `?1` / `?2` at `?3`, once.
        settle = "UPDATE parent_end_plans SET settled_at_ms = ?3
             WHERE parent_kind = ?1 AND parent_id = ?2 AND settled_at_ms IS NULL";
    }
}
