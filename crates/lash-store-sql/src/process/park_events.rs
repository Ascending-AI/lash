//! `process_park_events`: the durable feed of process park transitions
//! (FIG-3659 NOW-B).
//!
//! The process twin of `turn_park_events`, with the same shape: one row per
//! transition — `Parked`, `Unparked`, `Cancelled` — appended in the
//! transaction of the process event that changed the park, sequenced by
//! `process_park_clock` so `seq` order is commit order. The ledger outlives
//! the process row: a prune or a session deletion that removes the row keeps
//! its transitions, so the table holds no process foreign key.
//!
//! `kind` is the transition's class and `cause` what ended it — a
//! `ParkEventKind` decoded across the two columns plus `reason_json`, which
//! only a `Parked` row carries. Reads are cursor pages (`seq > ?`);
//! compaction deletes at or below a host-chosen cursor and raises the clock's
//! horizon so a stale cursor is refused typed rather than silently partial.

/// The table's unprefixed name.
pub const TABLE: &str = "process_park_events";

/// Every column an event row carries, in insert order.
pub const INSERT_COLUMNS: &str = "seq, process_id, park_id, kind, cause, reason_json, at_ms";

/// The read projection a feed page decodes.
pub const EVENT_COLUMNS: &str = "seq, process_id, park_id, kind, cause, reason_json, at_ms";

crate::statements! {
    /// `process_park_events` statements both backends issue verbatim.
    pub struct ProcessParkEventStatements @ "process_park_event" {
        /// Append transition `?4`/`?5` of park `?3` of process `?2`,
        /// sequenced `?1`, carrying reason `?6` when it is a `Parked`, at
        /// `?7`.
        insert_event = "INSERT INTO process_park_events (seq, process_id, park_id, kind, cause, reason_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

        /// The `?2` oldest events strictly after cursor `?1`, in commit order.
        select_events_after = "SELECT seq, process_id, park_id, kind, cause, reason_json, at_ms
             FROM process_park_events
             WHERE seq > ?1
             ORDER BY seq
             LIMIT ?2";

        /// Drop every event at or below cursor `?1`. Host-gated compaction;
        /// the clock's horizon is raised in the same transaction so a cursor
        /// at or below it is refused typed.
        delete_events_through = "DELETE FROM process_park_events
             WHERE seq <= ?1";
    }
}
