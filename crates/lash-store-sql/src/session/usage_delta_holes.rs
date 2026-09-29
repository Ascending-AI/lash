//! `usage_delta_holes`: the per-attempt holes behind bounded usage totals.

/// The table's unprefixed name.
pub const TABLE: &str = "usage_delta_holes";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, seq, call_id, attempt_ordinal, generation_id";

crate::statements! {
    /// `usage_delta_holes` statements shared by the two SQL backends.
    pub struct UsageDeltaHoleStatements @ "usage_delta_hole" {
        insert = "INSERT INTO usage_delta_holes
             (session_id, seq, call_id, attempt_ordinal, generation_id)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// A deleted session's holes become reclaimable with their usage row.
        delete_reclaimable = "DELETE FROM usage_delta_holes AS holes
             WHERE NOT EXISTS (SELECT 1 FROM usage_deltas AS usage
                               WHERE usage.session_id = holes.session_id
                                 AND usage.seq = holes.seq)";
    }
}
