//! `turn_parks`: the parked state of a driver-run turn (FIG-3586, FIG-3600,
//! FIG-3659), one row per session whose turn aborted on a refusal that parks
//! it.
//!
//! A park is live exactly while its turn is: any commit of the session clears
//! it in the commit's transaction, as does a cancel that withdraws the parked
//! turn's last held work, a queued-run settlement, and the session's deletion.
//! Every clear issues its delete with `RETURNING`, so the transition's feed
//! event — written in the same transaction — names the park that closed.
//!
//! The `list` statement forks per backend rather than sharing: a
//! variable-length reason set binds as one JSON array through `json_each` on
//! SQLite and as a text array through `= ANY(...)` on PostgreSQL.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_parks";

/// Every column a park row carries, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts, park_executable_generation";

/// The stored record's read projection.
pub const RECORD_COLUMNS: &str =
    "session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts";

/// The grouped count `count_parks_by_reason` reads for drain status.
///
/// Narrow on purpose: the deployment drain wants each reason's live park
/// count and nothing else, so the projection carries the reason code and the
/// aggregate — none of the park row's payload columns.
pub const REASON_COUNT_COLUMNS: &str = "reason_code, COUNT(*) AS parks";

/// The grouped count `count_retired_parks_by_executable_generation` reads for
/// drain status (FIG-3571).
///
/// Narrow on purpose: the deployment drain wants each retired executable
/// generation's live park count and nothing else, so the projection carries
/// the projected generation column and the aggregate — none of the park row's
/// payload columns.
pub const EXECUTABLE_GENERATION_COUNT_COLUMNS: &str =
    "park_executable_generation, COUNT(*) AS parks";

crate::statements! {
    /// `turn_parks` statements both backends issue verbatim.
    pub struct TurnParkStatements @ "turn_park" {
        /// Open session `?1`'s first park of turn `?2`: `park_id` `?3` is the
        /// feed sequence the `Parked` event was allocated, `?4` the reason
        /// code, `?5` the reason payload, `?6` the park instant, `?7` the same
        /// instant as `last_refused_ms`, `?8` = 1 attempt, `?9` the retired
        /// generation the reason names (`park_executable_generation`, NULL for any other
        /// reason; FIG-3571).
        insert = "INSERT INTO turn_parks (session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts, park_executable_generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";

        /// Re-park of the same turn `?2` in session `?1`: `park_id` and
        /// `since_ms` are kept, the reason refreshes, `last_refused_ms` moves
        /// to `?5`, `park_executable_generation` to `?6`, and `attempts` counts the
        /// refusal.
        update_same_turn = "UPDATE turn_parks
             SET reason_code = ?3, reason_json = ?4,
                 last_refused_ms = ?5, attempts = attempts + 1, park_executable_generation = ?6
             WHERE session_id = ?1 AND turn_id = ?2";

        /// Drop the park a different turn `?2` supersedes in session `?1`,
        /// returning what the `Unparked{Superseded}` event names.
        delete_for_supersede_returning = "DELETE FROM turn_parks
             WHERE session_id = ?1 AND turn_id <> ?2
             RETURNING turn_id, park_id";

        select_by_session = "SELECT session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts
             FROM turn_parks
             WHERE session_id = ?1";

        /// Clear session `?1`'s park, returning what the closing event names.
        delete_by_session_returning = "DELETE FROM turn_parks
             WHERE session_id = ?1
             RETURNING turn_id, park_id";

        /// Clear session `?1`'s park when it is turn `?2`'s: that turn
        /// committed, so it is no longer parked. The returned row feeds the
        /// `Unparked{TurnCommitted}` event.
        delete_for_turn_returning = "DELETE FROM turn_parks
             WHERE session_id = ?1 AND turn_id = ?2
             RETURNING turn_id, park_id";
    }
}
