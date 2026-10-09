//! The executions an agent contract observes: one overlay per execution,
//! joined with the workflow document each named, and the facts a contract
//! proof states about them.

use super::*;
use lash_sansio::SessionId;

/// One execution a contract core observed: what it did, and the workflow
/// document it ran when lash still answers it.
pub(super) struct ContractGraph {
    identity: lash::tracing::TraceLanguageExecutionIdentity,
    overlay: lash::workflow::WorkflowExecutionOverlay,
    document: Option<lash::workflow::WorkflowExecutionDocument>,
}

#[derive(Default)]
struct ObservedExecution {
    overlay: lash::workflow::WorkflowExecutionOverlayAccumulator,
    identity: Option<lash::tracing::TraceLanguageExecutionIdentity>,
    document: Option<lash::workflow::WorkflowDocumentRef>,
}

/// The executions a contract core observes: one bounded overlay per
/// execution key, fed by the core's product observer, and the documents
/// those executions named.
#[derive(Default)]
pub(super) struct ContractGraphs {
    executions: std::sync::Mutex<BTreeMap<String, ObservedExecution>>,
    documents: std::sync::Mutex<
        BTreeMap<lash::workflow::WorkflowDocumentRef, lash::workflow::WorkflowExecutionDocument>,
    >,
}

impl ContractGraphs {
    /// Read from `core` each document an observed execution named and this
    /// has not read yet. Lash holds a cell's module only while the cell's
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
            if let Ok(lash::workflow::WorkflowDocumentRead::Read(document)) =
                core.host_artifacts().execution_document(&reference).await
            {
                self.documents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(reference, *document);
            }
        }
    }

    /// Every observed execution, in key order, with the document each named.
    pub(super) async fn graphs(&self, core: &lash::LashCore) -> Vec<ContractGraph> {
        self.read_documents(core).await;
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
            .collect()
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

impl lash::tracing::TraceSink for ContractGraphs {
    fn append(
        &self,
        record: &lash::tracing::TraceRecord,
    ) -> Result<(), lash::tracing::TraceSinkError> {
        let key = match &record.event {
            lash::tracing::TraceEvent::LanguageExecution { event, .. } => {
                event.identity.graph_key()
            }
            lash::tracing::TraceEvent::StepBodyStarted { step } => {
                format!("process:{}", step.process_id)
            }
            _ => return Ok(()),
        };
        let mut executions = self
            .executions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let execution = executions.entry(key).or_default();
        if let lash::tracing::TraceEvent::LanguageExecution { event, .. } = &record.event {
            execution.identity = Some(event.identity.clone());
            if let lash::tracing::TraceLanguageExecutionPayload::ExecutionStarted = &event.payload {
                execution.document = Some(event.identity.document.clone());
            }
        }
        // One execution per accumulator, so the fold cannot refuse the record.
        let _ = execution.overlay.fold(std::slice::from_ref(record));
        Ok(())
    }
}

/// The document node that owns `site`, and the site's static description.
fn contract_document_site<'a>(
    body: &'a lash::vm::ir::WorkflowSubgraph,
    site: &lash::vm::WorkflowSiteRef,
) -> Option<(
    &'a lash::vm::ir::WorkflowNode,
    &'a lash::vm::WorkflowSiteDescriptor,
)> {
    body.nodes().into_iter().find_map(|node| {
        if node.id == site.node_id {
            return node
                .execution_sites
                .iter()
                .find(|candidate| candidate.site_path == site.site_path)
                .map(|found| (node, found));
        }
        let lash::vm::ir::WorkflowNodeKind::Container(container) = &node.kind else {
            return None;
        };
        container
            .child_subgraphs()
            .find_map(|(_, child)| contract_document_site(child, site))
    })
}

pub(super) fn agent_contract_graph_facts(
    graphs: &[ContractGraph],
    root_session_id: &SessionId,
) -> Value {
    let mut completed_process_entries = BTreeSet::new();
    let mut completed_labeled_resources = BTreeSet::new();
    let mut failed_labeled_resources = BTreeSet::new();
    let mut completed_labeled_nodes = BTreeSet::new();
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
            identity.document.entry,
            lash::workflow::WorkflowDocumentEntry::Process { .. }
        ) && matches!(
            graph.subject,
            lash::tracing::TraceRuntimeSubject::Process { .. }
        ) && graph.status == lash::tracing::TraceLanguageExecutionStatus::Completed
        {
            completed_process_entries.insert(identity.entry_name.clone());
        }
        // A site's kind and its node's label are the document's: the overlay
        // says only what the site was observed to do.
        let body = document
            .as_ref()
            .map(lash::workflow::WorkflowExecutionDocument::body);
        for site in &graph.sites {
            let Some((node, described)) =
                body.and_then(|body| contract_document_site(body, &site.site))
            else {
                continue;
            };
            let Some(label) = &node.label else {
                continue;
            };
            let title = label.title.as_str();
            let resource = described.kind == lash::tracing::ExecutionNodeKind::ResourceOperation;
            match &site.state.occurrence {
                lash::workflow::WorkflowOverlayOccurrence::Completed { .. } => {
                    if resource {
                        completed_labeled_resources.insert(title.to_string());
                    }
                    completed_labeled_nodes.insert(title.to_string());
                }
                lash::workflow::WorkflowOverlayOccurrence::Failed { .. } if resource => {
                    failed_labeled_resources.insert(title.to_string());
                }
                _ => {}
            }
        }
        for child in &graph.children {
            child_links.insert(format!(
                "{}->{}",
                identity.entry_name,
                child
                    .child
                    .document
                    .as_ref()
                    .map(|document| document.module_ref.as_str())
                    .unwrap_or("<unknown>")
            ));
        }
    }
    json!({
        "graph_count": graphs.len(),
        "status_counts": graph_status_counts,
        "completed_process_entries": completed_process_entries.into_iter().collect::<Vec<_>>(),
        "completed_labeled_resources": completed_labeled_resources.into_iter().collect::<Vec<_>>(),
        "failed_labeled_resources": failed_labeled_resources.into_iter().collect::<Vec<_>>(),
        "completed_labeled_nodes": completed_labeled_nodes.into_iter().collect::<Vec<_>>(),
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
