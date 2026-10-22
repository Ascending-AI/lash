//! Opaque positions and convergent entries of the process roster feed.

use serde::{Deserialize, Serialize};

use super::ProcessRecord;
use super::RetiredProcessStatus;
use lash_vocabulary::identity::ProcessId;

/// Opaque position in a store's Process Change Feed.
///
/// The wrapped sequence is meaningful only to the registry backend that issued
/// it. Backends expose constructors/accessors so external store implementations
/// can persist and bind the position, but consumers should treat values as
/// cursors, not comparable timestamps.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ProcessChangeCursor(u64);

impl ProcessChangeCursor {
    /// Constructs the backend-defined initial change-feed position for process-store implementors; callers must not treat it as a timestamp.
    pub fn initial() -> Self {
        Self(0)
    }

    pub fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Exposes the opaque sequence to the process-store implementor that issued it; consumers must not compare cursors from different backends.
    pub fn store_sequence(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTombstone {
    pub process_id: ProcessId,
    /// The status the process was pruned in.
    pub terminal_label: RetiredProcessStatus,
    pub pruned_at_ms: u64,
    pub pruned_change_seq: u64,
}

/// A convergent roster change with the opaque position issued for that row.
/// Resume strictly after `cursor` once this entry's projection is committed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessChange {
    pub cursor: ProcessChangeCursor,
    #[serde(flatten)]
    pub kind: ProcessChangeKind,
}

/// The current process row or the tombstone of a pruned process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessChangeKind {
    /// A requested, convergent query stays rich under rule A.
    Upsert {
        record: Box<ProcessRecord>,
    },
    Deleted {
        tombstone: ProcessTombstone,
    },
}
