//! `queued_work_items`: the ordered payloads of one queued work batch.

/// The table's unprefixed name.
pub const TABLE: &str = "queued_work_items";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "batch_id, item_index, item_id, payload_json";

/// One item as a hydrating reader sees it, in item order.
///
/// `batch_id` and `item_index` are absent because the read is already keyed by
/// the first and ordered by the second: carrying them would invite a caller to
/// re-derive an order the `ORDER BY` already fixed.
pub const ITEM_COLUMNS: &str = "item_id, payload_json";

/// [`ITEM_COLUMNS`] keyed by batch, for the multi-batch hydration that reads
/// one page of items for a whole claim rather than one query per batch.
pub const KEYED_ITEM_COLUMNS: &str = "batch_id, item_id, payload_json";

crate::statements! {
    /// `queued_work_items` statements both backends issue verbatim.
    pub struct QueuedItemStatements @ "queued_work_item" {
        insert_new = "INSERT INTO queued_work_items (batch_id, item_index, item_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)";

        /// Batch `?1`'s payloads, in item order.
        list_by_batch = "SELECT item_id, payload_json
             FROM queued_work_items
             WHERE batch_id = ?1
             ORDER BY item_index ASC";
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct ItemRootVerbStatements @ "queued_work_item" {
        delete_batch_items = "DELETE FROM queued_work_items WHERE batch_id = ?1";
    }
}
