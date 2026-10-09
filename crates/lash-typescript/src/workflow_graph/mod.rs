//! The TypeScript lens over a workflow document.
//!
//! A [`WorkflowGraph`] is a complete typed IR document owned by `lash_vm`:
//! it is projected, reconstructed and validated there, with no source text.
//! This module is the optional TypeScript view of one: it lowers source into
//! a document, prints a document's program as canonical TypeScript
//! ([`source_view`]), and parses the text of a single edited field. A
//! document this lens cannot spell is still a valid document; the lens
//! refuses it with a typed [`GraphRenderError`], and the graph is unaffected.
//!
//! The lens's text is canonical: comments and authored formatting are
//! discarded, and a node's `source_span` addresses the canonical output, not
//! the author's formatting.

use std::collections::BTreeMap;

use lash_vm::{
    Declaration, Expr, InvalidAst, LashVmHostEnvironment, ProcessDecl, Program, Span,
    WorkflowGraph, WorkflowGraphError, WorkflowGraphProjector, WorkflowGraphVersionRefusal,
    WorkflowNodeId, analyze_workflow_program, workflow_program_from_graph,
};
use thiserror::Error;

use crate::Diagnostic;

mod editable_text;
mod printer;
mod render_error;

pub use editable_text::{
    TypeScriptFragmentError, parse_typescript_assign_target, parse_typescript_expression,
    parse_typescript_process_statement, parse_typescript_statement,
};
pub use printer::{
    TypeScriptSourceError, typescript_assign_target_source, typescript_expression_source,
    typescript_program_source, typescript_statement_source,
};

/// Parse source, canonicalize it, and project it into a deterministic graph,
/// with optional host-derived, non-authoritative type facets.
///
/// The projection itself is `lash_vm`'s ([`WorkflowGraphProjector`]); this
/// dialect contributes the canonical text the node spans address. With no
/// environment the result is the draft: it
/// claims no runtime identity and carries no facets. With one, the source is
/// admitted (linked) against it, and the result is the admitted artifact's
/// runnable view ([`workflow_graph_from_artifact`]) with facets computed over
/// that artifact's resolved IR and paths, lifted declarations included. A
/// source that does not admit stays a draft: the facets of its canonical
/// program carry the link errors as node diagnostics, and it claims no
/// identity.
pub fn workflow_graph_from_source_with_facets(
    src: &str,
    environment: Option<&LashVmHostEnvironment>,
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
    analysis: Option<&lash_vm::WorkflowLinkAnalysis>,
) -> WorkflowGraph {
    let mut projector = WorkflowGraphProjector::new(program).with_spans(program.spans.clone());
    if let Some(analysis) = analysis {
        projector = projector.with_analysis(analysis);
    }
    projector.project()
}

/// The runnable view of an admitted module artifact: the graph of exactly the
/// program the artifact executes, carrying its source identity, with spans
/// addressing the artifact's canonical TypeScript text.
///
/// The artifact's program is printed and reparsed, and each canonical span is
/// carried to the artifact path it describes: a lifted process's body sits
/// under its literal's site in the text and under its declaration in the
/// artifact ([`lash_vm::ProcessOrigin::Lifted`]). A program with no
/// TypeScript spelling projects with no spans.
pub fn workflow_graph_from_artifact(artifact: &lash_vm::ModuleArtifact) -> WorkflowGraph {
    artifact_graph(artifact, None)
}

fn artifact_graph(
    artifact: &lash_vm::ModuleArtifact,
    analysis: Option<&lash_vm::WorkflowLinkAnalysis>,
) -> WorkflowGraph {
    let mut projector = WorkflowGraphProjector::new(artifact.ir())
        .with_source_identity(artifact.source_identity())
        .with_spans(canonical_spans(artifact.ir()).unwrap_or_default());
    if let Some(analysis) = analysis {
        projector = projector.with_analysis(analysis);
    }
    projector.project()
}

