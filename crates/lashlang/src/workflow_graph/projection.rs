//! IR -> [`WorkflowGraph`] projection (ADR 0100 R8).
//!
//! The projector reads structure only from the one ownership walk
//! ([`WorkflowProjection`]) and the IR's forms and structural roles. It names
//! no source syntax: the one piece of dialect text a graph carries, an opaque
//! statement's source, comes from an injected [`WorkflowStatementText`].

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{
    AssignTarget, AstPath, AttributeAssignParts, AttributeStep, Declaration, Expr, LabelMetadata,
    ListComprehensionClause, ProcessDecl, ProcessLiteralExpr, ProcessOrigin, Program,
    StructuralRole,
};
use crate::linker::WorkflowLinkAnalysis;
use crate::span::Span;

use super::{
    VariableVersion, WORKFLOW_GRAPH_SCHEMA_VERSION, WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
    WorkflowBody, WorkflowBodySlot, WorkflowContainer, WorkflowDeclaration, WorkflowEdge,
    WorkflowEdgeKind, WorkflowEffectKind, WorkflowGraph, WorkflowListComprehensionClause,
    WorkflowNode, WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowOwnership,
    WorkflowProcess, WorkflowProjection, WorkflowStatement, WorkflowSubgraph, WorkflowTerminalKind,
    execution_sites, projected_node_type_facets, statement_list, workflow_call_from_ir,
    workflow_effect_from_ir, workflow_node_id,
};

/// A dialect's source text for one opaque statement.
///
/// Opaque nodes carry a statement the graph does not decompose, as the text a
/// host shows and edits. The dialect that owns that text supplies it.
pub trait WorkflowStatementText {
    /// `available` names the identifiers bound before the statement runs.
    fn statement_text(&self, statement: &Expr, available: &[String]) -> String;
}

/// Renders no opaque text: for consumers that read identity, structure and
/// execution sites only, such as the runtime's trace maps.
pub struct NoStatementText;

impl WorkflowStatementText for NoStatementText {
    fn statement_text(&self, _statement: &Expr, _available: &[String]) -> String {
        String::new()
    }
}

/// Projects `program` as a draft graph, before any admission: it claims no
/// source identity.
pub fn workflow_graph_from_program(
    program: &Program,
    text: &dyn WorkflowStatementText,
) -> WorkflowGraph {
    WorkflowGraphProjector::new(program).project(text)
}

/// Projects an admitted module artifact: the graph of exactly the program the
/// artifact executes, carrying the artifact's source identity.
pub fn workflow_graph_from_artifact(
    artifact: &crate::ModuleArtifact,
    text: &dyn WorkflowStatementText,
) -> WorkflowGraph {
    WorkflowGraphProjector::new(artifact.ir())
        .with_source_identity(artifact.source_identity())
        .project(text)
}

/// A configurable IR projection.
pub struct WorkflowGraphProjector<'a> {
    program: &'a Program,
    source_identity: Option<String>,
    spans: BTreeMap<AstPath, Span>,
    analysis: Option<&'a WorkflowLinkAnalysis>,
    fleet_format: lash_core_execution::FleetFormat,
}

impl<'a> WorkflowGraphProjector<'a> {
    pub fn new(program: &'a Program) -> Self {
        Self {
            program,
            source_identity: None,
            spans: BTreeMap::new(),
            analysis: None,
            fleet_format: lash_core_execution::FleetFormat::current(),
        }
    }

    /// The `F` the bound writer's store recorded: a projector standing a
    /// document up for a durable write stamps `F`'s writer versions for the
    /// graph and facet surfaces (FIG-3796), never the bare build constants.
    pub fn with_fleet_format(mut self, fleet_format: lash_core_execution::FleetFormat) -> Self {
        self.fleet_format = fleet_format;
        self
    }

    /// The admitted definition identity the projected document names.
    pub fn with_source_identity(mut self, source_identity: String) -> Self {
        self.source_identity = Some(source_identity);
        self
    }

    /// Source spans keyed by the projected program's own AST paths.
    pub fn with_spans(mut self, spans: BTreeMap<AstPath, Span>) -> Self {
        self.spans = spans;
        self
    }

