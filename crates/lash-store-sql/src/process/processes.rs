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

/// What the unrecorded-opener-parent survey reports per scope: the canonical
/// parent key, its kind, and one record carrying it.
///
/// Narrow on purpose, and dialect-spelled twice because the two backends pick
/// their representative row differently. `parent_scope_id` is the
/// collision-free projection and is never parsed back; the kind disambiguates
/// a turn from a queue drain for the caller's confirmation read;
/// `record_json` is the authority. Every row sharing the key names the same
/// typed parent, so any child row answers for the group — SQLite picks it
/// with `MIN`, Postgres with `DISTINCT ON` — and neither needs an indexed
/// column the WHERE clause has already read.
pub const UNRECORDED_OPENER_PARENT_COLUMNS_SQLITE: &str =
    "child.parent_scope_id, child.parent_scope_kind, MIN(child.record_json)";
pub const UNRECORDED_OPENER_PARENT_COLUMNS_POSTGRES: &str =
    "ON (child.parent_scope_id) child.parent_scope_id, child.parent_scope_kind, child.record_json";

crate::statements! {
    /// `processes` statements both backends issue verbatim.
    pub struct ProcessStatements @ "process" {
        /// The stored record for `?1`.
        select_record_json_by_id = "SELECT record_json FROM processes WHERE process_id = ?1";

        exists_by_id = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)";

        /// The session `?1`'s wakes are delivered to, if any.
        select_wake_session_id = "SELECT wake_session_id FROM processes WHERE process_id = ?1";

        /// Retarget `?1`'s wake session to `?2`.
        set_wake_session_id = "UPDATE processes SET wake_session_id = ?2 WHERE process_id = ?1";

        clear_wake_session_for_session = "UPDATE processes SET wake_session_id = NULL WHERE wake_session_id = ?1";

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
