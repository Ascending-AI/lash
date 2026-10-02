//! `process_event_horizons`: the event prefix a host released of one process
//! (FIG-3482).
//!
//! One row per process that has released anything, deleted with the process.
//! `released_through` only rises: the release that writes it read the old
//! value in the same transaction, so the upsert overwrites.

/// The table's unprefixed name.
pub const TABLE: &str = "process_event_horizons";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "process_id, released_through";

crate::statements! {
    /// `process_event_horizons` statements both backends issue verbatim.
    pub struct EventHorizonStatements @ "process_event_horizon" {
        /// The highest sequence process `?1` released, if it released any.
        select_released_through = "SELECT released_through FROM process_event_horizons
                 WHERE process_id = ?1";

        /// Raise process `?1`'s horizon to `?2`.
        upsert = "INSERT INTO process_event_horizons (process_id, released_through)
                 VALUES (?1, ?2)
                 ON CONFLICT (process_id) DO UPDATE SET released_through = excluded.released_through";
    }
}
