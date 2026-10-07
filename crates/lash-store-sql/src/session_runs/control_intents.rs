//! `control_intents`: a session's close, as a versioned record. The store
//! half of the intent commits in the transaction that inserts the row and
//! wakes the session actor; `state` records whether the session's close
//! steps finished it. A `close_session` row outlives its session: it is the
//! deletion tombstone a deleted session's runs answer from.

/// The table's unprefixed name.
pub const TABLE: &str = "control_intents";

/// Every column a row decoder reads, in the order both backends index.
pub const ROW_COLUMNS: &str = "intent_id, session_id, format, kind_json, state_json, created_at_ms";

/// What recording an intent writes: [`ROW_COLUMNS`] minus the allocated
/// `intent_id`, plus the `kind`/`state` tags beside their JSON bodies so a
/// reader filters on the tag without decoding.
pub const INSERT_COLUMNS: &str =
    "session_id, format, kind, kind_json, state, state_json, created_at_ms";

crate::statements! {
    /// `control_intents` statements both backends issue verbatim.
    pub struct ControlIntentStatements @ "control_intent" {
        /// Session `?1`'s `close_session` intent: its deletion tombstone.
        select_close_session = "SELECT intent_id, session_id, format, kind_json, state_json, created_at_ms
             FROM control_intents
             WHERE session_id = ?1 AND kind = 'close_session'
             ORDER BY intent_id
             LIMIT 1";

        /// Intent `?1`.
        select_by_id = "SELECT intent_id, session_id, format, kind_json, state_json, created_at_ms
             FROM control_intents
             WHERE intent_id = ?1";

        /// At most `?2` intents after id `?1`, in id order: the operator's
        /// listing.
        list_after = "SELECT intent_id, session_id, format, kind_json, state_json, created_at_ms
             FROM control_intents
             WHERE intent_id > ?1
             ORDER BY intent_id
             LIMIT ?2";

        /// Record a new intent of session `?1` (format `?2`, kind `?3` with
        /// JSON `?4`, state `?5` with JSON `?6`, instant `?7`), answering its
        /// allocated id.
        insert = "INSERT INTO control_intents
                 (session_id, format, kind, kind_json, state, state_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING intent_id";

        /// Move intent `?1` to state `?2` (JSON `?3`) if it is still at
        /// state JSON `?4`: a compare-and-set, so zero rows means another
        /// writer moved it first.
        update_state = "UPDATE control_intents
             SET state = ?2, state_json = ?3
             WHERE intent_id = ?1 AND state_json = ?4";
    }
}