    /// Link facts for optional type facets, keyed by the projected program's
    /// AST paths.
    pub fn with_analysis(mut self, analysis: &'a WorkflowLinkAnalysis) -> Self {
        self.analysis = Some(analysis);
        self
    }

    pub fn project(&self, text: &dyn WorkflowStatementText) -> WorkflowGraph {
        let session = Session {
            projector: self,
            text,
            fleet_format: self.fleet_format,
        };
        session.project()
    }
}

struct Session<'p, 'a> {
    projector: &'p WorkflowGraphProjector<'a>,
    text: &'p dyn WorkflowStatementText,
    fleet_format: lash_core_execution::FleetFormat,
}

impl Session<'_, '_> {
    fn project(&self) -> WorkflowGraph {
        let program = self.projector.program;
        let mut declarations = Vec::with_capacity(program.declarations.len());
        for (index, declaration) in program.declarations.iter().enumerate() {
            match declaration {
                Declaration::Type(ty) => declarations.push(WorkflowDeclaration::Type(ty.clone())),
                Declaration::Process(process) => declarations.push(WorkflowDeclaration::Process(
                    self.project_process(process, index as u32),
                )),
                Declaration::Function(function) => {
                    declarations.push(WorkflowDeclaration::Function(function.clone()))
                }
            }
        }
        // A process literal a draft still carries inline is a process
        // container of the module the same way a declaration is (ADR 0095):
        // it projects as the declaration the linker will lift it to, named by
        // the same digest of its body and site.
        let mut literals = Vec::new();
        collect_process_literals(&program.main, &mut Vec::new(), &mut literals);
        for (path, literal) in literals {
            declarations.push(WorkflowDeclaration::Process(
                self.project_literal_process(&path, literal),
            ));
        }
        let mut versions = VersionState::default();
        let projection = WorkflowProjection::for_main(program);
        let main = self.project_body(
            projection.body(),
            "main",
            projection.ownership_map(),
            &mut versions,
        );
        WorkflowGraph {
            schema_version: self
                .fleet_format
                .writer_version(lash_core_execution::surface_format!(
                    WORKFLOW_GRAPH_SCHEMA_VERSION
                )),
            source_identity: self.projector.source_identity.clone(),
            facet_schema_version: self.projector.analysis.map(|_| {
                self.fleet_format
                    .writer_version(lash_core_execution::surface_format!(
                        WORKFLOW_TYPE_FACET_SCHEMA_VERSION
                    ))
            }),
            declarations,
            main,
        }
    }

    fn project_process(&self, process: &ProcessDecl, declaration_index: u32) -> WorkflowProcess {
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
        let projection = WorkflowProjection::for_process(
            &process.body,
            AstPath::declaration(declaration_index, Vec::new()),
        );
        WorkflowProcess {
            id: workflow_node_id(&owner, &[]),
            name: process.name.to_string(),
            display_name,
            description,
            name_source,
            params: process.params.clone(),
            signals: process.signals.clone(),
            return_ty: process.return_ty.clone(),
            origin: process.origin.clone(),
            body: self.project_body(
                projection.body(),
                &owner,
                projection.ownership_map(),
                &mut versions,
            ),
        }
    }

    /// A literal's body is addressed the way the declaration it lifts to is:
    /// its owner is the lifted name and its node paths are relative to the
    /// body, so the draft and the admitted artifact mint the same ids. Its
    /// facts stay keyed by where the literal sits in the draft.
    fn project_literal_process(
        &self,
        path: &[u32],
        literal: &ProcessLiteralExpr,
    ) -> WorkflowProcess {
        let name = crate::lifted_process_identity(&literal.body, path);
        let owner = format!("process:{name}");
        let mut versions = VersionState::default();
        for param in &literal.params {
            versions.seed(param.name.as_str());
        }
        let site = AstPath::main(path.to_vec());
        let projection = WorkflowProjection::for_process(&literal.body, site.child(0));
        WorkflowProcess {
            id: workflow_node_id(&owner, &[]),
            name: name.clone(),
            display_name: name,
            description: None,
            name_source: WorkflowNodeNameSource::Derived,
            params: literal.params.clone(),
            signals: Vec::new(),
            return_ty: None,
            origin: ProcessOrigin::Lifted {
                site,
                hidden_params: u32::try_from(literal.hidden_args.len()).unwrap_or(u32::MAX),
            },
            body: self.project_body(
                projection.body(),
                &owner,
                projection.ownership_map(),
                &mut versions,
            ),
        }
    }

