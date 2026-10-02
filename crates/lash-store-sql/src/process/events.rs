//! `process_events`: the append-only event log of one process.
//!
//! Full page reads select `event_json`. Lite page reads select only the indexed
//! ordering position and event type, so payload bytes never cross the database
//! boundary when a host asks for metadata.
//!
//! A host release (FIG-3482) rewrites the rows of a prefix in place: the event
//! keeps everything but its payload, and `released_payload_digest` holds the
//! payload's digest so the replay-key fence still matches a re-presented
//! append. The prefix's horizon lives in `process_event_horizons`; reads never
//! serve a row at or below it.

/// The table's unprefixed name.
pub const TABLE: &str = "process_events";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "process_id, sequence, event_type, idempotency_key, event_json";

crate::statements! {
    /// `process_events` statements both backends issue verbatim.
    pub struct EventStatements @ "process_event" {
        /// The event process `?1` already recorded under replay key `?2`, if
        /// any, and its payload digest when a release stripped its payload:
        /// the idempotency half of an append.
        select_by_replay_key = "SELECT event_json, released_payload_digest
                 FROM process_events
                 WHERE process_id = ?1 AND idempotency_key = ?2";

        /// The highest sequence process `?1` has recorded, or `NULL` when it
        /// has recorded none.
        select_max_sequence = "SELECT MAX(sequence) FROM process_events WHERE process_id = ?1";

        /// At most `?3` full rows process `?1` recorded after `?2`.
        page_full = "SELECT event_json FROM process_events
                 WHERE process_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3";

        /// The same bounded read without selecting `event_json`.
        page_lite = "SELECT sequence, event_type FROM process_events
                 WHERE process_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3";

        /// The last `?2` retained events of process `?1`, newest first: a
        /// released row is never part of the tail. The caller reverses them;
        /// the descending order is what lets the primary key serve the limit.
        list_recent = "SELECT event_json FROM process_events
                         WHERE process_id = ?1
                           AND sequence > COALESCE(
                               (SELECT released_through FROM process_event_horizons
                                WHERE process_id = ?1),
                               0)
                         ORDER BY sequence DESC LIMIT ?2";

        /// How many `?2`-typed events process `?1` recorded at or before
        /// sequence `?3`.
        count_by_type_through_sequence = "SELECT COUNT(*) FROM process_events
                 WHERE process_id = ?1 AND event_type = ?2 AND sequence <= ?3";

        /// At most `?4` rows of process `?1` after `?2` and at or below `?3`,
        /// in order: the next page of a prefix release.
        page_release = "SELECT sequence, event_json FROM process_events
                 WHERE process_id = ?1 AND sequence > ?2 AND sequence <= ?3
                 ORDER BY sequence ASC LIMIT ?4";

        /// Release event `?2` of process `?1`: `?3` is the event without its
        /// payload and `?4` the payload's digest, `NULL` for an event whose
        /// payload a release keeps.
        release = "UPDATE process_events
                 SET event_json = ?3, released_payload_digest = ?4
                 WHERE process_id = ?1 AND sequence = ?2";

        /// Record one event: `?1` process, `?2` sequence, `?3` type, `?4`
        /// replay key, `?5` event.
        insert = "INSERT INTO process_events (
                        process_id, sequence, event_type, idempotency_key, event_json
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5)";
    }
}

#[cfg(test)]
mod tests {
    use super::EventStatements;
    use crate::{Dialect, SchemaTables, TableLayout};

    const MAIN: TableLayout = TableLayout::new(&[SchemaTables::new("main", crate::TABLES)]);

    #[test]
    fn rendered_event_pages_pin_limit_and_payload_free_lite_projection() {
        let sqlite = EventStatements::render(Dialect::sqlite(MAIN));
        assert_eq!(
            sqlite.page_full.sql(),
            "SELECT event_json FROM main.process_events
                 WHERE process_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3"
        );
        assert_eq!(
            sqlite.page_lite.sql(),
            "SELECT sequence, event_type FROM main.process_events
                 WHERE process_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3"
        );

        let postgres = EventStatements::render(Dialect::postgres());
        assert_eq!(
            postgres.page_full.sql(),
            "SELECT event_json FROM lash_process_events
                 WHERE process_id = $1 AND sequence > $2
                 ORDER BY sequence ASC LIMIT $3"
        );
        assert_eq!(
            postgres.page_lite.sql(),
            "SELECT sequence, event_type FROM lash_process_events
                 WHERE process_id = $1 AND sequence > $2
                 ORDER BY sequence ASC LIMIT $3"
        );
        assert!(!postgres.page_lite.sql().contains("event_json"));
    }
}
