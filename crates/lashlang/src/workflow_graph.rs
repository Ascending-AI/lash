//! The workflow graph's typed document: value types, deterministic node
//! identity, and the execution-site join used by trace consumers.
//!
//! The graph is deliberately a semantic, canonical view rather than a CST:
//! comments and authored formatting are discarded. Hosts own graph mutation,
//! drafts, layout, and versioning.
//!
//! Projection, validation, printing and parsing live in `lash-typescript`
//! (`lash_typescript::workflow_graph`): TypeScript is the only cell language,
//! so every piece of the lens that renders or parses node text belongs with
//! that dialect. This module names no source syntax at all.

use serde::{Deserialize, Serialize};

use crate::LashlangExecutionSite;
use crate::ast::{FunctionDecl, ProcessParam, ProcessSignalDecl, TypeDecl, TypeExpr};
use crate::lexer::Span;
use crate::tracking::WorkflowExecutionSite;

mod execution_sites;
mod facets;

pub use execution_sites::{execution_sites, runtime_execution_site_for_workflow_site};
pub use facets::*;

/// Version of the serialized workflow graph contract.
pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 9;

/// A deterministic node identifier minted from canonical source and AST position.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowNodeId(String);

impl WorkflowNodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkflowNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The single serializable graph document used for editing and run overlays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowGraph {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet_schema_version: Option<u32>,
    #[serde(default)]
    pub declarations: Vec<WorkflowDeclaration>,
    pub main: WorkflowSubgraph,
}

impl WorkflowGraph {
    pub fn process(&self, name: &str) -> Option<&WorkflowProcess> {
        self.declarations
            .iter()
            .find_map(|declaration| match declaration {
                WorkflowDeclaration::Process(process) if process.name.as_str() == name => {
                    Some(process)
                }
                _ => None,
            })
    }

