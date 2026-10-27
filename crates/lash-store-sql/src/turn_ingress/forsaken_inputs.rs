//! `forsaken_turn_inputs`: the source keys a host closed before any input
//! was filed under them.
//!
//! One row per `(session, source key)`, written by the session's forsake
//! under the session's write authority, the same authority every admission
//! takes before it reads the key, so a forsake and an admission of one key
//! are serialized and exactly one of them holds it. A tombstone and a
//! `pending_turn_inputs` row never share a key. `input_id` is the id a send
//! under the key would have been filed under, so a follower's status read by
//! that id finds the tombstone.
//!
//! A tombstone lives as long as the settled input's idempotency record it
//! stands in for: it is deleted with its session, and no vacuum reclaims it.

/// The table's unprefixed name.
pub const TABLE: &str = "forsaken_turn_inputs";

crate::statements! {
    /// `forsaken_turn_inputs` statements both backends issue verbatim.
    pub struct ForsakenInputStatements @ "forsaken_turn_input" {
        /// Forsake source key `?2` of session `?1`, whose input would be
        /// `?3`, at `?4`, keeping a tombstone an earlier forsake wrote. The
        /// caller has read under the session's write authority that no input
        /// holds the key.
        insert = "INSERT INTO forsaken_turn_inputs (session_id, source_key, input_id, forsaken_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id, source_key) DO NOTHING";



        /// Reclaim session `?1`'s tombstones with the session.
        delete_by_session = "DELETE FROM forsaken_turn_inputs WHERE session_id = ?1";
    }
}
