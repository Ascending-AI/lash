//! The workbench's drawing of one execution: the workflow document's nodes
//! and edges, coloured by the execution overlay lash folds.
//!
//! Lash supplies two facts and no presentation: the immutable document (its
//! nodes, their labels and kinds, the arms of a branch) and the overlay (what
//! each execution site was observed to do). This module joins them into the
//! shape the page draws. Without the document, the drawing is the observed
//! sites alone, named by their node ids.

use std::collections::{BTreeMap, BTreeSet};

use lash::ProcessId;
use lash::tracing::{
    ExecutionNodeKind, TraceBranchSelection, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionStatus, TraceRuntimeScope, TraceRuntimeSubject,
};
use lash::vm::ir::{WorkflowContainer, WorkflowEdgeKind, WorkflowNodeKind, WorkflowSubgraph};
use lash::workflow::{
    WorkflowExecutionDocument, WorkflowExecutionOverlay, WorkflowOverlayCoverage,
    WorkflowOverlayMismatch, WorkflowOverlayOccurrence, WorkflowOverlaySettlement,
    WorkflowOverlaySite, WorkflowOverlaySiteReport,
};
use serde::Serialize;

/// One execution as the page draws it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraph {
    pub(crate) graph_key: String,
    pub(crate) scope: TraceRuntimeScope,
    pub(crate) subject: TraceRuntimeSubject,
    pub(crate) attempt: Option<u32>,
    pub(crate) source_identity: String,
    pub(crate) module_ref: String,
    pub(crate) entry_kind: String,
    pub(crate) entry_ref: Option<String>,
    pub(crate) entry_name: String,
    pub(crate) status: TraceLanguageExecutionStatus,
    pub(crate) settlement: Option<WorkflowOverlaySettlement>,
    pub(crate) coverage: WorkflowOverlayCoverage,
    pub(crate) nodes: Vec<ExecutionGraphNode>,
    pub(crate) edges: Vec<ExecutionGraphEdge>,
    pub(crate) children: Vec<ExecutionGraphChildLink>,
    pub(crate) mismatches: Vec<WorkflowOverlayMismatch>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraphLabel {
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
}

/// What a drawn node shows: what lash observed at its site, or that the
/// branch around it took the other arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub(crate) enum ExecutionGraphNodeState {
    Observed(WorkflowOverlayOccurrence),
    Skipped {
        status: &'static str,
        branch_node_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraphNode {
    pub(crate) id: String,
    pub(crate) kind: ExecutionNodeKind,
    pub(crate) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label_metadata: Option<ExecutionGraphLabel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch_selection: Option<TraceBranchSelection>,
    #[serde(flatten)]
    pub(crate) state: ExecutionGraphNodeState,
    pub(crate) summary: WorkflowOverlaySiteReport,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraphEdge {
    pub(crate) id: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraphChildLink {
    pub(crate) parent_graph_key: String,
    pub(crate) parent_node_id: String,
    pub(crate) child_graph_key: Option<String>,
    pub(crate) child_process_id: ProcessId,
    pub(crate) child_attempt: Option<u32>,
    pub(crate) child_module_ref: Option<String>,
    pub(crate) child_entry_ref: Option<String>,
    pub(crate) child_entry_name: Option<String>,
}

/// The site whose state a node with several sites shows: one in flight, else
/// the one that ended last, else any that was observed.
fn shown<'a>(sites: &[&'a WorkflowOverlaySite]) -> Option<&'a WorkflowOverlaySite> {
    let rank = |site: &&&WorkflowOverlaySite| match &site.occurrence {
        WorkflowOverlayOccurrence::Running { start, .. } => (3, Some(*start)),
        WorkflowOverlayOccurrence::Waiting { since, .. } => (3, Some(*since)),
        WorkflowOverlayOccurrence::Completed { end, .. }
        | WorkflowOverlayOccurrence::Failed { end, .. }
        | WorkflowOverlayOccurrence::Cancelled { end, .. } => (2, Some(*end)),
        WorkflowOverlayOccurrence::Incomplete { settled_at, .. } => (2, *settled_at),
        WorkflowOverlayOccurrence::Unobserved => (1, None),
    };
    sites.iter().max_by_key(rank).copied()
}

fn node(
    id: String,
    kind: ExecutionNodeKind,
    label: String,
    label_metadata: Option<ExecutionGraphLabel>,
    sites: &[&WorkflowOverlaySite],
) -> ExecutionGraphNode {
    let shown = shown(sites);
    ExecutionGraphNode {
        id,
        kind,
        label,
        label_metadata,
        branch_selection: sites.iter().find_map(|site| site.branch),
        state: ExecutionGraphNodeState::Observed(
            shown
                .map(|site| site.occurrence.clone())
                .unwrap_or_default(),
        ),
        summary: shown.map(|site| site.summary.clone()).unwrap_or_default(),
    }
}

struct Drawing<'a> {
    observed: BTreeMap<&'a str, Vec<&'a WorkflowOverlaySite>>,
    nodes: Vec<ExecutionGraphNode>,
    edges: Vec<ExecutionGraphEdge>,
}

