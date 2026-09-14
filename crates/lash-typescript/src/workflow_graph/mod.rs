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

use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    AssignTarget, Declaration, Expr, LabelMetadata, LashlangHostEnvironment,
    ListComprehensionClause, ProcessDecl, Program, Span, VariableVersion,
    WORKFLOW_GRAPH_SCHEMA_VERSION, WorkflowContainer, WorkflowDeclaration, WorkflowEdge,
    WorkflowEdgeKind, WorkflowEffectKind, WorkflowGraph, WorkflowLinkAnalysis,
    WorkflowListComprehensionClause, WorkflowNode, WorkflowNodeId, WorkflowNodeKind,
    WorkflowNodeNameSource, WorkflowProcess, WorkflowSubgraph, WorkflowTerminalKind,
    analyze_workflow_program,
};
use thiserror::Error;

use crate::Diagnostic;

mod editable_text;
mod literals;
mod render_helpers;
use render_helpers::*;
mod printer;

use crate::lower::process_run_body_path;
pub use editable_text::{
    TypeScriptFragmentError, parse_typescript_assign_target, parse_typescript_expression,
    parse_typescript_process_statement,
};
use editable_text::{
    assign_target_text, expression_text, opaque_process_run_body, parse_assignment_target_field,
    parse_comprehension_clauses, parse_expression_field, parse_simple_binding_field,
    parse_typescript_fragment, statement_text, with_assignment, workflow_clause,
};
use literals::collect_process_literals;
use printer::process_run_body as process_run_body_of;
pub use printer::{
    TypeScriptSourceError, typescript_assign_target_source, typescript_expression_source,
    typescript_program_source, typescript_statement_source,
};

/// Parse source, canonicalize it, and project it into a deterministic graph,
/// with optional host-derived, non-authoritative type facets.
///
/// Link errors become node diagnostics and never prevent graph projection. If
/// no environment is supplied, the result is the ordinary facet-free graph.
pub fn workflow_graph_from_source_with_facets(
    src: &str,
    environment: Option<&LashlangHostEnvironment>,
) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    let parsed = crate::parse(src)?;
    let canonical = typescript_program_source(&parsed)?;
    let canonical_program = crate::parse(&canonical)?;
    let analysis =
        environment.map(|environment| analyze_workflow_program(&canonical_program, environment));
    Ok(GraphProjector::new(&canonical, &canonical_program, analysis.as_ref(), false).project())
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
    CanonicalSource(#[from] TypeScriptSourceError),
    #[error("rendered workflow source did not parse: {message}")]
    RenderedSourceInvalid { message: String },
}

/// Parse source, canonicalize it, and project it into a deterministic graph.
pub fn workflow_graph_from_source(src: &str) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    workflow_graph_from_source_with_facets(src, None)
}

/// Project an already-lowered program into the workflow graph used by trace
/// consumers.
///
/// Dialect front-ends may lower into AST forms that deliberately have no
/// Lashlang source spelling. Keeping the projection on the lowered program
/// avoids discarding the trace inventory merely because that display-only
/// source round-trip is unavailable. Such expressions retain their typed
/// execution sites; their editable display text is a trace-only placeholder.
pub fn workflow_graph_from_program(program: &Program) -> WorkflowGraph {
    #[expect(
        clippy::expect_used,
        reason = "`Program` derives `Serialize` over plain data, so encoding it cannot fail"
    )]
    let hash_input = typescript_program_source(program)
        .unwrap_or_else(|_| serde_json::to_string(program).expect("program serializes"));
    GraphProjector::new(&hash_input, program, None, true).project()
}

/// Validate and render a graph through the canonical TypeScript printer.
pub fn workflow_graph_to_source(graph: &WorkflowGraph) -> Result<String, GraphRenderError> {
    validate_graph(graph)?;
    let program = graph_to_program(graph)?;
    let source = typescript_program_source(&program)?;
    crate::parse(&source).map_err(|error| GraphRenderError::RenderedSourceInvalid {
        message: error.to_string(),
    })?;
    Ok(source)
}

struct GraphProjector<'a> {
    program: &'a Program,
    source_hash: String,
    spans: BTreeMap<Vec<u32>, Span>,
    analysis: Option<&'a WorkflowLinkAnalysis>,
    allow_non_sourceable_expressions: bool,
}

