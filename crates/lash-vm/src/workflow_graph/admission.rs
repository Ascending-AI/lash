//! Admitting a workflow document against a host environment (FIG-5574).
//!
//! [`admit_workflow_graph`] is the one path from a document to an artifact:
//! it reconstructs the IR the document spells and links it against the
//! environment, so the linker derives every lifted declaration, type,
//! signature and host requirement again. Nothing the document states about
//! itself (ids, source identity, facets, lifted names, refined capture
//! types) is taken as true.
//!
//! A reference to a lifted process is linked by identity (FIG-5640). The
//! program handed to the linker holds the document's lifted declarations and
//! its references to them as they are: the linker derives each declaration
//! again where it is first referenced, names it by a digest of what it
//! derived ([`crate::lifted_process_name`]) and resolves every reference to
//! it. No reference is ever spelled as a variable, so no edit of a binding
//! name can change which process a reference means, and an unchanged lifted
//! process is the same process because it derives to the same content.
//!
//! No dialect is involved. A refusal is located at a node and expression
//! path of the submitted document, and an admission says which admitted node
//! each submitted node became. Both come from the linker's own path table,
//! never from positions: every expression of the program handed to the
//! linker is tagged, and the tags are read back from what it produced.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ast::{AstPath, AstRoot, Declaration, Expr, Program};
use crate::{LashVmHostEnvironment, LinkError, LinkedModule, Span, lifted_process_identity};

use super::projection::{collect_process_literals, expr_at, statement_addresses};
use super::reconstruction::workflow_program_from_graph;
use super::{WorkflowGraph, WorkflowNodeId, WorkflowSlotPath, workflow_graph_from_artifact};

/// A document admitted against a host environment.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowAdmission {
    pub linked: LinkedModule,
    /// The admitted document: the graph of the program the artifact
    /// executes, under the artifact's source identity.
    pub graph: WorkflowGraph,
    /// The admitted id of each node and process container of the submitted
    /// document, keyed by its id in the document's canonical form (which is
    /// the form a [`super::WorkflowDraft`] exports). A node admission did not
    /// keep as one statement has no entry.
    pub nodes: BTreeMap<WorkflowNodeId, WorkflowNodeId>,
}

/// Where in the submitted document an admission diagnostic points.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "at", deny_unknown_fields)]
pub enum WorkflowAdmissionLocation {
    Document,
    /// An expression of a node: `slot` is the path from the statement
    /// [`super::workflow_node_statement`] spells, empty for the statement
    /// itself.
    Node {
        node: WorkflowNodeId,
        slot: WorkflowSlotPath,
    },
    Process {
        name: String,
    },
    Function {
        name: String,
    },
}

/// What an admission refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkflowAdmissionDiagnosticKind {
    /// The document is not one this build reads, or does not spell a program.
    Document,
    /// The program the document spells breaks a rule of the IR itself.
    InvalidProgram,
    /// The host environment does not provide what the program requires: a
    /// module, an operation, a builtin or a language feature.
    HostRequirement,
    /// A name, process or type the program reads is not bound.
    UnresolvedName,
    /// A value does not have the type its place expects.
    Type,
    /// A construct sits where the language does not allow it.
    Placement,
    /// The selected entry is not a process the admitted module exports.
    Entry,
    /// The admitted program could not be made an artifact.
    Artifact,
}

/// One refusal, at the place it concerns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAdmissionDiagnostic {
    pub location: WorkflowAdmissionLocation,
    pub kind: WorkflowAdmissionDiagnosticKind,
    pub message: String,
}

/// A document that was not admitted. Nothing was derived from it.
#[derive(Clone, Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
#[error("the workflow document was refused admission with {} diagnostic(s)", .diagnostics.len())]
#[serde(deny_unknown_fields)]
pub struct WorkflowAdmissionRefusal {
    pub diagnostics: Vec<WorkflowAdmissionDiagnostic>,
}

impl WorkflowAdmissionRefusal {
    pub fn of(
        location: WorkflowAdmissionLocation,
        kind: WorkflowAdmissionDiagnosticKind,
        message: impl std::fmt::Display,
    ) -> Self {
        Self {
            diagnostics: vec![WorkflowAdmissionDiagnostic {
                location,
                kind,
                message: message.to_string(),
            }],
        }
    }

