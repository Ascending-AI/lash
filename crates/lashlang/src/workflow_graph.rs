//! Source-level Lashlang workflow graph projection and rendering.
//!
//! The graph is deliberately a semantic, canonical view rather than a CST:
//! comments and authored formatting are discarded. Hosts own graph mutation,
//! drafts, layout, and versioning; this module owns only the typed document,
//! deterministic identity, projection, validation, and canonical rendering.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ast::{
    AssignTarget, Declaration, Expr, FunctionDecl, LabelMetadata, ListComprehensionClause,
    ProcessDecl, ProcessParam, ProcessSignalDecl, Program, TypeDecl, TypeExpr,
};
use crate::linker::WorkflowLinkAnalysis;
use crate::runtime::is_pure_expr;
use crate::source::{CanonicalSourceError, canonical_program_source};
use crate::tracking::WorkflowExecutionSite;
use crate::{LashlangExecutionSite, ParseError, Span, parse};

mod editable_text;
mod execution_sites;
mod facets;

use editable_text::{
    assign_target_text, expression_text, parse_assignment_target_field,
    parse_comprehension_clauses, parse_expression_field, parse_simple_binding_field,
    with_assignment, workflow_clause,
};
use execution_sites::execution_sites;
pub use execution_sites::runtime_execution_site_for_workflow_site;
pub use facets::*;

