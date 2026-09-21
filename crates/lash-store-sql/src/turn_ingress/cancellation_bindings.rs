//! `turn_cancellation_bindings`: which turn-control authority a session admits.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancellation_bindings";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "session_id, binding_id, admitted_scope_json";

/// The binding as a validation reads it back.
///
/// `session_id` is absent because the read is keyed by it; the two columns left
/// are exactly the pair a presented binding is compared against.
pub const BINDING_COLUMNS: &str = "binding_id, admitted_scope_json";

crate::statements! {
    /// `turn_cancellation_bindings` statements both backends issue verbatim.
    pub struct CancellationBindingStatements @ "turn_cancellation_binding" {
        /// Session `?1`'s admitted binding.
        select_by_session = "SELECT binding_id, admitted_scope_json
             FROM turn_cancellation_bindings
             WHERE session_id = ?1";

        /// Delete session `?1`'s binding, on session deletion.
        delete_by_session = "DELETE FROM turn_cancellation_bindings WHERE session_id = ?1";
    }
}
