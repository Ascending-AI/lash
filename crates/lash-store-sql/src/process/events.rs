//! `process_events`: the append-only event log of one process.
//!
//! Headers and canonical replay digests are immutable. A release clears only
//! payload columns in bounded batches, preserving cancellation requests.
//! The atomic prefix horizon lives in `process_event_horizons`; reads never
//! serve an event at or below it. Lite pages select no JSON columns.

/// The table's unprefixed name.
pub const TABLE: &str = "process_events";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "process_id, sequence, event_type, idempotency_key, header_json, payload_json, replay_digest";

crate::statements! {
    /// `process_events` statements both backends issue verbatim.
    pub struct EventStatements @ "process_event" {
        /// The event process `?1` already recorded under replay key `?2`, if
        /// any, with its optional payload and immutable replay digest:
        /// the idempotency half of an append.
        select_by_replay_key = "SELECT header_json, payload_json, replay_digest
                 FROM process_events
                 WHERE process_id = ?1 AND idempotency_key = ?2";

        /// One indexed event under process `?1` and replay key `?2`, bounded
        /// by the snapshot sequence `?3`. The caller checks its sequence
        /// against the release horizon before decoding the payload.
        read_by_replay_key = "SELECT sequence, (substr(header_json, 1, length(header_json) - 1) || ',\"payload\":' || COALESCE(payload_json, 'null') || '}') FROM process_events
                 WHERE process_id = ?1 AND idempotency_key = ?2 AND sequence <= ?3";

        /// The highest sequence process `?1` has recorded, or `NULL` when it
        /// has recorded none.
        select_max_sequence = "SELECT MAX(sequence) FROM process_events WHERE process_id = ?1";

        /// At most `?3` full rows process `?1` recorded after `?2`.
        page_full = "SELECT (substr(header_json, 1, length(header_json) - 1) || ',\"payload\":' || COALESCE(payload_json, 'null') || '}') FROM process_events
                 WHERE process_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3";

        /// The same bounded read without selecting any JSON.
        page_lite = "SELECT sequence, event_type FROM process_events
                 WHERE process_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3";

        /// The last `?2` retained events of process `?1`, newest first: a
        /// released row is never part of the tail. The caller reverses them;
        /// the descending order is what lets the primary key serve the limit.
        list_recent = "SELECT (substr(header_json, 1, length(header_json) - 1) || ',\"payload\":' || COALESCE(payload_json, 'null') || '}') FROM process_events
                         WHERE process_id = ?1
                           AND sequence > COALESCE(
                               (SELECT released_through FROM process_event_horizons
                                WHERE process_id = ?1),
                               0)
                         ORDER BY sequence DESC LIMIT ?2";

        /// Bounds and size of the next release page, without loading payloads.
        page_release = "SELECT MAX(sequence), COUNT(*) FROM (
                 SELECT sequence FROM process_events
                 WHERE process_id = ?1 AND sequence > ?2 AND sequence <= ?3
                 ORDER BY sequence ASC LIMIT ?4
             ) AS release_page";

        /// Release one bounded interval, preserving the cancellation kind `?4`.
        release = "UPDATE process_events SET payload_json = NULL
                 WHERE process_id = ?1 AND sequence > ?2 AND sequence <= ?3
                   AND event_type != ?4";

        /// Record one event: `?1` process, `?2` sequence, `?3` type, `?4`
        /// replay key, `?5` header, `?6` payload and `?7` replay digest.
        insert = "INSERT INTO process_events (
                        process_id, sequence, event_type, idempotency_key, header_json, payload_json, replay_digest
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
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
            "SELECT (substr(header_json, 1, length(header_json) - 1) || ',\"payload\":' || COALESCE(payload_json, 'null') || '}') FROM main.process_events
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
            "SELECT (substr(header_json, 1, length(header_json) - 1) || ',\"payload\":' || COALESCE(payload_json, 'null') || '}') FROM lash_process_events
                 WHERE process_id = $1 AND sequence > $2
                 ORDER BY sequence ASC LIMIT $3"
        );
        assert_eq!(
            postgres.page_lite.sql(),
            "SELECT sequence, event_type FROM lash_process_events
                 WHERE process_id = $1 AND sequence > $2
                 ORDER BY sequence ASC LIMIT $3"
        );
        assert!(!postgres.page_lite.sql().contains("payload_json"));
    }
}