/// Version of the serialized workflow graph contract.
pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 5;

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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowContainer {
    If {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        condition: String,
        /// Whether the source's then branch is a statement block rather than a value expression.
        then_is_block: bool,
        /// Whether the source's else branch is a block rather than a direct value or `else if`.
        else_is_block: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        then_graph: Option<Box<WorkflowSubgraph>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        else_graph: Option<Box<WorkflowSubgraph>>,
    },
    For {
        binding: String,
        iterable: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Box<WorkflowSubgraph>>,
    },
    While {
        condition: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Box<WorkflowSubgraph>>,
    },
    ListComprehension {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        clauses: Vec<WorkflowListComprehensionClause>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        element: Option<Box<WorkflowSubgraph>>,
    },
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

#[derive(Debug, Error)]
pub enum WorkflowGraphBuildError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    CanonicalSource(#[from] CanonicalSourceError),
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum GraphRenderError {
    #[error("unsupported workflow graph schema version {found}; expected {expected}")]
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    #[error("duplicate workflow node id `{id}`")]
    DuplicateNodeId { id: String },
    #[error("edge `{edge_id}` references unknown {endpoint} node `{node_id}`")]
    UnknownNodeReference {
        edge_id: String,
        endpoint: &'static str,
        node_id: String,
    },
    #[error("node `{node_id}` is missing required child `{child}`")]
    MissingRequiredChild {
        node_id: String,
        child: &'static str,
    },
    #[error("node `{node_id}` has a payload incompatible with its kind: {message}")]
    InvalidNodePayload { node_id: String, message: String },
    #[error("node `{node_id}` has invalid `{field}` expression text: {message}")]
    InvalidExpression {
        node_id: String,
        field: &'static str,
        message: String,
    },
    #[error("node `{node_id}` has invalid `{field}` assignment target text: {message}")]
    InvalidAssignmentTarget {
        node_id: String,
        field: &'static str,
        message: String,
    },
    #[error("opaque node `{node_id}` is not exactly one valid statement: {message}")]
    InvalidOpaqueSource { node_id: String, message: String },
    #[error("duplicate process name `{name}`")]
    DuplicateProcessName { name: String },
    #[error(transparent)]
    CanonicalSource(#[from] CanonicalSourceError),
    #[error("rendered workflow source did not parse: {message}")]
    RenderedSourceInvalid { message: String },
}

/// Parse source, canonicalize it, and project it into a deterministic graph.
pub fn workflow_graph_from_source(src: &str) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    workflow_graph_from_source_with_facets(src, None)
}

/// Validate and render a graph through the canonical Lashlang source printer.
pub fn workflow_graph_to_source(graph: &WorkflowGraph) -> Result<String, GraphRenderError> {
    validate_graph(graph)?;
    let program = graph_to_program(graph)?;
    let source = canonical_program_source(&program)?;
    parse(&source).map_err(|error| GraphRenderError::RenderedSourceInvalid {
        message: error.to_string(),
    })?;
    Ok(source)
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

struct GraphProjector<'a> {
    program: &'a Program,
    source_hash: String,
    spans: BTreeMap<Vec<u32>, Span>,
    analysis: Option<&'a WorkflowLinkAnalysis>,
}

impl<'a> GraphProjector<'a> {
    fn new(
        canonical: &'a str,
        program: &'a Program,
        analysis: Option<&'a WorkflowLinkAnalysis>,
    ) -> Self {
        Self {
            program,
            source_hash: hex_digest("lash-workflow-source/v2", canonical.as_bytes()),
            spans: program
                .expression_source_spans
                .iter()
                .map(|source_span| (source_span.path.clone(), source_span.span))
                .collect(),
            analysis,
        }
    }

    fn project(&self) -> WorkflowGraph {
        let mut declarations = Vec::with_capacity(self.program.declarations.len());
        for declaration in &self.program.declarations {
            match declaration {
                Declaration::Type(ty) => declarations.push(WorkflowDeclaration::Type(ty.clone())),
                Declaration::Process(process) => {
                    declarations.push(WorkflowDeclaration::Process(self.project_process(process)))
                }
                Declaration::Function(function) => {
                    declarations.push(WorkflowDeclaration::Function(function.clone()))
                }
            }
        }
        let mut versions = VersionState::default();
        let main = self.project_block(&self.program.main, "main", &[], &mut versions);
        WorkflowGraph {
            schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
            facet_schema_version: self.analysis.map(|_| WORKFLOW_TYPE_FACET_SCHEMA_VERSION),
            declarations,
            main,
        }
    }

    fn project_process(&self, process: &ProcessDecl) -> WorkflowProcess {
        let owner = format!("process:{}", process.name);
        let (display_name, description, name_source) = match &process.label {
            Some(label) => (
                label.title.to_string(),
                label.description.as_ref().map(ToString::to_string),
                WorkflowNodeNameSource::Label,
            ),
            None => (
                process.name.to_string(),
                None,
                WorkflowNodeNameSource::Derived,
            ),
        };
        let mut versions = VersionState::default();
        for param in &process.params {
            versions.seed(param.name.as_str());
        }
        WorkflowProcess {
            id: self.node_id(&owner, &[], "process"),
            name: process.name.to_string(),
            display_name,
            description,
            name_source,
            params: process.params.clone(),
            signals: process.signals.clone(),
            return_ty: process.return_ty.clone(),
            body: self.project_block(&process.body, &owner, &[], &mut versions),
        }
    }

    fn project_block(
        &self,
        expr: &Expr,
        owner: &str,
        base_path: &[u32],
        versions: &mut VersionState,
    ) -> WorkflowSubgraph {
        let expressions = match expr {
            Expr::Block(expressions) => expressions.as_slice(),
            expression => std::slice::from_ref(expression),
        };
        let mut subgraph = WorkflowSubgraph::default();
        let mut previous_effect: Option<WorkflowNodeId> = None;
        for (index, expression) in expressions.iter().enumerate() {
            let mut path = base_path.to_vec();
            if matches!(expr, Expr::Block(_)) {
                path.push(index as u32);
            }
            let node = self.project_node(expression, owner, &path, versions);
            add_dependency_edges(&mut subgraph.edges, &node, expression, versions);
            if node_is_sequenced(&node) {
                if let Some(previous) = &previous_effect {
                    subgraph.edges.push(edge(
                        previous.clone(),
                        node.id.clone(),
                        WorkflowEdgeKind::Sequence,
                    ));
                }
                previous_effect = Some(node.id.clone());
            }
            versions.record_outputs(&node.outputs, &node.id);
            subgraph.nodes.push(node);
        }
        subgraph
    }

    fn project_node(
        &self,
        expression: &Expr,
        owner: &str,
        path: &[u32],
        versions: &mut VersionState,
    ) -> WorkflowNode {
        let (label, expression) = peel_label(expression);
        let source_span = if owner == "main" {
            self.spans.get(path).copied()
        } else {
            None
        };
        let available_variables: Vec<String> = versions.known.iter().cloned().collect();
        let (kind, derived_name, outputs) = self.project_kind(expression, owner, path, versions);
        let id = self.node_id(owner, path, kind_tag(&kind));
        let (name, description, name_source) = match label {
            Some(label) => (
                label.title.to_string(),
                label.description.as_ref().map(ToString::to_string),
                WorkflowNodeNameSource::Label,
            ),
            None => (derived_name, None, WorkflowNodeNameSource::Derived),
        };
        let execution_sites = execution_sites(expression, owner, path, label);
        let type_facets =
            projected_node_type_facets(self.analysis, expression, &available_variables, &id);
        WorkflowNode {
            id,
            name,
            description,
            name_source,
            kind,
            available_variables,
            type_facets,
            outputs,
            execution_sites,
            source_span,
        }
    }

    fn project_kind(
        &self,
        expression: &Expr,
        owner: &str,
        path: &[u32],
        versions: &mut VersionState,
    ) -> (WorkflowNodeKind, String, Vec<VariableVersion>) {
        let (binding, value, value_path) = assignment_parts(expression, path);
        if let Expr::Assign { target, expr } = expression
            && (!target.is_simple() || versions.is_known(target.root.as_str()))
        {
            return (
                WorkflowNodeKind::StateUpdate {
                    target: assign_target_text(target),
                    expression: expression_text(expr),
                },
                format!("update {}", target.root),
                vec![versions.allocate(target.root.as_str())],
            );
        }
        match value {
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut then_versions = versions.clone();
                let mut else_versions = versions.clone();
                let then_graph = self.project_block(
                    then_block,
                    owner,
                    &child_path(&value_path, 1),
                    &mut then_versions,
                );
                let else_graph = self.project_block(
                    else_block,
                    owner,
                    &child_path(&value_path, 2),
                    &mut else_versions,
                );
                let mut outputs = assignment_output(binding.as_ref(), versions);
                outputs.extend(versions.merge_outputs(&then_versions, &else_versions));
                (
                    WorkflowNodeKind::Container(WorkflowContainer::If {
                        binding: binding.as_ref().map(assign_target_text),
                        condition: expression_text(condition),
                        then_is_block: matches!(then_block.as_ref(), Expr::Block(_)),
                        else_is_block: matches!(else_block.as_ref(), Expr::Block(_)),
                        then_graph: Some(Box::new(then_graph)),
                        else_graph: Some(Box::new(else_graph)),
                    }),
                    "if".to_string(),
                    outputs,
                )
            }
            Expr::For {
                binding: loop_binding,
                iterable,
                body,
            } => {
                let mut body_versions = versions.clone();
                body_versions.shadow(loop_binding.as_str());
                let body_graph = self.project_block(
                    body,
                    owner,
                    &child_path(&value_path, 1),
                    &mut body_versions,
                );
                let outputs = loop_outputs(body, Some(loop_binding.as_str()), versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::For {
                        binding: loop_binding.to_string(),
                        iterable: expression_text(iterable),
                        body: Some(Box::new(body_graph)),
                    }),
                    format!("for {loop_binding}"),
                    outputs,
                )
            }
            Expr::While { condition, body } => {
                let mut body_versions = versions.clone();
                let body_graph = self.project_block(
                    body,
                    owner,
                    &child_path(&value_path, 1),
                    &mut body_versions,
                );
                let outputs = loop_outputs(body, None, versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::While {
                        condition: expression_text(condition),
                        body: Some(Box::new(body_graph)),
                    }),
                    "while".to_string(),
                    outputs,
                )
            }
            Expr::ListComprehension { element, clauses } => {
                let mut element_versions = versions.clone();
                for clause in clauses {
                    if let ListComprehensionClause::For { binding, .. } = clause {
                        element_versions.shadow(binding.as_str());
                    }
                }
                let element_graph = self.project_block(
                    element,
                    owner,
                    &child_path(&value_path, clauses.len() as u32),
                    &mut element_versions,
                );
                let outputs = assignment_output(binding.as_ref(), versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
                        binding: binding.as_ref().map(assign_target_text),
                        clauses: clauses.iter().map(workflow_clause).collect(),
                        element: Some(Box::new(element_graph)),
                    }),
                    "list comprehension".to_string(),
                    outputs,
                )
            }
            Expr::Finish(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Finish,
                    expression: expression_text(value),
                },
                "finish".to_string(),
                Vec::new(),
            ),
            Expr::Fail(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Fail,
                    expression: expression_text(value),
                },
                "fail".to_string(),
                Vec::new(),
            ),
            _ if (is_pure_expr(value) || matches!(value, Expr::TypeLiteral(_)))
                && binding.is_some() =>
            {
                let outputs = assignment_output(binding.as_ref(), versions);
                (
                    WorkflowNodeKind::Data {
                        binding: binding.as_ref().map(assign_target_text),
                        expression: expression_text(value),
                    },
                    data_name(value),
                    outputs,
                )
            }
            _ => {
                let outputs = assignment_output(binding.as_ref(), versions);
                if let Some(operation) = first_receiver_operation(value) {
                    (
                        WorkflowNodeKind::Call {
                            binding: binding.as_ref().map(assign_target_text),
                            operation: operation.to_string(),
                            expression: expression_text(value),
                        },
                        operation.to_string(),
                        outputs,
                    )
                } else if let Some(effect) = effect_kind(value) {
                    let name = effect_name(value, &effect);
                    (
                        WorkflowNodeKind::Effect {
                            binding: binding.as_ref().map(assign_target_text),
                            effect,
                            expression: expression_text(value),
                        },
                        name,
                        outputs,
                    )
                } else {
                    (
                        WorkflowNodeKind::Computation {
                            binding: binding.as_ref().map(assign_target_text),
                            expression: expression_text(value),
                        },
                        computation_name(value),
                        outputs,
                    )
                }
            }
        }
    }

    fn node_id(&self, owner: &str, path: &[u32], kind: &str) -> WorkflowNodeId {
        let path = if path.is_empty() {
            "root".to_string()
        } else {
            path.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(".")
        };
        let material = format!("{}\0{owner}\0{path}\0{kind}", self.source_hash);
        WorkflowNodeId(format!(
            "{kind}:{}",
            &hex_digest("lash-workflow-node/v2", material.as_bytes())[..24]
        ))
    }
}

