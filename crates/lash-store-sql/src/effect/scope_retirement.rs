//! `effect_scope_retirements`: the permanent fence of a retired execution
//! scope (ADR 0049).
//!
//! SQLite keeps this table in up to two files — the journal's own and a bound
//! process registry's — and addresses each through a schema qualifier, which
//! is why the SQLite side renders these statements once per schema instead of
//! building a qualified string per call.

/// The table's unprefixed name.
pub const TABLE: &str = "effect_scope_retirements";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "scope_id, retired_at_ms, artifact_cleanup_completed";

crate::statements! {
    /// `effect_scope_retirements` statements SQLite's effect journal issues.
    pub struct ScopeRetirementStatements @ "effect_scope_retirement" {
        /// Whether scope `?1` is fenced in this table.
        exists = "SELECT EXISTS(
                 SELECT 1 FROM effect_scope_retirements WHERE scope_id = ?1
             )";

        /// Lift the fence of scope `?1`.
        delete_by_scope = "DELETE FROM effect_scope_retirements WHERE scope_id = ?1";
    }
}
