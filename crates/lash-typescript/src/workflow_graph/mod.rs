//! The workflow-graph lens: projection, validation, and the canonical
//! TypeScript text surface.
//!
//! The graph's value types live in `lashlang`, which names no source syntax.
//! Everything that renders or parses text lives here, because TypeScript is
//! the only cell language and the lens's canonical text is TypeScript.
//!
//! The graph is deliberately a semantic, canonical view rather than a CST:
//! comments and authored formatting are discarded. Hosts own graph mutation,
//! drafts, layout, and versioning; this module owns projection, validation,
//! deterministic identity, and canonical rendering.
//!
//! A node's `source_span` addresses this lens's canonical TypeScript output,
//! which is the source text a host receives. It does not address the author's
//! pre-canonical formatting.

use std::collections::BTreeSet;

use lashlang::{
    AssignTarget, Declaration, Expr, LabelMetadata, LashlangHostEnvironment,
    ListComprehensionClause, ProcessDecl, Program, WORKFLOW_GRAPH_SCHEMA_VERSION,
    WorkflowContainer, WorkflowDeclaration, WorkflowGraph, WorkflowGraphProjector,
    WorkflowListComprehensionClause, WorkflowNode, WorkflowNodeId, WorkflowNodeKind,
    WorkflowNodeNameSource, WorkflowProcess, WorkflowStatementText, WorkflowSubgraph,
    WorkflowTerminalKind, analyze_workflow_program, workflow_call_to_ir, workflow_effect_to_ir,
};
use thiserror::Error;

use crate::Diagnostic;

mod editable_text;
mod printer;
mod render_error;

pub use editable_text::{
    TypeScriptFragmentError, parse_typescript_assign_target, parse_typescript_expression,
    parse_typescript_process_statement,
};
use editable_text::{opaque_wrapper_run_body, parse_typescript_fragment, statement_text};
pub use printer::{
    TypeScriptSourceError, typescript_assign_target_source, typescript_expression_source,
    typescript_program_source, typescript_statement_source,
};

/// Parse source, canonicalize it, and project it into a deterministic graph,
/// with optional host-derived, non-authoritative type facets.
///
/// The projection itself is `lashlang`'s ([`WorkflowGraphProjector`]); this
/// dialect contributes the canonical text the node spans address and the text
/// of opaque statements. With no environment the result is the draft: it
/// claims no runtime identity and carries no facets. With one, the source is
/// admitted (linked) against it, and the result is the admitted artifact's
/// runnable view ([`workflow_graph_from_artifact`]) with facets computed over
/// that artifact's resolved IR and paths, lifted declarations included. A
/// source that does not admit stays a draft: the facets of its canonical
/// program carry the link errors as node diagnostics, and it claims no
/// identity.
pub fn workflow_graph_from_source_with_facets(
    src: &str,
    environment: Option<&LashlangHostEnvironment>,
) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    let parsed = crate::parse(src)?;
    let canonical = typescript_program_source(&parsed)?;
    let canonical_program = crate::parse(&canonical)?;
    let Some(environment) = environment else {
        return Ok(draft_graph(&canonical_program, None));
    };
    match crate::link(src, environment) {
        Ok(linked) => {
            let analysis = analyze_workflow_program(linked.artifact.ir(), environment);
            Ok(artifact_graph(&linked.artifact, Some(&analysis)))
        }
        Err(_) => {
            let analysis = analyze_workflow_program(&canonical_program, environment);
            Ok(draft_graph(&canonical_program, Some(&analysis)))
        }
    }
}

fn draft_graph(
    program: &Program,
    analysis: Option<&lashlang::WorkflowLinkAnalysis>,
) -> WorkflowGraph {
    let mut projector = WorkflowGraphProjector::new(program).with_spans(program.spans.clone());
    if let Some(analysis) = analysis {
        projector = projector.with_analysis(analysis);
    }
    let mut graph = projector.project(&TypeScriptStatementText);
    present_loop_sources(&mut graph);
    graph
}

/// The runnable view of an admitted module artifact: the graph of exactly the
/// program the artifact executes, carrying its source identity, with spans
/// addressing the artifact's canonical TypeScript text.
///
/// The artifact's program is printed and reparsed, and each canonical span is
/// carried to the artifact path it describes: a lifted process's body sits
/// under its literal's site in the text and under its declaration in the
/// artifact ([`lashlang::ProcessOrigin::Lifted`]). A program with no
/// TypeScript spelling projects with no spans.
pub fn workflow_graph_from_artifact(artifact: &lashlang::ModuleArtifact) -> WorkflowGraph {
    artifact_graph(artifact, None)
}

