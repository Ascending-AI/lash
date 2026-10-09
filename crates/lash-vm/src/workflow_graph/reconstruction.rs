//! [`WorkflowGraph`] -> IR reconstruction.
//!
//! The inverse of the projector: it reads only a document's authoritative
//! fields and rebuilds the program they spell. No dialect is involved, so a
//! graph is validated, admitted and published as IR; printing it as source is
//! an optional lens over the result.
//!
//! Projection followed by reconstruction is the identity on a program that
//! passes [`crate::validate_ast`], which is what makes a document total: every
//! construct a program can hold has a typed place in the graph.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::ast::{
    AssignPathStep, AssignTarget, AttributeAssignParts, CatchClause, Declaration, Expr,
    FunctionExpr, LabelMetadata, ProcessDecl, ProcessLiteralExpr, ProcessOrigin,
    ProcessWrapperParts, Program, StructuralRole, TryExpr,
};

use super::{
    WorkflowContainer, WorkflowDeclaration, WorkflowGraph, WorkflowIrVersionRefusal, WorkflowNode,
    WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowProcess, WorkflowSubgraph,
    WorkflowTerminalKind, workflow_call_to_ir, workflow_effect_to_ir,
};

/// Why a workflow document does not spell a program.
#[derive(Clone, Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum WorkflowGraphError {
    #[error(transparent)]
    UnsupportedIrVersion(#[from] WorkflowIrVersionRefusal),
    #[error("duplicate workflow node id `{id}`")]
    DuplicateNodeId { id: String },
    #[error("duplicate process name `{name}`")]
    DuplicateProcessName { name: String },
    #[error("node `{node_id}` has a payload incompatible with its kind: {message}")]
    InvalidNodePayload { node_id: String, message: String },
    #[error("a body's form does not fit its nodes: {message}")]
    InvalidBodyForm { message: String },
    /// A process's origin is derived at admission, never authored: a declared
    /// process cannot take a lifted name, and a lifted one must still be
    /// carried by the literal it was lifted from or by a reference to it.
    #[error("process `{name}` has an origin its program does not derive: {message}")]
    ProcessOriginMismatch { name: String, message: String },
}

impl WorkflowGraphError {
    /// A stable identifier for the refusal, independent of its message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedIrVersion(_) => "unsupported_ir_version",
            Self::DuplicateNodeId { .. } => "duplicate_node_id",
            Self::DuplicateProcessName { .. } => "duplicate_process_name",
            Self::InvalidNodePayload { .. } => "invalid_node_payload",
            Self::InvalidBodyForm { .. } => "invalid_body_form",
            Self::ProcessOriginMismatch { .. } => "process_origin_mismatch",
        }
    }

    /// The node the refusal names, when it names one.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::DuplicateNodeId { id } => Some(id),
            Self::InvalidNodePayload { node_id, .. } => Some(node_id),
            Self::UnsupportedIrVersion(_)
            | Self::DuplicateProcessName { .. }
            | Self::InvalidBodyForm { .. }
            | Self::ProcessOriginMismatch { .. } => None,
        }
    }
}

/// Rebuilds the program a workflow document spells.
///
/// Only authoritative fields are read (see the module docs of
/// [`super`]): edges, ids, available variables, outputs, facets, execution
/// sites and the source identity never reach the result.
///
/// An admitted document rebuilds its artifact's program, lifted declarations
/// included. A draft that still holds a process literal inline rebuilds the
/// literal around the body of the process container the projector lifted it
/// to, which is where a host edits that body.
///
/// A lifted process's name is reserved to the linker, so a variable that
/// reads one can only be the reference to it: a host that carried the
/// expression holding the reference as text gets the reference back.
pub fn workflow_program_from_graph(graph: &WorkflowGraph) -> Result<Program, WorkflowGraphError> {
    let reconstruction = reconstruct(graph)?;
    if let Some(name) = reconstruction.orphaned.into_iter().next() {
        return Err(WorkflowGraphError::ProcessOriginMismatch {
            name,
            message: "no process literal or reference in the program carries it".to_string(),
        });
    }
    Ok(reconstruction.program)
}

