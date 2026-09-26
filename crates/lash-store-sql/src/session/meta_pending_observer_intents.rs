//! `session_meta_pending_observer_intents`: the ordered observer intents a
//! session's metadata carries, one row per intent.

/// The table's unprefixed name.
pub const TABLE: &str = "session_meta_pending_observer_intents";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, process_index, process_id";

/// One intent as the metadata load reads it back.
///
/// The full row minus `session_id`, which the read is already keyed by.
/// `process_index` stays: the decoder refuses a non-contiguous sequence, so
/// the index is evidence rather than an ordering hint.
pub const INTENT_COLUMNS: &str = "process_index, process_id";

crate::statements! {
    /// `session_meta_pending_observer_intents` statements both backends issue
    /// verbatim.
    pub struct ObserverIntentStatements @ "session_meta_observer_intent" {
        /// Session `?1`'s observer intents, in index order.
        select_for_session = "SELECT process_index, process_id
             FROM session_meta_pending_observer_intents
             WHERE session_id = ?1 ORDER BY process_index";

        insert = "INSERT INTO session_meta_pending_observer_intents
             (session_id, process_index, process_id)
             VALUES (?1, ?2, ?3)";

        delete_by_session =
            "DELETE FROM session_meta_pending_observer_intents WHERE session_id = ?1";
    }
}
