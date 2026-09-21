//! `trigger_mutation_receipts`: what one trigger operation decided, kept so
//! replaying the operation returns its original answer instead of
//! re-evaluating against newer state.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_mutation_receipts";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "operation_id, owner_kind, owner_id,
                request_fingerprint, result_json, created_at_ms";

/// What a replayed operation reads back: the fingerprint that says whether
/// this is the same request, and the result it is owed if it is.
///
/// Narrow because the receipt is keyed by the operation id the caller already
/// holds, and because the two owner columns exist for retention — they are
/// read by no operation, only deleted by one.
pub const RECEIPT_COLUMNS: &str = "request_fingerprint, result_json";

crate::statements! {
    /// `trigger_mutation_receipts` statements both backends issue verbatim.
    pub struct MutationReceiptStatements @ "trigger_mutation_receipt" {
        /// The receipt operation `?1` already wrote, if it wrote one.
        select_by_operation_id = "SELECT request_fingerprint, result_json
             FROM trigger_mutation_receipts
             WHERE operation_id = ?1";

        /// Record what operation `?1` decided.
        insert = "INSERT INTO trigger_mutation_receipts (
                operation_id, owner_kind, owner_id,
                request_fingerprint, result_json, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

        /// Drop host- and platform-owned receipts older than `?1`.
        ///
        /// Session-owned receipts are deliberately absent: they are reclaimed
        /// by the session's own retention pass, which has to see that no
        /// delivery still blocks the session first.
        prune_host_and_platform = "DELETE FROM trigger_mutation_receipts
             WHERE created_at_ms < ?1
               AND owner_kind IN ('host', 'platform')";
    }
}
