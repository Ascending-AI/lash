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

        /// Every intent of session `?1` but its `close_session` tombstone:
        /// the part of its deletion that forgets the verbs.
        delete_verbs_by_session = "DELETE FROM control_intents
             WHERE session_id = ?1 AND kind <> 'close_session'";
    }
}
