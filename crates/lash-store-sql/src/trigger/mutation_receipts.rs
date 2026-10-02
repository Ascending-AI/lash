//! `trigger_mutation_receipts`: what one trigger operation decided, kept so
//! replaying the operation returns its original answer instead of
//! re-evaluating against newer state.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_mutation_receipts";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "operation_id, owner_kind, owner_id,
                request_fingerprint, result_json, created_at_ms";

crate::statements! {
    /// `trigger_mutation_receipts` statements both backends issue verbatim.
    pub struct MutationReceiptStatements @ "trigger_mutation_receipt" {
        /// The receipt operation `?1` already wrote, if it wrote one.
        select_by_operation_id = "SELECT request_fingerprint, result_json
             FROM trigger_mutation_receipts
             WHERE operation_id = ?1";

        insert = "INSERT INTO trigger_mutation_receipts (
                operation_id, owner_kind, owner_id,
                request_fingerprint, result_json, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
    }
}
