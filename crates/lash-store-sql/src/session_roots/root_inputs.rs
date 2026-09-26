//! `session_root_inputs`: the root each accepted input of a session is bound
//! to. The recorded claim step binds the rows it claimed; a fork rebinds
//! held inputs to its new root.

/// The table's unprefixed name.
pub const TABLE: &str = "session_root_inputs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, input_id, root";

crate::statements! {
    /// `session_root_inputs` statements both backends issue verbatim.
    pub struct SessionRootInputStatements @ "session_root_input" {
        /// Bind input `?2` of session `?1` to root `?3` unless it is bound.
        insert = "INSERT INTO session_root_inputs (session_id, input_id, root) VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, input_id) DO NOTHING";

        /// The root input `?2` of session `?1` is bound to.
        select_root = "SELECT root FROM session_root_inputs WHERE session_id = ?1 AND input_id = ?2";

        /// Every binding of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_root_inputs WHERE session_id = ?1";
    }
}
