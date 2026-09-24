//! `turn_park_events`: the durable feed of turn park transitions (FIG-3659).
//!
//! One row per transition — `Parked`, `Unparked`, `Cancelled` — appended in
//! the transaction that changed the park, sequenced by `turn_park_clock` so
//! `seq` order is commit order. The ledger survives the session's deletion: a
//! `Cancelled{SessionDeleted}` event names a park whose session rows are
//! gone, so the table holds no session foreign key.
//!
//! `kind` is the transition's class and `cause` what ended it — a
//! `TurnParkEventKind` decoded across the two columns plus `reason_json`,
//! which only a `Parked` row carries. Reads are cursor pages (`seq > ?`);
//! compaction deletes at or below a host-chosen cursor and raises the clock's
//! horizon so a stale cursor is refused typed rather than silently partial.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_park_events";

/// Every column an event row carries, in insert order.
pub const INSERT_COLUMNS: &str =
    "seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms";

/// The read projection a feed page decodes.
pub const EVENT_COLUMNS: &str =
    "seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms";

crate::statements! {
    /// `turn_park_events` statements both backends issue verbatim.
    pub struct TurnParkEventStatements @ "turn_park_event" {
        /// Append transition `?5`/`?6` of park `?4` in session `?2`'s turn
        /// `?3`, sequenced `?1`, carrying reason `?7` when it is a `Parked`,
        /// at `?8`.
        insert_event = "INSERT INTO turn_park_events (seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

        /// The `?2` oldest events strictly after cursor `?1`, in commit order.
        select_events_after = "SELECT seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms
             FROM turn_park_events
             WHERE seq > ?1
             ORDER BY seq
             LIMIT ?2";

        /// Drop every event at or below cursor `?1`. Host-gated compaction;
        /// the clock's horizon is raised in the same transaction so a cursor
        /// at or below it is refused typed.
        delete_events_through = "DELETE FROM turn_park_events
             WHERE seq <= ?1";
    }
}
