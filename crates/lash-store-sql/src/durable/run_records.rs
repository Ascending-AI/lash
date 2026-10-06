//! Run records: the neutral statements of the `run_records` domain (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).

/// The table's unprefixed name. I0 (FIG-5194) creates it; its statements are its
/// owner's.
pub const TABLE: &str = "run_records";

crate::statements! {
    /// `run_records` statements both backends issue verbatim.
    pub struct RunRecordStatements @ "durable_run_record" {
        /// Whether owner `?1`'s run `?2` has a record at ordinal `?3`.
        ordinal_taken = "SELECT 1 FROM run_records
             WHERE owner_key = ?1 AND run_seq = ?2 AND ordinal = ?3";

        /// Whether owner `?1`'s run `?2` has an outcome for call `?3`.
        outcome_exists = "SELECT 1 FROM run_records
             WHERE owner_key = ?1 AND run_seq = ?2 AND call_id = ?3 AND kind = 'x_outcome'";

        /// Append owner `?1`'s run `?2` record `?3` of kind `?4` for call `?5`
        /// with body `?6`, written at epoch `?7`.
        append = "INSERT INTO run_records
                 (owner_key, run_seq, ordinal, kind, call_id, record_json, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

        /// Delete owner `?1`'s records of every run before `?2`.
        prune = "DELETE FROM run_records WHERE owner_key = ?1 AND run_seq < ?2";

        /// Every record of owner `?1`, by run and ordinal.
        read = "SELECT run_seq, ordinal, kind, call_id, record_json, written_epoch
             FROM run_records WHERE owner_key = ?1
             ORDER BY run_seq, ordinal";
    }
}