/// A document's program together with how its lifted process containers
/// reached it.
pub(super) struct Reconstruction {
    pub(super) program: Program,
    /// Each lifted container a literal of the program carries, with the
    /// [`Expr::children`] path of that literal from `main`.
    pub(super) carried: Vec<(String, Vec<u32>)>,
    /// The lifted containers no literal and no reference carries, which the
    /// program therefore does not hold.
    pub(super) orphaned: Vec<String>,
}

/// [`workflow_program_from_graph`], reporting the lifted containers the
/// program leaves out instead of refusing them: an edit that removes a
/// literal removes its container with it.
pub(super) fn reconstruct(graph: &WorkflowGraph) -> Result<Reconstruction, WorkflowGraphError> {
    WorkflowGraph::admit_ir_version(graph.ir_version)?;
    check_identities(graph)?;
    let lifted = graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) if process.origin.is_lifted() => {
                Some(process.name.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut main = body_expression(&graph.main)?;
    resolve_lifted_references(&mut main, &lifted);
    let mut bodies = Vec::new();
    for declaration in &graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            check_origin(process)?;
            let mut body = process_body(process)?;
            resolve_lifted_references(&mut body, &lifted);
            bodies.push(Some(body));
        } else {
            bodies.push(None);
        }
    }
    let mut referenced = BTreeSet::new();
    collect_process_references(&main, &mut referenced);
    for body in bodies.iter().flatten() {
        collect_process_references(body, &mut referenced);
    }
    let mut declarations = Vec::with_capacity(graph.declarations.len());
    let mut carried = CarriedProcesses::default();
    for (declaration, body) in graph.declarations.iter().zip(bodies) {
        match (declaration, body) {
            (WorkflowDeclaration::Function(function), _) => {
                declarations.push(Declaration::Function(function.clone()));
            }
            // A lifted process no reference names is not a declaration of the
            // program: it is the body of a literal the draft still holds
            // inline, and goes back into that literal.
            (WorkflowDeclaration::Process(process), Some(_))
                if process.origin.is_lifted() && !referenced.contains(process.name.as_str()) =>
            {
                carried.by_name.insert(process.name.clone(), process);
            }
            (WorkflowDeclaration::Process(process), Some(body)) => {
                let label =
                    (process.name_source == WorkflowNodeNameSource::Label).then(|| LabelMetadata {
                        title: process.display_name.clone().into(),
                        description: process.description.clone().map(Into::into),
                    });
                declarations.push(Declaration::Process(ProcessDecl {
                    name: process.name.clone().into(),
                    params: process.params.clone(),
                    return_ty: process.return_ty.clone(),
                    label,
                    origin: process.origin.clone(),
                    body,
                }));
            }
            (WorkflowDeclaration::Process(_), None) => {}
        }
    }
    splice_carried(&mut main, &mut Vec::new(), &mut Vec::new(), &mut carried)?;
    Ok(Reconstruction {
        program: Program {
            declarations,
            main,
            private_bindings: graph.private_bindings.clone(),
            spans: BTreeMap::new(),
        },
        carried: carried.claimed,
        orphaned: carried.by_name.into_keys().collect(),
    })
}