impl Drawing<'_> {
    /// Draw `body`. `skipped_by` names the branch whose other arm ran, when
    /// this body is an arm that did not.
    fn body(&mut self, body: &WorkflowSubgraph, skipped_by: Option<&str>) {
        for document_node in &body.nodes {
            let id = document_node.id.to_string();
            let sites = self.observed.get(id.as_str()).cloned().unwrap_or_default();
            if let Some(first) = document_node.execution_sites.first() {
                let label_metadata = (document_node.name_source
                    == lash::vm::ir::WorkflowNodeNameSource::Label)
                    .then(|| ExecutionGraphLabel {
                        title: document_node.name.clone(),
                        description: document_node.description.clone(),
                    });
                let mut drawn = node(
                    id.clone(),
                    first.kind,
                    first.label.clone(),
                    label_metadata,
                    &sites,
                );
                if let Some(branch) = skipped_by
                    && drawn.state
                        == ExecutionGraphNodeState::Observed(WorkflowOverlayOccurrence::Unobserved)
                {
                    drawn.state = ExecutionGraphNodeState::Skipped {
                        status: "skipped",
                        branch_node_id: branch.to_owned(),
                    };
                }
                self.nodes.push(drawn);
            }
            if let WorkflowNodeKind::Container(container) = &document_node.kind {
                let taken = sites.iter().find_map(|site| site.branch);
                for (slot, child) in container.child_subgraphs() {
                    let other_arm_ran = match (container, taken) {
                        (WorkflowContainer::If { .. }, Some(TraceBranchSelection::Then)) => {
                            slot == "else"
                        }
                        (WorkflowContainer::If { .. }, Some(TraceBranchSelection::Else)) => {
                            slot == "then"
                        }
                        _ => false,
                    };
                    self.body(
                        child,
                        if other_arm_ran {
                            Some(id.as_str())
                        } else {
                            skipped_by
                        },
                    );
                }
            }
        }
        for edge in &body.edges {
            self.edges.push(ExecutionGraphEdge {
                id: edge.id.clone(),
                from: edge.from.to_string(),
                to: edge.to.to_string(),
                label: match &edge.kind {
                    WorkflowEdgeKind::Sequence => "sequence".to_owned(),
                    WorkflowEdgeKind::DataDependency { variable, version } => {
                        format!("{variable}@{version}")
                    }
                },
            });
        }
    }
}

/// Draw `overlay` over `document`, the document its execution runs, for the
/// execution `identity` names.
pub(crate) fn draw(
    identity: &TraceLanguageExecutionIdentity,
    overlay: &WorkflowExecutionOverlay,
    document: Option<&WorkflowExecutionDocument>,
) -> ExecutionGraph {
    let mut observed = BTreeMap::<&str, Vec<&WorkflowOverlaySite>>::new();
    for site in &overlay.sites {
        observed
            .entry(site.site.node_id.as_str())
            .or_default()
            .push(site);
    }
    let mut drawing = Drawing {
        observed,
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    match document.map(WorkflowExecutionDocument::body) {
        Some(body) => {
            drawing.body(body, None);
            let drawn = drawing
                .nodes
                .iter()
                .map(|node| node.id.clone())
                .collect::<BTreeSet<_>>();
            drawing
                .edges
                .retain(|edge| drawn.contains(&edge.from) && drawn.contains(&edge.to));
        }
        None => {
            drawing.nodes = drawing
                .observed
                .iter()
                .map(|(id, sites)| {
                    node(
                        (*id).to_owned(),
                        ExecutionNodeKind::Step,
                        (*id).to_owned(),
                        None,
                        sites,
                    )
                })
                .collect();
        }
    }
    ExecutionGraph {
        graph_key: overlay.execution_key.clone(),
        scope: overlay.scope.clone(),
        subject: overlay.subject.clone(),
        attempt: overlay.generation.map(|generation| generation.attempt()),
        source_identity: identity.document.source_identity.clone(),
        module_ref: identity.document.module_ref.to_string(),
        entry_kind: match identity.document.entry {
            lash::workflow::WorkflowDocumentEntry::Main => "main",
            lash::workflow::WorkflowDocumentEntry::Process { .. } => "process",
        }
        .to_string(),
        entry_ref: match &identity.document.entry {
            lash::workflow::WorkflowDocumentEntry::Main => None,
            lash::workflow::WorkflowDocumentEntry::Process { process_ref } => {
                Some(process_ref.clone())
            }
        },
        entry_name: identity.entry_name.clone(),
        status: overlay.status,
        settlement: overlay.settlement,
        coverage: overlay.coverage,
        nodes: drawing.nodes,
        edges: drawing.edges,
        children: overlay
            .children
            .iter()
            .map(|child| ExecutionGraphChildLink {
                parent_graph_key: child.parent_execution_key.clone(),
                parent_node_id: child.parent_site.node_id.clone(),
                child_graph_key: child.child_execution_key.clone(),
                child_process_id: child.child_process_id.clone(),
                child_attempt: child.child_attempt,
                child_module_ref: child
                    .document
                    .as_ref()
                    .map(|document| document.module_ref.to_string()),
                child_entry_ref: child.document.as_ref().and_then(|document| {
                    match &document.entry {
                        lash::workflow::WorkflowDocumentEntry::Main => None,
                        lash::workflow::WorkflowDocumentEntry::Process { process_ref } => {
                            Some(process_ref.clone())
                        }
                    }
                }),
                child_entry_name: None,
            })
            .collect(),
        mismatches: overlay.mismatches.clone(),
    }
}
