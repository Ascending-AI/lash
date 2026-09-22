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
use crate::ast::{
    AssignTarget, AstString, Expr, FunctionDecl, ProcessParam, ProcessSignalDecl, TypeDecl,
    TypeExpr,
};
use crate::span::Span;
use crate::tracking::WorkflowExecutionSite;

mod execution_sites;
mod facets;

pub use execution_sites::{execution_sites, runtime_execution_site_for_workflow_site};
pub use facets::*;

/// Version of the serialized workflow graph contract.
pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 11;

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
        binding: Option<AssignTarget>,
        expression: Expr,
    },
    Call {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AssignTarget>,
        receiver: Expr,
        operation: String,
        #[serde(default)]
        arguments: Vec<WorkflowArgument>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        result_steps: Vec<WorkflowResultStep>,
    },
    Effect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AssignTarget>,
        effect: WorkflowEffectKind,
        #[serde(default)]
        arguments: Vec<WorkflowArgument>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        result_steps: Vec<WorkflowResultStep>,
    },
    Computation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AssignTarget>,
        expression: Expr,
    },
    StateUpdate {
        target: AssignTarget,
        expression: Expr,
    },
    Terminal {
        terminal: WorkflowTerminalKind,
        expression: Expr,
    },
    Container(WorkflowContainer),
    Opaque {
        source: String,
    },
}

/// One call or effect argument in graph order.
///
/// Type facets address these values with a serialized [`WorkflowSlotPath`].
/// Its typed call, argument, field, and index segments cannot collide when a
/// field contains punctuation. Nodes with several nested receiver calls add a
/// call segment in depth-first IR walk order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowArgument {
    Positional { value: Expr },
    Named { fields: Vec<(AstString, Expr)> },
}

/// Ordered wrappers around a call or effect, from the operation outwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowResultStep {
    Await,
    UnwrapResult,
}

/// Decomposes one receiver call into the graph's structural IR fields.
pub fn workflow_call_from_ir(
    expression: &Expr,
) -> Option<(Expr, String, Vec<WorkflowArgument>, Vec<WorkflowResultStep>)> {
    let (call, result_steps) = peel_call_result_steps(expression);
    let Expr::ReceiverCall {
        receiver,
        operation,
        args,
    } = call
    else {
        return None;
    };
    Some((
        receiver.as_ref().clone(),
        operation.to_string(),
        args.iter().map(workflow_argument_from_ir).collect(),
        result_steps,
    ))
}

/// Reconstructs the authoritative IR represented by a call node.
pub fn workflow_call_to_ir(
    receiver: &Expr,
    operation: &str,
    arguments: &[WorkflowArgument],
    result_steps: &[WorkflowResultStep],
) -> Expr {
    apply_result_steps(
        Expr::ReceiverCall {
            receiver: Box::new(receiver.clone()),
            operation: operation.into(),
            args: arguments.iter().map(workflow_argument_to_ir).collect(),
        },
        result_steps,
    )
}

/// Decomposes one effect into its exact kind, arguments, and outer result steps.
pub fn workflow_effect_from_ir(
    expression: &Expr,
) -> Option<(
    WorkflowEffectKind,
    Vec<WorkflowArgument>,
    Vec<WorkflowResultStep>,
)> {
    let (effect, result_steps) = peel_result_steps(expression);
    let (kind, args) = match effect {
        Expr::Await(value) => (WorkflowEffectKind::AwaitJoin, vec![value.as_ref().clone()]),
        Expr::WaitSignal { name } => (
            WorkflowEffectKind::WaitSignal,
            vec![Expr::String(name.clone())],
        ),
        Expr::SleepFor(value) => (WorkflowEffectKind::SleepFor, vec![value.as_ref().clone()]),
        Expr::SleepUntil(value) => (WorkflowEffectKind::SleepUntil, vec![value.as_ref().clone()]),
        Expr::Print(value) => (WorkflowEffectKind::Print, vec![value.as_ref().clone()]),
        Expr::Yield(value) => (WorkflowEffectKind::Yield, vec![value.as_ref().clone()]),
        Expr::Break => (WorkflowEffectKind::Break, Vec::new()),
        Expr::Continue => (WorkflowEffectKind::Continue, Vec::new()),
        _ => return None,
    };
    Some((
        kind,
        args.iter().map(workflow_argument_from_ir).collect(),
        result_steps,
    ))
}