impl<'a> GraphProjector<'a> {
    fn new(
        canonical: &'a str,
        program: &'a Program,
        analysis: Option<&'a WorkflowLinkAnalysis>,
        allow_non_sourceable_expressions: bool,
    ) -> Self {
        Self {
            program,
            source_hash: hex_digest("lash-workflow-source/v3", canonical.as_bytes()),
            spans: program
                .expression_source_spans
                .iter()
                .map(|source_span| (source_span.path.clone(), source_span.span))
                .collect(),
            analysis,
            allow_non_sourceable_expressions,
        }
    }

    fn expression_text(&self, expression: &Expr) -> String {
        expression_text(expression, self.allow_non_sourceable_expressions)
    }

    fn statement_text(&self, expression: &Expr, versions: &VersionState) -> String {
        let bound = versions.known.iter().cloned().collect::<Vec<_>>();
        statement_text(expression, &bound, self.allow_non_sourceable_expressions)
    }

    fn workflow_clause(&self, clause: &ListComprehensionClause) -> WorkflowListComprehensionClause {
        workflow_clause(clause, self.allow_non_sourceable_expressions)
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
        // A process literal in an argument is a process container of the
        // module the same way a `defineProcess` binding is (ADR 0095): it
        // projects as its own declaration, named identically to what the
        // linker will lift it to (canonical body plus AST path), addressed at
        // the path where it sits. Its name is invented — no authored binding
        // ships it — so the canonical printer emits nothing for it and its
        // authored arrow re-parses straight off the call site.
        let mut literals = Vec::new();
        collect_process_literals(&self.program.main, &mut Vec::new(), &mut literals);
        for (path, literal) in literals {
            declarations.push(WorkflowDeclaration::Process(
                self.project_literal_process(&path, literal),
            ));
        }
        let mut versions = VersionState::default();
        let main = self.project_block(&self.program.main, "main", &[], &mut versions);
        WorkflowGraph {
            schema_version: WORKFLOW_GRAPH_SCHEMA_VERSION,
            facet_schema_version: self
                .analysis
                .map(|_| lashlang::WORKFLOW_TYPE_FACET_SCHEMA_VERSION),
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
            // `defineProcess` lowers to a wrapper that translates an uncaught
            // error into process failure. Only the inner `run` body was
            // authored, so that is what the graph shows — addressed by the
            // wrapper's own AST path so node identity and execution-site
            // correlation stay keyed on the real path (FIG-3033).
            body: match process_run_body_path(process) {
                Some((path, body)) => self.project_block(body, &owner, &path, &mut versions),
                None => self.project_block(&process.body, &owner, &[], &mut versions),
            },
        }
    }

    fn project_block(
        &self,
        expr: &Expr,
        owner: &str,
        base_path: &[u32],
        versions: &mut VersionState,
    ) -> WorkflowSubgraph {
        // The lowerer wraps every authored statement block as
        // `Block([Block(inner), Undefined])` and ends `inner` with the block's
        // completion value. Neither is authored text, so the projection
        // descends through the wrapper — keeping the AST path, which node
        // identity and execution-site correlation are both keyed on — and
        // drops the trailing completion value.
        let mut expr = expr;
        let mut base_path = base_path.to_vec();
        let mut unwrapped = false;
        while let Some(inner) = printer::block_wrapper_inner(expr) {
            base_path = lashlang::child_path(&base_path, 0);
            expr = inner;
            unwrapped = true;
        }
        let mut expressions = match expr {
            Expr::Block(expressions) => expressions.as_slice(),
            expression => std::slice::from_ref(expression),
        };
        if let [rest @ .., last] = expressions
            && printer::trailing_is_generated(last, unwrapped)
        {
            expressions = rest;
        }
        let indexed = matches!(expr, Expr::Block(_));
        self.project_statements(
            expressions,
            owner,
            &base_path,
            indexed.then_some(0),
            versions,
        )
    }