/// The canonical spans of an admitted program, keyed by the program's own
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
fn canonical_spans(ir: &Program) -> Option<BTreeMap<lash_vm::AstPath, Span>> {
    let canonical = typescript_program_source(ir).ok()?;
    let draft = crate::parse(&canonical).ok()?;
    let bodies = process_body_text_paths(ir);
    let spans = draft
        .spans
        .iter()
        .filter_map(|(path, span)| {
            let (path, span) = (path.clone(), *span);
            let artifact_path = if path.root == lash_vm::AstRoot::Main {
                bodies
                    .iter()
                    .filter_map(|(index, body)| {
                        let rest = path.steps.strip_prefix(body.as_slice())?;
                        Some((
                            body.len(),
                            lash_vm::AstPath::declaration(*index, rest.to_vec()),
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
                    origin: lash_vm::ProcessOrigin::Lifted { site, .. },
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
                lash_vm::AstRoot::Main => Some(Vec::new()),
                lash_vm::AstRoot::Declaration(parent) => bodies.get(&parent).cloned(),
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

fn expr_at<'p>(program: &'p Program, path: &lash_vm::AstPath) -> Option<&'p Expr> {
    let mut expr = match path.root {
        lash_vm::AstRoot::Main => &program.main,
        lash_vm::AstRoot::Declaration(index) => match program.declarations.get(index as usize)? {
            Declaration::Process(process) => &process.body,
            Declaration::Function(function) => &function.body,
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

/// The source a `for .. of` loop iterates, as its author wrote it.
///
/// The lowerer iterates a snapshot of the source (`Lash.ArrayFromIterable`),
/// and a workflow document carries that call because it is what runs. The
/// printer re-derives the snapshot from the loop header, so a host showing
/// the loop as TypeScript shows this.
pub fn typescript_for_of_source(iterable: &Expr) -> &Expr {
    match printer::stdlib_call(iterable, "Lash.ArrayFromIterable") {
        Some([source]) => source,
        _ => iterable,
    }
}

/// The failure wrapper this dialect lowers around a process's run body,
/// for a host that adds a process or changes one's authored parameters.
pub fn typescript_process_wrapper(
    params: &[lash_vm::ProcessParam],
) -> lash_vm::WorkflowProcessWrapper {
    let wrapped = crate::lower::process_run_wrapper(
        Expr::Function(Box::new(lash_vm::FunctionExpr {
            name: None,
            js_name: None,
            receiver: None,
            params: params.iter().map(|param| param.name.clone()).collect(),
            captures: Vec::new(),
            body: Box::new(Expr::Absent),
        })),
        params
            .iter()
            .map(|param| Expr::Variable(param.name.clone()))
            .collect(),
    );
    #[expect(
        clippy::expect_used,
        reason = "the lowerer's one wrapper builder produces the role's shape, which is what the reader accepts"
    )]
    lash_vm::WorkflowProcessWrapper::of(&wrapped).expect("the lowerer builds a process wrapper")
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkflowGraphBuildError {
    #[error(transparent)]
    Parse(#[from] Diagnostic),
    #[error(transparent)]
    CanonicalSource(#[from] TypeScriptSourceError),
}

/// Why a workflow document has no TypeScript rendering.
///
/// `Document` and `InvalidProgram` are refusals of the document itself, from
/// the IR's own reconstruction and validation. `CanonicalSource` is the
/// lens's refusal: the document is a valid program the TypeScript printer has
/// no spelling for. The two expression variants are for a host that parses a
/// node's edited text through this lens and reports a rejected field.
#[derive(Clone, Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum GraphRenderError {
    #[error(transparent)]
    UnsupportedSchemaVersion(#[from] WorkflowGraphVersionRefusal),
    #[error(transparent)]
    Document(#[from] WorkflowGraphError),
    #[error("the workflow document does not spell a valid program: {0}")]
    InvalidProgram(#[from] InvalidAst),
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
    #[error(transparent)]
    CanonicalSource(#[from] TypeScriptSourceError),
}

/// Parse source, canonicalize it, and project it into a deterministic graph.
pub fn workflow_graph_from_source(src: &str) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    workflow_graph_from_source_with_facets(src, None)
}

/// Validate a workflow document as IR.
///
/// The document is reconstructed to its program by `lash_vm` and that
/// program is held to the IR's own rules ([`lash_vm::validate_ast`]). No
/// TypeScript is printed or parsed, so a document this lens cannot spell
/// still validates.
pub fn validate(graph: &WorkflowGraph) -> Result<(), GraphRenderError> {
    validate_for_fleet(graph, lash_core_execution::FleetFormat::current())
}

/// Validate under the fleet epoch that pinned the graph document.
pub fn validate_for_fleet(
    graph: &WorkflowGraph,
    fleet: lash_core_execution::FleetFormat,
) -> Result<(), GraphRenderError> {
    validated_program(graph, fleet)?;
    Ok(())
}

/// The program a valid document spells.
fn validated_program(
    graph: &WorkflowGraph,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Program, GraphRenderError> {
    WorkflowGraph::admit_schema_version_for_fleet(graph.schema_version, fleet)?;
    let program = workflow_program_from_graph(graph)?;
    lash_vm::validate_ast(&program)?;
    Ok(program)
}

/// The canonical TypeScript of a valid workflow document.
pub fn workflow_graph_to_source(graph: &WorkflowGraph) -> Result<String, GraphRenderError> {
    workflow_graph_to_source_for_fleet(graph, lash_core_execution::FleetFormat::current())
}

/// Render under the fleet epoch that pinned the graph document.
pub fn workflow_graph_to_source_for_fleet(
    graph: &WorkflowGraph,
    fleet: lash_core_execution::FleetFormat,
) -> Result<String, GraphRenderError> {
    let program = validated_program(graph, fleet)?;
    Ok(typescript_program_source(&program)?)
}

/// The TypeScript view of one workflow document: its canonical source and
/// where each node sits in it.
///
/// A view belongs to the document it was made from. `source_identity` is that
/// document's admitted identity, `None` for a draft, and the spans are keyed
/// by that document's own node ids, so a view is never read against another
/// revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceView {
    pub source_identity: Option<String>,
    /// Canonical TypeScript for the document's program.
    pub source: String,
    /// The canonical span of each node the text holds a statement for.
    pub spans: BTreeMap<WorkflowNodeId, Span>,
}

/// The TypeScript view of `graph`, or the typed reason this lens has none.
///
/// Sourceability is a property of the lens, not of the document: a refusal
/// here leaves the graph a complete, valid, editable program.
pub fn source_view(graph: &WorkflowGraph) -> Result<SourceView, GraphRenderError> {
    let program = validated_program(graph, lash_core_execution::FleetFormat::current())?;
    let source = typescript_program_source(&program)?;
    // Spans come from the canonical text's own parse. The document's nodes
    // are paired with the nodes of that projection by where they sit, so a
    // host's own node ids key the result.
    let canonical = WorkflowGraphProjector::new(&program)
        .with_spans(canonical_spans(&program).unwrap_or_default())
        .project();
    let located = canonical
        .nodes()
        .filter_map(|node| Some((node.id.clone(), node.source_span?)))
        .collect::<BTreeMap<_, _>>();
    let spans = lash_vm::reconcile(graph, &canonical)
        .pairs
        .into_iter()
        .filter_map(|pair| Some((pair.submitted, *located.get(&pair.reprojected)?)))
        .collect();
    Ok(SourceView {
        source_identity: graph.source_identity.clone(),
        source,
        spans,
    })
}