    fn document(message: impl std::fmt::Display) -> Self {
        Self::of(
            WorkflowAdmissionLocation::Document,
            WorkflowAdmissionDiagnosticKind::Document,
            message,
        )
    }
}

/// Admits `graph` against `environment`: structure, then linking, then the
/// artifact. A refusal derives nothing.
pub fn admit_workflow_graph(
    graph: &WorkflowGraph,
    environment: &LashVmHostEnvironment,
) -> Result<WorkflowAdmission, WorkflowAdmissionRefusal> {
    WorkflowGraph::admit_schema_version_for_fleet(
        graph.schema_version,
        lash_core_execution::FleetFormat::current(),
    )
    .map_err(WorkflowAdmissionRefusal::document)?;
    let submitted = workflow_program_from_graph(graph).map_err(|error| {
        let location = error
            .node_id()
            .and_then(|node| WorkflowNodeId::new(node).ok())
            .map(|node| WorkflowAdmissionLocation::Node {
                node,
                slot: WorkflowSlotPath::default(),
            })
            .unwrap_or(WorkflowAdmissionLocation::Document);
        WorkflowAdmissionRefusal::of(location, WorkflowAdmissionDiagnosticKind::Document, error)
    })?;
    let submitted_addresses = statement_addresses(&submitted);

    // Tag every expression with a span that is its index, so the linker's
    // path table says where each one went and which one an error is at.
    let mut tagged = Vec::new();
    tag(&submitted.main, AstPath::main(Vec::new()), &mut tagged);
    for (index, declaration) in (0u32..).zip(&submitted.declarations) {
        let body = match declaration {
            Declaration::Process(process) => &process.body,
            Declaration::Function(function) => &function.body,
        };
        tag(body, AstPath::declaration(index, Vec::new()), &mut tagged);
    }
    let mut program = submitted.clone();
    program.spans = tagged
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), tag_span(index)))
        .collect();
    let tags = tagged
        .iter()
        .enumerate()
        .map(|(index, path)| (path, index))
        .collect::<BTreeMap<_, _>>();

    let linked = LinkedModule::link(program, environment).map_err(|error| {
        let location = error
            .span()
            .and_then(|span| tagged.get(span.start))
            .map(|path| locate(&submitted, &submitted_addresses, path))
            .unwrap_or(WorkflowAdmissionLocation::Document);
        WorkflowAdmissionRefusal::of(location, link_error_kind(&error), &error)
    })?;

    let admitted_program = linked.artifact.ir();
    let admitted_graph = workflow_graph_from_artifact(&linked.artifact);
    // Where each tagged expression is in the admitted program. A lifted
    // literal's expressions keep their old entries beside the new ones; only
    // the paths the admitted program has are read.
    let mut landed = BTreeMap::new();
    for (path, span) in linked.spans() {
        if expr_at(admitted_program, path).is_some() {
            landed.entry(span.start).or_insert_with(|| path.clone());
        }
    }
    let admitted_at = |submitted: &AstPath| landed.get(tags.get(submitted)?);
    let admitted_ids = statement_addresses(admitted_program)
        .into_iter()
        .map(|(id, path)| (path, id))
        .collect::<BTreeMap<_, _>>();

    let mut nodes = BTreeMap::new();
    for (id, path) in &submitted_addresses {
        if let Some(admitted) = admitted_at(path).and_then(|path| admitted_ids.get(path)) {
            nodes.insert(id.clone(), admitted.clone());
        }
    }
    // A declared process keeps its name. A lifted process's container, and
    // the container of a literal the document still holds, is the
    // declaration its body landed in.
    let mut bodies = Vec::new();
    for (index, declaration) in (0u32..).zip(&submitted.declarations) {
        let Declaration::Process(process) = declaration else {
            continue;
        };
        if process.origin.is_lifted() {
            bodies.push((
                process.name.to_string(),
                AstPath::declaration(index, Vec::new()),
            ));
        } else if let Some(from) = graph_process_id(graph, &process.name)
            && let Some(to) = graph_process_id(&admitted_graph, &process.name)
        {
            nodes.insert(from, to);
        }
    }
    let mut literals = Vec::new();
    collect_process_literals(&submitted.main, &mut Vec::new(), &mut literals);
    for (path, literal) in literals {
        bodies.push((
            lifted_process_identity(&literal.body, &path),
            AstPath::main(path).child(0),
        ));
    }
    for (name, body) in bodies {
        let Some(AstPath {
            root: AstRoot::Declaration(index),
            steps,
        }) = admitted_at(&body)
        else {
            continue;
        };
        if !steps.is_empty() {
            continue;
        }
        let Some(Declaration::Process(process)) =
            admitted_program.declarations.get(*index as usize)
        else {
            continue;
        };
        if let Some(from) = graph_process_id(graph, &name)
            && let Some(to) = graph_process_id(&admitted_graph, &process.name)
        {
            nodes.insert(from, to);
        }
    }
    Ok(WorkflowAdmission {
        linked,
        graph: admitted_graph,
        nodes,
    })
}

