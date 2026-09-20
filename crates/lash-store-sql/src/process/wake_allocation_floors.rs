//! `wake_allocation_floors`: the highest sequence a sender has allocated to
//! one target session for one process.
//!
//! The floor is monotonic, so the insert is an upsert taking the maximum — and
//! that is the one statement that forks, because SQLite spells the maximum
//! `MAX` over `excluded` and PostgreSQL spells it `GREATEST` over `EXCLUDED`.

/// The table's unprefixed name.
pub const TABLE: &str = "wake_allocation_floors";

/// Every column, in insert order. The whole row is the floor and its key.
pub const INSERT_COLUMNS: &str = "target_session_id, process_id, allocation_floor";

crate::statements! {
    /// `wake_allocation_floors` statements both backends issue verbatim.
    pub struct WakeAllocationFloorStatements @ "wake_allocation_floor" {
        /// The floor session `?1` holds for process `?2`, if any.
        select_floor = "SELECT allocation_floor FROM wake_allocation_floors
                     WHERE target_session_id = ?1 AND process_id = ?2";

        /// Drop every floor aimed at session `?1`, which is going away.
        delete_by_session = "DELETE FROM wake_allocation_floors WHERE target_session_id = ?1";
    }
}
