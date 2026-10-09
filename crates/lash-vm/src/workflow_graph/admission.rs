//! Admitting a workflow document against a host environment (FIG-5574).
//!
//! [`admit_workflow_graph`] is the one path from a document to an artifact:
//! it reconstructs the IR the document spells, puts every lifted process
//! back into the literal it was lifted from, and links the result against
//! the environment, so the linker derives every lifted declaration, type,
//! signature and host requirement again. Nothing the document states about
//! itself (ids, source identity, facets, lifted names, refined capture
//! types) is taken as true.
//!
//! A lifted process is named by a digest of the literal it was first lifted
//! from, which an admitted document no longer holds. So that an unchanged
//! lifted process is the same process, admission keeps the name the
//! document gives a lifted declaration exactly when the declaration the
//! linker derived for its literal equals it in everything but that name
//! (parameters, types, site and body). The name is then a label of content
//! the linker produced, never a claim it accepted.
//!
//! No dialect is involved. A refusal is located at a node and expression
//! path of the submitted document, and an admission says which admitted node
//! each submitted node became. Both come from the linker's own path table,
//! never from positions: every expression of the program handed to the
//! linker is tagged, and the tags are read back from what it produced.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ast::{AstPath, AstRoot, AstString, Declaration, Expr, ProcessLiteralExpr, Program};
use crate::{
    LashVmHostEnvironment, LinkError, LinkedModule, ProcessOrigin, Span, TypeExpr,
    lifted_process_identity,
};

