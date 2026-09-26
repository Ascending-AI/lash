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
pub const INSERT_COLUMNS: &str = "process_id, start_key, originator_id,
                wake_session_id, identity_kind, identity_label, created_at_ms, updated_at_ms,
                last_event_sequence, change_seq, status, parent_scope_kind, parent_scope_id,
                on_parent_end, cancel_requested_at_ms, record_json";

/// The parked projection's columns, written by every fold that changes the
/// park and `NULL` at registration (FIG-3659 NOW-B). A `retired_generation`
/// park also names its retired executable generation (FIG-3571).
pub const PARKED_COLUMNS: &str = "parked_since_ms, parked_reason_code, park_executable_generation";

/// The grouped read `summarize_parked` answers drain and the parked-work
/// gauges from: each reason's live park count and its oldest `since_ms`, and
/// none of the record's payload.
pub const PARK_SUMMARY_COLUMNS: &str = "parked_reason_code, COUNT(*), MIN(parked_since_ms)";

/// The grouped count `count_retired_parks_by_executable_generation` reads for
/// drain status (FIG-3571).
///
/// Narrow on purpose: the deployment drain wants each retired executable
/// generation's live process park count and nothing else, so the projection
/// carries the projected generation column and the aggregate — none of the
/// record's payload.
pub const EXECUTABLE_GENERATION_COUNT_COLUMNS: &str = "park_executable_generation, COUNT(*)";

/// The key and the record: what a read reports when the caller needs both the
/// row's identity and its contents.
///
/// Narrow on purpose, and the only two-column projection of this table. A
/// prune candidate is reported by the key it is pruned by plus the record that
/// names its artifacts; a `UNION ALL` arm carries the key it is ordered by
/// beside the record the caller decodes. Neither needs an indexed column it
/// has already filtered on, because every one of them is in the record.
pub const KEYED_RECORD_COLUMNS: &str = "process_id, record_json";

/// What the preflight's started-process walk reports per live process
/// (FIG-3571): the key it pages by, the status and wake session a drain list
/// names it by, and the record whose start stamp and input the probe compares.
///
/// Narrow on purpose: none of the indexed projections the record already
/// holds, only the key and the two columns a drain list shows beside it.
pub const PREFLIGHT_STARTED_COLUMNS: &str = "process_id, status, wake_session_id, record_json";

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

        /// The retained process registered under start key `?1`, if any: the
        /// idempotency half of a registration (ADR 0107).
        select_record_json_by_start_key = "SELECT record_json FROM processes WHERE start_key = ?1";

        exists_by_id = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)";

        /// The session `?1`'s wakes are delivered to, if any.
        select_wake_session_id = "SELECT wake_session_id FROM processes WHERE process_id = ?1";

        /// Retarget `?1`'s wake session to `?2`.
        set_wake_session_id = "UPDATE processes SET wake_session_id = ?2 WHERE process_id = ?1";

        clear_wake_session_for_session = "UPDATE processes SET wake_session_id = NULL WHERE wake_session_id = ?1";

        /// The identity columns are absent because none of them is mutable.
        /// `?8`/`?9` are the parked projection: the live park's `since_ms`
        /// and reason code, both `NULL` while the process is not parked.
        /// `?10` is the retired executable generation a `retired_generation`
        /// park names, `NULL` for any other park (FIG-3571).
        update_mutable_columns = "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, status = ?4,
                 last_event_sequence = ?5, cancel_requested_at_ms = ?6, record_json = ?7,
                 parked_since_ms = ?8, parked_reason_code = ?9, park_executable_generation = ?10
             WHERE process_id = ?1";

        /// Live process parks per reason code, with each code's oldest
        /// `since_ms`, over the parked projection's partial index.
        summarize_parked = "SELECT parked_reason_code, COUNT(*), MIN(parked_since_ms)
             FROM processes
             WHERE parked_since_ms IS NOT NULL
             GROUP BY parked_reason_code";

        /// Live retired-generation process parks grouped by the generation
        /// their start recorded (FIG-3571): read off the projected, indexed
        /// `park_executable_generation` column, never the record.
        count_retired_parks_by_executable_generation = "SELECT park_executable_generation, COUNT(*)
             FROM processes
             WHERE park_executable_generation IS NOT NULL
             GROUP BY park_executable_generation";

        /// Every live process, whole. The unpaged read behind the in-memory
        /// worklist rebuild; the paged worklist scans are dialect-only because
        /// SQLite pins them to a partial index by name.
        collect_non_terminal_records = "SELECT record_json FROM processes
                         WHERE {{live_process_status(status)}}
                         ORDER BY process_id ASC";
    }
}