    fn project_body(
        &self,
        body: &WorkflowBody<'_>,
        owner: &str,
        ownership: &WorkflowOwnership,
        versions: &mut VersionState,
    ) -> WorkflowSubgraph {
        let mut subgraph = WorkflowSubgraph::default();
        let mut previous_effect: Option<WorkflowNodeId> = None;
        for statement in &body.statements {
            let node = self.project_node(statement, owner, ownership, versions);
            add_dependency_edges(&mut subgraph.edges, &node, statement.expr, versions);
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
        statement: &WorkflowStatement<'_>,
        owner: &str,
        ownership: &WorkflowOwnership,
        versions: &mut VersionState,
    ) -> WorkflowNode {
        let (label, expression) = peel_label(statement.expr);
        let facts_path = if label.is_some() {
            statement.ast_path.child(0)
        } else {
            statement.ast_path.clone()
        };
        let path = statement.node_path.indices();
        let source_span = self.source_span(&statement.ast_path);
        let available_variables: Vec<String> = versions.known.iter().cloned().collect();
        let (kind, derived_name, outputs) =
            self.project_kind(statement, expression, owner, ownership, versions);
        let id = workflow_node_id(owner, path);
        let (name, description, name_source) = match label {
            Some(label) => (
                label.title.to_string(),
                label.description.as_ref().map(ToString::to_string),
                WorkflowNodeNameSource::Label,
            ),
            None => (derived_name, None, WorkflowNodeNameSource::Derived),
        };
        let execution_sites = execution_sites(expression, owner, &facts_path, ownership, label);
        let type_facets = projected_node_type_facets(
            self.projector.analysis,
            &facts_path,
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

    /// The span of a statement, or of the nearest enclosing expression a
    /// front end recorded one for: a statement the front end gave a completion
    /// value carries its span on that wrapper.
    fn source_span(&self, ast_path: &AstPath) -> Option<Span> {
        let spans = &self.projector.spans;
        let mut path = ast_path.clone();
        loop {
            if let Some(span) = spans.get(&path) {
                return Some(*span);
            }
            let wrapper = path.steps.len() >= 2
                && path.steps[path.steps.len() - 2..] == [0, 0]
                && self.is_completion_wrapper(&AstPath {
                    root: path.root,
                    steps: path.steps[..path.steps.len() - 2].to_vec(),
                });
            if !wrapper {
                return None;
            }
            path.steps.truncate(path.steps.len() - 2);
        }
    }

    fn is_completion_wrapper(&self, path: &AstPath) -> bool {
        matches!(
            expr_at(self.projector.program, path),
            Some(Expr::Role {
                role: StructuralRole::Completion,
                ..
            })
        )
    }

    fn project_kind(
        &self,
        statement: &WorkflowStatement<'_>,
        expression: &Expr,
        owner: &str,
        ownership: &WorkflowOwnership,
        versions: &mut VersionState,
    ) -> (WorkflowNodeKind, String, Vec<VariableVersion>) {
        if let Some((target, value, update)) = attribute_assignment(expression) {
            let name = format!("update {}", target.root);
            let outputs = vec![versions.allocate(target.root.as_str())];
            return (
                WorkflowNodeKind::StateUpdate {
                    target,
                    expression: value.clone(),
                    update,
                },
                name,
                outputs,
            );
        }
        let (binding, value) = assignment_parts(expression);
        if let Expr::Assign { target, expr } = expression
            && (!target.is_simple() || versions.is_known(target.root.as_str()))
            && !matches!(
                expr.as_ref(),
                Expr::If { .. }
                    | Expr::For { .. }
                    | Expr::While { .. }
                    | Expr::ListComprehension { .. }
            )
        {
            return (
                WorkflowNodeKind::StateUpdate {
                    target: target.clone(),
                    expression: expr.as_ref().clone(),
                    update: None,
                },
                format!("update {}", target.root),
                vec![versions.allocate(target.root.as_str())],
            );
        }
        let empty = WorkflowBody {
            slot: None,
            ast_path: statement.ast_path.clone(),
            statements: Vec::new(),
        };
        let body_in = |slot| statement.body(slot).unwrap_or(&empty);
        match value {
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut then_versions = versions.clone();
                let mut else_versions = versions.clone();
                let then_graph = self.project_body(
                    body_in(WorkflowBodySlot::Then),
                    owner,
                    ownership,
                    &mut then_versions,
                );
                let else_graph = self.project_body(
                    body_in(WorkflowBodySlot::Else),
                    owner,
                    ownership,
                    &mut else_versions,
                );
                let mut outputs = assignment_output(binding.as_ref(), versions);
                outputs.extend(versions.merge_outputs(&then_versions, &else_versions));
                (
                    WorkflowNodeKind::Container(WorkflowContainer::If {
                        binding: binding.clone(),
                        condition: condition.as_ref().clone(),
                        then_is_block: is_statement_block(then_block),
                        // A missing `else` is an empty statement list, and an
                        // `else if` chain is a list holding one statement `if`
                        // — which is the chain, not a block branch.
                        else_is_block: (is_statement_block(else_block)
                            || matches!(else_block.as_ref(), Expr::Undefined))
                            && else_if_chain(else_block).is_none(),
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
                bind,
                body: _,
            } => {
                // The names the body reads: the bind's, when the loop binds
                // its element into authored names, else the element itself.
                let mut visible = BTreeSet::new();
                match bind {
                    Some(bind) => collect_assigned_roots(bind, &mut visible),
                    None => {
                        visible.insert(loop_binding.to_string());
                    }
                }
                let mut body_versions = versions.clone();
                for name in &visible {
                    body_versions.shadow(name);
                }
                let mut scoped = visible;
                scoped.insert(loop_binding.to_string());
                let loop_body = body_in(WorkflowBodySlot::LoopBody);
                let body_graph = self.project_body(loop_body, owner, ownership, &mut body_versions);
                let outputs = loop_outputs(loop_body, &scoped, versions);
                // A bind that only copies the element into one name is that
                // name's binding: the container names it directly, and only a
                // bind that does more (a destructuring) travels as IR.
                let (binding, bind) = match copied_binding(loop_binding, bind.as_deref()) {
                    Some(authored) => (authored.to_string(), None),
                    None => (loop_binding.to_string(), bind.as_deref().cloned()),
                };
                let name = format!("for {binding}");
                (
                    WorkflowNodeKind::Container(WorkflowContainer::For {
                        binding,
                        iterable: iterable.as_ref().clone(),
                        bind,
                        body: Box::new(body_graph),
                    }),
                    name,
                    outputs,
                )
            }
            Expr::While { condition, .. } => {
                let mut body_versions = versions.clone();
                let loop_body = body_in(WorkflowBodySlot::LoopBody);
                let body_graph = self.project_body(loop_body, owner, ownership, &mut body_versions);
                let outputs = loop_outputs(loop_body, &BTreeSet::new(), versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::While {
                        condition: condition.as_ref().clone(),
                        body: Box::new(body_graph),
                    }),
                    "while".to_string(),
                    outputs,
                )
            }
            Expr::ListComprehension { clauses, .. } => {
                let mut element_versions = versions.clone();
                for clause in clauses {
                    if let ListComprehensionClause::For { binding, .. } = clause {
                        element_versions.shadow(binding.as_str());
                    }
                }
                let element_graph = self.project_body(
                    body_in(WorkflowBodySlot::ComprehensionElement),
                    owner,
                    ownership,
                    &mut element_versions,
                );
                let outputs = assignment_output(binding.as_ref(), versions);
                (
                    WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
                        binding: binding.clone(),
                        clauses: clauses.iter().map(workflow_clause_from_ir).collect(),
                        element: Box::new(element_graph),
                    }),
                    "list comprehension".to_string(),
                    outputs,
                )
            }
            Expr::Finish(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Finish,
                    expression: value.clone(),
                },
                "finish".to_string(),
                Vec::new(),
            ),
            Expr::Fail(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Fail,
                    expression: value.clone(),
                },
                "fail".to_string(),
                Vec::new(),
            ),
            // A function body ends by returning, and in a process body that
            // return is the process's finish.
            Expr::Return(_) => (
                WorkflowNodeKind::Terminal {
                    terminal: WorkflowTerminalKind::Finish,
                    expression: value.clone(),
                },
                "return".to_string(),
                Vec::new(),
            ),
            // Statement shapes the graph does not decompose travel as their
            // dialect's own text, so a host still sees what was authored.
            Expr::Try(_)
            | Expr::Throw(_)
            | Expr::Role {
                role: StructuralRole::Scope,
                ..
            } => {
                let available = versions.known.iter().cloned().collect::<Vec<_>>();
                (
                    WorkflowNodeKind::Opaque {
                        source: self.text.statement_text(value, &available),
                    },
                    opaque_name(value).to_string(),
                    Vec::new(),
                )
            }
            _ if (is_pure_value(value) || matches!(value, Expr::TypeLiteral(_)))
                && binding.is_some() =>
            {
                let outputs = assignment_output(binding.as_ref(), versions);
                (
                    WorkflowNodeKind::Data {
                        binding: binding.clone(),
                        expression: value.clone(),
                    },
                    data_name(value),
                    outputs,
                )
            }
            _ => {
                let outputs = assignment_output(binding.as_ref(), versions);
                if let Some((receiver, operation, arguments, result_steps)) =
                    workflow_call_from_ir(value)
                {
                    (
                        WorkflowNodeKind::Call {
                            binding: binding.clone(),
                            receiver,
                            operation: operation.clone(),
                            arguments,
                            result_steps,
                        },
                        operation,
                        outputs,
                    )
                } else if let Some((effect, arguments, result_steps)) =
                    workflow_effect_from_ir(value)
                {
                    let name = effect_name(value, &effect);
                    (
                        WorkflowNodeKind::Effect {
                            binding: binding.clone(),
                            effect,
                            arguments,
                            result_steps,
                        },
                        name,
                        outputs,
                    )
                } else {
                    (
                        WorkflowNodeKind::Computation {
                            binding: binding.clone(),
                            expression: value.clone(),
                        },
                        computation_name(value),
                        outputs,
                    )
                }
            }
        }
    }
}

