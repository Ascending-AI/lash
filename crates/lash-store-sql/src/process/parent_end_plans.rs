//! `parent_end_plans`: one row per ended parent scope.
//!
//! A process scope's plan lives on its `processes` row while that row exists
//! (FIG-7701); a prune moves it here unless retention reclaims it.
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
        /// A process scope `?5` whose row exists keeps its plan on that row
        /// instead ([`pack`](Self::pack)); `?5` is `NULL` for other scopes.
        insert_if_absent = "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
             SELECT ?1, ?2, ?3, ?4
             WHERE NOT EXISTS (SELECT 1 FROM processes WHERE process_id = ?5)
             ON CONFLICT (parent_kind, parent_id) DO NOTHING";

        /// Record process `?1`'s own scope `?2` ended at `?4` with payload
        /// `?3` on its row (FIG-7701), unless a plan for it is already
        /// recorded on the row or in the table: the first stamp stands.
        pack = "UPDATE processes SET end_plan_scope_id = ?2, end_plan_payload = ?3,
                 end_plan_ended_at_ms = ?4
             WHERE process_id = ?1 AND end_plan_scope_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM parent_end_plans
                   WHERE parent_kind = 'process' AND parent_id = ?2)";

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
                     WHERE session_id = ?1 AND run = ?2), FALSE) AS has_holds,
                 EXISTS(SELECT 1 FROM processes
                     WHERE lifetime = 'until' AND lifetime_scope_kind = 'turn'
                       AND lifetime_scope_id = ?3 AND cancel_requested_at_ms IS NULL
                       AND status IN ('running', 'waiting')) AS has_until";

        /// `?3` names the process whose row may hold the plan, `NULL` for
        /// scopes that are not processes.
        exists = "SELECT 1 FROM parent_end_plans WHERE parent_kind = ?1 AND parent_id = ?2
             UNION ALL
             SELECT 1 FROM processes WHERE process_id = ?3 AND end_plan_scope_id = ?2";

        /// Scope `?1` / `?2`'s typed payload and end instant.
        /// `?3` as in [`exists`](Self::exists).
        select_stamps = "SELECT parent_payload, ended_at_ms
             FROM parent_end_plans
             WHERE parent_kind = ?1 AND parent_id = ?2
             UNION ALL
             SELECT end_plan_payload, end_plan_ended_at_ms
             FROM processes WHERE process_id = ?3 AND end_plan_scope_id = ?2";
    }
}
