//! `processes`: one row per registered process.
//!
//! The row is wide and its authoritative form is `record_json`; the other
//! columns are the indexed projection a worklist, a retention sweep or a
//! change feed filters on. No caller reads the whole row, so this module
//! declares the **named** projections that exist and nothing else.
//!
//! `status` carries domain vocabulary (`lash_core::ProcessStatus`). A
//! statement here names a lifecycle partition as a `{{term(column)}}` token
//! and never spells it; the ownership gate refuses the literal.

/// The table's unprefixed name.
pub const TABLE: &str = "processes";

/// Every column, in insert order. The only statements that name all of them
/// are the two backends' registration inserts.
pub const INSERT_COLUMNS: &str = "process_id, incarnation, registration_fingerprint, originator_id,
                wake_session_id, identity_kind, identity_label, created_at_ms, updated_at_ms,
                last_event_sequence, change_seq, status, parent_scope_kind, parent_scope_id,
                on_parent_end, cancel_requested_at_ms, record_json";

/// The key and the record: what a read reports when the caller needs both the
/// row's identity and its contents.
///
/// Narrow on purpose, and the only two-column projection of this table. A
/// prune candidate is reported by the key it is pruned by plus the record that
/// names its artifacts; a `UNION ALL` arm carries the key it is ordered by
/// beside the record the caller decodes. Neither needs an indexed column it
/// has already filtered on, because every one of them is in the record.
pub const KEYED_RECORD_COLUMNS: &str = "process_id, record_json";

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

        /// Whether `?1` is registered, without reading any of its row.
        exists_by_id = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)";

        /// The session `?1`'s wakes are delivered to, if any.
        select_wake_session_id = "SELECT wake_session_id FROM processes WHERE process_id = ?1";

        /// Retarget `?1`'s wake session to `?2`.
        set_wake_session_id = "UPDATE processes SET wake_session_id = ?2 WHERE process_id = ?1";

        /// Drop every wake subscription aimed at session `?1`, which is going
        /// away.
        clear_wake_session_for_session = "UPDATE processes SET wake_session_id = NULL WHERE wake_session_id = ?1";

        /// Write back the columns a process event can move: `?1` process,
        /// `?2` updated-at, `?3` change sequence, `?4` status, `?5` last event
        /// sequence, `?6` cancel-requested-at, `?7` record.
        ///
        /// The identity columns are absent because none of them is mutable:
        /// a re-registration writes a new row rather than rewriting this one.
        update_mutable_columns = "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, status = ?4,
                 last_event_sequence = ?5, cancel_requested_at_ms = ?6, record_json = ?7
             WHERE process_id = ?1";

        /// Every live process, whole. The unpaged read behind the in-memory
        /// worklist rebuild; the paged worklist scans are dialect-only because
        /// SQLite pins them to a partial index by name.
        collect_non_terminal_records = "SELECT record_json FROM processes
                         WHERE {{live_process_status(status)}}
                         ORDER BY process_id ASC";
    }
}