#[derive(Clone, Default)]
struct VersionState {
    next: BTreeMap<String, u32>,
    current: BTreeMap<String, (u32, WorkflowNodeId)>,
    known: BTreeSet<String>,
}

impl VersionState {
    fn seed(&mut self, variable: &str) {
        self.known.insert(variable.to_string());
        self.next.entry(variable.to_string()).or_insert(1);
    }

    fn allocate(&mut self, variable: &str) -> VariableVersion {
        self.known.insert(variable.to_string());
        let version = *self.next.entry(variable.to_string()).or_insert(1);
        self.next.insert(variable.to_string(), version + 1);
        VariableVersion {
            variable: variable.to_string(),
            version,
        }
    }

    fn is_known(&self, variable: &str) -> bool {
        self.known.contains(variable)
    }

    fn shadow(&mut self, variable: &str) {
        self.known.insert(variable.to_string());
        self.current.remove(variable);
        self.next.insert(variable.to_string(), 1);
    }

    fn record_outputs(&mut self, outputs: &[VariableVersion], node: &WorkflowNodeId) {
        for output in outputs {
            self.current
                .insert(output.variable.clone(), (output.version, node.clone()));
            self.next
                .entry(output.variable.clone())
                .and_modify(|next| *next = (*next).max(output.version + 1))
                .or_insert(output.version + 1);
        }
    }

