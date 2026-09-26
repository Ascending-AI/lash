//! `turn_parks` and `turn_park_clock` statements only PostgreSQL issues.
//!
//! The list fork is the reason set: a variable-length code list binds as a
//! text array through `= ANY(...)`, where SQLite binds one JSON array through
//! `json_each`. The clock forks on the singleton flag — `BOOLEAN TRUE` here,
//! `INTEGER 1` on SQLite — and on the bump reporting its value through
//! `RETURNING` in the same round trip that takes the row lock, where SQLite
//! issues the update and the read separately under its write lock.

lash_store_sql::statements! {
    /// `turn_parks` statements only PostgreSQL issues.
    pub(crate) struct TurnParkPostgresStatements @ "turn_park" {
        /// The deployment's parked turns as a `?1`-row page in
        /// `(since_ms, session_id)` order: optionally one session `?2`, only
        /// parks at or before `?3`, strictly after keyset `?4`/`?5`, reason
        /// codes drawn from the text array `?6` (`NULL` means all).
        list = "SELECT session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts, park_build_generation
             FROM turn_parks
             WHERE (?2 IS NULL OR session_id = ?2)
               AND (?3 IS NULL OR since_ms <= ?3)
               AND (?4 IS NULL OR since_ms > ?4 OR (since_ms = ?4 AND session_id > ?5))
               AND (?6 IS NULL OR reason_code = ANY(?6))
             ORDER BY since_ms, session_id
             LIMIT ?1";

        /// Session `?1`'s park under a row lock: the write that follows it —
        /// a same-turn re-park or a superseding park — decides on what this
        /// read saw, so under READ COMMITTED the lock is what makes the
        /// decision atomic. The read is the table's full record projection
        /// (`park_build_generation` included, FIG-3795) even though the
        /// decision itself reads only the id, turn and counter columns.
        select_for_update_by_session = "SELECT session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts, park_build_generation
             FROM turn_parks
             WHERE session_id = ?1
             FOR UPDATE";
    }
}

lash_store_sql::statements! {
    /// `turn_park_clock` statements only PostgreSQL issues.
    pub(crate) struct TurnParkClockPostgresStatements @ "turn_park_clock" {
        /// Allocate one feed sequence and report it. The row lock the update
        /// takes orders writers, so the allocated order is commit order.
        bump_returning = "UPDATE turn_park_clock
             SET current_seq = current_seq + 1
             WHERE singleton = TRUE
             RETURNING current_seq";

        /// The cursor below which a feed read is refused
        /// `ParkFeedCursorCompacted`, under a share lock: a compaction that
        /// is still committing must not let a read at a stale cursor pass
        /// unrefused while its events are already gone.
        select_compaction_horizon_for_share = "SELECT compaction_horizon
             FROM turn_park_clock
             WHERE singleton = TRUE
             FOR SHARE";

        /// The allocated sequence under a row lock, read before compaction:
        /// locking the clock first serializes against concurrent bumps, and
        /// clamping `through` to it keeps the horizon from rising past events
        /// the feed has not yet committed.
        select_current_for_update = "SELECT current_seq
             FROM turn_park_clock
             WHERE singleton = TRUE
             FOR UPDATE";

        /// Raise the compaction horizon to `?1`, never lowering it.
        /// `GREATEST` is PostgreSQL's spelling of SQLite's two-argument `MAX`.
        raise_compaction_horizon = "UPDATE turn_park_clock
             SET compaction_horizon = GREATEST(compaction_horizon, ?1)
             WHERE singleton = TRUE";
    }
}
