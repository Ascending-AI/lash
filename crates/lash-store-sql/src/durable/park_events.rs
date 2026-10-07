//! The operator's park feed: the neutral statements of the `park_events` domain (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).

/// The table's unprefixed name.
pub const TABLE: &str = "park_events";

crate::statements! {
    /// `park_events` statements both backends issue verbatim.
    pub struct ParkEventStatements @ "durable_park_events" {
        /// Append an entry for actor `?1` of kind `?2` with reason `?3` at `?4`.
        append = "INSERT INTO park_events (actor_key, kind, reason_json, at_ms)
             VALUES (?1, ?2, ?3, ?4)";

        /// Up to `?2` entries after position `?1`, oldest first.
        page = "SELECT seq, actor_key, kind, reason_json, at_ms FROM park_events
             WHERE seq > ?1
             ORDER BY seq
             LIMIT ?2";
    }
}