/// The expression at `path` in `program`, if the path addresses one.
fn expr_at<'p>(program: &'p Program, path: &AstPath) -> Option<&'p Expr> {
    let mut expression = match path.root {
        crate::ast::AstRoot::Main => &program.main,
        crate::ast::AstRoot::Declaration(index) => {
            match program.declarations.get(index as usize)? {
                Declaration::Process(process) => &process.body,
                Declaration::Function(function) => &function.body,
                Declaration::Type(_) => return None,
            }
        }
    };
    for step in &path.steps {
        expression = expression.children().nth(*step as usize)?;
    }
    Some(expression)
}

/// The authored target and value of an attribute assignment whose object is a
/// plain variable: the only shape a graph state update can name.
/// A member assignment role as a state update: its target, then either its
/// value or, for a compound update, the operand and operator.
fn attribute_assignment(
    expression: &Expr,
) -> Option<(AssignTarget, &Expr, Option<crate::UpdateOperator>)> {
    let Expr::Role {
        role: StructuralRole::AttributeAssign,
        expr,
    } = expression
    else {
        return None;
    };
    let parts = AttributeAssignParts::of(expr)?;
    let Expr::Variable(root) = parts.object else {
        return None;
    };
    let step = match parts.step {
        AttributeStep::Field(field) => crate::AssignPathStep::Field(field.clone()),
        AttributeStep::Index(index) => crate::AssignPathStep::Index(index.clone()),
    };
    let (value, update) = match parts.update {
        Some(update) => (update.operand, Some(update.operator)),
        None => (parts.value, None),
    };
    Some((
        AssignTarget {
            root: root.clone(),
            steps: vec![step],
        },
        value,
        update,
    ))
}

