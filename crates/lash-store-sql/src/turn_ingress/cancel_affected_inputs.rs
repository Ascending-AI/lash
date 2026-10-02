//! `turn_cancel_affected_inputs`: one immutable snapshot per affected item.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancel_affected_inputs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind, batch_id";

/// What the settlement reads back: the item, its payload, what the
/// cancellation decided to do with it, its kind and — for a wake — the batch
/// that carries it.
///
/// The key columns are the read's own parameters, so projecting them again
/// would return the caller its own arguments once per row.
pub const SETTLEMENT_COLUMNS: &str = "input_id, input_json, disposition, item_kind, batch_id";

crate::statements! {
    /// Ordered cancellation receipt snapshots, shared by both stores.
    pub struct CancelAffectedInputStatements @ "turn_cancel_affected_input" {
        /// Turn `?2` of session `?1`'s recorded dispositions, host inputs and
        /// held wakes alike, in the order the turn observed them.
        ///
        /// `ordinal` is the order and it is a total one — it is the third
        /// component of the primary key — so no tie needs breaking.
        select_by_turn = "SELECT input_id, input_json, disposition, item_kind, batch_id
             FROM turn_cancel_affected_inputs
             WHERE session_id = ?1 AND turn_id = ?2
             ORDER BY ordinal ASC";

        /// The next ordinal is computed inside the insert rather than read
        /// first: the caller already holds the cancel-request row's lock, and
        /// deriving it in one statement is what keeps the ordinal allocation
        /// and the append in a single round trip. An item already recorded
        /// is not recorded again: an input the interrupted turn's commit
        /// recorded is still addressed to that turn, its delivery unchanged,
        /// when the root's terminal write sweeps the turns it ends.
        append_at_next_ordinal = "INSERT INTO turn_cancel_affected_inputs (
                 session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind,
                 batch_id
             )
             SELECT
                 ?1, ?2,
                 (SELECT COALESCE(MAX(ordinal) + 1, 0)
                    FROM turn_cancel_affected_inputs
                   WHERE session_id = ?1 AND turn_id = ?2),
                 ?3, ?4, ?5, ?6, ?7
             WHERE NOT EXISTS (
                 SELECT 1 FROM turn_cancel_affected_inputs
                  WHERE session_id = ?1 AND turn_id = ?2 AND item_kind = ?6 AND input_id = ?3
             )";
    }
}
