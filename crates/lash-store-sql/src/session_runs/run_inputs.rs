//! An input's run is retained on its pending_turn_inputs row, including after
//! settlement. Fork release clears only live bindings.

crate::statements! {
    /// Input-run readers and explicit bindings, shared by both stores.
    pub struct SessionRunInputStatements @ "session_run_input" {
        /// Bind an existing input, keeping an equal binding and refusing a
        /// different one in the caller's transaction.
        bind = "UPDATE pending_turn_inputs
             SET admitted_run = ?3,
                 admitted_by = CASE WHEN {{nonterminal_turn_input_state(state)}} THEN CAST(?3 AS TEXT) ELSE NULL END
             WHERE session_id = ?1 AND input_id = ?2
               AND (admitted_run IS NULL OR admitted_run = ?3)";

        select_run = "SELECT admitted_run FROM pending_turn_inputs WHERE session_id = ?1 AND input_id = ?2";

        /// What a follower of input `?2` of session `?1` reads, in one
        /// statement and so one snapshot: the run it is bound to, whether its
        /// row is still open, the session's standing fault with its instant,
        /// and whether the host forsook its id. A claim binds the input and
        /// moves its row in one transaction, so no read here sees one
        /// without the other.
        select_status = "SELECT
             (SELECT b.admitted_run FROM pending_turn_inputs b
              WHERE b.session_id = ?1 AND b.input_id = ?2),
             EXISTS (SELECT 1 FROM pending_turn_inputs i
                     WHERE i.session_id = ?1 AND i.input_id = ?2
                       AND {{nonterminal_turn_input_state(i.state)}}),
             (SELECT m.fault_json FROM session_meta m WHERE m.session_id = ?1),
             (SELECT m.fault_at_ms FROM session_meta m WHERE m.session_id = ?1),
             EXISTS (SELECT 1 FROM forsaken_turn_inputs f
                     WHERE f.session_id = ?1 AND f.input_id = ?2)";

        /// Turn scopes of inputs admitted to run `?2` of session `?1`.
        /// The source key names the input's turn; an unkeyed input uses its id.
        bound_turn_scopes = "SELECT DISTINCT COALESCE(source_key, input_id)
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run = ?2
             ORDER BY 1";

        bound_inputs = "SELECT input_id FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run = ?2 AND {{nonterminal_turn_input_state(state)}}";
    }
}
