use super::*;

impl WorkflowExecutionOverlay {
    /// Apply the process's committed terminal or terminal durable snapshot.
    /// Provisional execution outcomes cannot override this authority, and a
    /// provisional record that arrives later never reopens it.
    pub fn settle(&mut self, settlement: WorkflowOverlaySettlement) {
        let mut accumulator = WorkflowExecutionOverlayAccumulator::resume(self, self.history_limit);
        accumulator.settle(settlement);
        if let Some(settled) = accumulator.snapshot() {
            *self = settled;
        }
    }
}

impl WorkflowOverlayTerminal {
    pub(super) fn execution_status(self) -> LanguageExecutionStatus {
        match self {
            Self::Completed => LanguageExecutionStatus::Completed,
            Self::Cancelled => LanguageExecutionStatus::Cancelled,
            Self::Failed | Self::Abandoned => LanguageExecutionStatus::Failed,
        }
    }
}

/// End the evicted occurrence a site still shows in flight when its process
/// was cancelled at `end`.
pub(super) fn settle_retained_site(site: &mut WorkflowOverlaySiteState, end: DateTime<Utc>) {
    let (occurrence, start) = match site.occurrence {
        WorkflowOverlayOccurrence::Running { occurrence, start } => (occurrence, Some(start)),
        WorkflowOverlayOccurrence::Waiting {
            occurrence, start, ..
        } => (occurrence, start),
        _ => return,
    };
    site.occurrence = WorkflowOverlayOccurrence::Cancelled {
        occurrence,
        start,
        end,
    };
    site.summary.ended(WorkflowOverlayTerminalRecord {
        occurrence,
        status: WorkflowOverlayTerminalStatus::Cancelled,
        end,
    });
}

/// Both direct process execution and process-scoped effects are provisional
/// until their process actor commits the terminal.
pub(super) fn is_process_subject(subject: &crate::TraceRuntimeSubject) -> bool {
    match subject {
        crate::TraceRuntimeSubject::Process { .. } => true,
        crate::TraceRuntimeSubject::Effect { address, .. } => matches!(
            address.execution_scope,
            lash_sansio::ExecutionScope::Process { .. }
        ),
    }
}

/// Retain the distinction between a missing site outcome and a proven one.
pub(super) fn settle_incomplete_site(
    site: &mut WorkflowOverlaySiteState,
    settlement: WorkflowOverlaySettlement,
) {
    if settlement.cancelled_at().is_some() {
        return;
    }
    let (occurrence, start) = match site.occurrence {
        WorkflowOverlayOccurrence::Running { occurrence, start } => (occurrence, Some(start)),
        WorkflowOverlayOccurrence::Waiting {
            occurrence, start, ..
        } => (occurrence, start),
        _ => return,
    };
    site.occurrence = WorkflowOverlayOccurrence::Incomplete {
        occurrence,
        start,
        settled_at: settlement.occurred_at,
        terminal: settlement.terminal,
    };
}

impl WorkflowOverlaySettlement {
    pub(super) fn refine(mut self, previous: Option<Self>) -> Self {
        if let Some(previous) = previous
            && previous.terminal == self.terminal
        {
            self.occurred_at = self.occurred_at.or(previous.occurred_at);
        }
        self
    }

    /// When the process was cancelled, if this is a dated cancellation.
    pub(super) fn cancelled_at(self) -> Option<DateTime<Utc>> {
        match self.terminal {
            WorkflowOverlayTerminal::Cancelled => self.occurred_at,
            _ => None,
        }
    }
}