use super::projection::{expr_at, statement_addresses};
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
            .map(|node| WorkflowAdmissionLocation::Node {
                node: WorkflowNodeId::new(node.to_string()),
                slot: WorkflowSlotPath::default(),
            })
            .unwrap_or(WorkflowAdmissionLocation::Document);
        WorkflowAdmissionRefusal::of(location, WorkflowAdmissionDiagnosticKind::Document, error)
    })?;
    let submitted_addresses = statement_addresses(&submitted);
    let Delinked {
        mut program,
        roots,
        literals,
        spliced,
    } = delink(&submitted);

    // Tag every expression with a span that is its index, so the linker's
    // path table says where each one went and which one an error is at.
    let mut tagged = Vec::new();
    tag(&program.main, AstPath::main(Vec::new()), &mut tagged);
    for (index, declaration) in (0u32..).zip(&program.declarations) {
        let body = match declaration {
            Declaration::Process(process) => &process.body,
            Declaration::Function(function) => &function.body,
        };
        tag(body, AstPath::declaration(index, Vec::new()), &mut tagged);
    }
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

    let retain = |linked: &mut Program| {
        let derived = spliced
            .iter()
            .filter_map(|(literal, declared)| {
                let body = tags.get(&literal.child(0))?;
                let index = linked.spans.iter().find_map(|(path, span)| {
                    (span.start == *body && path.steps.is_empty()).then_some(path.root)
                })?;
                let AstRoot::Declaration(index) = index else {
                    return None;
                };
                Some((index as usize, *declared))
            })
            .collect::<Vec<_>>();
        retain_lifted_names(linked, derived);
    };
    let linked = LinkedModule::link_then(program, environment, retain).map_err(|error| {
        let location = error
            .span()
            .and_then(|span| tagged.get(span.start))
            .and_then(|path| submitted_path(&roots, path))
            .map(|path| locate(&submitted, &submitted_addresses, &path))
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
    let admitted_at = |delinked: &AstPath| landed.get(tags.get(delinked)?);
    let admitted_ids = statement_addresses(admitted_program)
        .into_iter()
        .map(|(id, path)| (path, id))
        .collect::<BTreeMap<_, _>>();

    let mut nodes = BTreeMap::new();
    for (id, path) in &submitted_addresses {
        let admitted = relocate(&roots, path)
            .and_then(|delinked| admitted_at(&delinked))
            .and_then(|path| admitted_ids.get(path));
        if let Some(admitted) = admitted {
            nodes.insert(id.clone(), admitted.clone());
        }
    }
    // A declared process keeps its name; a literal's container is the
    // declaration its body landed in.
    for declaration in &submitted.declarations {
        if let Declaration::Process(process) = declaration
            && process.origin.is_declared()
            && let Some(from) = graph_process_id(graph, &process.name)
            && let Some(to) = graph_process_id(&admitted_graph, &process.name)
        {
            nodes.insert(from, to);
        }
    }
    for (name, literal) in literals {
        let Some(AstPath {
            root: AstRoot::Declaration(index),
            steps,
        }) = admitted_at(&literal.child(0))
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

/// A program with every lifted process back in its literal: the form the
/// linker accepts.
struct Delinked<'p> {
    program: Program,
    /// The literals put back, each with the lifted declaration it came from.
    spliced: Vec<(AstPath, &'p crate::ProcessDecl)>,
    /// Where each root of the submitted program is in `program`.
    roots: BTreeMap<AstRoot, AstPath>,
    /// Every process literal of `program` the submitted document has a
    /// container for: the container's process name and the literal's path.
    literals: Vec<(String, AstPath)>,
}

struct Lifted<'p> {
    index: u32,
    process: &'p crate::ProcessDecl,
}

struct Delinker<'p> {
    lifted: BTreeMap<&'p str, Lifted<'p>>,
    /// The binding each spliced literal is assigned to, by lifted name: what
    /// a later reference to the lifted process read before linking.
    aliases: BTreeMap<&'p str, AstString>,
    roots: BTreeMap<AstRoot, AstPath>,
    literals: Vec<(String, AstPath)>,
    spliced: Vec<(AstPath, &'p crate::ProcessDecl)>,
}

fn delink(submitted: &Program) -> Delinked<'_> {
    let mut delinker = Delinker {
        lifted: BTreeMap::new(),
        aliases: BTreeMap::new(),
        roots: BTreeMap::from([(AstRoot::Main, AstPath::main(Vec::new()))]),
        literals: Vec::new(),
        spliced: Vec::new(),
    };
    let mut kept = Vec::new();
    for (index, declaration) in (0u32..).zip(&submitted.declarations) {
        match declaration {
            Declaration::Process(process) if process.origin.is_lifted() => {
                delinker
                    .lifted
                    .insert(process.name.as_str(), Lifted { index, process });
            }
            declaration => {
                let position = u32::try_from(kept.len()).unwrap_or(u32::MAX);
                delinker.roots.insert(
                    AstRoot::Declaration(index),
                    AstPath::declaration(position, Vec::new()),
                );
                kept.push(declaration.clone());
            }
        }
    }
    // Program order: a literal is bound before anything reads its binding,
    // so the first reference to a lifted process is the literal's own site.
    let mut main = submitted.main.clone();
    delinker.expression(&mut main, &AstPath::main(Vec::new()), None, true);
    for (position, declaration) in (0u32..).zip(&mut kept) {
        let body = match declaration {
            Declaration::Process(process) => &mut process.body,
            Declaration::Function(function) => &mut function.body,
        };
        delinker.expression(
            body,
            &AstPath::declaration(position, Vec::new()),
            None,
            false,
        );
    }
    // A lifted declaration nothing references any more leaves the program,
    // as its literal did.
    Delinked {
        program: Program {
            declarations: kept,
            main,
            private_bindings: submitted.private_bindings.clone(),
            spans: BTreeMap::new(),
        },
        roots: delinker.roots,
        literals: delinker.literals,
        spliced: delinker.spliced,
    }
}

impl<'p> Delinker<'p> {
    /// Rewrites `expression`, which sits at `path` of the delinked program.
    /// `bound` is the name a plain assignment gives it; `in_main` is whether
    /// the submitted program held it in `main`, where the document has a
    /// container for an inline literal.
    fn expression(
        &mut self,
        expression: &mut Expr,
        path: &AstPath,
        bound: Option<&AstString>,
        in_main: bool,
    ) {
        if let Expr::ProcessRef { process } = expression {
            let name = process.to_string();
            if let Some((key, lifted)) = self.lifted.remove_entry(name.as_str()) {
                if let Some(bound) = bound {
                    self.aliases.insert(key, bound.clone());
                }
                self.roots
                    .insert(AstRoot::Declaration(lifted.index), path.child(0));
                self.literals.push((name, path.clone()));
                self.spliced.push((path.clone(), lifted.process));
                *expression = Expr::ProcessLiteral(Box::new(literal_of(lifted.process)));
                let Expr::ProcessLiteral(literal) = expression else {
                    return;
                };
                self.expression(&mut literal.body, &path.child(0), None, false);
            } else if let Some(alias) = self.aliases.get(name.as_str()) {
                *expression = Expr::Variable(alias.clone());
            }
            return;
        }
        if in_main && let Expr::ProcessLiteral(literal) = expression {
            self.literals.push((
                lifted_process_identity(&literal.body, &path.legacy_steps()),
                path.clone(),
            ));
        }
        let bound = match expression {
            Expr::Assign { target, .. } if target.steps.is_empty() => Some(target.root.clone()),
            _ => None,
        };
        for (index, child) in (0u32..).zip(expression.children_mut()) {
            self.expression(child, &path.child(index), bound.as_ref(), in_main);
        }
    }
}

/// The literal a lifted declaration was lifted from. A capture's type is the
/// linker's to refine from the scope the literal sits in, so it goes back as
/// unknown.
fn literal_of(process: &crate::ProcessDecl) -> ProcessLiteralExpr {
    let (hidden_params, declared_return_ty) = match &process.origin {
        ProcessOrigin::Lifted {
            hidden_params,
            declared_return_ty,
            ..
        } => (*hidden_params as usize, declared_return_ty.clone()),
        ProcessOrigin::Declared => (0, process.return_ty.clone()),
    };
    let authored = process.params.len().saturating_sub(hidden_params);
    let mut params = process.params.clone();
    let mut hidden_args = params.split_off(authored);
    for hidden in &mut hidden_args {
        hidden.ty = TypeExpr::Any;
    }
    ProcessLiteralExpr {
        params,
        hidden_args,
        return_ty: declared_return_ty,
        body: Box::new(process.body.clone()),
    }
}

/// Gives each declaration of `linked` named in `derived` the name of the
/// submitted lifted declaration beside it, when the two are otherwise equal.
/// Declarations name one another, so equality is judged under the renaming
/// itself and a declaration that fails it takes its dependents with it.
fn retain_lifted_names(linked: &mut Program, mut derived: Vec<(usize, &crate::ProcessDecl)>) {
    let renaming = |derived: &[(usize, &crate::ProcessDecl)], linked: &Program| {
        derived
            .iter()
            .filter_map(|(index, declared)| match linked.declarations.get(*index)? {
                Declaration::Process(process) => {
                    Some((process.name.clone(), declared.name.clone()))
                }
                Declaration::Function(_) => None,
            })
            .collect::<BTreeMap<_, _>>()
    };
    loop {
        let names = renaming(&derived, linked);
        let before = derived.len();
        derived.retain(|(index, declared)| {
            let Some(Declaration::Process(process)) = linked.declarations.get(*index) else {
                return false;
            };
            let mut renamed = process.clone();
            rename_process(&mut renamed, &names);
            renamed == **declared
        });
        if derived.len() == before {
            break;
        }
    }
    let names = renaming(&derived, linked);
    let mut taken = std::collections::BTreeSet::new();
    let distinct = linked
        .declarations
        .iter()
        .all(|declaration| match declaration {
            Declaration::Process(process) => {
                taken.insert(names.get(&process.name).unwrap_or(&process.name).clone())
            }
            Declaration::Function(_) => true,
        });
    if names.is_empty() || !distinct {
        return;
    }
    rename_references(&mut linked.main, &names);
    for declaration in &mut linked.declarations {
        match declaration {
            Declaration::Process(process) => rename_process(process, &names),
            Declaration::Function(function) => rename_references(&mut function.body, &names),
        }
    }
}

fn rename_process(process: &mut crate::ProcessDecl, names: &BTreeMap<AstString, AstString>) {
    if let Some(name) = names.get(&process.name) {
        process.name = name.clone();
    }
    rename_references(&mut process.body, names);
}

fn rename_references(expression: &mut Expr, names: &BTreeMap<AstString, AstString>) {
    if let Expr::ProcessRef { process } = expression
        && let Some(name) = names.get(process)
    {
        *process = name.clone();
    }
    for child in expression.children_mut() {
        rename_references(child, names);
    }
}

/// Where the expression at `path` of the submitted program is in the
/// delinked one.
fn relocate(roots: &BTreeMap<AstRoot, AstPath>, path: &AstPath) -> Option<AstPath> {
    let root = roots.get(&path.root)?;
    let mut steps = root.steps.clone();
    steps.extend_from_slice(&path.steps);
    Some(AstPath {
        root: root.root,
        steps,
    })
}

/// The inverse of [`relocate`]: the deepest submitted root that holds `path`.
fn submitted_path(roots: &BTreeMap<AstRoot, AstPath>, path: &AstPath) -> Option<AstPath> {
    roots
        .iter()
        .filter(|(_, root)| root.root == path.root && path.steps.starts_with(&root.steps))
        .max_by_key(|(_, root)| root.steps.len())
        .map(|(submitted, root)| AstPath {
            root: *submitted,
            steps: path.steps[root.steps.len()..].to_vec(),
        })
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
            slot: WorkflowSlotPath::structural(slots),
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
