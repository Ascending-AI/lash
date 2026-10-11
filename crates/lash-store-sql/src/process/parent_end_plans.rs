//! `parent_end_plans`: one row per ended parent scope.
//!
//! Process, operation and session scopes retain their typed close plans.
//! Turn scopes instead use their retained session_runs terminal; joined input
//! aliases resolve through the input row's retained admitted_run column.

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

        /// A turn's terminal, including a joined input's turn alias. No
        /// independently stored close plan exists for a turn.
        turn_terminal = "SELECT terminal_at_ms FROM session_runs
             WHERE session_id = ?1 AND terminal_kind IS NOT NULL
               AND (run = ?2 OR run IN (
                   SELECT admitted_run FROM pending_turn_inputs
                   WHERE session_id = ?1 AND COALESCE(source_key, input_id) = ?2))
             LIMIT 1";

        /// Remember that the turn registered a consumer hold, even if it
        /// releases that hold before closing. Abandonment uses the same marker.
        mark_turn_holds = "UPDATE session_runs SET scope_has_holds = TRUE
             WHERE session_id = ?1 AND terminal_kind IS NULL
               AND (run = ?2 OR run IN (
                   SELECT admitted_run FROM pending_turn_inputs
                   WHERE session_id = ?1 AND COALESCE(source_key, input_id) = ?2))
             RETURNING run";

        /// The close's existing child probe also reads the registration marker;
        /// a turn with no holds pays no hold-release or mark-forget statement.
        turn_close = "SELECT
                 COALESCE((SELECT scope_has_holds FROM session_runs
                     WHERE session_id = ?1 AND run = ?2), FALSE),
                 EXISTS(SELECT 1 FROM processes
                     WHERE lifetime = 'until' AND lifetime_scope_kind = 'turn'
                       AND lifetime_scope_id = ?3 AND cancel_requested_at_ms IS NULL
                       AND status IN ('running', 'waiting'))";

        exists = "SELECT 1 FROM parent_end_plans WHERE parent_kind = ?1 AND parent_id = ?2";

        /// Scope `?1` / `?2`'s typed payload and end instant.
        select_stamps = "SELECT parent_payload, ended_at_ms
             FROM parent_end_plans
             WHERE parent_kind = ?1 AND parent_id = ?2";
    }
}