    fn merge_outputs(&mut self, left: &Self, right: &Self) -> Vec<VariableVersion> {
        let variables = left
            .current
            .keys()
            .chain(right.current.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let changed = variables
            .into_iter()
            .filter(|variable| {
                left.current.get(variable) != self.current.get(variable)
                    || right.current.get(variable) != self.current.get(variable)
            })
            .collect::<Vec<_>>();
        changed
            .into_iter()
            .map(|variable| self.allocate(&variable))
            .collect()
    }
}

fn validate_graph(graph: &WorkflowGraph) -> Result<(), GraphRenderError> {
    if graph.schema_version != WORKFLOW_GRAPH_SCHEMA_VERSION {
        return Err(GraphRenderError::UnsupportedSchemaVersion {
            found: graph.schema_version,
            expected: WORKFLOW_GRAPH_SCHEMA_VERSION,
        });
    }
    let mut all_ids = BTreeSet::new();
    validate_subgraph(&graph.main, &mut all_ids)?;
    let mut process_names = BTreeSet::new();
    for declaration in &graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            if !process_names.insert(process.name.clone()) {
                return Err(GraphRenderError::DuplicateProcessName {
                    name: process.name.clone(),
                });
            }
            if !all_ids.insert(process.id.clone()) {
                return Err(GraphRenderError::DuplicateNodeId {
                    id: process.id.to_string(),
                });
            }
            validate_subgraph(&process.body, &mut all_ids)?;
        }
    }
    Ok(())
}

