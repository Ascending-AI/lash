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

use crate::PluginError;
use lash_vocabulary::identity::ProcessId;

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

/// A process record as its registry stored it.
///
/// # Errors
///
/// [`PluginError::StoredDataCorrupt`] for a record that does not decode.
pub fn decode_record(json: &str) -> Result<crate::process::ProcessRecord, PluginError> {
    serde_json::from_str(json).map_err(|error| PluginError::StoredDataCorrupt {
        record_kind: "process_registry".to_string(),
        message: error.to_string(),
    })
}

/// What a registry's point read of `process_id` answers from its stored
/// row: the retained record, `None` for a process no registry knew.
///
/// # Errors
///
/// [`PluginError::ProcessNoLongerRetained`] for a pruned process, and
/// [`PluginError::StoredDataCorrupt`] for a row that does not decode.
pub fn stored_record(
    process_id: &ProcessId,
    stored: &lash_durable::domain::ProcessRegistryRow,
) -> Result<Option<crate::process::ProcessRecord>, PluginError> {
    use lash_durable::domain::ProcessRegistryRow;
    match stored {
        ProcessRegistryRow::Retained(json) => decode_record(json).map(Some),
        ProcessRegistryRow::Pruned {
            terminal_label,
            pruned_at_ms,
        } => {
            let pruned_at_ms =
                u64::try_from(*pruned_at_ms).map_err(|_| PluginError::StoredDataCorrupt {
                    record_kind: "ProcessTombstone".to_string(),
                    message: format!("pruned_at_ms must be non-negative, got {pruned_at_ms}"),
                })?;
            Err(process_no_longer_retained(ProcessTombstoneStamp::from_row(
                process_id,
                terminal_label,
                pruned_at_ms,
            )?))
        }
        ProcessRegistryRow::Absent => Ok(None),
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

/// Overlay only fields independently writable by the registry, preserving the
/// acknowledged owner state. Both warm opens and fenced commit capture use it.
pub fn refresh_frontier(
    view: &mut lash_durable::domain::ProcessView,
    frontier: lash_durable::domain::ProcessFrontier,
) -> Result<(), PluginError> {
    let lash_durable::domain::ProcessRegistryRow::Retained(json) = &view.registry else {
        return Ok(());
    };
    let mut record = decode_record(json)?;
    record.last_event_sequence = frontier.last_event_sequence;
    record.updated_at_ms = frontier.updated_at_ms;
    record.cancel_request = frontier.cancel.map(Box::new);
    record.external_ref = frontier
        .external_ref
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|error| PluginError::StoredDataCorrupt {
            record_kind: "process external reference".into(),
            message: error.to_string(),
        })?;
    if let Some(row) = &mut view.row {
        row.cancel_requested_at = record
            .cancel_request
            .as_ref()
            .map(|cancel| i64::try_from(cancel.requested_at_ms).map(lash_durable::DurableInstant))
            .transpose()
            .map_err(|error| PluginError::StoredDataCorrupt {
                record_kind: "process cancel".into(),
                message: error.to_string(),
            })?;
    }
    view.registry =
        lash_durable::domain::ProcessRegistryRow::Retained(encode_view_record(&record)?);
    Ok(())
}

/// Reassemble exactly the registry's three storage owners, in query order.
pub fn encode_view_record(record: &crate::process::ProcessRecord) -> Result<String, PluginError> {
    let encode = |error: serde_json::Error| PluginError::StoredDataCorrupt {
        record_kind: "process registry".into(),
        message: error.to_string(),
    };
    let start = serde_json::to_string(&record.start_storage()).map_err(encode)?;
    let execution = serde_json::to_string(&record.execution_storage()).map_err(encode)?;
    let lifecycle = serde_json::to_string(&record.lifecycle_storage()).map_err(encode)?;
    Ok(format!(
        "{},{},{}",
        &start[..start.len() - 1],
        &execution[1..execution.len() - 1],
        &lifecycle[1..]
    ))
}
