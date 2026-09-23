//! `turn_parks`: the parked state of a driver-run turn (FIG-3586, FIG-3600),
//! one row per session whose turn aborted on a refusal that parks it.
//!
//! A park is live exactly while its turn is: any commit of the session clears
//! it in the commit's transaction, as does a cancel that withdraws the parked
//! turn's last held work, a queued-run settlement, and the session's deletion.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_parks";

crate::statements! {
    /// `turn_parks` statements both backends issue verbatim.
    pub struct TurnParkStatements @ "turn_park" {
        /// Park session `?1`'s turn `?2`, replacing any park it holds.
        upsert = "INSERT INTO turn_parks (session_id, turn_id, reason_json, parked_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id) DO UPDATE SET
                turn_id = excluded.turn_id,
                reason_json = excluded.reason_json,
                parked_at_ms = excluded.parked_at_ms";

        select_by_session = "SELECT turn_id, reason_json, parked_at_ms
             FROM turn_parks
             WHERE session_id = ?1";

        delete_by_session = "DELETE FROM turn_parks WHERE session_id = ?1";

        /// Clear session `?1`'s park when it is turn `?2`'s: that turn
        /// committed, so it is no longer parked.
        delete_for_turn = "DELETE FROM turn_parks WHERE session_id = ?1 AND turn_id = ?2";
    }
}