fn validate_subgraph(
    graph: &WorkflowSubgraph,
    all_ids: &mut BTreeSet<WorkflowNodeId>,
) -> Result<(), GraphRenderError> {
    let local_ids = graph
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    if local_ids.len() != graph.nodes.len() {
        let mut seen = BTreeSet::new();
        let id = graph
            .nodes
            .iter()
            .find(|node| !seen.insert(node.id.clone()))
            .expect("a duplicate exists")
            .id
            .to_string();
        return Err(GraphRenderError::DuplicateNodeId { id });
    }
    for node in &graph.nodes {
        if !all_ids.insert(node.id.clone()) {
            return Err(GraphRenderError::DuplicateNodeId {
                id: node.id.to_string(),
            });
        }
        validate_node(node, all_ids)?;
    }
    for edge in &graph.edges {
        if !local_ids.contains(&edge.from) && !all_ids.contains(&edge.from) {
            return Err(GraphRenderError::UnknownNodeReference {
                edge_id: edge.id.clone(),
                endpoint: "source",
                node_id: edge.from.to_string(),
            });
        }
        if !local_ids.contains(&edge.to) && !all_ids.contains(&edge.to) {
            return Err(GraphRenderError::UnknownNodeReference {
                edge_id: edge.id.clone(),
                endpoint: "target",
                node_id: edge.to.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_node(
    node: &WorkflowNode,
    all_ids: &mut BTreeSet<WorkflowNodeId>,
) -> Result<(), GraphRenderError> {
    match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::If {
            then_is_block,
            else_is_block,
            then_graph,
            else_graph,
            ..
        }) => {
            let then_graph =
                then_graph
                    .as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "then_graph",
                    })?;
            let else_graph =
                else_graph
                    .as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "else_graph",
                    })?;
            validate_subgraph(then_graph, all_ids)?;
            validate_subgraph(else_graph, all_ids)?;
            if !then_is_block && *else_is_block {
                return invalid_payload(
                    node,
                    "expression if cannot have a statement-block else branch",
                );
            }
            if !then_is_block && (then_graph.nodes.len() != 1 || else_graph.nodes.len() != 1) {
                return invalid_payload(
                    node,
                    "expression-if branches must contain exactly one value node",
                );
            }
            if *then_is_block && !else_is_block {
                let is_direct_else_if = matches!(
                    else_graph.nodes.as_slice(),
                    [WorkflowNode {
                        kind: WorkflowNodeKind::Container(WorkflowContainer::If {
                            then_is_block: true,
                            ..
                        }),
                        ..
                    }]
                );
                if !is_direct_else_if {
                    return invalid_payload(
                        node,
                        "non-block statement-if else branch must be a direct else if",
                    );
                }
            }
        }
        WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) => {
            let body = body
                .as_deref()
                .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                    node_id: node.id.to_string(),
                    child: "body",
                })?;
            validate_subgraph(body, all_ids)?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }) => {
            let body = body
                .as_deref()
                .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                    node_id: node.id.to_string(),
                    child: "body",
                })?;
            validate_subgraph(body, all_ids)?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
            clauses,
            element,
            ..
        }) => {
            if clauses.is_empty() {
                return invalid_payload(
                    node,
                    "list-comprehension container requires at least one clause",
                );
            }
            let element =
                element
                    .as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "element",
                    })?;
            validate_subgraph(element, all_ids)?;
            if element.nodes.len() != 1 {
                return invalid_payload(
                    node,
                    "list-comprehension element must contain exactly one node",
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn invalid_payload<T>(node: &WorkflowNode, message: &str) -> Result<T, GraphRenderError> {
    Err(GraphRenderError::InvalidNodePayload {
        node_id: node.id.to_string(),
        message: message.to_string(),
    })
}

fn graph_to_program(graph: &WorkflowGraph) -> Result<Program, GraphRenderError> {
    let mut declarations = Vec::with_capacity(graph.declarations.len());
    for declaration in &graph.declarations {
        declarations.push(match declaration {
            WorkflowDeclaration::Type(ty) => Declaration::Type(ty.clone()),
            WorkflowDeclaration::Function(function) => Declaration::Function(function.clone()),
            WorkflowDeclaration::Process(process) => {
                let label =
                    (process.name_source == WorkflowNodeNameSource::Label).then(|| LabelMetadata {
                        title: process.display_name.clone().into(),
                        description: process.description.clone().map(Into::into),
                    });
                Declaration::Process(ProcessDecl {
                    name: process.name.clone().into(),
                    params: process.params.clone(),
                    signals: process.signals.clone(),
                    return_ty: process.return_ty.clone(),
                    label,
                    body: subgraph_to_block(&process.body, RenderContext::Process)?,
                })
            }
        });
    }
    Ok(Program {
        declarations,
        main: subgraph_to_block(&graph.main, RenderContext::Main)?,
        declaration_spans: Vec::new(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    })
}

#[derive(Clone, Copy)]
enum RenderContext {
    Main,
    Process,
}

fn subgraph_to_block(
    graph: &WorkflowSubgraph,
    context: RenderContext,
) -> Result<Expr, GraphRenderError> {
    graph
        .nodes
        .iter()
        .map(|node| node_to_expr(node, context))
        .collect::<Result<Vec<_>, _>>()
        .map(Expr::Block)
}

fn node_to_expr(node: &WorkflowNode, context: RenderContext) -> Result<Expr, GraphRenderError> {
    let expression = match &node.kind {
        WorkflowNodeKind::Data {
            binding,
            expression,
        } => {
            let expression = parse_expression_field(node, "expression", expression)?;
            if !is_pure_expr(&expression) && !matches!(expression, Expr::TypeLiteral(_)) {
                return invalid_payload(node, "data expression is effectful");
            }
            with_assignment(node, binding, expression, true)?
        }
        WorkflowNodeKind::Call {
            binding,
            expression,
            operation,
        } => {
            let expression = parse_expression_field(node, "expression", expression)?;
            if first_receiver_operation(&expression) != Some(operation.as_str()) {
                return invalid_payload(
                    node,
                    "call operation does not match its receiver-call expression",
                );
            }
            with_assignment(node, binding, expression, true)?
        }
        WorkflowNodeKind::Effect {
            binding,
            expression,
            effect,
        } => {
            let expression = parse_expression_field(node, "expression", expression)?;
            if effect_kind(&expression).as_ref() != Some(effect) {
                return invalid_payload(node, "effect kind does not match its expression");
            }
            with_assignment(node, binding, expression, true)?
        }
        WorkflowNodeKind::Computation {
            binding,
            expression,
        } => {
            let expression = parse_expression_field(node, "expression", expression)?;
            with_assignment(node, binding, expression, true)?
        }
        WorkflowNodeKind::StateUpdate { target, expression } => {
            let target = parse_assignment_target_field(node, "target", target)?;
            let [output] = node.outputs.as_slice() else {
                return invalid_payload(node, "state update must have exactly one output");
            };
            if target.root.as_str() != output.variable {
                return invalid_payload(node, "state-update target root must match its output");
            }
            Expr::Assign {
                target,
                expr: Box::new(parse_expression_field(node, "expression", expression)?),
            }
        }
        WorkflowNodeKind::Terminal {
            terminal,
            expression,
        } => {
            let expression = parse_expression_field(node, "expression", expression)?;
            let valid = matches!(
                (terminal, &expression),
                (WorkflowTerminalKind::Finish, Expr::Finish(_))
                    | (WorkflowTerminalKind::Fail, Expr::Fail(_))
            );
            if !valid {
                return invalid_payload(node, "terminal kind does not match its expression");
            }
            expression
        }
        WorkflowNodeKind::Container(WorkflowContainer::If {
            binding,
            condition,
            then_is_block,
            else_is_block,
            then_graph,
            else_graph,
        }) => {
            let then_graph =
                then_graph
                    .as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "then_graph",
                    })?;
            let else_graph =
                else_graph
                    .as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "else_graph",
                    })?;
            with_assignment(
                node,
                binding,
                Expr::If {
                    condition: Box::new(parse_expression_field(node, "condition", condition)?),
                    then_block: Box::new(subgraph_to_branch(
                        node,
                        then_graph,
                        context,
                        *then_is_block,
                        "then_graph",
                    )?),
                    else_block: Box::new(subgraph_to_branch(
                        node,
                        else_graph,
                        context,
                        *else_is_block,
                        "else_graph",
                    )?),
                },
                true,
            )?
        }
        WorkflowNodeKind::Container(WorkflowContainer::For {
            binding,
            iterable,
            body,
        }) => Expr::For {
            binding: parse_simple_binding_field(node, "binding", binding)?.root,
            iterable: Box::new(parse_expression_field(node, "iterable", iterable)?),
            body: Box::new(subgraph_to_block(
                body.as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "body",
                    })?,
                context,
            )?),
        },
        WorkflowNodeKind::Container(WorkflowContainer::While { condition, body }) => Expr::While {
            condition: Box::new(parse_expression_field(node, "condition", condition)?),
            body: Box::new(subgraph_to_block(
                body.as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "body",
                    })?,
                context,
            )?),
        },
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
            binding,
            clauses,
            element,
        }) => {
            let element =
                element
                    .as_deref()
                    .ok_or_else(|| GraphRenderError::MissingRequiredChild {
                        node_id: node.id.to_string(),
                        child: "element",
                    })?;
            let Expr::Block(mut expressions) = subgraph_to_block(element, context)? else {
                unreachable!("subgraph rendering always returns a block")
            };
            if expressions.len() != 1 {
                return invalid_payload(
                    node,
                    "list-comprehension element must contain exactly one node",
                );
            }
            with_assignment(
                node,
                binding,
                Expr::ListComprehension {
                    element: Box::new(expressions.remove(0)),
                    clauses: parse_comprehension_clauses(node, clauses)?,
                },
                true,
            )?
        }
        WorkflowNodeKind::Opaque { source } => parse_opaque_statement(node, source, context)?,
    };
    Ok(if node.name_source == WorkflowNodeNameSource::Label {
        Expr::LabelAnnotated {
            label: LabelMetadata {
                title: node.name.clone().into(),
                description: node.description.clone().map(Into::into),
            },
            expr: Box::new(expression),
        }
    } else {
        expression
    })
}

