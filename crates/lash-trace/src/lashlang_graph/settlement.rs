use super::*;

impl TraceLashlangGraph {
    /// Apply the process's committed terminal or terminal durable snapshot.
    /// Provisional execution outcomes cannot override this authority.
    pub fn settle(&mut self, settlement: TraceLashlangGraphSettlement) {
        let settlement = settlement.refine(self.settlement);
        let graph_key = self.graph_key.clone();
        *self = materialize_graph(
            canonical_language_identity(Some(self), &[]),
            self.execution_map.clone(),
            self.history.clone(),
            self.conflicts.clone(),
            self.history_limit,
            self.node_retention.clone(),
            ExecutionProjection {
                status: self.status,
                settlement: Some(settlement),
            },
        );
        self.graph_key = graph_key;
    }
}

impl TraceLashlangGraphTerminal {
    pub(super) fn execution_status(self) -> LanguageExecutionStatus {
        match self {
            Self::Completed => LanguageExecutionStatus::Completed,
            Self::Cancelled => LanguageExecutionStatus::Cancelled,
            Self::Failed | Self::Abandoned => LanguageExecutionStatus::Failed,
        }
    }
}

pub(super) fn settle_retained_nodes<'a>(
    nodes: impl Iterator<Item = &'a mut TraceLashlangGraphNode>,
    settlement: TraceLashlangGraphSettlement,
) {
    if settlement.terminal != TraceLashlangGraphTerminal::Cancelled {
        return;
    }
    let Some(end) = settlement.occurred_at else {
        return;
    };
    for node in nodes {
        let (occurrence, start) = match node.observation {
            TraceLashlangNodeObservation::Running { occurrence, start } => {
                (occurrence, Some(start))
            }
            TraceLashlangNodeObservation::Waiting {
                occurrence, start, ..
            } => (occurrence, start),
            _ => continue,
        };
        node.observation = TraceLashlangNodeObservation::Cancelled {
            occurrence,
            start,
            end,
        };
        let terminal = TraceLashlangNodeTerminalRecord {
            occurrence,
            status: TraceLashlangNodeTerminalStatus::Cancelled,
            end,
        };
        node.summary.terminal_count += 1;
        if node.summary.first_terminal.is_none() {
            node.summary.first_terminal = Some(terminal.clone());
        }
        node.summary.last_terminal = Some(terminal);
    }
}

pub(super) fn observed_execution_status(
    current: LanguageExecutionStatus,
    event: &TraceLanguageExecution,
) -> LanguageExecutionStatus {
    // Both direct process execution and process-scoped effects are provisional
    // until their process actor commits the terminal.
    let process = match &event.identity.subject {
        crate::TraceRuntimeSubject::Process { .. } => true,
        crate::TraceRuntimeSubject::Effect { address, .. } => matches!(
            address.execution_scope,
            lash_sansio::ExecutionScope::Process { .. }
        ),
    };
    if process {
        return current;
    }
    match &event.payload {
        TraceLanguageExecutionPayload::ExecutionFinished { status, .. } => {
            canonical_execution_status(current, *status)
        }
        _ => current,
    }
}

/// Retain the distinction between a missing node outcome and a proven one.
pub(super) fn settle_incomplete_nodes<'a>(
    nodes: impl Iterator<Item = &'a mut TraceLashlangGraphNode>,
    settlement: TraceLashlangGraphSettlement,
) {
    if settlement.terminal == TraceLashlangGraphTerminal::Cancelled
        && settlement.occurred_at.is_some()
    {
        return;
    }
    for node in nodes {
        let (occurrence, start) = match node.observation {
            TraceLashlangNodeObservation::Running { occurrence, start } => {
                (occurrence, Some(start))
            }
            TraceLashlangNodeObservation::Waiting {
                occurrence, start, ..
            } => (occurrence, start),
            _ => continue,
        };
        node.observation = TraceLashlangNodeObservation::Incomplete {
            occurrence,
            start,
            settled_at: settlement.occurred_at,
            terminal: settlement.terminal,
        };
    }
}

impl TraceLashlangGraphSettlement {
    pub(super) fn refine(mut self, previous: Option<Self>) -> Self {
        if let Some(previous) = previous
            && previous.terminal == self.terminal
        {
            self.occurred_at = self.occurred_at.or(previous.occurred_at);
        }
        self
    }
}
