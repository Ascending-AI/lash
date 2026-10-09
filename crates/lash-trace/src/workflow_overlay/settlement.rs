use super::*;

impl WorkflowExecutionOverlay {
    /// Apply the process's committed terminal or terminal durable snapshot.
    /// Provisional execution outcomes cannot override this authority, and a
    /// provisional record that arrives later never reopens it.
    pub fn settle(&mut self, settlement: WorkflowOverlaySettlement) {
        let settlement = settlement.refine(self.settlement);
        *self = materialize_overlay(
            ExecutionRef::of(self),
            DocumentState::of(self),
            self.history.clone(),
            self.conflicts.clone(),
            self.history_limit,
            self.retention.clone(),
            ExecutionProjection {
                status: self.status,
                settlement: Some(settlement),
            },
        );
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

pub(super) fn settle_retained_sites<'a>(
    sites: impl Iterator<Item = &'a mut WorkflowOverlaySite>,
    settlement: WorkflowOverlaySettlement,
) {
    if settlement.terminal != WorkflowOverlayTerminal::Cancelled {
        return;
    }
    let Some(end) = settlement.occurred_at else {
        return;
    };
    for site in sites {
        let (occurrence, start) = match site.occurrence {
            WorkflowOverlayOccurrence::Running { occurrence, start } => (occurrence, Some(start)),
            WorkflowOverlayOccurrence::Waiting {
                occurrence, start, ..
            } => (occurrence, start),
            _ => continue,
        };
        site.occurrence = WorkflowOverlayOccurrence::Cancelled {
            occurrence,
            start,
            end,
        };
        let terminal = WorkflowOverlayTerminalRecord {
            occurrence,
            status: WorkflowOverlayTerminalStatus::Cancelled,
            end,
        };
        site.summary.terminal_count += 1;
        if site.summary.first_terminal.is_none() {
            site.summary.first_terminal = Some(terminal.clone());
        }
        site.summary.last_terminal = Some(terminal);
    }
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

pub(super) fn observed_execution_status(
    current: LanguageExecutionStatus,
    subject: &crate::TraceRuntimeSubject,
    fact: &WorkflowOverlayFact,
) -> LanguageExecutionStatus {
    if is_process_subject(subject) {
        return current;
    }
    match fact {
        WorkflowOverlayFact::Language {
            payload: TraceLanguageExecutionPayload::ExecutionFinished { status, .. },
        } => canonical_execution_status(current, *status),
        _ => current,
    }
}

/// Retain the distinction between a missing site outcome and a proven one.
pub(super) fn settle_incomplete_sites<'a>(
    sites: impl Iterator<Item = &'a mut WorkflowOverlaySite>,
    settlement: WorkflowOverlaySettlement,
) {
    if settlement.terminal == WorkflowOverlayTerminal::Cancelled && settlement.occurred_at.is_some()
    {
        return;
    }
    for site in sites {
        let (occurrence, start) = match site.occurrence {
            WorkflowOverlayOccurrence::Running { occurrence, start } => (occurrence, Some(start)),
            WorkflowOverlayOccurrence::Waiting {
                occurrence, start, ..
            } => (occurrence, start),
            _ => continue,
        };
        site.occurrence = WorkflowOverlayOccurrence::Incomplete {
            occurrence,
            start,
            settled_at: settlement.occurred_at,
            terminal: settlement.terminal,
        };
    }
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
}

/// Which of two facts under one identity is canonical when both finish the
/// execution with different statuses: `Some(true)` for `left`. `None` when
/// they are not two such finishes.
pub(super) fn canonical_execution_status_of(
    left: &WorkflowOverlayFact,
    right: &WorkflowOverlayFact,
) -> Option<bool> {
    let (
        WorkflowOverlayFact::Language {
            payload: TraceLanguageExecutionPayload::ExecutionFinished { status: left, .. },
        },
        WorkflowOverlayFact::Language {
            payload: TraceLanguageExecutionPayload::ExecutionFinished { status: right, .. },
        },
    ) = (left, right)
    else {
        return None;
    };
    (left != right).then(|| canonical_execution_status(*left, *right) == *left)
}