fn subgraph_to_branch(
    node: &WorkflowNode,
    graph: &WorkflowSubgraph,
    context: RenderContext,
    is_block: bool,
    child: &'static str,
) -> Result<Expr, GraphRenderError> {
    if is_block {
        return subgraph_to_block(graph, context);
    }
    let [branch] = graph.nodes.as_slice() else {
        return Err(GraphRenderError::InvalidNodePayload {
            node_id: node.id.to_string(),
            message: format!("non-block {child} must contain exactly one node"),
        });
    };
    node_to_expr(branch, context)
}

fn parse_opaque_statement(
    node: &WorkflowNode,
    source: &str,
    context: RenderContext,
) -> Result<Expr, GraphRenderError> {
    let program = match context {
        RenderContext::Main => parse(source),
        RenderContext::Process => parse(&format!("process __workflow_graph__() {{\n{source}\n}}")),
    }
    .map_err(|error| GraphRenderError::InvalidOpaqueSource {
        node_id: node.id.to_string(),
        message: error.to_string(),
    })?;
    let expressions = match context {
        RenderContext::Main => match program.main {
            Expr::Block(expressions) => expressions,
            expression => vec![expression],
        },
        RenderContext::Process => {
            let Some(Declaration::Process(process)) = program.declarations.into_iter().next()
            else {
                return invalid_payload(node, "opaque process wrapper did not produce a process");
            };
            match process.body {
                Expr::Block(expressions) => expressions,
                expression => vec![expression],
            }
        }
    };
    if expressions.len() != 1 {
        return Err(GraphRenderError::InvalidOpaqueSource {
            node_id: node.id.to_string(),
            message: format!("expected one statement, found {}", expressions.len()),
        });
    }
    Ok(expressions.into_iter().next().expect("one expression"))
}

