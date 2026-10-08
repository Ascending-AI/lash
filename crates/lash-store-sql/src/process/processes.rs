//! `processes`: one row per registered process.
//!
//! The row is wide and its authoritative form is `record_json`; the other
//! columns are the indexed projection a non-terminal scan, a retention sweep or a
//! change feed filters on. No caller reads the whole row, so this module
//! declares the **named** projections that exist and nothing else.
//!
//! `status`, `last_event_sequence` and `cancel_requested_at_ms` are generated
//! by the database from `record_json` in both dialects: a statement reads and
//! filters on them and none writes them, so a save is the record alone.
//!
//! `status` carries domain vocabulary (`lash_core::ProcessStatus`). A
//! statement here names a lifecycle partition as a `{{term(column)}}` token
//! and never spells it.

/// The table's unprefixed name.
pub const TABLE: &str = "processes";

/// Every column a statement writes, in insert order. The only statements
/// that name all of them are the two backends' registration inserts.
pub const INSERT_COLUMNS: &str = "process_id, start_key, originator_id,
                identity_kind, identity_label, created_at_ms, updated_at_ms,
                change_seq, lifetime_scope_kind, lifetime_scope_id,
                lifetime, record_json, consumer_hold_key,
                consumer_hold_scope_kind, consumer_hold_scope_id, consumer_hold_cancels";

/// What the change feed reports for a live row.
///
/// The feed unions live rows with tombstones, so both arms carry the same
/// three result columns: the sequence the caller resumes from, the kind that
/// says which arm produced the row, and the payload. The literal `'upsert'` is
/// the arm's name, not a lifecycle label.
pub const CHANGE_FEED_UPSERT_COLUMNS: &str = "change_seq, 'upsert' AS kind, record_json AS payload";

crate::statements! {
    /// `processes` statements both backends issue verbatim.
    pub struct ProcessStatements @ "process" {
        /// The stored record for `?1`.
        select_record_json_by_id = "SELECT record_json FROM processes WHERE process_id = ?1";

        /// The retained process registered under start key `?1`, if any: the
        /// idempotency half of a registration (ADR 0107).
        select_record_json_by_start_key = "SELECT record_json FROM processes WHERE start_key = ?1";

        exists_by_id = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)";

        /// Release the consumer hold `?2` holds on `?1` (ADR 0116 §3.6): a
        /// no-op when the row carries another hold or none.
        release_consumer_hold = "UPDATE processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL, consumer_hold_cancels = NULL
             WHERE process_id = ?1 AND consumer_hold_key = ?2";

        /// The processes held under the consumer hold `?1` whose call owes
        /// them a cancel now that it is abandoned (ADR 0116 §3.4).
        select_owed_cancels = "SELECT process_id FROM processes
             WHERE consumer_hold_key = ?1 AND consumer_hold_cancels IS TRUE
             ORDER BY process_id";

        /// Release every consumer hold owned by the scope `(?1, ?2)`: the
        /// scope's close ends every wait its calls still hold.
        release_consumer_holds_owned_by = "UPDATE processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL, consumer_hold_cancels = NULL
             WHERE consumer_hold_scope_kind = ?1 AND consumer_hold_scope_id = ?2";

        /// The identity columns are absent because none of them is mutable.
        update_mutable_columns = "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, record_json = ?4
             WHERE process_id = ?1";

        /// Every live process, whole. The unpaged read behind the in-memory
        /// registry rebuild; the paged non-terminal scans are backend-only because
        /// SQLite pins them to a partial index by name.
        collect_non_terminal_records = "SELECT record_json FROM processes
                         WHERE {{live_process_status(status)}}
                         ORDER BY process_id ASC";
    }
}
