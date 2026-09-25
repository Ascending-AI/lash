//! `turn_parks` and `turn_park_clock` statements only SQLite issues.
//!
//! The list fork is the reason set: a variable-length code list binds as one
//! JSON array through `json_each`, where PostgreSQL binds a text array through
//! `= ANY(...)`. The clock forks on the singleton flag — `INTEGER 1` here,
//! `BOOLEAN TRUE` on PostgreSQL — and on the bump reporting its value:
//! PostgreSQL's `RETURNING` folds the read into the update's round trip, where
//! SQLite issues the two statements back to back under its write lock.

lash_store_sql::statements! {
    /// `turn_parks` statements only SQLite issues.
    pub(crate) struct TurnParkSqliteStatements @ "turn_park" {
        /// The deployment's parked turns as a `?1`-row page in
        /// `(since_ms, session_id)` order: optionally one session `?2`, only
        /// parks at or before `?3`, strictly after keyset `?4`/`?5`, reason
        /// codes drawn from the JSON array `?6` (`NULL` means all).
        list = "SELECT session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts
             FROM turn_parks
             WHERE (?2 IS NULL OR session_id = ?2)
               AND (?3 IS NULL OR since_ms <= ?3)
               AND (?4 IS NULL OR since_ms > ?4 OR (since_ms = ?4 AND session_id > ?5))
               AND (?6 IS NULL OR reason_code IN (SELECT value FROM json_each(?6)))
             ORDER BY since_ms, session_id
             LIMIT ?1";
    }
}

lash_store_sql::statements! {
    /// `turn_park_clock` statements only SQLite issues.
    pub(crate) struct TurnParkClockSqliteStatements @ "turn_park_clock" {
        /// Allocate one feed sequence. The write transaction's lock orders
        /// writers, so the allocated order is commit order.
        bump = "UPDATE turn_park_clock SET current_seq = current_seq + 1
             WHERE singleton = 1";

        /// The sequence the last `bump` allocated.
        select_current = "SELECT current_seq FROM turn_park_clock
             WHERE singleton = 1";

        /// The cursor below which a feed read is refused
        /// `ParkFeedCursorCompacted`.
        select_compaction_horizon = "SELECT compaction_horizon FROM turn_park_clock
             WHERE singleton = 1";

        /// Raise the compaction horizon to `?1` when it is higher.
        raise_compaction_horizon = "UPDATE turn_park_clock
             SET compaction_horizon = MAX(compaction_horizon, ?1)
             WHERE singleton = 1";
    }
}

/// Live retired-generation parks per the generation their admission recorded
/// (FIG-3571), read off the projected `park_executable_generation` column.
pub(crate) fn count_retired_parks_by_executable_generation(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<std::collections::BTreeMap<lash_core_execution::ExecutableGeneration, usize>>
{
    let mut statement = conn.prepare(
        super::turn_ingress_sql()
            .family
            .count_retired_parks_by_executable_generation
            .sql(),
    )?;
    statement
        .query_map([], |row| {
            Ok((
                lash_core_execution::ExecutableGeneration::new(row.get::<_, String>(0)?),
                usize::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
            ))
        })?
        .collect()
}
