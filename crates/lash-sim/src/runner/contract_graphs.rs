//! The executions an agent contract observes: one overlay per execution,
//! joined with the workflow document each named, and the facts a contract
//! proof states about them.

use super::*;
use lash_vocabulary::identity::SessionId;

/// One execution a contract core observed: what it did, and the workflow
/// document it ran when lash still answers it.
pub(super) struct ContractGraph {
    identity: lash::tracing::TraceLanguageExecutionIdentity,
    overlay: lash::workflow::WorkflowExecutionOverlay,
    document: Option<lash::workflow::WorkflowDocument>,
}

#[derive(Default)]
struct ObservedExecution {
    overlay: lash::workflow::WorkflowExecutionOverlayAccumulator,
    identity: Option<lash::tracing::TraceLanguageExecutionIdentity>,
    document: Option<ContractDocument>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ContractDocument {
    Linked(lash::workflow::LinkedStart),
    Callable(lash_vocabulary::identity::ProcessId),
}

/// The executions a contract core observes: one bounded overlay per
/// execution key, fed by the core's product observer, and the documents
/// those executions named.
#[derive(Default)]
pub(super) struct ContractGraphs {
    executions: std::sync::Mutex<BTreeMap<String, ObservedExecution>>,
    documents: std::sync::Mutex<BTreeMap<ContractDocument, lash::workflow::WorkflowDocument>>,
}

impl ContractGraphs {
    /// Read from `core` each document an observed execution named and this
    /// has not read yet. Lash holds a cell's document only while the cell's
    /// turn executes, so a contract reads on every activity of the turn.
    async fn read_documents(&self, core: &lash::LashCore) {
        let wanted = {
            let documents = self
                .documents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.executions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter_map(|execution| execution.document.clone())
                .filter(|reference| !documents.contains_key(reference))
                .collect::<std::collections::BTreeSet<_>>()
        };
        for reference in wanted {
            let document = match &reference {
                ContractDocument::Linked(start) => {
                    match core.host_artifacts().execution_document(start).await {
                        Ok(lash::workflow::WorkflowDocumentRead::Read(read)) => Some(*read),
                        _ => None,
                    }
                }
                // A process inspection supplies this execution's selected function
                // and frozen captures; a CallableId alone does not name its captures.
                ContractDocument::Callable(process) => {
                    match core.processes().graph(process).await {
                        Ok(lash::workflow::WorkflowRead::Inspected(read)) => Some(read.document),
                        _ => None,
                    }
                }
            };
            if let Some(document) = document {
                self.documents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(reference, document);
            }
        }
    }

    /// Every observed execution, in key order, with the document each named.
    pub(super) async fn graphs(&self, core: &lash::LashCore) -> Vec<ContractGraph> {
        self.read_documents(core).await;
        let mut graphs = {
            let documents = self
                .documents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.executions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter_map(|execution| {
                    Some(ContractGraph {
                        identity: execution.identity.clone()?,
                        overlay: execution.overlay.snapshot()?,
                        document: execution
                            .document
                            .as_ref()
                            .and_then(|reference| documents.get(reference))
                            .cloned(),
                    })
                })
                .collect::<Vec<_>>()
        };
        // Language finishes are provisional for a process. Its durable snapshot
        // supplies the terminal authority before the contract describes completion.
        for graph in &mut graphs {
            if let lash::tracing::TraceRuntimeSubject::Process { process_id } =
                &graph.overlay.subject
                && let Ok(Some(process)) = core.processes().get(process_id).await
                && let Some(terminal) = process.terminal()
            {
                use lash::workflow::WorkflowOverlayTerminal as Terminal;
                use lash_execution_contracts::process::TerminalProcessStatus as Status;
                graph
                    .overlay
                    .settle(lash::workflow::WorkflowOverlaySettlement {
                        terminal: match terminal.status() {
                            Status::Completed => Terminal::Completed,
                            Status::Failed => Terminal::Failed,
                            Status::Cancelled => Terminal::Cancelled,
                            Status::Abandoned => Terminal::Abandoned,
                        },
                        occurred_at: process.lifecycle.terminal_at_ms().map(|millis| {
                            (std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis))
                                .into()
                        }),
                    });
            }
        }
        graphs
    }
}

