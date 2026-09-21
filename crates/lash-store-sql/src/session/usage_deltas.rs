//! `usage_deltas`: the append-only token ledger, one row per recorded usage
//! entry of one operation.

/// The table's unprefixed name.
pub const TABLE: &str = "usage_deltas";

/// Every column, in insert order.
///
/// `seq` is absent: it is the table's own monotonic key, assigned by the
/// backend, and the read below is the only thing that names it.
pub const INSERT_COLUMNS: &str = "session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json";

/// One ledger entry as the session load folds it.
///
/// The identity columns are deliberately absent. They exist to make the insert
/// idempotent — the ledger is merged by
/// `merge_token_ledger_entries_checked`, which reads only the accounting
/// columns — and a projection that carried them would invite a caller to
/// re-derive identity from a row instead of from the operation it belongs to.
pub const LEDGER_COLUMNS: &str = "source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json";

crate::statements! {
    /// `usage_deltas` statements both backends issue verbatim.
    pub struct UsageDeltaStatements @ "usage_delta" {
        /// Session `?1`'s ledger, in the order the rows were appended.
        select_for_session = "SELECT source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json
             FROM usage_deltas WHERE session_id = ?1 ORDER BY seq ASC";

        /// Drop every ledger row of a deleted session whose operation no
        /// longer has a receipt.
        ///
        /// A live ledger is never eligible: it is what rebuilds a resumed
        /// session's accounting. The anti-join runs after the receipt sweep in
        /// the same transaction, so a row becomes eligible exactly when its
        /// receipt was swept.
        delete_reclaimable = "DELETE FROM usage_deltas AS usage
             WHERE EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = usage.session_id)
               AND NOT EXISTS (SELECT 1 FROM runtime_turn_commits AS receipt
                               WHERE receipt.session_id = usage.session_id
                                 AND receipt.turn_id = usage.operation_storage_key)";
    }
}