/// A body position that holds statements rather than one value expression.
fn is_statement_block(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Block(_)
            | Expr::Role {
                role: StructuralRole::Completion,
                ..
            }
    )
}

/// The single statement `if` an `else if` chain is, if this else branch is
/// one: an `if` with a statement-block then branch, alone or as the only
/// statement of the branch.
pub fn else_if_chain(expression: &Expr) -> Option<&Expr> {
    match statement_list(expression).as_slice() {
        [listed] => match listed.expr {
            nested @ Expr::If { then_block, .. } if is_statement_block(then_block) => Some(nested),
            _ => None,
        },
        _ => None,
    }
}

/// The one name a bind copies the loop element into, when that is all the
/// bind does.
fn copied_binding<'e>(binding: &str, bind: Option<&'e Expr>) -> Option<&'e str> {
    let Some(Expr::Block(items)) = bind else {
        return None;
    };
    match items.as_slice() {
        [Expr::Assign { target, expr }]
            if target.is_simple()
                && matches!(expr.as_ref(), Expr::Variable(name) if name.as_str() == binding) =>
        {
            Some(target.root.as_str())
        }
        _ => None,
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

fn add_dependency_edges(
    edges: &mut Vec<WorkflowEdge>,
    node: &WorkflowNode,
    expression: &Expr,
    versions: &VersionState,
) {
    let mut variables = BTreeSet::new();
    collect_free_variables(expression, &BTreeSet::new(), &mut variables);
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
            bind,
            body,
        } => {
            collect_free_variables(iterable, bound, variables);
            let mut body_bound = bound.clone();
            body_bound.insert(binding.to_string());
            if let Some(bind) = bind {
                collect_free_variables(bind, &body_bound, variables);
                let mut assigned = BTreeSet::new();
                collect_assigned_roots(bind, &mut assigned);
                body_bound.extend(assigned);
            }
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
            &lash_sansio::core_support::blake3_domain_hash_hex(
                "lash-workflow-edge/v2",
                material.as_bytes()
            )[..24]
        ),
        from,
        to,
        kind,
    }
}