/// A contract turn's activity sink: it records each activity in `recorded`
/// after reading the documents the turn's executions have named so far.
struct ContractTurnEvents {
    recorded: Arc<RuntimeProofRecordingEvents>,
    graphs: Arc<ContractGraphs>,
    core: lash::LashCore,
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for ContractTurnEvents {
    async fn emit(&self, activity: lash::TurnActivity) {
        self.graphs.read_documents(&self.core).await;
        self.recorded.emit(activity).await;
    }
}

pub(super) fn contract_turn_events(
    recorded: &Arc<RuntimeProofRecordingEvents>,
    graphs: &Arc<ContractGraphs>,
    core: &lash::LashCore,
) -> Arc<dyn lash::TurnActivitySink> {
    Arc::new(ContractTurnEvents {
        recorded: Arc::clone(recorded),
        graphs: Arc::clone(graphs),
        core: core.clone(),
    })
}

impl lash::tracing::ProductObservationSink for ContractGraphs {
    fn observe(&self, record: &lash::tracing::TraceRecord) {
        let key = match &record.event {
            lash::tracing::TraceEvent::LanguageExecution { event, .. } => event.graph_key(),
            lash::tracing::TraceEvent::StepBodyStarted { step } => {
                format!("process:{}", step.process_id)
            }
            _ => return,
        };
        let mut executions = self
            .executions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let execution = executions.entry(key).or_default();
        if let lash::tracing::TraceEvent::LanguageExecution { event, .. } = &record.event {
            execution.identity = Some(event.identity.clone());
            if let lash::tracing::TraceLanguageExecutionPayload::ExecutionStarted {
                subject, ..
            } = &event.payload
            {
                execution.document = match &event.identity.document {
                    lash::tracing::TraceExecutionCode::Start(start) => {
                        Some(ContractDocument::Linked(start.clone()))
                    }
                    lash::tracing::TraceExecutionCode::Callable(_) => match subject {
                        lash::tracing::TraceRuntimeSubject::Process { process_id } => {
                            Some(ContractDocument::Callable(process_id.clone()))
                        }
                        _ => None,
                    },
                };
            }
        }
        // One execution per accumulator, so the fold cannot refuse the record.
        let _ = execution.overlay.fold(std::slice::from_ref(record));
    }
}

/// The effect performed at `site`, whether the `perform` is a statement
/// action or an action nested in an expression (`K-EVAL-008`).
fn contract_document_effect<'a>(
    document: &'a lash::workflow::WorkflowDocument,
    site: &lash::workflow::document::PositionLabel,
) -> Option<&'a lash_vocabulary::names::QualifiedName> {
    use lash::workflow::graph::{ActNode, ActionNode, ExprNode, NodeKind};

    match &document
        .graph()
        .nodes()
        .iter()
        .find(|node| node.site == *site)?
        .kind
    {
        NodeKind::Action(ActionNode::Perform { slot, .. })
        | NodeKind::Expr(ExprNode::Act {
            kind: ActNode::Perform { slot, .. },
            ..
        }) => document
            .document()
            .imports
            .get(slot)
            .map(|import| &import.name),
        _ => None,
    }
}

pub(super) fn agent_contract_graph_facts(
    graphs: &[ContractGraph],
    root_session_id: &SessionId,
) -> Value {
    let mut completed_process_entries = BTreeSet::new();
    let mut completed_effects = BTreeSet::new();
    let mut failed_effects = BTreeSet::new();
    let mut child_links = BTreeSet::new();
    let mut graph_status_counts = BTreeMap::<String, usize>::new();
    let mut child_session_exec_completed_count = 0usize;
    let mut child_session_exec_failed_count = 0usize;
    for ContractGraph {
        identity,
        overlay: graph,
        document,
    } in graphs
    {
        *graph_status_counts
            .entry(trace_lash_vm_status_label(graph.status).to_string())
            .or_default() += 1;
        if graph.scope.session_id.as_ref() != Some(root_session_id)
            && matches!(
                &graph.subject,
                lash::tracing::TraceRuntimeSubject::Effect { .. }
            )
        {
            match graph.status {
                lash::tracing::TraceLanguageExecutionStatus::Completed => {
                    child_session_exec_completed_count += 1;
                }
                lash::tracing::TraceLanguageExecutionStatus::Failed => {
                    child_session_exec_failed_count += 1;
                }
                _ => {}
            }
        }
        if matches!(
            &identity.document,
            lash::tracing::TraceExecutionCode::Callable(_)
                | lash::tracing::TraceExecutionCode::Start(lash::workflow::LinkedStart {
                    selector: lash::workflow::StartSelector::Export(_),
                    ..
                })
        ) && matches!(
            graph.subject,
            lash::tracing::TraceRuntimeSubject::Process { .. }
        ) && graph.status == lash::tracing::TraceLanguageExecutionStatus::Completed
        {
            completed_process_entries.insert(identity.entry_name.clone());
        }
        // Which effect a site performs is the document's: the overlay says
        // only what the site was observed to do.
        for site in &graph.sites {
            let Some(effect) = document
                .as_ref()
                .and_then(|document| contract_document_effect(document, &site.site.site))
            else {
                continue;
            };
            match &site.state.occurrence {
                lash::workflow::WorkflowOverlayOccurrence::Completed { .. } => {
                    completed_effects.insert(effect.to_string());
                }
                lash::workflow::WorkflowOverlayOccurrence::Failed { .. } => {
                    failed_effects.insert(effect.to_string());
                }
                _ => {}
            }
        }
        for child in &graph.children {
            child_links.insert(format!(
                "{}->{}",
                identity.entry_name,
                child.child.document.as_ref().map_or_else(
                    || "<unknown>".to_owned(),
                    |document| document.link.to_string()
                )
            ));
        }
    }
    json!({
        "graph_count": graphs.len(),
        "status_counts": graph_status_counts,
        "completed_process_entries": completed_process_entries.into_iter().collect::<Vec<_>>(),
        "completed_effects": completed_effects.into_iter().collect::<Vec<_>>(),
        "failed_effects": failed_effects.into_iter().collect::<Vec<_>>(),
        "child_links": child_links.into_iter().collect::<Vec<_>>(),
        "child_session_exec_completed_count": child_session_exec_completed_count,
        "child_session_exec_failed_count": child_session_exec_failed_count,
    })
}

fn trace_lash_vm_status_label(status: lash::tracing::TraceLanguageExecutionStatus) -> &'static str {
    match status {
        lash::tracing::TraceLanguageExecutionStatus::Running => "running",
        lash::tracing::TraceLanguageExecutionStatus::Completed => "completed",
        lash::tracing::TraceLanguageExecutionStatus::Failed => "failed",
        lash::tracing::TraceLanguageExecutionStatus::Cancelled => "cancelled",
    }
}
