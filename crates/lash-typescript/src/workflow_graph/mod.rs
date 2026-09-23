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

/// Parse source, canonicalize it, and project it into a deterministic draft
/// graph, with optional host-derived, non-authoritative type facets.
///
/// The projection itself is `lashlang`'s ([`WorkflowGraphProjector`]); this
/// dialect contributes the canonical text the node spans address and the text
/// of opaque statements. Link errors become node diagnostics and never prevent
/// graph projection. If no environment is supplied, the result is the ordinary
/// facet-free graph.
pub fn workflow_graph_from_source_with_facets(
    src: &str,
    environment: Option<&LashlangHostEnvironment>,
) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    let parsed = crate::parse(src)?;
    let canonical = typescript_program_source(&parsed)?;
    let canonical_program = crate::parse(&canonical)?;
    let analysis =
        environment.map(|environment| analyze_workflow_program(&canonical_program, environment));
    let mut projector = WorkflowGraphProjector::new(&canonical_program)
        .with_source_identity(source_identity(&canonical))
        .with_spans(canonical_program.spans.clone());
    if let Some(analysis) = analysis.as_ref() {
        projector = projector.with_analysis(analysis);
    }
    let mut graph = projector.project(&TypeScriptStatementText);
    present_loop_sources(&mut graph);
    Ok(graph)
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
    validated_source(graph)?;
    Ok(())
}

/// Validate and render a graph through the canonical TypeScript printer.
pub fn workflow_graph_to_source(graph: &WorkflowGraph) -> Result<String, GraphRenderError> {
    validated_source(graph)
}

fn validated_source(graph: &WorkflowGraph) -> Result<String, GraphRenderError> {
    let program = validated_program(graph)?;
    let source = typescript_program_source(&program)?;
    crate::parse(&source).map_err(|error| GraphRenderError::RenderedSourceInvalid {
        message: error.to_string(),
    })?;
    Ok(source)
}

fn validated_program(graph: &WorkflowGraph) -> Result<Program, GraphRenderError> {
    validate_graph(graph)?;
    graph_to_program(graph)
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
        let program = graph_to_program(&graph).expect("graph converts to IR");
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

/// Hashes the draft projection's canonical source.
fn source_identity(canonical: &str) -> String {
    lash_sansio::core_support::blake3_domain_hash_hex("lash-workflow-source/v3", canonical)
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

fn graph_to_program(graph: &WorkflowGraph) -> Result<Program, GraphRenderError> {
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
    };
    let mut main = subgraph_to_block(&graph.main, context)?;
    splice_lifted_bodies(&mut main, &mut lifted.into_iter(), context)?;
    Ok(Program {
        declarations,
        main,
        spans: Default::default(),
    })
}

/// Splices each lifted process's rendered body back into the literal it was
/// projected from (FIG-3118).
///
/// A top-level `const`-bound `async` arrow is a process literal in `main`
/// (FIG-2999), and the lens projects it twice: the statement node carries the
/// arrow as authored text, and the body is projected as its own lifted process
/// declaration so the nodes inside are editable. The statement's text is the
/// pre-edit arrow, so rendering `main` alone drops every edit made inside a
/// process container. The lens owns those bodies, so the rendered subgraph is
/// spliced over the literal's body here.
///
/// Literals are matched to declarations positionally: `project` collects
/// literals in `Expr::children` walk order and pushes one declaration per
/// literal in that order, and this walk is the same order over `children_mut`.
/// Matching on the lifted name instead would not work — the name digests the
/// body and the *lowered* AST path, and the rebuilt program is the printer's
/// input, not a lowered program, so neither half survives the round trip.
///
/// A host may add or delete a process-literal-bearing statement without
/// touching the declaration list, so the two sequences can differ in length:
/// a literal with no declaration left keeps its authored body, and a
/// declaration with no literal left renders nowhere.
fn splice_lifted_bodies<'a>(
    expr: &mut Expr,
    lifted: &mut impl Iterator<Item = &'a WorkflowProcess>,
    context: RenderContext<'_>,
) -> Result<(), GraphRenderError> {
    if let Expr::ProcessLiteral(literal) = expr
        && let Some(process) = lifted.next()
    {
        let body = subgraph_to_block(
            &process.body,
            RenderContext {
                scope: RenderScope::Process,
                processes: context.processes,
            },
        )?;
        literal.params = process.params.clone();
        *literal.body = process_wrapper(&process.params, body);
    }
    for child in expr.children_mut() {
        splice_lifted_bodies(child, lifted, context)?;
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
        WorkflowNodeKind::StateUpdate { target, expression } => {
            let [output] = node.outputs.as_slice() else {
                return invalid_payload(node, "state update must have exactly one output");
            };
            if target.root.as_str() != output.variable {
                return invalid_payload(node, "state-update target root must match its output");
            }
            Expr::Assign {
                target: target.clone(),
                expr: Box::new(expression.clone()),
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
