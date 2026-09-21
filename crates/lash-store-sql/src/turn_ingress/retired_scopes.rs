//! `turn_cancel_retired_scopes`: the cancellation scopes no closure may enter
//! again.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancel_retired_scopes";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "scope_id";

crate::statements! {
    /// `turn_cancel_retired_scopes` statements both backends issue verbatim.
    pub struct RetiredScopeStatements @ "turn_cancel_retired_scope" {
        /// Whether scope `?1` has been retired.
        exists_for_scope = "SELECT EXISTS(SELECT 1 FROM turn_cancel_retired_scopes
             WHERE scope_id = ?1)";
    }
}
