//! `process_events`: the append-only event log of one process incarnation.
//!
//! Every read of this table reports `event_json` and nothing else — the
//! indexed columns exist to find a row, and the decoded event carries all of
//! them — so the only projection wider than one column is the insert.

/// The table's unprefixed name.
pub const TABLE: &str = "process_events";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "process_id, process_incarnation, sequence, event_type, idempotency_key, event_json";

crate::statements! {
    /// `process_events` statements both backends issue verbatim.
    pub struct EventStatements @ "process_event" {
        /// The event process `?1` already recorded under replay key `?2`, if
        /// any: the idempotency half of an append.
        select_by_replay_key = "SELECT event_json
                 FROM process_events
                 WHERE process_id = ?1 AND idempotency_key = ?2";

        /// The highest sequence process `?1` has recorded, or `NULL` when it
        /// has recorded none.
        select_max_sequence = "SELECT MAX(sequence) FROM process_events WHERE process_id = ?1";

        /// Everything incarnation `?1` / `?2` recorded after sequence `?3`.
        list_after_sequence = "SELECT event_json FROM process_events
                 WHERE process_id = ?1 AND process_incarnation = ?2 AND sequence > ?3
                 ORDER BY sequence ASC";

        /// The last `?2` events of process `?1`, newest first. The caller
        /// reverses them; the descending order is what lets the primary key
        /// serve the limit.
        list_recent = "SELECT event_json FROM process_events
                         WHERE process_id = ?1 ORDER BY sequence DESC LIMIT ?2";

        /// How many `?2`-typed events process `?1` recorded at or before
        /// sequence `?3`.
        count_by_type_through_sequence = "SELECT COUNT(*) FROM process_events
                 WHERE process_id = ?1 AND event_type = ?2 AND sequence <= ?3";

        /// The same count, narrowed to incarnation `?2`.
        count_by_incarnation_type_through_sequence = "SELECT COUNT(*) FROM process_events
                 WHERE process_id = ?1 AND process_incarnation = ?2
                   AND event_type = ?3 AND sequence <= ?4";

        /// Record one event: `?1` process, `?2` incarnation, `?3` sequence,
        /// `?4` type, `?5` replay key, `?6` event.
        insert = "INSERT INTO process_events (
                        process_id, process_incarnation, sequence, event_type, idempotency_key, event_json
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
    }
}
