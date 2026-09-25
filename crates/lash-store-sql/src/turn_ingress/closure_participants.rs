//! `turn_cancel_closure_participants`: who is still inside one cancellation
//! closure's scope.
//!
//! SQLite reaches this table through every schema an effect host has attached,
//! because the question "is this scope still occupied?" is asked from the
//! journal's connection as well as the catalog's; the statements below are
//! rendered once per SQLite schema rather than built per call.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancel_closure_participants";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "scope_id, participant_id, scope_json";

crate::statements! {
    /// `turn_cancel_closure_participants` statements SQLite's effect journal
    /// issues (PostgreSQL journals no effects, ADR 0104).
    pub struct ClosureParticipantStatements @ "turn_cancel_closure_participant" {
        /// Admit `?2` to scope `?1`, keeping an existing admission.
        insert_new = "INSERT INTO turn_cancel_closure_participants (
                 scope_id, participant_id, scope_json
             )
             VALUES (?1, ?2, ?3)
             ON CONFLICT (scope_id, participant_id) DO NOTHING";

        /// Release `?2` from scope `?1`.
        delete_participant = "DELETE FROM turn_cancel_closure_participants
             WHERE scope_id = ?1 AND participant_id = ?2";

        /// Whether scope `?1` still holds a participant.
        exists_for_scope = "SELECT EXISTS(
                 SELECT 1 FROM turn_cancel_closure_participants
                 WHERE scope_id = ?1
             )";
    }
}
