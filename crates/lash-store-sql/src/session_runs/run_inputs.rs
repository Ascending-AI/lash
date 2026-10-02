//! `session_run_inputs`: the run each accepted input of a session is bound
//! to. The recorded admission step binds the rows it admitted, the commit
//! that applies a checkpoint-admitted input binds it to the run that
//! applied it, and a fork rebinds held inputs to its new run.

/// The table's unprefixed name.
pub const TABLE: &str = "session_run_inputs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, input_id, run";

crate::statements! {
    /// `session_run_inputs` statements both backends issue verbatim.
    pub struct SessionRunInputStatements @ "session_run_input" {
        /// Bind input `?2` of session `?1` to run `?3` unless it is bound.
        insert = "INSERT INTO session_run_inputs (session_id, input_id, run) VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, input_id) DO NOTHING";

        /// The run input `?2` of session `?1` is bound to.
        select_run = "SELECT run FROM session_run_inputs WHERE session_id = ?1 AND input_id = ?2";

        /// Turn scopes of inputs admitted to run `?2` of session `?1`.
        /// The source key names the input's turn; an unkeyed input uses its id.
        bound_turn_scopes = "SELECT DISTINCT COALESCE(i.source_key, i.input_id)
             FROM session_run_inputs b
             JOIN pending_turn_inputs i ON i.session_id = b.session_id AND i.input_id = b.input_id
             WHERE b.session_id = ?1 AND b.run = ?2
             ORDER BY 1";

        /// Every binding of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_run_inputs WHERE session_id = ?1";
    }
}

crate::statements! {
    /// Statements for parked-run control and recovery.
    pub struct RunInputVerbStatements @ "session_run_input" {
        bound_inputs = "SELECT b.input_id FROM session_run_inputs b JOIN pending_turn_inputs i ON i.session_id = b.session_id AND i.input_id = b.input_id WHERE b.session_id = ?1 AND b.run = ?2 AND {{nonterminal_turn_input_state(i.state)}}";
        rebind = "UPDATE session_run_inputs SET run = ?3 WHERE session_id = ?1 AND input_id = ?2";
        unbind = "DELETE FROM session_run_inputs WHERE session_id = ?1 AND input_id = ?2";
    }
}
