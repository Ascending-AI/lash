//! `control_intents`: an operator's verb on a parked root, or a session's
//! close, as a versioned record (ADR 0104 O4). The store half of the intent
//! commits in the transaction that inserts the row; `state` tracks the engine
//! half. A `close_session` row outlives its session: it is the deletion
//! tombstone a deleted session's roots answer from.

/// The table's unprefixed name.
pub const TABLE: &str = "control_intents";

/// Every column a row decoder reads, in the order both backends index.
pub const ROW_COLUMNS: &str =
    "intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref";

/// What recording an intent writes: [`ROW_COLUMNS`] minus the allocated
/// `intent_id`, plus the `kind`/`state` tags beside their JSON bodies so a
/// reader filters on the tag without decoding.
pub const INSERT_COLUMNS: &str =
    "session_id, format, kind, kind_json, state, state_json, attempts, created_at_ms, engine_ref";

crate::statements! {
    /// `control_intents` statements both backends issue verbatim.
    pub struct ControlIntentStatements @ "control_intent" {
        /// Open intents (pending, or failed and retryable) after id `?1`, in
        /// id order, at most `?2`.
        select_open_after = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE intent_id > ?1 AND state IN ('pending', 'failed_retryable')
             ORDER BY intent_id
             LIMIT ?2";

        /// Session `?1`'s `close_session` intent: its deletion tombstone.
        select_close_session = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE session_id = ?1 AND kind = 'close_session'
             ORDER BY intent_id
             LIMIT 1";

        /// Intent `?1`.
        select_by_id = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE intent_id = ?1";

        /// Session `?1`'s open verbs (pending, or failed and retryable), in
        /// id order: what its close supersedes.
        select_open_verbs_by_session = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
             FROM control_intents
             WHERE session_id = ?1 AND kind <> 'close_session'
               AND state IN ('pending', 'failed_retryable')
             ORDER BY intent_id";

        /// Record a new intent of session `?1` (format `?2`, kind `?3` with
        /// JSON `?4`, state `?5` with JSON `?6`, instant `?7`, engine handle
        /// `?8`), answering its allocated id.
        insert = "INSERT INTO control_intents
                 (session_id, format, kind, kind_json, state, state_json, attempts, created_at_ms, engine_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)
             RETURNING intent_id";

        /// Move intent `?1` to state `?2` (JSON `?3`) at attempt count `?4`,
        /// if it is still at state JSON `?5` and attempt count `?6`: a
        /// compare-and-set, so zero rows means another writer moved it first.
        update_state = "UPDATE control_intents
             SET state = ?2, state_json = ?3, attempts = ?4
             WHERE intent_id = ?1 AND state_json = ?5 AND attempts = ?6";

        /// Every intent of session `?1` but its `close_session` tombstone:
        /// the part of its deletion that forgets the verbs.
        delete_verbs_by_session = "DELETE FROM control_intents
             WHERE session_id = ?1 AND kind <> 'close_session'";
    }
}
