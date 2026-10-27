//! Run facts: the neutral statements of the `run_records` domain (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`). Each row names its RoundFact checkpoint
//! graph node (FIG-5887 §7).

/// The table's unprefixed name. I0 (FIG-5194) creates it; its statements are its
/// owner's.
pub const TABLE: &str = "run_records";

crate::statements! {
    /// `run_records` statements both backends issue verbatim.
    pub struct RunRecordStatements @ "durable_run_record" {
        /// Whether owner `?1`'s run `?2` has a fact at ordinal `?3`.
        ordinal_taken = "SELECT 1 FROM run_records
             WHERE owner_key = ?1 AND run_seq = ?2 AND ordinal = ?3";

        /// Owner `?1`'s run `?2` fact `?3`'s node.
        root = "SELECT record_ref FROM run_records
             WHERE owner_key = ?1 AND run_seq = ?2 AND ordinal = ?3";

        /// Delete owner `?1`'s facts of every run before `?2`, returning
        /// each RoundFact node they rooted.
        prune = "DELETE FROM run_records WHERE owner_key = ?1 AND run_seq < ?2
             RETURNING record_ref";

        /// Only owner `?1`'s run `?2`, in ordinal order.
        read_of = "SELECT run_seq, ordinal, kind, call_id, record_ref, written_epoch
             FROM run_records WHERE owner_key = ?1 AND run_seq = ?2
             ORDER BY ordinal";

        /// Delete every fact of owner `?1`: a consumed cell's, or a pruned
        /// process's. Returns each RoundFact node it held.
        delete_owner = "DELETE FROM run_records WHERE owner_key = ?1 RETURNING record_ref";

        /// Delete every fact whose owner key starts with `?1`: the cells of
        /// one turn at its end, or every owner of one session at its delete.
        /// Returns each RoundFact node it held.
        delete_under = "DELETE FROM run_records
             WHERE substr(owner_key, 1, length(CAST(?1 AS TEXT))) = CAST(?1 AS TEXT)
             RETURNING record_ref";

        /// Every fact of owner `?1`, by run and ordinal.
        read = "SELECT run_seq, ordinal, kind, call_id, record_ref, written_epoch
             FROM run_records WHERE owner_key = ?1
             ORDER BY run_seq, ordinal";
    }
}