fn graph_process_id(graph: &WorkflowGraph, name: &str) -> Option<WorkflowNodeId> {
    graph.process(name).map(|process| process.id.clone())
}

fn tag_span(index: usize) -> Span {
    Span {
        start: index,
        end: index + 1,
    }
}

fn tag(expression: &Expr, path: AstPath, out: &mut Vec<AstPath>) {
    for (index, child) in (0u32..).zip(expression.children()) {
        tag(child, path.child(index), out);
    }
    out.push(path);
}

/// The node and expression path that hold the expression at `path` of the
/// submitted program.
fn locate(
    submitted: &Program,
    addresses: &[(WorkflowNodeId, AstPath)],
    path: &AstPath,
) -> WorkflowAdmissionLocation {
    let holder = addresses
        .iter()
        .filter(|(_, address)| address.root == path.root && path.steps.starts_with(&address.steps))
        .max_by_key(|(_, address)| address.steps.len());
    if let Some((node, address)) = holder {
        let slots = expr_at(submitted, address)
            .and_then(|statement| statement.slot_path(&path.steps[address.steps.len()..]))
            .unwrap_or_default();
        return WorkflowAdmissionLocation::Node {
            node: node.clone(),
            slot: WorkflowSlotPath::new(slots),
        };
    }
    let AstRoot::Declaration(index) = path.root else {
        return WorkflowAdmissionLocation::Document;
    };
    match submitted.declarations.get(index as usize) {
        Some(Declaration::Process(process)) => WorkflowAdmissionLocation::Process {
            name: process.name.to_string(),
        },
        Some(Declaration::Function(function)) => WorkflowAdmissionLocation::Function {
            name: function.name.to_string(),
        },
        None => WorkflowAdmissionLocation::Document,
    }
}

fn link_error_kind(error: &LinkError) -> WorkflowAdmissionDiagnosticKind {
    use WorkflowAdmissionDiagnosticKind as Kind;
    match error {
        LinkError::InvalidAst { .. } => Kind::InvalidProgram,
        LinkError::UnknownBuiltin { .. }
        | LinkError::UnknownResource { .. }
        | LinkError::UnresolvedReceiver { .. }
        | LinkError::UnknownResourceOperation { .. }
        | LinkError::AmbiguousModuleOperation { .. }
        | LinkError::BareToolCall { .. }
        | LinkError::FeatureDisabled { .. }
        | LinkError::OpaqueHostDescriptorAccess { .. } => Kind::HostRequirement,
        LinkError::UnknownProcess { .. }
        | LinkError::UnknownName { .. }
        | LinkError::UnknownType { .. }
        | LinkError::FunctionNameIsNotAValue { .. } => Kind::UnresolvedName,
        LinkError::DuplicateDeclaration { .. }
        | LinkError::DuplicateProcessParam { .. }
        | LinkError::DuplicateFunctionParam { .. }
        | LinkError::ForbiddenInFunction { .. }
        | LinkError::FunctionShadowsBuiltin { .. }
        | LinkError::ProcessLiteralOutsideProcessSlot { .. }
        | LinkError::ProcessLifecycleOutsideProcess { .. } => Kind::Placement,
        LinkError::ModuleHash { .. } => Kind::Artifact,
        _ => Kind::Type,
    }
}

#[cfg(test)]
mod tests;