fn artifact_graph(
    artifact: &lashlang::ModuleArtifact,
    analysis: Option<&lashlang::WorkflowLinkAnalysis>,
) -> WorkflowGraph {
    let mut projector = WorkflowGraphProjector::new(artifact.ir())
        .with_source_identity(artifact.source_identity())
        .with_spans(canonical_artifact_spans(artifact).unwrap_or_default());
    if let Some(analysis) = analysis {
        projector = projector.with_analysis(analysis);
    }
    let mut graph = projector.project(&TypeScriptStatementText);
    present_loop_sources(&mut graph);
    graph
}

/// The canonical spans of an artifact's program, keyed by the artifact's own
/// paths.
///
/// The printed program holds every process body inline: a declared process
/// prints as the literal `main` binds it to, and a lifted one as the literal at
/// its site, which is itself inside another process body when it was lifted
/// out of one. Each body's text position is resolved from those two facts, and
/// each canonical span moves to the declaration path under the deepest body
/// holding it. A span is kept only when the artifact has a node at its path of
/// the same form as the text's node there, so a path the printing and the
/// linking do not share can never carry another node's span.
fn canonical_artifact_spans(
    artifact: &lashlang::ModuleArtifact,
) -> Option<std::collections::BTreeMap<lashlang::AstPath, lashlang::Span>> {
    let ir = artifact.ir();
    let canonical = typescript_program_source(ir).ok()?;
    let draft = crate::parse(&canonical).ok()?;
    let bodies = process_body_text_paths(ir);
    let spans = draft
        .spans
        .iter()
        .filter_map(|(path, span)| {
            let (path, span) = (path.clone(), *span);
            let artifact_path = if path.root == lashlang::AstRoot::Main {
                bodies
                    .iter()
                    .filter_map(|(index, body)| {
                        let rest = path.steps.strip_prefix(body.as_slice())?;
                        Some((
                            body.len(),
                            lashlang::AstPath::declaration(*index, rest.to_vec()),
                        ))
                    })
                    .max_by_key(|(depth, _)| *depth)
                    .map_or_else(|| path.clone(), |(_, path)| path)
            } else {
                path.clone()
            };
            let text = expr_at(&draft, &path)?;
            let admitted = expr_at(ir, &artifact_path)?;
            same_form(text, admitted).then_some((artifact_path, span))
        })
        .collect();
    Some(spans)
}

/// Where the printed program holds each process declaration's body: the
/// `main` path of the literal body it prints as.
fn process_body_text_paths(ir: &Program) -> Vec<(u32, Vec<u32>)> {
    let index_of = |name: &str| {
        ir.declarations
            .iter()
            .position(|declaration| {
                matches!(declaration, Declaration::Process(process) if process.name == name)
            })
            .and_then(|index| u32::try_from(index).ok())
    };
    let mut bodies = std::collections::BTreeMap::new();
    // A declared process prints as the literal its first `main` binding holds.
    if let Expr::Block(statements) = &ir.main {
        for (position, statement) in statements.iter().enumerate() {
            if let Expr::Assign { target, expr } = statement
                && target.is_simple()
                && let Expr::ProcessRef { process } = expr.as_ref()
                && let Some(index) = index_of(process.as_str())
                && matches!(
                    &ir.declarations[index as usize],
                    Declaration::Process(process) if process.origin.is_declared()
                )
                && let Ok(position) = u32::try_from(position)
            {
                bodies.entry(index).or_insert_with(|| vec![position, 0, 0]);
            }
        }
    }
    // A lifted process prints as the literal at its site; a site inside
    // another process body resolves once that body's text position is known.
    loop {
        let mut resolved = false;
        for (index, declaration) in ir.declarations.iter().enumerate() {
            let (
                Ok(index),
                Declaration::Process(ProcessDecl {
                    origin: lashlang::ProcessOrigin::Lifted { site, .. },
                    ..
                }),
            ) = (u32::try_from(index), declaration)
            else {
                continue;
            };
            if bodies.contains_key(&index) {
                continue;
            }
            let base = match site.root {
                lashlang::AstRoot::Main => Some(Vec::new()),
                lashlang::AstRoot::Declaration(parent) => bodies.get(&parent).cloned(),
            };
            if let Some(mut body) = base {
                body.extend_from_slice(&site.steps);
                body.push(0);
                bodies.insert(index, body);
                resolved = true;
            }
        }
        if !resolved {
            break;
        }
    }
    bodies.into_iter().collect()
}