fn node_is_sequenced(node: &WorkflowNode) -> bool {
    !matches!(node.kind, WorkflowNodeKind::Data { .. })
}

fn assignment_parts(expression: &Expr) -> (Option<AssignTarget>, &Expr) {
    match expression {
        Expr::Assign { target, expr } => (Some(target.clone()), expr),
        _ => (None, expression),
    }
}

fn workflow_clause_from_ir(clause: &ListComprehensionClause) -> WorkflowListComprehensionClause {
    match clause {
        ListComprehensionClause::For { binding, iterable } => {
            WorkflowListComprehensionClause::For {
                binding: binding.to_string(),
                iterable: iterable.clone(),
            }
        }
        ListComprehensionClause::If { condition } => WorkflowListComprehensionClause::If {
            condition: condition.clone(),
        },
    }
}

fn assignment_output(
    binding: Option<&AssignTarget>,
    versions: &mut VersionState,
) -> Vec<VariableVersion> {
    binding
        .map(|target| vec![versions.allocate(target.root.as_str())])
        .unwrap_or_default()
}

/// The variables a loop body writes that outlive one iteration: the roots of
/// its assignment statements, at any depth of its visible hierarchy, minus the
/// names the loop binds itself.
fn loop_outputs(
    body: &WorkflowBody<'_>,
    scoped: &BTreeSet<String>,
    versions: &mut VersionState,
) -> Vec<VariableVersion> {
    let mut assigned = BTreeSet::new();
    collect_statement_roots(body, &mut assigned);
    assigned
        .into_iter()
        .filter(|variable| !scoped.contains(variable))
        .map(|variable| versions.allocate(&variable))
        .collect()
}

fn collect_statement_roots(body: &WorkflowBody<'_>, assigned: &mut BTreeSet<String>) {
    for statement in &body.statements {
        let (_, expression) = peel_label(statement.expr);
        if let Some((target, _, _)) = attribute_assignment(expression) {
            assigned.insert(target.root.to_string());
        } else if let Expr::Assign { target, .. } = expression {
            assigned.insert(target.root.to_string());
        }
        for child in &statement.bodies {
            collect_statement_roots(child, assigned);
        }
    }
}