/// Reconstructs the authoritative IR represented by an effect node.
pub fn workflow_effect_to_ir(
    effect: WorkflowEffectKind,
    arguments: &[WorkflowArgument],
    result_steps: &[WorkflowResultStep],
) -> Option<Expr> {
    let values = arguments
        .iter()
        .map(workflow_argument_to_ir)
        .collect::<Vec<_>>();
    let expression = match (effect, values.as_slice()) {
        (WorkflowEffectKind::AwaitJoin, [value]) => Expr::Await(Box::new(value.clone())),
        (WorkflowEffectKind::WaitSignal, [Expr::String(name)]) => {
            Expr::WaitSignal { name: name.clone() }
        }
        (WorkflowEffectKind::SleepFor, [value]) => Expr::SleepFor(Box::new(value.clone())),
        (WorkflowEffectKind::SleepUntil, [value]) => Expr::SleepUntil(Box::new(value.clone())),
        (WorkflowEffectKind::Print, [value]) => Expr::Print(Box::new(value.clone())),
        (WorkflowEffectKind::Yield, [value]) => Expr::Yield(Box::new(value.clone())),
        (WorkflowEffectKind::Break, []) => Expr::Break,
        (WorkflowEffectKind::Continue, []) => Expr::Continue,
        _ => return None,
    };
    Some(apply_result_steps(expression, result_steps))
}

fn workflow_argument_from_ir(value: &Expr) -> WorkflowArgument {
    match value {
        Expr::Record(fields) => WorkflowArgument::Named {
            fields: fields.clone(),
        },
        value => WorkflowArgument::Positional {
            value: value.clone(),
        },
    }
}

fn workflow_argument_to_ir(argument: &WorkflowArgument) -> Expr {
    match argument {
        WorkflowArgument::Positional { value } => value.clone(),
        WorkflowArgument::Named { fields } => Expr::Record(fields.clone()),
    }
}

fn peel_result_steps(expression: &Expr) -> (&Expr, Vec<WorkflowResultStep>) {
    let mut expression = expression;
    let mut outer_steps = Vec::new();
    loop {
        match expression {
            Expr::ResultUnwrap(inner) => {
                outer_steps.push(WorkflowResultStep::UnwrapResult);
                expression = inner;
            }
            _ => {
                outer_steps.reverse();
                return (expression, outer_steps);
            }
        }
    }
}

fn peel_call_result_steps(expression: &Expr) -> (&Expr, Vec<WorkflowResultStep>) {
    let mut expression = expression;
    let mut outer_steps = Vec::new();
    loop {
        match expression {
            Expr::Await(inner) => {
                outer_steps.push(WorkflowResultStep::Await);
                expression = inner;
            }
            Expr::ResultUnwrap(inner) => {
                outer_steps.push(WorkflowResultStep::UnwrapResult);
                expression = inner;
            }
            _ => {
                outer_steps.reverse();
                return (expression, outer_steps);
            }
        }
    }
}

fn apply_result_steps(mut expression: Expr, result_steps: &[WorkflowResultStep]) -> Expr {
    for step in result_steps {
        expression = match step {
            WorkflowResultStep::Await => Expr::Await(Box::new(expression)),
            WorkflowResultStep::UnwrapResult => Expr::ResultUnwrap(Box::new(expression)),
        };
    }
    expression
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEffectKind {
    AwaitJoin,
    WaitSignal,
    SleepFor,
    SleepUntil,
    Print,
    Yield,
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
        binding: Option<AssignTarget>,
        condition: Expr,
        /// Whether the source's then branch is a statement block rather than a value expression.
        then_is_block: bool,
        /// Whether the source's else branch is a block rather than a direct value or `else if`.
        else_is_block: bool,
        then_graph: Box<WorkflowSubgraph>,
        else_graph: Box<WorkflowSubgraph>,
    },
    For {
        binding: String,
        iterable: Expr,
        body: Box<WorkflowSubgraph>,
    },
    While {
        condition: Expr,
        body: Box<WorkflowSubgraph>,
    },
    ListComprehension {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AssignTarget>,
        clauses: Vec<WorkflowListComprehensionClause>,
        element: Box<WorkflowSubgraph>,
    },
}

impl WorkflowContainer {
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
    For { binding: String, iterable: Expr },
    If { condition: Expr },
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
#[expect(
    clippy::expect_used,
    reason = "a child index into an in-memory AST whose children are enumerated far below u32::MAX fits, per the message"
)]
pub fn child_path(path: &[u32], child: impl TryInto<u32>) -> Vec<u32> {
    let mut result = path.to_vec();
    result.push(child.try_into().ok().expect("AST child index fits u32"));
    result
}
