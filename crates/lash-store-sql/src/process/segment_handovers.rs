//! `process_segment_handovers`: the parked continuation of a segmented run.

/// The table's unprefixed name.
pub const TABLE: &str = "process_segment_handovers";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "process_id, segment_ordinal, handover_json";

/// What the SQLite preflight walk reports for a parked segment.
///
/// Wide because the walk is a report: it mints the keyset cursor in the
/// projection so the resume filter and the `ORDER BY` cannot disagree, and it
/// carries the owning process's status, wake session and record because a
/// parked continuation can only be judged against the inputs the registry
/// holds. It stays separate from [`PG_WALK_COLUMNS`] because the two walks
/// page differently — a minted text cursor here, a row-value comparison there.
pub const SQLITE_WALK_COLUMNS: &str =
    "handovers.process_id || ':' || printf('%020d', handovers.segment_ordinal) AS walk_cursor,
        handovers.process_id    AS process_id,
        handovers.handover_json AS handover_json,
        processes.status          AS status,
        processes.wake_session_id AS wake_session_id,
        processes.record_json     AS record_json";

/// What the PostgreSQL preflight walk reports for a parked segment.
///
/// The same six facts as [`SQLITE_WALK_COLUMNS`], carrying the ordinal itself
/// rather than a minted cursor: this walk resumes by comparing the two ordered
/// columns as a row value, which cannot disagree with the ordering of the same
/// columns under any collation.
pub const PG_WALK_COLUMNS: &str = "handovers.process_id,
         handovers.segment_ordinal,
         handovers.handover_json,
         process.status,
         process.wake_session_id,
         process.record_json";

crate::statements! {
    /// `process_segment_handovers` statements both backends issue verbatim.
    pub struct SegmentHandoverStatements @ "process_segment_handover" {
        /// The handover parked for process `?1` at segment `?2`.
        select_by_ordinal = "SELECT handover_json FROM process_segment_handovers
                 WHERE process_id = ?1 AND segment_ordinal = ?2";

        /// The newest handover parked for process `?1`.
        select_latest = "SELECT handover_json FROM process_segment_handovers
                 WHERE process_id = ?1
                 ORDER BY segment_ordinal DESC LIMIT 1";

        /// Drop every handover of process `?1` older than the one before `?2`:
        /// a segment keeps its immediate predecessor so a crashed handover can
        /// be re-applied.
        delete_superseded = "DELETE FROM process_segment_handovers
                 WHERE process_id = ?1 AND segment_ordinal < ?2 - 1";

        /// Drop every handover of process `?1`.
        delete_by_process = "DELETE FROM process_segment_handovers WHERE process_id = ?1";
    }
}