/// Every root a bind block assigns: the names a loop binds per iteration.
fn collect_assigned_roots(expression: &Expr, assigned: &mut BTreeSet<String>) {
    if let Expr::Assign { target, .. } = expression {
        assigned.insert(target.root.to_string());
    }
    for child in expression.children() {
        collect_assigned_roots(child, assigned);
    }
}

fn peel_label(expression: &Expr) -> (Option<&LabelMetadata>, &Expr) {
    match expression {
        Expr::LabelAnnotated { label, expr } => (Some(label), expr),
        _ => (None, expression),
    }
}

/// Every inline process literal in `main`, with its path, in walk order.
fn collect_process_literals<'a>(
    expr: &'a Expr,
    path: &mut Vec<u32>,
    literals: &mut Vec<(Vec<u32>, &'a ProcessLiteralExpr)>,
) {
    if let Expr::ProcessLiteral(literal) = expr {
        literals.push((path.clone(), literal));
    }
    for (index, child) in (0u32..).zip(expr.children()) {
        path.push(index);
        collect_process_literals(child, path, literals);
        path.pop();
    }
}

fn effect_name(expression: &Expr, effect: &WorkflowEffectKind) -> String {
    let descriptor_expression = match expression {
        Expr::ResultUnwrap(inner) => inner.as_ref(),
        _ => expression,
    };
    if let Some((_, label)) = crate::execution_site_descriptor(descriptor_expression) {
        return label.into_owned();
    }
    match effect {
        WorkflowEffectKind::AwaitJoin => "await",
        WorkflowEffectKind::Print => "print",
        WorkflowEffectKind::Break => "break",
        WorkflowEffectKind::Continue => "continue",
        // The remaining effects all carry a compiler execution-site descriptor.
        WorkflowEffectKind::WaitSignal
        | WorkflowEffectKind::SleepFor
        | WorkflowEffectKind::SleepUntil
        | WorkflowEffectKind::Yield => {
            unreachable!("execution-site effects must have a compiler descriptor")
        }
    }
    .to_string()
}

/// A builtin's display name. A source builtin shows as itself; any other
/// builtin is a VM opcode a front end emits, not a name a person wrote, so it
/// shows as the kind of thing it is.
fn builtin_name(name: &str) -> String {
    match name {
        "__typescript_await_array" => "await all".to_string(),
        name if crate::builtin_names().any(|builtin| builtin == name) => name.to_string(),
        _ => "computation".to_string(),
    }
}

fn data_name(expression: &Expr) -> String {
    match expression {
        Expr::BuiltinCall { name, .. } => builtin_name(name),
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
        Expr::BuiltinCall { name, .. } => return builtin_name(name),
        Expr::Binary { .. } => "binary computation",
        Expr::Unary { .. } => "unary computation",
        Expr::Field { .. } => "field computation",
        Expr::Index { .. } => "index computation",
        Expr::ResultUnwrap(_) => "result computation",
        _ => "computation",
    }
    .to_string()
}

/// Purity as the graph means it: an awaited composite is a computation even
/// when its operands are pure.
fn is_pure_value(expression: &Expr) -> bool {
    crate::is_pure_expr(expression) && !awaits(expression)
}

fn awaits(expression: &Expr) -> bool {
    match expression {
        Expr::Await(_) => true,
        // A closure's or a process literal's body awaits on its own account.
        Expr::Function(_) | Expr::ProcessLiteral(_) => false,
        Expr::BuiltinCall { name, .. } if name.as_str() == "__typescript_await_pending" => true,
        _ => expression.children().any(awaits),
    }
}

fn opaque_name(expression: &Expr) -> &'static str {
    match expression {
        Expr::Try(_) => "try",
        Expr::Throw(_) => "throw",
        Expr::Return(_) => "return",
        Expr::Break => "break",
        Expr::Continue => "continue",
        Expr::Role {
            role: StructuralRole::Scope,
            ..
        } => "block",
        _ => "statement",
    }
}