/// The statement a node spells, label included: the root every
/// [`super::WorkflowSlotPath`] of [`crate::ExprSlot`] segments is read from.
pub fn workflow_node_statement(node: &WorkflowNode) -> Result<Expr, WorkflowGraphError> {
    let invalid = |message: &str| WorkflowGraphError::InvalidNodePayload {
        node_id: node.id.to_string(),
        message: message.to_string(),
    };
    let expression = match &node.kind {
        WorkflowNodeKind::Data {
            binding,
            expression,
        }
        | WorkflowNodeKind::Computation {
            binding,
            expression,
        } => assigned(binding, expression.clone()),
        WorkflowNodeKind::Call {
            binding,
            receiver,
            operation,
            arguments,
            result_steps,
        } => assigned(
            binding,
            workflow_call_to_ir(receiver, operation, arguments, result_steps),
        ),
        WorkflowNodeKind::Effect {
            binding,
            effect,
            arguments,
            result_steps,
        } => assigned(
            binding,
            workflow_effect_to_ir(*effect, arguments, result_steps)
                .ok_or_else(|| invalid("effect arguments do not match its kind"))?,
        ),
        WorkflowNodeKind::StateUpdate {
            target,
            expression,
            update,
            pinned: None,
        } => {
            if update.is_some() {
                return Err(invalid(
                    "a compound update pins the member it updates and names its slots",
                ));
            }
            Expr::Assign {
                target: target.clone(),
                expr: Box::new(expression.clone()),
            }
        }
        WorkflowNodeKind::StateUpdate {
            target,
            expression,
            update,
            pinned: Some(pinned),
        } => {
            let member = || invalid("a pinned update's target is one member step of a variable");
            let [step] = target.steps.as_slice() else {
                return Err(member());
            };
            let (field, key) = match (step, &pinned.key) {
                (AssignPathStep::Field(field), None) => (Some(field.clone()), None),
                (AssignPathStep::Index(index), Some(key)) => {
                    (None, Some((key.clone(), index.clone())))
                }
                _ => return Err(member()),
            };
            let value = match update {
                None => expression.clone(),
                Some(operator) => AttributeAssignParts::update_value(
                    &pinned.base,
                    pinned.key.as_ref(),
                    field.as_ref(),
                    *operator,
                    expression.clone(),
                )
                .ok_or_else(member)?,
            };
            AttributeAssignParts::build(
                pinned.base.clone(),
                key,
                pinned.result.clone(),
                Expr::Variable(target.root.clone()),
                field,
                value,
            )
            .ok_or_else(member)?
        }
        WorkflowNodeKind::Terminal {
            terminal,
            expression,
        } => {
            let fits = matches!(
                (terminal, expression),
                (
                    WorkflowTerminalKind::Finish,
                    Expr::Finish(_) | Expr::FunctionReturn(_)
                ) | (WorkflowTerminalKind::Fail, Expr::Fail(_))
            );
            if !fits {
                return Err(invalid("terminal kind does not match its expression"));
            }
            expression.clone()
        }
        WorkflowNodeKind::Throw { value } => Expr::Throw(Box::new(value.clone())),
        WorkflowNodeKind::Container(container) => container_expression(container)?,
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

fn container_expression(container: &WorkflowContainer) -> Result<Expr, WorkflowGraphError> {
    let value = match container {
        WorkflowContainer::If {
            condition,
            then_graph,
            else_graph,
            ..
        } => Expr::If {
            condition: Box::new(condition.clone()),
            then_block: Box::new(body_expression(then_graph)?),
            else_block: Box::new(body_expression(else_graph)?),
        },
        WorkflowContainer::For {
            element,
            authored_element,
            iterable,
            bind,
            body,
            ..
        } => Expr::For {
            binding: element.clone().into(),
            authored_binding: authored_element.clone().map(Into::into),
            iterable: Box::new(iterable.clone()),
            bind: bind.clone().map(Box::new),
            body: Box::new(body_expression(body)?),
        },
        WorkflowContainer::While {
            condition, body, ..
        } => Expr::While {
            condition: Box::new(condition.clone()),
            body: Box::new(body_expression(body)?),
        },
        WorkflowContainer::Try {
            body,
            catch,
            finally,
            ..
        } => Expr::Try(Box::new(TryExpr {
            body: Box::new(body_expression(body)?),
            catch: catch
                .as_ref()
                .map(|catch| {
                    Ok::<_, WorkflowGraphError>(CatchClause {
                        binding: catch.binding.clone().into(),
                        body: Box::new(body_expression(&catch.body)?),
                    })
                })
                .transpose()?,
            finally: finally
                .as_ref()
                .map(|finally| body_expression(finally).map(Box::new))
                .transpose()?,
        })),
        WorkflowContainer::Scope { body, .. } => Expr::Role {
            role: StructuralRole::Scope,
            expr: Box::new(body_expression(body)?),
        },
    };
    Ok(match container.binding() {
        Some(target) => Expr::Assign {
            target: target.clone(),
            expr: Box::new(value),
        },
        None => value,
    })
}

fn assigned(binding: &Option<AssignTarget>, expression: Expr) -> Expr {
    match binding {
        Some(target) => Expr::Assign {
            target: target.clone(),
            expr: Box::new(expression),
        },
        None => expression,
    }
}

fn body_expression(graph: &WorkflowSubgraph) -> Result<Expr, WorkflowGraphError> {
    let statements = graph
        .nodes
        .iter()
        .map(workflow_node_statement)
        .collect::<Result<Vec<_>, _>>()?;
    graph.form.body(statements)
}

/// A process's whole body: its run body inside the failure wrapper, when it
/// has one.
fn process_body(process: &WorkflowProcess) -> Result<Expr, WorkflowGraphError> {
    let body = body_expression(&process.body)?;
    Ok(match &process.wrapper {
        None => body,
        Some(wrapper) => ProcessWrapperParts::build(
            FunctionExpr {
                name: wrapper.name.clone(),
                js_name: wrapper.js_name.clone(),
                receiver: wrapper.receiver.clone(),
                params: wrapper.params.clone(),
                captures: wrapper.captures.clone(),
                body: Box::new(body),
            },
            wrapper
                .driver
                .as_ref()
                .map(|driver| (driver.builtin.clone(), driver.arguments.clone())),
            wrapper.arguments.clone(),
            wrapper.catch_binding.clone(),
        ),
    })
}

fn check_identities(graph: &WorkflowGraph) -> Result<(), WorkflowGraphError> {
    fn subgraph(
        graph: &WorkflowSubgraph,
        ids: &mut BTreeSet<WorkflowNodeId>,
    ) -> Result<(), WorkflowGraphError> {
        for node in &graph.nodes {
            if !ids.insert(node.id.clone()) {
                return Err(WorkflowGraphError::DuplicateNodeId {
                    id: node.id.to_string(),
                });
            }
            if let WorkflowNodeKind::Container(container) = &node.kind {
                for (_, child) in container.child_subgraphs() {
                    subgraph(child, ids)?;
                }
            }
        }
        Ok(())
    }
    let mut ids = BTreeSet::new();
    subgraph(&graph.main, &mut ids)?;
    let mut names = BTreeSet::new();
    for declaration in &graph.declarations {
        let WorkflowDeclaration::Process(process) = declaration else {
            continue;
        };
        if !names.insert(process.name.as_str()) {
            return Err(WorkflowGraphError::DuplicateProcessName {
                name: process.name.clone(),
            });
        }
        if !ids.insert(process.id.clone()) {
            return Err(WorkflowGraphError::DuplicateNodeId {
                id: process.id.to_string(),
            });
        }
        subgraph(&process.body, &mut ids)?;
    }
    Ok(())
}

/// The origin checks a process's own fields decide.
fn check_origin(process: &WorkflowProcess) -> Result<(), WorkflowGraphError> {
    let lifted_name = process.name.starts_with(crate::LIFTED_PROCESS_NAME_PREFIX);
    let message = match &process.origin {
        ProcessOrigin::Declared if lifted_name => {
            "a declared process cannot take a lifted process's name"
        }
        ProcessOrigin::Lifted { .. } if !lifted_name => "a lifted process is named by a digest",
        ProcessOrigin::Lifted { hidden_params, .. }
            if *hidden_params as usize > process.params.len() =>
        {
            "a lifted process has more hidden parameters than parameters"
        }
        _ => return Ok(()),
    };
    Err(WorkflowGraphError::ProcessOriginMismatch {
        name: process.name.clone(),
        message: message.to_string(),
    })
}

fn resolve_lifted_references(expression: &mut Expr, lifted: &BTreeSet<&str>) {
    if lifted.is_empty() {
        return;
    }
    if let Expr::Variable(name) = expression
        && lifted.contains(name.as_str())
    {
        *expression = Expr::ProcessRef {
            process: name.clone(),
        };
        return;
    }
    for child in expression.children_mut() {
        resolve_lifted_references(child, lifted);
    }
}

fn collect_process_references(expression: &Expr, names: &mut BTreeSet<String>) {
    let mut pending = vec![expression];
    while let Some(expression) = pending.pop() {
        if let Expr::ProcessRef { process } = expression {
            names.insert(process.to_string());
        }
        pending.extend(expression.children());
    }
}

/// The lifted process containers of a draft that no reference names: each is
/// the body of a literal the program still holds inline.
#[derive(Default)]
struct CarriedProcesses<'a> {
    by_name: BTreeMap<String, &'a WorkflowProcess>,
    /// The containers taken so far, each with its literal's path.
    claimed: Vec<(String, Vec<u32>)>,
}

impl<'a> CarriedProcesses<'a> {
    /// The container `literal` carries: one whose name `literal` digests to
    /// at the container's own site, preferring the one lifted at the
    /// literal's current position.
    ///
    /// Which literal a container belongs to is derived, never read from an
    /// authored position (FIG-3571). A container's name is the digest of the
    /// literal it was lifted from together with the site it was lifted at,
    /// so a literal carries the container exactly when it still digests to
    /// that name at that site, wherever the literal sits now.
    fn take_for_literal(
        &mut self,
        literal: &ProcessLiteralExpr,
        path: &[u32],
        unlabelled: &[u32],
    ) -> Result<Option<&'a WorkflowProcess>, WorkflowGraphError> {
        let candidates = self
            .by_name
            .values()
            .filter_map(|process| match &process.origin {
                ProcessOrigin::Lifted { site, .. }
                    if crate::lifted_process_identity(&literal.body, &site.steps)
                        == process.name =>
                {
                    Some((site.steps.as_slice(), process.name.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let name = match candidates
            .iter()
            .find(|(site, _)| *site == path || *site == unlabelled)
        {
            Some((_, name)) => (*name).to_string(),
            None => match candidates.as_slice() {
                [] => return Ok(None),
                [(_, name)] => (*name).to_string(),
                [(_, name), ..] => {
                    return Err(WorkflowGraphError::ProcessOriginMismatch {
                        name: (*name).to_string(),
                        message: "more than one lifted process could claim a moved literal"
                            .to_string(),
                    });
                }
            },
        };
        Ok(self.by_name.remove(&name))
    }
}

/// Puts each carried container's body back into the literal that carries it.
///
/// The projector shows an inline process literal twice: the statement that
/// holds it carries the literal, and its body is a process container so the
/// statements inside are nodes. The container is where that body is edited,
/// so it is the authority, and the literal's own copy is replaced by it.
fn splice_carried(
    expression: &mut Expr,
    path: &mut Vec<u32>,
    unlabelled: &mut Vec<u32>,
    carried: &mut CarriedProcesses<'_>,
) -> Result<(), WorkflowGraphError> {
    if carried.by_name.is_empty() {
        return Ok(());
    }
    if let Expr::ProcessLiteral(literal) = expression
        && let Some(process) = carried.take_for_literal(literal, path, unlabelled)?
    {
        carried.claimed.push((process.name.clone(), path.clone()));
        let (hidden, return_ty) = match &process.origin {
            ProcessOrigin::Lifted {
                hidden_params,
                declared_return_ty,
                ..
            } => (*hidden_params as usize, declared_return_ty.clone()),
            ProcessOrigin::Declared => (0, None),
        };
        let (params, hidden_args) = process
            .params
            .split_at(process.params.len().saturating_sub(hidden));
        literal.params = params.to_vec();
        literal.hidden_args = hidden_args.to_vec();
        literal.return_ty = return_ty;
        *literal.body = process_body(process)?;
    }
    let label = matches!(expression, Expr::LabelAnnotated { .. });
    for (index, child) in (0u32..).zip(expression.children_mut()) {
        path.push(index);
        if !label {
            unlabelled.push(index);
        }
        splice_carried(child, path, unlabelled, carried)?;
        path.pop();
        if !label {
            unlabelled.pop();
        }
    }
    Ok(())
}
