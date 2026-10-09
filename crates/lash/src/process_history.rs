//! Paging a process's durable event history.
//!
//! [`Processes::events`](crate::Processes::events) reads one page of the
//! events a process committed, from a [`ProcessHistoryContinuation`]. The
//! continuation names a process and the last sequence its reader holds; it
//! is a position in the durable log and nothing else. It establishes no live
//! continuity: a host that follows a process observes it
//! ([`Processes::observe`](crate::Processes::observe)), whose cursor is the
//! process feed's own.

use std::sync::Arc;

use lash_core::{
    PluginError, ProcessEventHistoryRetention, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessRegistry,
};
use lash_sansio::ProcessId;
use serde::{Deserialize, Serialize};

/// Where a durable event read continues: after `after_sequence` of the
/// process it names. A host may persist it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessHistoryContinuation {
    process_id: ProcessId,
    after_sequence: u64,
}

impl ProcessHistoryContinuation {
    /// Before the first event of `process_id`.
    pub fn start(process_id: ProcessId) -> Self {
        Self::after(process_id, 0)
    }

    /// After the event at `after_sequence` of `process_id`.
    pub fn after(process_id: ProcessId, after_sequence: u64) -> Self {
        Self {
            process_id,
            after_sequence,
        }
    }

    pub fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    /// The sequence of the last event the reader holds; zero before the
    /// first.
    pub fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// One durable event page and where the next read continues.
#[derive(Clone, Debug)]
pub struct ProcessEventsRead {
    pub outcome: lash_core::facade_support::ObservedProcessEventReadOutcome,
    /// After the last event this page returned; after the released prefix
    /// when the read started inside one; unchanged when the page returned
    /// nothing or the process was pruned.
    pub next: ProcessHistoryContinuation,
}

/// Read one durable event page after `from`.
pub(crate) async fn read_events(
    registry: &Arc<dyn ProcessRegistry>,
    from: ProcessHistoryContinuation,
    limit: std::num::NonZeroUsize,
    mode: ProcessEventQueryMode,
) -> Result<ProcessEventsRead, PluginError> {
    let observer = lash_core::facade_support::ProcessWorkObserver::new(Arc::clone(registry));
    match registry.require_process_id(&from.process_id).await {
        Ok(_) => {}
        Err(PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            return Ok(ProcessEventsRead {
                outcome: ProcessEventReadOutcome::NoLongerRetained(
                    ProcessEventHistoryRetention::Pruned {
                        terminal_label,
                        pruned_at_ms,
                    },
                ),
                next: from,
            });
        }
        Err(error) => return Err(error),
    }
    let outcome = observer
        .event_page(&from.process_id, from.after_sequence, limit, mode)
        .await?;
    let last = match &outcome {
        ProcessEventReadOutcome::Retained(page) => {
            page.last_sequence(|event| event.sequence, |event| event.sequence)
        }
        // The reader resumes after the released prefix it was told about.
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Released {
            released_through,
        }) => Some(*released_through),
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Pruned {
            ..
        }) => None,
    };
    let next = match last {
        Some(sequence) => ProcessHistoryContinuation::after(from.process_id, sequence),
        None => from,
    };
    Ok(ProcessEventsRead { outcome, next })
}

#[cfg(test)]
mod tests;