fn collect_subgraph_nodes<'a>(graph: &'a WorkflowSubgraph, nodes: &mut Vec<&'a WorkflowNode>) {
    for node in &graph.nodes {
        nodes.push(node);
        match &node.kind {
            WorkflowNodeKind::Container(WorkflowContainer::If {
                then_graph,
                else_graph,
                ..
            }) => {
                if let Some(graph) = then_graph {
                    collect_subgraph_nodes(graph, nodes);
                }
                if let Some(graph) = else_graph {
                    collect_subgraph_nodes(graph, nodes);
                }
            }
            WorkflowNodeKind::Container(WorkflowContainer::For {
                body: Some(graph), ..
            }) => collect_subgraph_nodes(graph, nodes),
            WorkflowNodeKind::Container(WorkflowContainer::While {
                body: Some(graph), ..
            }) => collect_subgraph_nodes(graph, nodes),
            WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
                element: Some(graph),
                ..
            }) => collect_subgraph_nodes(graph, nodes),
            _ => {}
        }
    }
}

fn add_dependency_edges(
    edges: &mut Vec<WorkflowEdge>,
    node: &WorkflowNode,
    expression: &Expr,
    versions: &VersionState,
) {
    let mut variables = BTreeSet::new();
    collect_variables(expression, &mut variables);
    for variable in variables {
        if let Some((version, producer)) = versions.current.get(&variable) {
            edges.push(edge(
                producer.clone(),
                node.id.clone(),
                WorkflowEdgeKind::DataDependency {
                    variable,
                    version: *version,
                },
            ));
        }
    }
}

fn collect_variables(expression: &Expr, variables: &mut BTreeSet<String>) {
    collect_free_variables(expression, &BTreeSet::new(), variables);
}

fn collect_free_variables(
    expression: &Expr,
    bound: &BTreeSet<String>,
    variables: &mut BTreeSet<String>,
) {
    match expression {
        Expr::Variable(variable) if !bound.contains(variable.as_str()) => {
            variables.insert(variable.to_string());
        }
        Expr::Assign { target, .. }
            if !target.is_simple() && !bound.contains(target.root.as_str()) =>
        {
            variables.insert(target.root.to_string());
            for child in expression.children() {
                collect_free_variables(child, bound, variables);
            }
        }
        Expr::For {
            binding,
            iterable,
            body,
        } => {
            collect_free_variables(iterable, bound, variables);
            let mut body_bound = bound.clone();
            body_bound.insert(binding.to_string());
            collect_free_variables(body, &body_bound, variables);
        }
        Expr::ListComprehension { element, clauses } => {
            let mut clause_bound = bound.clone();
            for clause in clauses {
                match clause {
                    ListComprehensionClause::For { binding, iterable } => {
                        collect_free_variables(iterable, &clause_bound, variables);
                        clause_bound.insert(binding.to_string());
                    }
                    ListComprehensionClause::If { condition } => {
                        collect_free_variables(condition, &clause_bound, variables);
                    }
                }
            }
            collect_free_variables(element, &clause_bound, variables);
        }
        _ => {
            for child in expression.children() {
                collect_free_variables(child, bound, variables);
            }
        }
    }
}

fn edge(from: WorkflowNodeId, to: WorkflowNodeId, kind: WorkflowEdgeKind) -> WorkflowEdge {
    let kind_key = match &kind {
        WorkflowEdgeKind::Sequence => "sequence".to_string(),
        WorkflowEdgeKind::DataDependency { variable, version } => {
            format!("data:{variable}:{version}")
        }
    };
    let material = format!("{}\0{}\0{kind_key}", from.as_str(), to.as_str());
    WorkflowEdge {
        id: format!(
            "edge:{}",
            &hex_digest("lash-workflow-edge/v2", material.as_bytes())[..24]
        ),
        from,
        to,
        kind,
    }
}

fn node_is_sequenced(node: &WorkflowNode) -> bool {
    !matches!(node.kind, WorkflowNodeKind::Data { .. })
}

fn assignment_parts<'a>(
    expression: &'a Expr,
    path: &[u32],
) -> (Option<AssignTarget>, &'a Expr, Vec<u32>) {
    match expression {
        Expr::Assign { target, expr } => {
            let dynamic_indices = target
                .steps
                .iter()
                .filter(|step| matches!(step, crate::AssignPathStep::Index(_)))
                .count() as u32;
            (
                Some(target.clone()),
                expr,
                child_path(path, dynamic_indices),
            )
        }
        _ => (None, expression, path.to_vec()),
    }
}

fn assignment_output(
    binding: Option<&AssignTarget>,
    versions: &mut VersionState,
) -> Vec<VariableVersion> {
    binding
        .filter(|target| target.is_simple())
        .map(|target| vec![versions.allocate(target.root.as_str())])
        .unwrap_or_default()
}

fn loop_outputs(
    body: &Expr,
    scoped_binding: Option<&str>,
    versions: &mut VersionState,
) -> Vec<VariableVersion> {
    let mut assigned = BTreeSet::new();
    collect_assignment_roots(body, &mut assigned);
    if let Some(binding) = scoped_binding {
        assigned.remove(binding);
    }
    assigned
        .into_iter()
        .map(|variable| versions.allocate(&variable))
        .collect()
}

fn collect_assignment_roots(expression: &Expr, assigned: &mut BTreeSet<String>) {
    if let Expr::Assign { target, .. } = expression {
        assigned.insert(target.root.to_string());
    }
    for child in expression.children() {
        collect_assignment_roots(child, assigned);
    }
}

