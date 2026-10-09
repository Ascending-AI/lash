//! The workbench's drawing of one execution: the execution sites of the
//! kernel document it runs and the control flow between them, coloured by
//! the execution overlay lash folds.
//!
//! Lash supplies two facts and no presentation: the document with its
//! derived graph (every action, where it sits, what it calls or performs,
//! the control edges between statements) and the overlay (what each
//! execution site was observed to do, one row for each task that ran it).
//! This module joins them into the shape the page draws; every label here
//! is the workbench's. Without the document, the drawing is the observed
//! sites alone, named by their sites.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use lash::ProcessId;
use lash::tracing::{
    TraceLanguageExecutionIdentity, TraceLanguageExecutionStatus, TraceRuntimeScope,
    TraceRuntimeSubject,
};
use lash::workflow::document::{JoinMode, Site};
use lash::workflow::graph::{
    ActionNode, CalleeNode, EdgeKind, ExecutionSite, Graph, NodeId, NodeKind, Reference, SiteKind,
    Target,
};
use lash::workflow::{
    WorkflowDocument, WorkflowDocumentEntry, WorkflowExecutionOverlay, WorkflowOverlayMismatch,
    WorkflowOverlayOccurrence, WorkflowOverlaySettlement, WorkflowOverlaySite,
    WorkflowOverlaySiteReport,
};
use serde::Serialize;

/// One execution as the page draws it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraph {
    pub(crate) graph_key: String,
    pub(crate) scope: TraceRuntimeScope,
    pub(crate) subject: TraceRuntimeSubject,
    pub(crate) attempt: Option<u32>,
    /// The identity of the kernel document the execution runs.
    pub(crate) document: String,
    pub(crate) entry_kind: String,
    /// The entry function the execution starts; `None` for `main`.
    pub(crate) entry_function: Option<String>,
    pub(crate) entry_name: String,
    pub(crate) status: TraceLanguageExecutionStatus,
    pub(crate) settlement: Option<WorkflowOverlaySettlement>,
    pub(crate) coverage: ExecutionGraphCoverage,
    pub(crate) nodes: Vec<ExecutionGraphNode>,
    pub(crate) edges: Vec<ExecutionGraphEdge>,
    pub(crate) children: Vec<ExecutionGraphChildLink>,
    pub(crate) mismatches: Vec<WorkflowOverlayMismatch>,
}

/// How much of the execution the drawing can speak for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ExecutionGraphCoverage {
    /// The overlay was held to the document's own sites.
    pub(crate) document_loaded: bool,
    /// The overlay retains the execution's start.
    pub(crate) start_observed: bool,
}

/// What kind of action a drawn node is. `Site` is an observed site whose
/// document the workbench does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionGraphNodeKind {
    Call,
    Perform,
    Sleep,
    Join,
    JoinMany,
    Yield,
    Spawn,
    Cancel,
    Site,
}

impl From<SiteKind> for ExecutionGraphNodeKind {
    fn from(kind: SiteKind) -> Self {
        match kind {
            SiteKind::Call => Self::Call,
            SiteKind::Perform => Self::Perform,
            SiteKind::Sleep => Self::Sleep,
            SiteKind::Join => Self::Join,
            SiteKind::JoinMany => Self::JoinMany,
            SiteKind::Yield => Self::Yield,
            SiteKind::Spawn => Self::Spawn,
            SiteKind::Cancel => Self::Cancel,
        }
    }
}

/// One execution site. Its state is the row the node shows; `tasks` counts
/// the tasks that ran the site, so a fan-out's site says how wide it was.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ExecutionGraphNode {
    /// The site, as the document addresses it.
    pub(crate) id: String,
    pub(crate) kind: ExecutionGraphNodeKind,
    pub(crate) label: String,
    /// How many enclosing loops the site sits in.
    pub(crate) loop_depth: usize,
    pub(crate) tasks: usize,
    #[serde(flatten)]
    pub(crate) state: WorkflowOverlayOccurrence,
    /// How long the shown occurrence ran, when both of its ends were seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<i64>,
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
    pub(crate) child_document: Option<String>,
    pub(crate) child_entry_function: Option<String>,
    pub(crate) child_entry_name: Option<String>,
}