    /// Project a statement list already normalised out of its block wrapper.
    ///
    /// `start` is the AST-path index of the first statement, or `None` when the
    /// list is a single non-block expression that carries no index of its own.
    fn project_statements(
        &self,
        expressions: &[Expr],
        owner: &str,
        base_path: &[u32],
        start: Option<usize>,
        versions: &mut VersionState,
    ) -> WorkflowSubgraph {
        let mut subgraph = WorkflowSubgraph::default();
        let mut previous_effect: Option<WorkflowNodeId> = None;
        for (index, expression) in expressions.iter().enumerate() {
            let mut path = base_path.to_vec();
            if let Some(start) = start {
                path.push((start + index) as u32);
            }
            // A statement the lowerer wrapped to give it a value is projected
            // as the statement itself, one AST step further down.
            let mut expression = expression;
            while let statement = printer::authored_statement(expression)
                && !std::ptr::eq(
                    std::ptr::from_ref(statement),
                    std::ptr::from_ref(expression),
                )
            {
                path = lashlang::child_path(&path, 0);
                expression = statement;
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
        let execution_sites = lashlang::execution_sites(expression, owner, path, label);
        let type_facets = lashlang::projected_node_type_facets(
            self.analysis,
            expression,
            &available_variables,
            &id,
        );
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
        if let Some((target, value)) = printer::assignment_sugar(expression) {
            return (
                WorkflowNodeKind::StateUpdate {
                    target: assign_target_text(&target, self.allow_non_sourceable_expressions),
                    expression: self.expression_text(value),
                },
                format!("update {}", target.root),
                vec![versions.allocate(target.root.as_str())],
            );
        }
        let (binding, value, value_path) = assignment_parts(expression, path);
        if let Expr::Assign { target, expr } = expression
            && (!target.is_simple() || versions.is_known(target.root.as_str()))
        {
            return (
                WorkflowNodeKind::StateUpdate {
                    target: assign_target_text(target, self.allow_non_sourceable_expressions),
                    expression: self.expression_text(expr),
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
                    &lashlang::child_path(&value_path, 1),
                    &mut then_versions,
                );
                let else_graph = self.project_block(
                    else_block,
                    owner,
                    &lashlang::child_path(&value_path, 2),
                    &mut else_versions,
                );
                let mut outputs = assignment_output(binding.as_ref(), versions);
                outputs.extend(versions.merge_outputs(&then_versions, &else_versions));
                (
                    WorkflowNodeKind::Container(WorkflowContainer::If {
                        binding: binding.as_ref().map(|target| {
                            assign_target_text(target, self.allow_non_sourceable_expressions)
                        }),
                        condition: self.expression_text(condition),
                        then_is_block: matches!(then_block.as_ref(), Expr::Block(_)),
                        // The lowerer spells a missing `else` as the unit
                        // value, which renders as the empty block it was, and
                        // an `else if` chain as a block holding the one nested
                        // `if` — which is the chain, not a block branch.
                        else_is_block: matches!(
                            else_block.as_ref(),
                            Expr::Block(_) | Expr::Undefined
                        ) && printer::else_if_chain(else_block).is_none(),
                        then_graph: Box::new(then_graph),
                        else_graph: Box::new(else_graph),
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
                // `for (const x of xs)` lowers to a generated element binding
                // over `Lash.ArrayFromIterable(xs)` whose body opens by copying
                // the element into the authored binding `x`. The node shows the
                // authored loop; the AST path still addresses the real body.
                let sugar = printer::for_of_sugar(loop_binding.as_str(), iterable, body);
                let (loop_binding, iterable, body_base, body_start, rest) = match sugar {
                    Some((authored, source, rest)) => (
                        authored,
                        source,
                        lashlang::child_path(&value_path, 1),
                        1,
                        Some(rest),
                    ),
                    None => (
                        loop_binding.as_str(),
                        iterable.as_ref(),
                        lashlang::child_path(&value_path, 1),
                        0,
                        None,
                    ),
                };
                let mut body_versions = versions.clone();
                body_versions.shadow(loop_binding);
                let body_graph = match rest {
                    None => self.project_block(body, owner, &body_base, &mut body_versions),
                    Some([single]) if matches!(single, Expr::Block(_)) => self.project_block(
                        single,
                        owner,
                        &lashlang::child_path(&body_base, body_start),
                        &mut body_versions,
                    ),
                    Some(rest) => self.project_statements(
                        rest,
                        owner,
                        &body_base,
                        Some(body_start),
                        &mut body_versions,
                    ),
                };
                let outputs = loop_outputs(body, Some(loop_binding), versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::For {
                        binding: loop_binding.to_string(),
                        iterable: self.expression_text(iterable),
                        body: Box::new(body_graph),
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
                    &lashlang::child_path(&value_path, 1),
                    &mut body_versions,
                );
                let outputs = loop_outputs(body, None, versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::While {
                        condition: self.expression_text(condition),
                        body: Box::new(body_graph),
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
                    &lashlang::child_path(&value_path, clauses.len() as u32),
                    &mut element_versions,
                );
                let outputs = assignment_output(binding.as_ref(), versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
                        binding: binding.as_ref().map(|target| {
                            assign_target_text(target, self.allow_non_sourceable_expressions)
                        }),
                        clauses: clauses
                            .iter()
                            .map(|clause| self.workflow_clause(clause))
                            .collect(),
                        element: Box::new(element_graph),
                    }),
                    "list comprehension".to_string(),
                    outputs,
                )
            }
            Expr::Finish(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Finish,
                    expression: self.expression_text(value),
                },
                "finish".to_string(),
                Vec::new(),
            ),
            Expr::Fail(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Fail,
                    expression: self.expression_text(value),
                },
                "fail".to_string(),
                Vec::new(),
            ),
            // Statement shapes the lens does not decompose into workflow
            // structure yet travel as their own canonical TypeScript text, so
            // a host still sees and edits exactly what was authored.
            // TypeScript has no cell-only `finish` inside a process: a run
            // body ends by returning, and that return is the process's finish.
            Expr::Return(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Finish,
                    expression: self.statement_text(value, versions),
                },
                "return".to_string(),
                Vec::new(),
            ),
            Expr::Try(_) | Expr::Throw(_) => (
                WorkflowNodeKind::Opaque {
                    source: self.statement_text(value, versions),
                },
                opaque_name(value).to_string(),
                Vec::new(),
            ),
            _ if (is_pure_value(value) || matches!(value, Expr::TypeLiteral(_)))
                && binding.is_some() =>
            {
                let outputs = assignment_output(binding.as_ref(), versions);
                (
                    WorkflowNodeKind::Data {
                        binding: binding.as_ref().map(|target| {
                            assign_target_text(target, self.allow_non_sourceable_expressions)
                        }),
                        expression: self.expression_text(value),
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
                            binding: binding.as_ref().map(|target| {
                                assign_target_text(target, self.allow_non_sourceable_expressions)
                            }),
                            operation: operation.to_string(),
                            expression: self.expression_text(value),
                        },
                        operation.to_string(),
                        outputs,
                    )
                } else if let Some(effect) = effect_kind(value) {
                    let name = effect_name(value, &effect);
                    (
                        WorkflowNodeKind::Effect {
                            binding: binding.as_ref().map(|target| {
                                assign_target_text(target, self.allow_non_sourceable_expressions)
                            }),
                            effect,
                            expression: self.expression_text(value),
                        },
                        name,
                        outputs,
                    )
                } else {
                    (
                        WorkflowNodeKind::Computation {
                            binding: binding.as_ref().map(|target| {
                                assign_target_text(target, self.allow_non_sourceable_expressions)
                            }),
                            expression: self.expression_text(value),
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
        WorkflowNodeId::new(format!(
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
    for declaration in &graph.declarations {
        declarations.push(match declaration {
            WorkflowDeclaration::Type(ty) => Declaration::Type(ty.clone()),
            WorkflowDeclaration::Function(function) => Declaration::Function(function.clone()),
            WorkflowDeclaration::Process(process) => {
                // A lifted literal is not a module declaration: its authored
                // arrow travels inline at the call site that passed it, so the
                // rebuilt program carries no declaration for it. Its subgraph
                // still renders — each node inside re-parses into that arrow's
                // body — it just reprojects through the call node's text.
                if process
                    .name
                    .starts_with(lashlang::LIFTED_PROCESS_NAME_PREFIX)
                {
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
    Ok(Program {
        declarations,
        main: subgraph_to_block(
            &graph.main,
            RenderContext {
                scope: RenderScope::Main,
                processes: &process_names,
            },
        )?,
        declaration_spans: Vec::new(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    })
}

/// What a node is being rendered back into.
#[derive(Clone, Copy)]
struct RenderContext<'a> {
    scope: RenderScope,
    /// The process names the module declares.
    ///
    /// `const child = defineProcess(..)` projects as the module binding its
    /// process reference. The reference is not ordinary text — the name is
    /// bound by the very statement that reads it — so it is rebuilt from the
    /// declaration list instead of parsed.
    processes: &'a [String],
}

impl RenderContext<'_> {
    /// The module's `defineProcess` bindings, as a fragment parse sees them.
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
    // The module's own binding of a declared process.
    if context.scope == RenderScope::Main
        && let WorkflowNodeKind::Data {
            binding: Some(binding),
            expression,
        } = &node.kind
        && context.processes.iter().any(|name| name == expression)
    {
        return Ok(Expr::Assign {
            target: parse_simple_binding_field(node, "binding", binding)?,
            expr: Box::new(Expr::ProcessRef {
                process: expression.clone().into(),
            }),
        });
    }
    let expression = match &node.kind {
        WorkflowNodeKind::Data {
            binding,
            expression,
        } => {
            let expression = parse_expression_field(node, "expression", expression, context)?;
            if !lashlang::is_pure_expr(&expression) && !matches!(expression, Expr::TypeLiteral(_)) {
                return invalid_payload(node, "data expression is effectful");
            }
            with_assignment(node, binding, expression, true)?
        }
        WorkflowNodeKind::Call {
            binding,
            expression,
            operation,
        } => {
            let expression = parse_expression_field(node, "expression", expression, context)?;
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
            // Loop control carries no payload and is not an expression the
            // language will parse outside its loop, so it renders from the
            // effect kind rather than from its own text.
            let expression = match effect {
                WorkflowEffectKind::Break => Expr::Break,
                WorkflowEffectKind::Continue => Expr::Continue,
                _ => parse_expression_field(node, "expression", expression, context)?,
            };
            if effect_kind(&expression).as_ref() != Some(effect) {
                return invalid_payload(node, "effect kind does not match its expression");
            }
            with_assignment(node, binding, expression, true)?
        }
        WorkflowNodeKind::Computation {
            binding,
            expression,
        } => {
            let expression = parse_expression_field(node, "expression", expression, context)?;
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
                expr: Box::new(parse_expression_field(
                    node,
                    "expression",
                    expression,
                    context,
                )?),
            }
        }
        WorkflowNodeKind::Terminal {
            terminal,
            expression,
        } => {
            // A process terminal is a `return` statement, which is not an
            // expression the language will parse in expression position.
            let expression = match context.scope {
                RenderScope::Process => parse_opaque_statement(node, expression, context)?,
                RenderScope::Main => {
                    parse_expression_field(node, "expression", expression, context)?
                }
            };
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
            expression
        }
        WorkflowNodeKind::Container(container) => match container {
            WorkflowContainer::If {
                binding,
                condition,
                then_is_block,
                else_is_block,
                then_graph,
                else_graph,
            } => with_assignment(
                node,
                binding,
                Expr::If {
                    condition: Box::new(parse_expression_field(
                        node,
                        "condition",
                        condition,
                        context,
                    )?),
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
            )?,
            WorkflowContainer::For {
                binding,
                iterable,
                body,
            } => Expr::For {
                binding: parse_simple_binding_field(node, "binding", binding)?.root,
                iterable: Box::new(parse_expression_field(node, "iterable", iterable, context)?),
                body: Box::new(subgraph_to_block(body, context)?),
            },
            WorkflowContainer::While { condition, body } => Expr::While {
                condition: Box::new(parse_expression_field(
                    node,
                    "condition",
                    condition,
                    context,
                )?),
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
                with_assignment(
                    node,
                    binding,
                    Expr::ListComprehension {
                        element: Box::new(expressions.remove(0)),
                        clauses: parse_comprehension_clauses(node, clauses, context)?,
                    },
                    true,
                )?
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
        RenderScope::Main => printer::statement_block_contents(&program.main).to_vec(),
        RenderScope::Process => {
            let Some(Declaration::Process(process)) = program.declarations.into_iter().next()
            else {
                return invalid_payload(node, "opaque process wrapper did not produce a process");
            };
            let Some(body) = opaque_process_run_body(&process) else {
                return invalid_payload(node, "opaque process wrapper did not produce a run body");
            };
            printer::statement_block_contents(body).to_vec()
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
    Ok(expression)
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
                .filter(|step| matches!(step, lashlang::AssignPathStep::Index(_)))
                .count() as u32;
            (
                Some(target.clone()),
                expr,
                lashlang::child_path(path, dynamic_indices),
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
    // A member assignment lowers to a block writing generated temporaries, so
    // the write the loop publishes is the one the author spelled, and the
    // temporaries it travels through are never a loop-carried variable.
    if let Some((target, value)) = printer::assignment_sugar(expression) {
        assigned.insert(target.root.to_string());
        collect_assignment_roots(value, assigned);
        return;
    }
    if let Expr::Assign { target, .. } = expression
        && !target.root.starts_with(crate::GENERATED_BINDING_PREFIX)
    {
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
