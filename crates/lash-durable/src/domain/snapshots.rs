//! VM snapshots (V0, then L7; L6 writes `p/<pid>` through L7's store): the
//! latest snapshot per execution, compare-and-set on its revision
//! (ADR 0132 §8).

use crate::ids::Epoch;

use super::keys::ExecKey;

/// A snapshot's revision within its execution, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotRev(pub u64);

/// One execution's latest snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRow {
    /// The execution.
    pub exec: ExecKey,
    /// Its revision.
    pub rev: SnapshotRev,
    /// The encoded snapshot, inline: a cell's VM continuation and broker
    /// ledger, or a process engine's state, as its writer encoded it.
    pub snapshot_ref: String,
    /// The executable identity the continuation was captured on.
    pub executable_identity: String,
    /// The snapshot's format version.
    pub format_version: u32,
    /// The epoch of the commit that wrote it.
    pub written_epoch: Epoch,
}

/// A snapshot write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotWrite {
    /// Replace the execution's snapshot at `expected` with revision
    /// `expected + 1` (revision 1 when `expected` is `None`). Refused with
    /// [`DomainRefusal::SnapshotRevConflict`](super::DomainRefusal::SnapshotRevConflict)
    /// when the stored revision is not `expected`.
    Put {
        /// The execution.
        exec: ExecKey,
        /// The revision replaced.
        expected: Option<SnapshotRev>,
        /// The encoded snapshot, inline.
        snapshot_ref: String,
        /// The executable identity.
        executable_identity: String,
        /// The format version.
        format_version: u32,
    },
    /// Delete the execution's snapshot once it ended.
    Delete {
        /// The execution.
        exec: ExecKey,
    },
}