/// The row whose state a site with several rows shows: one in flight, else
/// the one that ended last, else any that was observed.
fn shown<'a>(sites: &[&'a WorkflowOverlaySite]) -> Option<&'a WorkflowOverlaySite> {
    let rank = |site: &&&WorkflowOverlaySite| match &site.state.occurrence {
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
    kind: ExecutionGraphNodeKind,
    label: String,
    loop_depth: usize,
    sites: &[&WorkflowOverlaySite],
) -> ExecutionGraphNode {
    let shown = shown(sites);
    ExecutionGraphNode {
        id,
        kind,
        label,
        loop_depth,
        tasks: sites.len(),
        duration_ms: shown.and_then(|site| site.state.occurrence.duration_ms()),
        state: shown
            .map(|site| site.state.occurrence.clone())
            .unwrap_or_default(),
        summary: shown
            .map(|site| site.state.summary.clone())
            .unwrap_or_default(),
    }
}

fn reference_label(graph: &Graph, reference: &Reference) -> String {
    match reference {
        Reference::Binding(binding) => graph.binding(*binding).name.to_string(),
        Reference::Session(name) => name.to_string(),
    }
}

fn callee_label(graph: &Graph, callee: &CalleeNode) -> String {
    match callee {
        CalleeNode::Declared(name) => name.to_string(),
        CalleeNode::Value(reference) => reference_label(graph, reference),
        CalleeNode::Library(function) => function.to_string(),
    }
}

/// The workbench's name for the action at `site`: what it calls, performs
/// or spawns, else the kind of action it is.
fn site_label(graph: &Graph, site: &ExecutionSite) -> String {
    match &graph.node(site.node).kind {
        NodeKind::Action(ActionNode::Perform { effect, .. }) => effect.to_string(),
        NodeKind::Action(ActionNode::Call { callee, .. }) => callee_label(graph, callee),
        NodeKind::Action(ActionNode::Spawn { callee, .. }) => {
            format!("spawn {}", callee_label(graph, callee))
        }
        NodeKind::Action(ActionNode::Sleep { .. }) => "sleep".to_owned(),
        NodeKind::Action(ActionNode::Join { .. }) => "join".to_owned(),
        NodeKind::Action(ActionNode::JoinMany { mode, .. }) => match mode {
            JoinMode::All => "join all",
            JoinMode::AllSettled => "join all settled",
            JoinMode::Race => "join race",
            JoinMode::Any => "join any",
        }
        .to_owned(),
        NodeKind::Action(ActionNode::Yield) => "yield".to_owned(),
        NodeKind::Action(ActionNode::Cancel { .. }) => "cancel".to_owned(),
        NodeKind::Block(_) | NodeKind::Stmt(_) | NodeKind::Expr(_) => site.site.to_string(),
    }
}

const fn edge_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Next => "next",
        EdgeKind::Then => "then",
        EdgeKind::Else => "else",
        EdgeKind::Body => "body",
        EdgeKind::Break => "break",
        EdgeKind::Continue => "continue",
        EdgeKind::Return => "return",
        EdgeKind::Throw => "throw",
        EdgeKind::Catch => "catch",
        EdgeKind::Finally => "finally",
        EdgeKind::End => "end",
        EdgeKind::Call => "call",
        EdgeKind::Spawn => "spawn",
        EdgeKind::Reference => "reference",
    }
}

