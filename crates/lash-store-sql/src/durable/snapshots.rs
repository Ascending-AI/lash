//! VM snapshots: the neutral statements of the `snapshots` domain (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).

/// The table's unprefixed name. I0 (FIG-5194) creates it; its statements are its
/// owner's.
pub const TABLE: &str = "exec_snapshots";

crate::statements! {
    /// `exec_snapshots` statements both backends issue verbatim.
    pub struct SnapshotStatements @ "durable_snapshot" {
        /// Execution `?1`'s stored revision.
        rev = "SELECT rev FROM exec_snapshots WHERE exec_key = ?1";

        /// Insert execution `?1`'s first snapshot `?2` (identity `?3`, format
        /// `?4`) at epoch `?5`.
        insert = "INSERT INTO exec_snapshots
                 (exec_key, rev, snapshot_ref, executable_identity, format_version, written_epoch)
             VALUES (?1, 1, ?2, ?3, ?4, ?5)";

        /// Replace execution `?1`'s snapshot at revision `?2` with the next:
        /// `?3` (identity `?4`, format `?5`) at epoch `?6`. No row when the
        /// stored revision is not `?2`.
        replace = "UPDATE exec_snapshots
             SET rev = rev + 1, snapshot_ref = ?3, executable_identity = ?4,
                 format_version = ?5, written_epoch = ?6
             WHERE exec_key = ?1 AND rev = ?2
             RETURNING rev";

        /// Delete execution `?1`'s snapshot.
        delete = "DELETE FROM exec_snapshots WHERE exec_key = ?1";

        /// Execution `?1`'s snapshot.
        read = "SELECT rev, snapshot_ref, executable_identity, format_version, written_epoch
             FROM exec_snapshots WHERE exec_key = ?1";
    }
}
