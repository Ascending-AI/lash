//! `trigger_occurrence_tombstones`: one payload-free row per occurrence
//! retention has reclaimed, kept so an ingest that presents the identity
//! again writes nothing back.
//!
//! The trigger store owns the row. It is written in the transaction that
//! deletes its occurrence, and the host's occurrence reclaim pass compacts it
//! once it is older than that pass's cutoff.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_occurrence_tombstones";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "occurrence_id, reclaimed_at_ms";

crate::statements! {
    /// `trigger_occurrence_tombstones` statements both backends issue
    /// verbatim.
    pub struct OccurrenceTombstoneStatements @ "trigger_occurrence_tombstone" {
        /// When occurrence `?1` was reclaimed, if it was.
        select_by_occurrence_id = "SELECT reclaimed_at_ms
             FROM trigger_occurrence_tombstones
             WHERE occurrence_id = ?1";

        /// Compact every tombstone written before cutoff `?1`.
        compact = "DELETE FROM trigger_occurrence_tombstones
             WHERE reclaimed_at_ms < ?1";
    }
}