fn peel_label(expression: &Expr) -> (Option<&LabelMetadata>, &Expr) {
    match expression {
        Expr::LabelAnnotated { label, expr } => (Some(label), expr),
        _ => (None, expression),
    }
}

fn first_receiver_operation(expression: &Expr) -> Option<&str> {
    match expression {
        Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
        Expr::Await(expr) => match expr.as_ref() {
            Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
            Expr::ResultUnwrap(inner) => match inner.as_ref() {
                Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
                _ => None,
            },
            _ => None,
        },
        Expr::ResultUnwrap(expr) => match expr.as_ref() {
            Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
            Expr::Await(inner) => match inner.as_ref() {
                Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn effect_kind(expression: &Expr) -> Option<WorkflowEffectKind> {
    if let Expr::ResultUnwrap(expr) = expression {
        return direct_effect_kind(expr);
    }
    direct_effect_kind(expression)
}

fn direct_effect_kind(expression: &Expr) -> Option<WorkflowEffectKind> {
    match expression {
        Expr::StartProcess(_) => Some(WorkflowEffectKind::StartProcess),
        Expr::Await(_) => Some(WorkflowEffectKind::AwaitJoin),
        Expr::SignalRun { .. } => Some(WorkflowEffectKind::SignalRun),
        Expr::WaitSignal { .. } => Some(WorkflowEffectKind::WaitSignal),
        Expr::SleepFor(_) | Expr::SleepUntil(_) => Some(WorkflowEffectKind::Sleep),
        Expr::Cancel(_) => Some(WorkflowEffectKind::Cancel),
        Expr::Print(_) => Some(WorkflowEffectKind::Print),
        Expr::Yield(_) => Some(WorkflowEffectKind::Yield),
        Expr::Wake(_) => Some(WorkflowEffectKind::Wake),
        Expr::Break => Some(WorkflowEffectKind::Break),
        Expr::Continue => Some(WorkflowEffectKind::Continue),
        _ => None,
    }
}

fn effect_name(expression: &Expr, effect: &WorkflowEffectKind) -> String {
    match expression {
        Expr::StartProcess(start) => format!("start {}", start.process),
        Expr::WaitSignal { name } => format!("wait_signal {name}"),
        Expr::SleepFor(_) => "sleep for".to_string(),
        Expr::SleepUntil(_) => "sleep until".to_string(),
        _ => match effect {
            WorkflowEffectKind::StartProcess => "start process",
            WorkflowEffectKind::AwaitJoin => "await",
            WorkflowEffectKind::SignalRun => "signal_run",
            WorkflowEffectKind::WaitSignal => "wait_signal",
            WorkflowEffectKind::Sleep => "sleep",
            WorkflowEffectKind::Cancel => "cancel",
            WorkflowEffectKind::Print => "print",
            WorkflowEffectKind::Yield => "yield",
            WorkflowEffectKind::Wake => "wake",
            WorkflowEffectKind::Break => "break",
            WorkflowEffectKind::Continue => "continue",
        }
        .to_string(),
    }
}

fn data_name(expression: &Expr) -> String {
    match expression {
        Expr::BuiltinCall { name, .. } => name.to_string(),
        Expr::List(_) => "list".to_string(),
        Expr::Record(_) => "record".to_string(),
        Expr::Tuple(_) => "tuple".to_string(),
        Expr::Variable(name) => name.to_string(),
        _ => "data".to_string(),
    }
}

fn computation_name(expression: &Expr) -> String {
    match expression {
        Expr::Tuple(_) => "tuple computation",
        Expr::List(_) => "list computation",
        Expr::Record(_) => "record computation",
        Expr::BuiltinCall { name, .. } => return name.to_string(),
        Expr::Binary { .. } => "binary computation",
        Expr::Unary { .. } => "unary computation",
        Expr::Field { .. } => "field computation",
        Expr::Index { .. } => "index computation",
        Expr::ResultUnwrap(_) => "result computation",
        _ => "computation",
    }
    .to_string()
}

fn kind_tag(kind: &WorkflowNodeKind) -> &'static str {
    match kind {
        WorkflowNodeKind::Data { .. } => "data",
        WorkflowNodeKind::Call { .. } => "call",
        WorkflowNodeKind::Effect { .. } => "effect",
        WorkflowNodeKind::Computation { .. } => "computation",
        WorkflowNodeKind::StateUpdate { .. } => "state_update",
        WorkflowNodeKind::Terminal { .. } => "terminal",
        WorkflowNodeKind::Container(_) => "container",
        WorkflowNodeKind::Opaque { .. } => "opaque",
    }
}

fn child_path(path: &[u32], child: impl TryInto<u32>) -> Vec<u32> {
    let mut result = path.to_vec();
    result.push(child.try_into().ok().expect("AST child index fits u32"));
    result
}

fn hex_digest(domain: &str, bytes: &[u8]) -> String {
    lash_sansio::core_support::blake3_domain_hash_hex(domain, bytes)
}

#[cfg(test)]
mod tests;