    /// Iterates over every node, including nodes in nested containers and processes.
    pub fn nodes(&self) -> impl Iterator<Item = &WorkflowNode> {
        let mut nodes = Vec::new();
        collect_subgraph_nodes(&self.main, &mut nodes);
        for declaration in &self.declarations {
            if let WorkflowDeclaration::Process(process) = declaration {
                collect_subgraph_nodes(&process.body, &mut nodes);
            }
        }
        nodes.into_iter()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowDeclaration {
    Type(TypeDecl),
    Process(WorkflowProcess),
    /// A declared pure function, carried through the document unprojected.
    ///
    /// A process becomes a container with a child subgraph because its body is
    /// made of durable steps the editor has to show and reorder. A function's
    /// body contains no effects by construction, so it contributes no steps and
    /// no execution sites: projecting it into nodes would invent graph
    /// structure with nothing behind it. It travels verbatim instead, exactly
    /// as `Type` does, so a round trip through the document is lossless.
    Function(FunctionDecl),
}

/// A named process is a container with its own child subgraph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowProcess {
    pub id: WorkflowNodeId,
    pub name: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub name_source: WorkflowNodeNameSource,
    #[serde(default)]
    pub params: Vec<ProcessParam>,
    #[serde(default)]
    pub signals: Vec<ProcessSignalDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_ty: Option<TypeExpr>,
    pub body: WorkflowSubgraph,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeNameSource {
    Label,
    Derived,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSubgraph {
    #[serde(default)]
    pub nodes: Vec<WorkflowNode>,
    #[serde(default)]
    pub edges: Vec<WorkflowEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNode {
    pub id: WorkflowNodeId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub name_source: WorkflowNodeNameSource,
    pub kind: WorkflowNodeKind,
    /// Identifiers visible before this node executes, in stable lexical order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_variables: Vec<String>,
    /// Optional host-derived type information. It is never used to render source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_facets: Option<WorkflowNodeTypeFacets>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<VariableVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_sites: Vec<WorkflowExecutionSite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Data {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        expression: String,
    },
    Call {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        operation: String,
        expression: String,
    },
    Effect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        effect: WorkflowEffectKind,
        expression: String,
    },
    Computation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        expression: String,
    },
    StateUpdate {
        target: String,
        expression: String,
    },
    Terminal {
        terminal: WorkflowTerminalKind,
        expression: String,
    },
    Container(WorkflowContainer),
    Opaque {
        source: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEffectKind {
    StartProcess,
    AwaitJoin,
    SignalRun,
    WaitSignal,
    Sleep,
    Cancel,
    Print,
    Yield,
    Wake,
    Break,
    Continue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalKind {
    Finish,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "container_kind", rename_all = "snake_case")]
pub enum WorkflowContainer {
    If {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        condition: String,
        /// Whether the source's then branch is a statement block rather than a value expression.
        then_is_block: bool,
        /// Whether the source's else branch is a block rather than a direct value or `else if`.
        else_is_block: bool,
        then_graph: Box<WorkflowSubgraph>,
        else_graph: Box<WorkflowSubgraph>,
    },
    For {
        binding: String,
        iterable: String,
        body: Box<WorkflowSubgraph>,
    },
    While {
        condition: String,
        body: Box<WorkflowSubgraph>,
    },
    ListComprehension {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        clauses: Vec<WorkflowListComprehensionClause>,
        element: Box<WorkflowSubgraph>,
    },
}

impl WorkflowContainer {
    /// Iterates over this container's named child subgraphs in source order.
    pub fn child_subgraphs(&self) -> impl Iterator<Item = (&'static str, &WorkflowSubgraph)> {
        let children = match self {
            Self::If {
                then_graph,
                else_graph,
                ..
            } => [
                Some(("then", then_graph.as_ref())),
                Some(("else", else_graph.as_ref())),
            ],
            Self::For { body, .. } | Self::While { body, .. } => {
                [Some(("body", body.as_ref())), None]
            }
            Self::ListComprehension { element, .. } => [Some(("element", element.as_ref())), None],
        };
        children.into_iter().flatten()
    }

    /// Iterates mutably over this container's named child subgraphs in source order.
    pub fn child_subgraphs_mut(
        &mut self,
    ) -> impl Iterator<Item = (&'static str, &mut WorkflowSubgraph)> {
        let children = match self {
            Self::If {
                then_graph,
                else_graph,
                ..
            } => [
                Some(("then", then_graph.as_mut())),
                Some(("else", else_graph.as_mut())),
            ],
            Self::For { body, .. } | Self::While { body, .. } => {
                [Some(("body", body.as_mut())), None]
            }
            Self::ListComprehension { element, .. } => [Some(("element", element.as_mut())), None],
        };
        children.into_iter().flatten()
    }
}

/// One editable list-comprehension clause.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowListComprehensionClause {
    For { binding: String, iterable: String },
    If { condition: String },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VariableVersion {
    pub variable: String,
    pub version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowEdge {
    pub id: String,
    pub from: WorkflowNodeId,
    pub to: WorkflowNodeId,
    pub kind: WorkflowEdgeKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowEdgeKind {
    DataDependency { variable: String, version: u32 },
    Sequence,
}

impl WorkflowNodeId {
    /// Wraps an already-minted node identifier.
    ///
    /// The projector that mints these lives in `lash-typescript`, so the
    /// constructor is public; the value is opaque everywhere else.
    pub fn new(id: String) -> Self {
        Self(id)
    }
}

/// Resolve a runtime execution site to the workflow node that owns it.
///
/// This keeps runtime events unchanged: the host joins an observed site to the
/// graph using the source-level entry/path descriptor carried by the site.
pub fn node_id_for_execution_site(
    graph: &WorkflowGraph,
    site: &LashlangExecutionSite,
) -> Option<WorkflowNodeId> {
    graph
        .nodes()
        .find(|node| {
            node.execution_sites
                .iter()
                .any(|candidate| candidate.same_location(&site.workflow_site))
        })
        .map(|node| node.id.clone())
}

fn collect_subgraph_nodes<'a>(graph: &'a WorkflowSubgraph, nodes: &mut Vec<&'a WorkflowNode>) {
    for node in &graph.nodes {
        nodes.push(node);
        if let WorkflowNodeKind::Container(container) = &node.kind {
            for (_, child) in container.child_subgraphs() {
                collect_subgraph_nodes(child, nodes);
            }
        }
    }
}

/// Extends an AST path with one child index.
///
/// Node identity is path-keyed, so the projector and the execution-site walk
/// must agree on how a child path is spelled; this is that single spelling.
pub fn child_path(path: &[u32], child: impl TryInto<u32>) -> Vec<u32> {
    let mut result = path.to_vec();
    result.push(child.try_into().ok().expect("AST child index fits u32"));
    result
}
