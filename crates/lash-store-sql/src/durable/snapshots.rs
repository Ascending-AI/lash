//! Execution roots: the neutral statements of the `snapshots` domain (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`). Each row names its execution's ExecRoot
//! checkpoint graph node (FIG-5887 §7).

/// The table's unprefixed name. I0 (FIG-5194) creates it; its statements are its
/// owner's.
pub const TABLE: &str = "exec_snapshots";

crate::statements! {
    /// `exec_snapshots` statements both backends issue verbatim.
    pub struct SnapshotStatements @ "durable_snapshot" {
        /// Execution `?1`'s revision and root.
        root = "SELECT rev, root_ref FROM exec_snapshots WHERE exec_key = ?1";

        /// Execution `?1`'s row.
        read = "SELECT rev, root_ref, written_epoch, commit_id FROM exec_snapshots
             WHERE exec_key = ?1";

        /// Every row whose execution key starts with `?1`, by key: the cells
        /// of one turn.
        under = "SELECT exec_key, rev, root_ref, written_epoch, commit_id
             FROM exec_snapshots
             WHERE substr(exec_key, 1, length(CAST(?1 AS TEXT))) = CAST(?1 AS TEXT)
             ORDER BY exec_key";

        /// Insert execution `?1`'s first root `?2` by commit `?3` at epoch `?4`.
        insert = "INSERT INTO exec_snapshots (exec_key, rev, root_ref, commit_id, written_epoch)
             VALUES (?1, 1, ?2, ?3, ?4)";

        /// Replace execution `?1`'s root at revision `?2` with `?3` by commit
        /// `?4` at epoch `?5`. No row when the stored revision is not `?2`.
        replace = "UPDATE exec_snapshots
             SET rev = rev + 1, root_ref = ?3, commit_id = ?4, written_epoch = ?5
             WHERE exec_key = ?1 AND rev = ?2
             RETURNING rev";

        /// Retire execution `?1`'s root `?2`. No row when it is not `?2`.
        retire = "DELETE FROM exec_snapshots WHERE exec_key = ?1 AND root_ref = ?2
             RETURNING exec_key";

        /// Delete execution `?1`'s row, whichever root it holds.
        clear = "DELETE FROM exec_snapshots WHERE exec_key = ?1";

        /// Delete every row whose execution key starts with `?1`: the cells
        /// one turn stopped in, at its end, or one session's, at its delete.
        clear_under = "DELETE FROM exec_snapshots
             WHERE substr(exec_key, 1, length(CAST(?1 AS TEXT))) = CAST(?1 AS TEXT)";
    }
}