/// The edges between drawn sites. The document's edges run between
/// statements, most of which the drawing leaves out, so an edge here joins
/// a site to each site control reaches next through undrawn statements, and
/// carries the kind of the first edge taken.
fn site_edges(graph: &Graph) -> Vec<ExecutionGraphEdge> {
    // The statement each site's action belongs to, and the site drawn there.
    let mut site_of = BTreeMap::<NodeId, &ExecutionSite>::new();
    for site in graph.execution_sites() {
        if let Some(statement) = graph.node_at(&site.statement) {
            site_of.insert(statement.id, site);
        }
    }
    let mut edges = BTreeSet::<(String, String, &'static str)>::new();
    for (statement, from) in &site_of {
        let mut pending = graph
            .edges_from(*statement)
            .chain(graph.edges_from(from.node))
            .map(|edge| (edge.to, edge.kind))
            .collect::<VecDeque<_>>();
        let mut seen = BTreeSet::new();
        while let Some((to, kind)) = pending.pop_front() {
            let Target::Node(to) = to else {
                continue;
            };
            if !seen.insert((to, kind)) {
                continue;
            }
            match site_of.get(&to) {
                Some(reached) => {
                    edges.insert((
                        from.site.to_string(),
                        reached.site.to_string(),
                        edge_label(kind),
                    ));
                }
                None => pending.extend(graph.edges_from(to).map(|edge| (edge.to, kind))),
            }
        }
    }
    edges
        .into_iter()
        .map(|(from, to, label)| ExecutionGraphEdge {
            id: format!("{from}->{to}:{label}"),
            from,
            to,
            label: label.to_owned(),
        })
        .collect()
}

fn entry_function(entry: &WorkflowDocumentEntry) -> Option<String> {
    match entry {
        WorkflowDocumentEntry::Main => None,
        WorkflowDocumentEntry::Entry { function } => Some(function.to_string()),
    }
}

/// Draw `overlay` over `document`, the document its execution runs, for the
/// execution `identity` names.
pub(crate) fn draw(
    identity: &TraceLanguageExecutionIdentity,
    overlay: &WorkflowExecutionOverlay,
    document: Option<&WorkflowDocument>,
) -> ExecutionGraph {
    let mut observed = BTreeMap::<&Site, Vec<&WorkflowOverlaySite>>::new();
    for site in &overlay.sites {
        observed.entry(&site.site.site).or_default().push(site);
    }
    let (nodes, edges) = match document.map(WorkflowDocument::graph) {
        Some(graph) => (
            graph
                .execution_sites()
                .iter()
                .map(|site| {
                    node(
                        site.site.to_string(),
                        site.kind.into(),
                        site_label(graph, site),
                        site.loops.len(),
                        observed.get(&site.site).map_or(&[], Vec::as_slice),
                    )
                })
                .collect(),
            site_edges(graph),
        ),
        None => (
            observed
                .iter()
                .map(|(site, sites)| {
                    node(
                        site.to_string(),
                        ExecutionGraphNodeKind::Site,
                        site.to_string(),
                        0,
                        sites,
                    )
                })
                .collect(),
            Vec::new(),
        ),
    };
    ExecutionGraph {
        graph_key: overlay.execution_key(),
        scope: overlay.scope.clone(),
        subject: overlay.subject.clone(),
        attempt: overlay.generation.map(|generation| generation.attempt()),
        document: identity.document.document.to_string(),
        entry_kind: match identity.document.entry {
            WorkflowDocumentEntry::Main => "main",
            WorkflowDocumentEntry::Entry { .. } => "entry",
        }
        .to_string(),
        entry_function: entry_function(&identity.document.entry),
        entry_name: identity.entry_name.clone(),
        status: overlay.status,
        settlement: overlay.settlement,
        coverage: ExecutionGraphCoverage {
            document_loaded: overlay.document.is_loaded(),
            start_observed: overlay.coverage.start_observed,
        },
        nodes,
        edges,
        children: overlay
            .children
            .iter()
            .map(|child| ExecutionGraphChildLink {
                parent_graph_key: overlay.execution_key(),
                parent_node_id: child.parent_site.site.to_string(),
                child_graph_key: child.child_execution_key(),
                child_process_id: child.child.process_id.clone(),
                child_attempt: child.child.attempt,
                child_document: child
                    .child
                    .document
                    .as_ref()
                    .map(|document| document.document.to_string()),
                child_entry_function: child
                    .child
                    .document
                    .as_ref()
                    .and_then(|document| entry_function(&document.entry)),
                child_entry_name: None,
            })
            .collect(),
        mismatches: overlay.mismatches.clone(),
    }
}
