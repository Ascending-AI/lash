//! Pure transition tables shared by every durable process registry.
//!
//! A durable registry backend reads rows, asks this table what the observation
//! means, and applies the write the table prescribes. The backend never decides
//! a process schedulable, never classifies a missing process, and never parses a
//! persisted label: those are the decisions that must be identical on SQLite,
//! on PostgreSQL, and on anything else that implements
//! [`ProcessRegistry`](crate::ProcessRegistry), and duplicating them is how
//! they drift.
//!
//! There is no SQL, no clock, no I/O and nothing `async` here. Time-dependent
//! decisions receive the instant supplied by the backend transaction that owns
//! them.

use crate::ProcessId;
use crate::plugin::PluginError;

// ---------------------------------------------------------------------------
// Terminal / tombstone classification
// ---------------------------------------------------------------------------

/// The stamp a pruned process leaves behind in its tombstone row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessTombstoneStamp {
    /// The status the row carried when it was pruned.
    pub terminal_label: crate::RetiredProcessStatus,
    /// When the prune removed the process row.
    pub pruned_at_ms: u64,
}

impl ProcessTombstoneStamp {
    /// The stamp of `process_id`'s stored tombstone row.
    ///
    /// # Errors
    ///
    /// A stored label that is not a retired status: the row is corrupt, and
    /// is refused rather than reported under a status it never had.
    pub fn from_row(
        process_id: &ProcessId,
        terminal_label: &str,
        pruned_at_ms: u64,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            terminal_label: retired_process_status_from_label(process_id, terminal_label)?,
            pruned_at_ms,
        })
    }
}

/// The single reader of a tombstone's stored `terminal_label`: an
/// unrecognised or live label is a refusal, never a default.
pub fn retired_process_status_from_label(
    process_id: &ProcessId,
    label: &str,
) -> Result<crate::RetiredProcessStatus, PluginError> {
    crate::RetiredProcessStatus::from_label(label).ok_or_else(|| {
        PluginError::Session(format!(
            "process `{process_id}` tombstone has unknown retired status `{label}`"
        ))
    })
}

/// Refusal for a process whose row was pruned but whose tombstone is retained.
pub fn process_no_longer_retained(stamp: ProcessTombstoneStamp) -> PluginError {
    PluginError::ProcessNoLongerRetained {
        terminal_label: stamp.terminal_label,
        pruned_at_ms: stamp.pruned_at_ms,
    }
}

/// Refusal for a process id no registry ever knew.
pub fn unknown_process(process_id: &ProcessId) -> PluginError {
    PluginError::ProcessUnknown {
        process_id: process_id.clone(),
    }
}

/// Classify a lookup that found no live process row.
///
/// The three-way split — retained row, retained tombstone, never known — is
/// what lets a host tell "this finished and was reaped" from "this id is
/// wrong", so the two absent cases must never collapse into one error.
pub fn absent_process_error(
    process_id: &ProcessId,
    tombstone: Option<ProcessTombstoneStamp>,
) -> PluginError {
    match tombstone {
        Some(stamp) => process_no_longer_retained(stamp),
        None => unknown_process(process_id),
    }
}

/// The `status` column labels a process row carries while it is still live —
/// that is, while lash may still act on it.
///
/// Both registries' retention SQL is written against exactly this set —
/// `status IN ('running', 'waiting')` selects live rows, `status NOT IN (…)`
/// selects retention candidates. The set is a constant rather than a query
/// fragment on purpose: the registries keep their literal SQL, and
/// `process_status_labels_partition_live_from_retired` is what fails if a new
/// variant would silently land on the wrong side.
pub const LIVE_PROCESS_STATUS_LABELS: [&str; 2] = ["running", "waiting"];

/// The `status` column labels retention may reclaim, i.e. the exact complement
/// of [`LIVE_PROCESS_STATUS_LABELS`] that both registries' prune SQL selects
/// with `status NOT IN ('running', 'waiting')`.
///
/// Reclaiming a row is a retention act, never an outcome claim.
pub const RETIRED_PROCESS_STATUS_LABELS: [&str; 4] =
    ["completed", "failed", "cancelled", "abandoned"];

// ---------------------------------------------------------------------------
// Wake reconciliation vocabulary
// ---------------------------------------------------------------------------