fn expr_at<'p>(program: &'p Program, path: &lashlang::AstPath) -> Option<&'p Expr> {
    let mut expr = match path.root {
        lashlang::AstRoot::Main => &program.main,
        lashlang::AstRoot::Declaration(index) => match program.declarations.get(index as usize)? {
            Declaration::Process(process) => &process.body,
            Declaration::Function(function) => &function.body,
            Declaration::Type(_) => return None,
        },
    };
    for step in &path.steps {
        expr = expr.children().nth(*step as usize)?;
    }
    Some(expr)
}

/// Whether the text's node and the artifact's node at one path are the same
/// node: the same form, or a form the linker resolves in place (an inline
/// process literal to its declaration's reference, a module path to its
/// resource).
fn same_form(text: &Expr, admitted: &Expr) -> bool {
    matches!(
        (text, admitted),
        (
            Expr::ProcessLiteral(_) | Expr::Variable(_),
            Expr::ProcessRef { .. }
        ) | (Expr::Variable(_) | Expr::Field { .. }, Expr::ResourceRef(_))
    ) || std::mem::discriminant(text) == std::mem::discriminant(admitted)
}

/// Shows a `for .. of` loop's authored source in its container.
///
/// The lowerer iterates a snapshot of the source (`Lash.ArrayFromIterable`),
/// and the IR projection carries that call. A TypeScript author wrote the
/// source itself, and the printer re-derives the snapshot from the header, so
/// the container holds what the author can edit.
fn present_loop_sources(graph: &mut WorkflowGraph) {
    fn subgraph(graph: &mut WorkflowSubgraph) {
        for node in &mut graph.nodes {
            if let WorkflowNodeKind::Container(container) = &mut node.kind {
                if let WorkflowContainer::For { iterable, .. } = container
                    && let Some([source]) = printer::stdlib_call(iterable, "Lash.ArrayFromIterable")
                {
                    *iterable = source.clone();
                }
                for (_, child) in container.child_subgraphs_mut() {
                    subgraph(child);
                }
            }
        }
    }
    subgraph(&mut graph.main);
    for declaration in &mut graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            subgraph(&mut process.body);
        }
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkflowGraphBuildError {
    #[error(transparent)]
    Parse(#[from] Diagnostic),
    #[error(transparent)]
    CanonicalSource(#[from] TypeScriptSourceError),
}

#[derive(Clone, Debug, Error, PartialEq)]
#[cfg_attr(test, derive(strum::EnumDiscriminants))]
#[cfg_attr(test, strum_discriminants(derive(strum::EnumIter, PartialOrd, Ord)))]
#[non_exhaustive]
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
    #[error("node `{node_id}` has a payload incompatible with its kind: {message}")]
    InvalidNodePayload { node_id: String, message: String },
    #[error("node `{node_id}` has invalid `{field}` expression text: {message}")]
    InvalidExpression {
        node_id: String,
        field: String,
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
    /// A process's origin is derived at admission, never authored: a declared
    /// process cannot take a lifted name, and a lifted one must still name the
    /// literal it was lifted from, at its site, with its hidden parameters.
    #[error("process `{name}` has an origin its program does not derive: {message}")]
    ProcessOriginMismatch { name: String, message: String },
    #[error(transparent)]
    CanonicalSource(#[from] TypeScriptSourceError),
    #[error("rendered workflow source did not parse: {message}")]
    RenderedSourceInvalid { message: String },
}

/// Parse source, canonicalize it, and project it into a deterministic graph.
pub fn workflow_graph_from_source(src: &str) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    workflow_graph_from_source_with_facets(src, None)
}

/// Validate a graph with every check used by rendering.
///
/// This checks document-wide invariants, converts every node back to IR, and
/// verifies that the resulting program has a canonical TypeScript spelling
/// which parses back successfully. The final-parse check prints once to an
/// internal buffer, but this function returns no source.
pub fn validate(graph: &WorkflowGraph) -> Result<(), GraphRenderError> {
    validated_source(graph, &BTreeSet::new())?;
    Ok(())
}

/// Validate and render a graph through the canonical TypeScript printer.
pub fn workflow_graph_to_source(graph: &WorkflowGraph) -> Result<String, GraphRenderError> {
    validated_source(graph, &BTreeSet::new())
}

/// Validate and render the graph of a session cell: `globals` are the names
/// earlier cells of its session bound, which the cell reads and its final
/// parse must know, as the cell's own link did.
pub fn workflow_graph_to_source_in_session(
    graph: &WorkflowGraph,
    globals: &BTreeSet<String>,
) -> Result<String, GraphRenderError> {
    validated_source(graph, globals)
}

fn validated_source(
    graph: &WorkflowGraph,
    globals: &BTreeSet<String>,
) -> Result<String, GraphRenderError> {
    let program = validated_program(graph, globals)?;
    let source = typescript_program_source(&program)?;
    crate::parse_with_globals(&source, globals).map_err(|error| {
        GraphRenderError::RenderedSourceInvalid {
            message: error.to_string(),
        }
    })?;
    Ok(source)
}

fn validated_program(
    graph: &WorkflowGraph,
    globals: &BTreeSet<String>,
) -> Result<Program, GraphRenderError> {
    validate_graph(graph)?;
    graph_to_program(graph, globals)
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    fn final_parse_failure_graph() -> WorkflowGraph {
        let mut graph = workflow_graph_from_source("finish(1);\n").expect("fixture projects");
        let terminal = graph
            .main
            .nodes
            .iter_mut()
            .find_map(|node| match &mut node.kind {
                WorkflowNodeKind::Terminal { expression, .. } => Some(expression),
                _ => None,
            })
            .expect("fixture contains a terminal node");
        *terminal = Expr::Return(Box::new(Expr::Number(1.0)));
        graph
    }

    #[test]
    fn final_parse_fixture_passes_every_preceding_check() {
        let graph = final_parse_failure_graph();
        validate_graph(&graph).expect("graph invariants hold");
        let program = graph_to_program(&graph, &BTreeSet::new()).expect("graph converts to IR");
        let source = typescript_program_source(&program).expect("IR prints");
        assert_eq!(source, "return 1;\n");
        assert!(
            crate::parse(&source).is_err(),
            "printed source must not parse"
        );
        assert!(matches!(
            validate(&graph),
            Err(GraphRenderError::RenderedSourceInvalid { .. })
        ));
    }

    #[test]
    fn validate_prints_once_per_call_for_the_final_parse_check() {
        let graph = workflow_graph_from_source("finish(1);\n").expect("fixture projects");
        printer::reset_program_print_count();

        const VALIDATIONS: usize = 4;
        for _ in 0..VALIDATIONS {
            validate(&graph).expect("valid graph validates");
        }

        assert_eq!(printer::program_print_count(), VALIDATIONS);
    }
}

/// The TypeScript text of an opaque statement node.
///
/// This is the dialect half of the projection (ADR 0100 R8): the projector in
/// `lashlang` decides structure, and TypeScript supplies the source text a
/// host shows for a statement the graph does not decompose.
pub struct TypeScriptStatementText;

impl WorkflowStatementText for TypeScriptStatementText {
    fn statement_text(&self, statement: &Expr, available: &[String]) -> String {
        statement_text(statement, available)
    }
}

/// Rebuild the process wrapper around an authored run body.
///
/// The graph shows the authored body; the wrapper that turns an uncaught error
/// into process failure is structure the lowerer owns, so it is rebuilt by the
/// lowerer's one builder rather than stored.
fn process_wrapper(params: &[lashlang::ProcessParam], body: Expr) -> Expr {
    crate::lower::process_run_wrapper(
        Expr::Function(Box::new(lashlang::FunctionExpr {
            name: None,
            js_name: None,
            receiver: None,
            params: params.iter().map(|param| param.name.clone()).collect(),
            captures: Vec::new(),
            body: Box::new(body),
        })),
        params
            .iter()
            .map(|param| Expr::Variable(param.name.clone()))
            .collect(),
    )
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
            validate_process_origin(process)?;
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

/// The origin checks a process's own fields can decide; the splice checks the
/// rest against the literal at the site ([`splice_lifted_bodies`]).
fn validate_process_origin(process: &WorkflowProcess) -> Result<(), GraphRenderError> {
    let mismatch = |message: &str| {
        Err(GraphRenderError::ProcessOriginMismatch {
            name: process.name.clone(),
            message: message.to_string(),
        })
    };
    match &process.origin {
        lashlang::ProcessOrigin::Declared
            if process
                .name
                .starts_with(lashlang::LIFTED_PROCESS_NAME_PREFIX) =>
        {
            mismatch("a declared process cannot take a lifted process's name")
        }
        lashlang::ProcessOrigin::Lifted { .. }
            if !process
                .name
                .starts_with(lashlang::LIFTED_PROCESS_NAME_PREFIX) =>
        {
            mismatch("a lifted process is named by its literal's digest")
        }
        lashlang::ProcessOrigin::Lifted { hidden_params, .. }
            if *hidden_params as usize > process.params.len() =>
        {
            mismatch("a lifted process has more hidden parameters than parameters")
        }
        // TypeScript declares no processes, so the lens has no spelling for
        // a literal lifted out of a declared process's body.
        lashlang::ProcessOrigin::Lifted { site, .. } if site.root != lashlang::AstRoot::Main => {
            mismatch("the TypeScript lens renders only literals lifted from main")
        }
        _ => Ok(()),
    }
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
        // The set is smaller than the vector, so a repeated id is always found;
        // name the graph rather than panic if that ever stops holding.
        let id = graph
            .nodes
            .iter()
            .find(|node| !seen.insert(node.id.clone()))
            .map_or_else(|| "<unknown>".to_string(), |node| node.id.to_string());
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
    if let WorkflowNodeKind::Container(container) = &node.kind {
        for (_, child) in container.child_subgraphs() {
            validate_subgraph(child, all_ids)?;
        }
    }
    match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::If {
            then_is_block,
            else_is_block,
            then_graph,
            else_graph,
            ..
        }) => {
            let (then_graph, else_graph) = (then_graph.as_ref(), else_graph.as_ref());
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

fn graph_to_program(
    graph: &WorkflowGraph,
    globals: &BTreeSet<String>,
) -> Result<Program, GraphRenderError> {
    let process_names = graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process.name.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut declarations = Vec::with_capacity(graph.declarations.len());
    let mut lifted: Vec<&WorkflowProcess> = Vec::new();
    for declaration in &graph.declarations {
        declarations.push(match declaration {
            WorkflowDeclaration::Type(ty) => Declaration::Type(ty.clone()),
            WorkflowDeclaration::Function(function) => Declaration::Function(function.clone()),
            WorkflowDeclaration::Process(process) => {
                // A lifted literal is not a module declaration: its authored
                // arrow travels inline where it sits, so the rebuilt program
                // carries no declaration for it. It is not skipped either —
                // the lens owns its body in both directions, so the rendered
                // subgraph is spliced back into the literal below (FIG-3118).
                if process.origin.is_lifted() {
                    lifted.push(process);
                    continue;
                }
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
                    origin: process.origin.clone(),
                    body: process_wrapper(
                        &process.params,
                        subgraph_to_block(
                            &process.body,
                            RenderContext {
                                scope: RenderScope::Process,
                                processes: &process_names,
                                globals,
                            },
                        )?,
                    ),
                })
            }
        });
    }
    let context = RenderContext {
        scope: RenderScope::Main,
        processes: &process_names,
        globals,
    };
    let mut main = subgraph_to_block(&graph.main, context)?;
    splice_lifted_bodies(&mut main, lifted, context)?;
    Ok(Program {
        language: lashlang::SourceLanguage::new(crate::TYPESCRIPT_LANGUAGE),
        declarations,
        main,
        // A graph renders to source, which the lowering re-admits; the
        // private roles of its bindings come back from that lowering.
        private_bindings: Default::default(),
        spans: Default::default(),
    })
}

/// Splices each lifted process's rendered body back into the literal it was
/// projected from (FIG-3118).
///
/// A top-level `const`-bound `async` arrow is a process literal in `main`
/// (FIG-2999), and the lens projects it twice: the statement node carries the
/// arrow, and the body is projected as its own lifted process declaration so
/// the nodes inside are editable. Rendering `main` alone would drop every edit
/// made inside a process container, so the rendered body is spliced back.
///
/// Which literal a declaration belongs to is derived, never read from an
/// authored position (FIG-3571). A declaration's name is the digest of the
/// literal it was lifted from together with the site it was lifted at, so a
/// literal carries the declaration exactly when it still digests to that name
/// at that site — wherever the literal sits now. Adding, removing or reordering
/// statements around a literal moves it without changing that proof, and
/// admission re-derives the origin of the rendered program. An admitted
/// program that holds the literal as a reference to its declaration names it
/// directly.
///
/// A declaration no literal or reference carries — its origin, name or literal
/// was edited — is refused, as is a literal two declarations could claim and a
/// reference naming a lifted process twice. Nothing is spliced into a literal
/// it was not projected from; a literal no declaration claims keeps its
/// authored body.
fn splice_lifted_bodies(
    main: &mut Expr,
    lifted: Vec<&WorkflowProcess>,
    context: RenderContext<'_>,
) -> Result<(), GraphRenderError> {
    let mut pending = LiftedBodies::default();
    for process in lifted {
        if process.origin.is_lifted() {
            pending.by_name.insert(process.name.clone(), process);
        }
    }
    splice_at(
        main,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut pending,
        context,
    )?;
    match pending.by_name.into_values().next() {
        Some(process) => Err(GraphRenderError::ProcessOriginMismatch {
            name: process.name.clone(),
            message: "no process literal or reference in the program carries it".to_string(),
        }),
        None => Ok(()),
    }
}

/// The lifted declarations not yet spliced, by name.
#[derive(Default)]
struct LiftedBodies<'a> {
    by_name: std::collections::BTreeMap<String, &'a WorkflowProcess>,
    spliced: BTreeSet<String>,
}

impl<'a> LiftedBodies<'a> {
    fn take_named(&mut self, name: &str) -> Option<&'a WorkflowProcess> {
        let process = self.by_name.remove(name)?;
        self.spliced.insert(name.to_string());
        Some(process)
    }

    /// The declaration `literal` carries: one whose name `literal` digests to
    /// at the declaration's own site, preferring the one lifted at the
    /// literal's current position.
    fn take_for_literal(
        &mut self,
        literal: &lashlang::ProcessLiteralExpr,
        path: &[u32],
        unlabelled: &[u32],
    ) -> Result<Option<&'a WorkflowProcess>, GraphRenderError> {
        let carried = self
            .by_name
            .values()
            .filter_map(|process| match &process.origin {
                lashlang::ProcessOrigin::Lifted { site, .. }
                    if lashlang::lifted_process_identity(&literal.body, &site.steps)
                        == process.name =>
                {
                    Some((site.steps.as_slice(), process.name.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let name = match carried
            .iter()
            .find(|(site, _)| *site == path || *site == unlabelled)
        {
            Some((_, name)) => name.clone(),
            None => match carried.as_slice() {
                [] => return Ok(None),
                [(_, name)] => name.clone(),
                [(_, name), ..] => {
                    return Err(GraphRenderError::ProcessOriginMismatch {
                        name: name.clone(),
                        message: "more than one lifted process could claim a moved literal"
                            .to_string(),
                    });
                }
            },
        };
        Ok(self.take_named(&name))
    }
}

/// The lifted declaration `expr` refers to, if it is a reference to one.
///
/// An admitted view spells the reference as `ProcessRef`. A host that carries
/// node text spells it as the reference's printed name, which reads back as a
/// variable; since lifted names are reserved to the linker, a variable naming
/// a lifted declaration of this graph can only be that reference.
fn lifted_reference(expr: &Expr, pending: &LiftedBodies<'_>) -> Option<String> {
    let name = match expr {
        Expr::ProcessRef { process } => process.as_str(),
        Expr::Variable(name) => name.as_str(),
        _ => return None,
    };
    (pending.by_name.contains_key(name) || pending.spliced.contains(name)).then(|| name.to_string())
}

fn splice_at(
    expr: &mut Expr,
    path: &mut Vec<u32>,
    unlabelled: &mut Vec<u32>,
    pending: &mut LiftedBodies<'_>,
    context: RenderContext<'_>,
) -> Result<(), GraphRenderError> {
    if let Expr::ProcessLiteral(literal) = expr
        && let Some(process) = pending.take_for_literal(literal, path, unlabelled)?
    {
        let mut statements = match subgraph_to_block(
            &process.body,
            RenderContext {
                scope: RenderScope::Process,
                processes: context.processes,
                globals: context.globals,
            },
        )? {
            Expr::Block(statements) => statements,
            other => vec![other],
        };
        statements.push(Expr::Undefined);
        let body = Expr::Role {
            role: lashlang::StructuralRole::Completion,
            expr: Box::new(Expr::Block(statements)),
        };
        literal.params = process.params.clone();
        *literal.body = process_wrapper(&process.params, body);
    } else if let Some(name) = lifted_reference(expr, pending)
        && pending.spliced.contains(name.as_str())
    {
        return Err(GraphRenderError::ProcessOriginMismatch {
            name,
            message: "a lifted process is carried by one literal, not referenced twice".to_string(),
        });
    } else if let Some(name) = lifted_reference(expr, pending)
        && let Some(process) = pending.take_named(&name)
    {
        // An admitted program holds the lifted literal as a reference to its
        // declaration; the rendered program holds the literal itself, rebuilt
        // from the declaration: its authored parameters, the captures its
        // hidden parameters carry, and the rendered body.
        let lashlang::ProcessOrigin::Lifted { hidden_params, .. } = &process.origin else {
            unreachable!("only lifted processes are spliced")
        };
        let authored = process
            .params
            .len()
            .checked_sub(*hidden_params as usize)
            .ok_or_else(|| GraphRenderError::ProcessOriginMismatch {
                name: process.name.clone(),
                message: "more hidden parameters than parameters".to_string(),
            })?;
        let (params, hidden) = process.params.split_at(authored);
        let mut statements = match subgraph_to_block(
            &process.body,
            RenderContext {
                scope: RenderScope::Process,
                processes: context.processes,
                globals: context.globals,
            },
        )? {
            Expr::Block(statements) => statements,
            other => vec![other],
        };
        statements.push(Expr::Undefined);
        let body = Expr::Role {
            role: lashlang::StructuralRole::Completion,
            expr: Box::new(Expr::Block(statements)),
        };
        *expr = Expr::ProcessLiteral(Box::new(lashlang::ProcessLiteralExpr {
            params: params.to_vec(),
            hidden_args: hidden.to_vec(),
            body: Box::new(process_wrapper(params, body)),
        }));
    }
    let label = matches!(expr, Expr::LabelAnnotated { .. });
    for (index, child) in expr.children_mut().enumerate() {
        let step = u32::try_from(index).unwrap_or(u32::MAX);
        path.push(step);
        if !label {
            unlabelled.push(step);
        }
        splice_at(child, path, unlabelled, pending, context)?;
        path.pop();
        if !label {
            unlabelled.pop();
        }
    }
    Ok(())
}

/// What a node is being rendered back into.
#[derive(Clone, Copy)]
struct RenderContext<'a> {
    scope: RenderScope,
    /// The process names the module declares.
    ///
    /// `const child = async (..) => ..` projects as the module binding its
    /// process reference. The reference is not ordinary text — the name is
    /// bound by the very statement that reads it — so it is rebuilt from the
    /// declaration list instead of parsed.
    processes: &'a [String],
    /// The session's globals: names earlier cells of the session bound, which
    /// the cell's own link admitted and an opaque statement may therefore
    /// read.
    globals: &'a BTreeSet<String>,
}

impl RenderContext<'_> {
    /// The module's process bindings, as a fragment parse sees them.
    fn process_bindings(&self) -> std::collections::BTreeSet<String> {
        self.processes.iter().cloned().collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RenderScope {
    Main,
    Process,
}

fn subgraph_to_block(
    graph: &WorkflowSubgraph,
    context: RenderContext<'_>,
) -> Result<Expr, GraphRenderError> {
    graph
        .nodes
        .iter()
        .map(|node| node_to_expr(node, context))
        .collect::<Result<Vec<_>, _>>()
        .map(Expr::Block)
}

fn node_to_expr(node: &WorkflowNode, context: RenderContext<'_>) -> Result<Expr, GraphRenderError> {
    let expression = match &node.kind {
        WorkflowNodeKind::Data {
            binding,
            expression,
        } => {
            if !lashlang::is_pure_expr(expression) && !matches!(expression, Expr::TypeLiteral(_)) {
                return invalid_payload(node, "data expression is effectful");
            }
            with_assignment_ir(binding, expression.clone())
        }
        WorkflowNodeKind::Call {
            binding,
            receiver,
            operation,
            arguments,
            result_steps,
        } => {
            let expression = workflow_call_to_ir(receiver, operation, arguments, result_steps);
            with_assignment_ir(binding, expression)
        }
        WorkflowNodeKind::Effect {
            binding,
            effect,
            arguments,
            result_steps,
        } => {
            let expression =
                workflow_effect_to_ir(*effect, arguments, result_steps).ok_or_else(|| {
                    GraphRenderError::InvalidNodePayload {
                        node_id: node.id.to_string(),
                        message: "effect arguments do not match its kind".to_string(),
                    }
                })?;
            with_assignment_ir(binding, expression)
        }
        WorkflowNodeKind::Computation {
            binding,
            expression,
        } => with_assignment_ir(binding, expression.clone()),
        WorkflowNodeKind::StateUpdate {
            target,
            expression,
            update,
        } => {
            let [output] = node.outputs.as_slice() else {
                return invalid_payload(node, "state update must have exactly one output");
            };
            if target.root.as_str() != output.variable {
                return invalid_payload(node, "state-update target root must match its output");
            }
            match update {
                Some(operator) => {
                    let Some(role) =
                        crate::lower::attribute_update(target, *operator, expression.clone())
                    else {
                        return invalid_payload(
                            node,
                            "an update's target is one member step of a variable",
                        );
                    };
                    role
                }
                None => Expr::Assign {
                    target: target.clone(),
                    expr: Box::new(expression.clone()),
                },
            }
        }
        WorkflowNodeKind::Terminal {
            terminal,
            expression,
        } => {
            let valid = matches!(
                (terminal, &expression),
                (
                    WorkflowTerminalKind::Finish,
                    Expr::Finish(_) | Expr::Return(_)
                ) | (WorkflowTerminalKind::Fail, Expr::Fail(_))
            );
            if !valid {
                return invalid_payload(node, "terminal kind does not match its expression");
            }
            expression.clone()
        }
        WorkflowNodeKind::Container(container) => match container {
            WorkflowContainer::If {
                binding,
                condition,
                then_is_block,
                else_is_block,
                then_graph,
                else_graph,
            } => with_assignment_ir(
                binding,
                Expr::If {
                    condition: Box::new(condition.clone()),
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
            ),
            WorkflowContainer::For {
                binding,
                iterable,
                bind,
                body,
            } => Expr::For {
                binding: binding.clone().into(),
                iterable: Box::new(iterable.clone()),
                bind: bind.clone().map(Box::new),
                body: Box::new(subgraph_to_block(body, context)?),
            },
            WorkflowContainer::While { condition, body } => Expr::While {
                condition: Box::new(condition.clone()),
                body: Box::new(subgraph_to_block(body, context)?),
            },
            WorkflowContainer::ListComprehension {
                binding,
                clauses,
                element,
            } => {
                let Expr::Block(mut expressions) = subgraph_to_block(element, context)? else {
                    unreachable!("subgraph rendering always returns a block")
                };
                if expressions.len() != 1 {
                    return invalid_payload(
                        node,
                        "list-comprehension element must contain exactly one node",
                    );
                }
                with_assignment_ir(
                    binding,
                    Expr::ListComprehension {
                        element: Box::new(expressions.remove(0)),
                        clauses: clauses.iter().map(workflow_clause_to_ir).collect(),
                    },
                )
            }
        },
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
    context: RenderContext<'_>,
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
    context: RenderContext<'_>,
) -> Result<Expr, GraphRenderError> {
    let (program, prelude) = parse_typescript_fragment(node, source, context)?;
    let expressions = match context.scope {
        RenderScope::Main => printer::statement_block_contents(&program.main),
        RenderScope::Process => {
            let Some(body) = opaque_wrapper_run_body(&program) else {
                return invalid_payload(node, "opaque process wrapper did not produce a run body");
            };
            printer::statement_block_contents(body)
        }
    };
    let expressions = expressions.into_iter().skip(prelude).collect::<Vec<_>>();
    if expressions.len() != 1 {
        return Err(GraphRenderError::InvalidOpaqueSource {
            node_id: node.id.to_string(),
            message: format!("expected one statement, found {}", expressions.len()),
        });
    }
    #[expect(
        clippy::expect_used,
        reason = "the length check above returned for any count other than one"
    )]
    let expression = expressions.into_iter().next().expect("one expression");
    Ok(expression.clone())
}

fn workflow_clause_to_ir(clause: &WorkflowListComprehensionClause) -> ListComprehensionClause {
    match clause {
        WorkflowListComprehensionClause::For { binding, iterable } => {
            ListComprehensionClause::For {
                binding: binding.clone().into(),
                iterable: iterable.clone(),
            }
        }
        WorkflowListComprehensionClause::If { condition } => ListComprehensionClause::If {
            condition: condition.clone(),
        },
    }
}

fn with_assignment_ir(binding: &Option<AssignTarget>, expression: Expr) -> Expr {
    match binding {
        Some(target) => Expr::Assign {
            target: target.clone(),
            expr: Box::new(expression),
        },
        None => expression,
    }
}
