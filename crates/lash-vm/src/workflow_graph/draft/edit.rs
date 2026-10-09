//! The typed edits of a draft and how each changes the document.
//!
//! Every authoritative field of the document has an edit that changes it:
//! statements are inserted, cloned, removed, moved and replaced; any
//! expression of any statement is replaced through its slot path; headers,
//! labels, bindings, body layouts, declarations and process wrappers each have
//! their own. An edit states its subject by handle and its content as typed
//! IR, and never as text.

use std::collections::BTreeSet;

use crate::ast::{
    AssignPathStep, AssignTarget, AstString, Declaration, Expr, ExprSlot, FunctionDecl,
    LabelMetadata, ProcessParam, TypeExpr,
};
use crate::workflow_graph::{
    WorkflowBodyItem, WorkflowBodyShape, WorkflowCatch, WorkflowContainer, WorkflowDeclaration,
    WorkflowGraph, WorkflowMemberStep, WorkflowNode, WorkflowNodeId, WorkflowNodeKind,
    WorkflowProcess, WorkflowProcessWrapper, WorkflowSlotPath, WorkflowStateWrite,
    WorkflowSubgraph, workflow_node_statement,
};

use super::scope::{self, FrameRoot, Lexical, Role};
use super::{
    Ambient, Owner, Refused, State, WorkflowBodyRef, WorkflowDraftHandle,
    WorkflowEditDiagnosticKind as Kind, WorkflowEditLocation, WorkflowNodeSource, body_slots,
    child_body, child_body_mut, children, expr_at,
};

/// A variable of a draft, named where the document binds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowBindingRef {
    /// The variable `name` as the node `at` reads it, or as the process
    /// container `at` declares it: a binding of a statement, a loop element,
    /// a catch binding or a parameter.
    Variable {
        at: WorkflowDraftHandle,
        name: AstString,
    },
    /// A parameter or local of the function value or process literal at
    /// `function`, a slot path from the statement of `node`.
    Nested {
        node: WorkflowDraftHandle,
        function: WorkflowSlotPath,
        name: AstString,
    },
    /// A parameter or local of a declared function.
    Function {
        function: AstString,
        name: AstString,
    },
}

/// An edge a host dragged between two nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowEdgeDrag {
    /// `to` should run right after `from`.
    Sequence {
        from: WorkflowDraftHandle,
        to: WorkflowDraftHandle,
    },
    /// The expression of `to` at `slot` should read what `from` binds.
    Data {
        from: WorkflowDraftHandle,
        to: WorkflowDraftHandle,
        slot: WorkflowSlotPath,
    },
}

/// The expression root a slot path addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowExpressionRef {
    /// A node's statement, outside the child bodies that have their own nodes.
    Node(WorkflowDraftHandle),
    /// The body expression of a declared function.
    Function(AstString),
    /// A process wrapper's run call. `Arg(i)` addresses a run argument;
    /// `Callee/Arg(i + 1)` addresses a driver argument (argument zero is the
    /// run function). The run body is edited through its own nodes.
    ProcessWrapper(WorkflowDraftHandle),
}

impl WorkflowExpressionRef {
    pub(super) fn location(&self, slot: WorkflowSlotPath) -> WorkflowEditLocation {
        match self {
            Self::Node(node) => WorkflowEditLocation::Node { node: *node, slot },
            Self::Function(name) => WorkflowEditLocation::Function {
                name: name.clone(),
                slot,
            },
            Self::ProcessWrapper(process) => WorkflowEditLocation::ProcessWrapper {
                process: *process,
                slot,
            },
        }
    }
}

/// How a body arranges its statements: which runs of them a completion value
/// closes as a group, and what closes the body itself.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowBodyLayout {
    /// A statement list, closed by `completion` when it has one.
    List {
        items: Vec<WorkflowBodyLayoutItem>,
        completion: Option<Expr>,
    },
    /// The body is its one statement, with no list around it.
    Statement { node: WorkflowDraftHandle },
}

/// One entry of a [`WorkflowBodyLayout::List`].
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowBodyLayoutItem {
    Node(WorkflowDraftHandle),
    /// The statements of `items` as one nested list closed by `value`.
    Group {
        items: Vec<WorkflowBodyLayoutItem>,
        value: Expr,
    },
}

impl WorkflowBodyLayout {
    /// The statements the layout names, in order.
    fn handles(&self) -> Vec<WorkflowDraftHandle> {
        fn collect(items: &[WorkflowBodyLayoutItem], out: &mut Vec<WorkflowDraftHandle>) {
            for item in items {
                match item {
                    WorkflowBodyLayoutItem::Node(node) => out.push(*node),
                    WorkflowBodyLayoutItem::Group { items, .. } => collect(items, out),
                }
            }
        }
        let mut out = Vec::new();
        match self {
            Self::List { items, .. } => collect(items, &mut out),
            Self::Statement { node } => out.push(*node),
        }
        out
    }

    /// The shape that arranges `nodes`, the layout's statements in order.
    fn shape(self, nodes: &mut impl Iterator<Item = WorkflowNode>) -> WorkflowBodyShape {
        fn arrange(
            items: Vec<WorkflowBodyLayoutItem>,
            nodes: &mut impl Iterator<Item = WorkflowNode>,
        ) -> Vec<WorkflowBodyItem> {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    WorkflowBodyLayoutItem::Node(_) => {
                        out.extend(
                            nodes
                                .next()
                                .map(|node| WorkflowBodyItem::Node(Box::new(node))),
                        );
                    }
                    WorkflowBodyLayoutItem::Group { items, value } => {
                        out.push(WorkflowBodyItem::Group {
                            items: arrange(items, nodes),
                            value,
                        });
                    }
                }
            }
            out
        }
        match self {
            Self::List { items, completion } => WorkflowBodyShape::List {
                items: arrange(items, nodes),
                completion: completion.map(Box::new),
            },
            Self::Statement { .. } => match nodes.next() {
                Some(node) => WorkflowBodyShape::Statement {
                    node: Box::new(node),
                },
                None => WorkflowBodyShape::default(),
            },
        }
    }
}

/// One typed change to a draft.
///
/// A statement is given as the IR expression it is, label included
/// ([`workflow_node_statement`] reads one back); `before` places it ahead of
/// that statement of the body, or at the body's end.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WorkflowEdit {
    InsertNode {
        body: WorkflowBodyRef,
        before: Option<WorkflowDraftHandle>,
        statement: Expr,
    },
    /// Copies a node and everything under it; the copy has handles of its
    /// own.
    CloneNode {
        node: WorkflowDraftHandle,
        body: WorkflowBodyRef,
        before: Option<WorkflowDraftHandle>,
    },
    RemoveNode {
        node: WorkflowDraftHandle,
    },
    MoveNode {
        node: WorkflowDraftHandle,
        body: WorkflowBodyRef,
        before: Option<WorkflowDraftHandle>,
    },
    /// Replaces a node's whole statement. The node keeps its handle; the
    /// statements of its former child bodies are deleted.
    ReplaceNode {
        node: WorkflowDraftHandle,
        statement: Expr,
    },
    /// Replaces the expression at `slot` from `target`. Node paths must be
    /// non-empty and stay outside child bodies; a function's empty path
    /// addresses its body itself. Wrapper paths address run or driver arguments.
    ReplaceExpression {
        target: WorkflowExpressionRef,
        slot: WorkflowSlotPath,
        expression: Expr,
    },
    /// Sets or clears the target a node assigns its value to.
    SetBinding {
        node: WorkflowDraftHandle,
        binding: Option<AssignTarget>,
    },
    /// Renames a variable in its binders and in every use that resolves to
    /// it.
    RenameBinding {
        binding: WorkflowBindingRef,
        name: AstString,
    },
    /// Sets the condition of an `if` or a `while`.
    SetCondition {
        node: WorkflowDraftHandle,
        condition: Expr,
    },
    /// Sets or clears the authored label of a node or a process container.
    SetLabel {
        target: WorkflowDraftHandle,
        label: Option<LabelMetadata>,
    },
    /// Sets how a loop binds each element.
    SetLoopBinding {
        node: WorkflowDraftHandle,
        element: AstString,
        authored_element: Option<AstString>,
        bind: Option<Expr>,
    },
    /// Gives a `try` a catch clause binding the thrown value, renames the
    /// binding of the one it has, or removes the clause with its body.
    SetCatch {
        node: WorkflowDraftHandle,
        binding: Option<AstString>,
    },
    /// Gives a `try` an empty `finally` body, or removes the one it has.
    SetFinally {
        node: WorkflowDraftHandle,
        present: bool,
    },
    /// Sets how a body arranges its statements: its completion value and
    /// its completion groups. The layout names exactly the body's
    /// statements, in their order; [`WorkflowEdit::MoveNode`] reorders them.
    SetBodyLayout {
        body: WorkflowBodyRef,
        layout: WorkflowBodyLayout,
    },
    /// Declares a process with an empty body.
    InsertProcess {
        name: AstString,
        params: Vec<ProcessParam>,
        return_ty: Option<TypeExpr>,
    },
    /// Removes a declared process. A lifted process is refused as derived:
    /// it goes when the literal or the last reference to it does.
    RemoveProcess {
        process: WorkflowDraftHandle,
    },
    /// Renames a declared process and every reference to it.
    RenameProcess {
        process: WorkflowDraftHandle,
        name: AstString,
    },
    /// Sets a process's authored parameters and declared output. A lifted
    /// process keeps its captures after them. A failure wrapper that passes
    /// the parameters straight through is rewritten to pass the new ones.
    SetProcessSignature {
        process: WorkflowDraftHandle,
        params: Vec<ProcessParam>,
        return_ty: Option<TypeExpr>,
    },
    SetProcessWrapper {
        process: WorkflowDraftHandle,
        wrapper: Option<WorkflowProcessWrapper>,
    },
    InsertFunction {
        function: FunctionDecl,
    },
    /// Replaces the declared function of the same name.
    ReplaceFunction {
        function: FunctionDecl,
    },
    RemoveFunction {
        name: AstString,
    },
    SetPrivateBindings {
        bindings: BTreeSet<AstString>,
    },
}

fn at_node(node: WorkflowDraftHandle) -> WorkflowEditLocation {
    WorkflowEditLocation::Node {
        node,
        slot: WorkflowSlotPath::default(),
    }
}

fn at_body(body: &WorkflowBodyRef) -> WorkflowEditLocation {
    match body {
        WorkflowBodyRef::Main => WorkflowEditLocation::Document,
        WorkflowBodyRef::Process(process) => WorkflowEditLocation::Process { process: *process },
        WorkflowBodyRef::Child { node, .. } => at_node(*node),
    }
}

fn at_process(process: WorkflowDraftHandle) -> WorkflowEditLocation {
    WorkflowEditLocation::Process { process }
}

fn unknown(handle: WorkflowDraftHandle) -> Refused {
    (
        WorkflowEditLocation::Document,
        Kind::UnknownHandle { handle },
    )
}

impl State {
    pub(super) fn edit(&mut self, edit: WorkflowEdit, ambient: &Ambient) -> Result<(), Refused> {
        match edit {
            WorkflowEdit::InsertNode {
                body,
                before,
                statement,
            } => {
                let handle = self.mint(WorkflowNodeSource::Authored);
                self.insert(&body, before, statement_node(handle, statement))?;
                self.settled(at_body(&body))
            }
            WorkflowEdit::CloneNode { node, body, before } => {
                let mut copy = node_ref(&self.working, node)
                    .ok_or_else(|| unknown(node))?
                    .clone();
                self.rehandle(&mut copy);
                self.insert(&body, before, copy)?;
                self.settled(at_body(&body))
            }
            WorkflowEdit::RemoveNode { node } => {
                let removed = self.take(node)?;
                self.forget(&removed, false);
                self.settled(WorkflowEditLocation::Document)
            }
            WorkflowEdit::MoveNode { node, body, before } => {
                let subject = node_ref(&self.working, node).ok_or_else(|| unknown(node))?;
                if let WorkflowBodyRef::Child { node: parent, .. } = &body
                    && node_ref_in(subject, *parent).is_some()
                {
                    return Err((at_node(node), Kind::MoveIntoOwnSubtree));
                }
                if before == Some(node) {
                    return Err((at_node(node), Kind::AnchorOutsideBody { anchor: node }));
                }
                let (source, _) = parent_of(&self.working, node).ok_or_else(|| unknown(node))?;
                let reframed = root_of(&self.working, &source) != root_of(&self.working, &body);
                let moved = self.take(node)?;
                self.insert(&body, before, moved)?;
                self.journal.moved.insert(node);
                if reframed {
                    self.journal.reframed.insert(node);
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::ReplaceNode { node, statement } => {
                let replaced = node_mut(&mut self.working, node).ok_or_else(|| unknown(node))?;
                let former = std::mem::replace(replaced, statement_node(node, statement));
                for (_, child) in children(&former) {
                    for node in child.nodes() {
                        self.forget(node, false);
                    }
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::ReplaceExpression {
                target,
                slot,
                expression,
            } => {
                let location = target.location(slot.clone());
                let refuse = |kind| (location.clone(), kind);
                let slots = slot.slots();
                match target {
                    WorkflowExpressionRef::Node(node) => {
                        if slots.is_empty() {
                            return Err(refuse(Kind::UnknownSlot));
                        }
                        let subject = node_mut(&mut self.working, node)
                            .ok_or_else(|| refuse(Kind::UnknownHandle { handle: node }))?;
                        replace_expression(subject, slots, expression).map_err(refuse)?;
                    }
                    WorkflowExpressionRef::Function(name) => {
                        let function = self
                            .working
                            .declarations
                            .iter_mut()
                            .find_map(|declaration| match declaration {
                                WorkflowDeclaration::Function(function)
                                    if function.name == name =>
                                {
                                    Some(function)
                                }
                                _ => None,
                            })
                            .ok_or_else(|| refuse(Kind::UnknownFunction { name }))?;
                        *function
                            .body
                            .at_slots_mut(slots)
                            .ok_or_else(|| refuse(Kind::UnknownSlot))? = expression;
                    }
                    WorkflowExpressionRef::ProcessWrapper(process) => {
                        let subject = process_mut(&mut self.working, process)
                            .ok_or_else(|| refuse(Kind::UnknownHandle { handle: process }))?;
                        let wrapper = subject.wrapper.as_mut().ok_or_else(|| {
                            refuse(Kind::EditDoesNotApply {
                                expected: "a process with a failure wrapper",
                            })
                        })?;
                        *wrapper_argument_mut(wrapper, slots).map_err(refuse)? = expression;
                    }
                }
                self.journal.expression_edits.push(location.clone());
                self.settled(location)
            }
            WorkflowEdit::SetBinding { node, binding } => {
                let subject = node_mut(&mut self.working, node).ok_or_else(|| unknown(node))?;
                set_binding(subject, binding).map_err(|kind| (at_node(node), kind))?;
                self.settled(at_node(node))
            }
            WorkflowEdit::RenameBinding { binding, name } => {
                self.rename_binding(binding, name, ambient)
            }
            WorkflowEdit::SetCondition { node, condition } => {
                match container_mut(&mut self.working, node)? {
                    WorkflowContainer::If {
                        condition: current, ..
                    }
                    | WorkflowContainer::While {
                        condition: current, ..
                    } => *current = condition,
                    _ => {
                        return Err((
                            at_node(node),
                            Kind::EditDoesNotApply {
                                expected: "an `if` or a `while`",
                            },
                        ));
                    }
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::SetLabel { target, label } => {
                if let Some(process) = process_mut(&mut self.working, target) {
                    process.label = label;
                    return self.settled(at_process(target));
                }
                node_mut(&mut self.working, target)
                    .ok_or_else(|| unknown(target))?
                    .label = label;
                self.settled(at_node(target))
            }
            WorkflowEdit::SetLoopBinding {
                node,
                element,
                authored_element,
                bind,
            } => {
                let WorkflowContainer::For {
                    element: current,
                    authored_element: authored,
                    bind: current_bind,
                    ..
                } = container_mut(&mut self.working, node)?
                else {
                    return Err((
                        at_node(node),
                        Kind::EditDoesNotApply {
                            expected: "a `for` loop",
                        },
                    ));
                };
                *current = element.to_string();
                *authored = authored_element.map(|name| name.to_string());
                *current_bind = bind;
                self.settled(at_node(node))
            }
            WorkflowEdit::SetCatch { node, binding } => {
                let WorkflowContainer::Try { catch, .. } = container_mut(&mut self.working, node)?
                else {
                    return Err((
                        at_node(node),
                        Kind::EditDoesNotApply {
                            expected: "a `try`",
                        },
                    ));
                };
                let removed = match binding {
                    Some(binding) => {
                        match catch {
                            Some(catch) => catch.binding = binding.to_string(),
                            None => {
                                *catch = Some(WorkflowCatch {
                                    binding: binding.to_string(),
                                    body: Box::default(),
                                });
                            }
                        }
                        None
                    }
                    None => catch.take().map(|catch| catch.body),
                };
                for node in removed.iter().flat_map(|body| body.nodes()) {
                    self.forget(node, false);
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::SetFinally { node, present } => {
                let WorkflowContainer::Try { finally, .. } =
                    container_mut(&mut self.working, node)?
                else {
                    return Err((
                        at_node(node),
                        Kind::EditDoesNotApply {
                            expected: "a `try`",
                        },
                    ));
                };
                let removed = if present {
                    finally.get_or_insert_with(Box::default);
                    None
                } else {
                    finally.take()
                };
                for node in removed.iter().flat_map(|body| body.nodes()) {
                    self.forget(node, false);
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::SetBodyLayout { body, layout } => {
                let target = body_mut(&mut self.working, &body)
                    .ok_or((at_body(&body), Kind::UnknownBody))?;
                let named = layout.handles();
                let held = target.nodes();
                if named.len() != held.len()
                    || named
                        .iter()
                        .zip(held)
                        .any(|(named, held)| held.id != named.id())
                {
                    return Err((
                        at_body(&body),
                        Kind::EditDoesNotApply {
                            expected: "a layout of exactly the body's statements, in their order",
                        },
                    ));
                }
                let nodes = std::mem::take(&mut target.body).into_nodes();
                target.body = layout.shape(&mut nodes.into_iter());
                self.settled(at_body(&body))
            }
            WorkflowEdit::InsertProcess {
                name,
                params,
                return_ty,
            } => {
                let handle = self.mint(WorkflowNodeSource::Authored);
                self.working
                    .declarations
                    .push(WorkflowDeclaration::Process(WorkflowProcess {
                        id: handle.id(),
                        name: name.to_string(),
                        label: None,
                        params,
                        return_ty,
                        origin: crate::ProcessOrigin::Declared,
                        wrapper: None,
                        body: WorkflowSubgraph::default(),
                    }));
                self.settled(at_process(handle))
            }
            WorkflowEdit::RemoveProcess { process } => {
                let declared =
                    process_mut(&mut self.working, process).ok_or_else(|| unknown(process))?;
                // A lifted container follows its literal or the references
                // to it: removing those is what removes it.
                if declared.origin.is_lifted() {
                    return Err((at_process(process), Kind::DerivedProcess));
                }
                let id = process.id();
                self.working.declarations.retain(|declaration| {
                    !matches!(declaration, WorkflowDeclaration::Process(declared) if declared.id == id)
                });
                self.settled(WorkflowEditLocation::Document)
            }
            WorkflowEdit::RenameProcess { process, name } => {
                let declared =
                    process_mut(&mut self.working, process).ok_or_else(|| unknown(process))?;
                if declared.origin.is_lifted() {
                    return Err((at_process(process), Kind::DerivedProcess));
                }
                let former = AstString::from(declared.name.as_str());
                if self
                    .working
                    .process(&name)
                    .is_some_and(|existing| existing.id != process.id())
                {
                    return Err((
                        at_process(process),
                        Kind::InvalidProgram(crate::InvalidAst::DuplicateDeclaration {
                            name: name.to_string(),
                        }),
                    ));
                }
                let declared =
                    process_mut(&mut self.working, process).ok_or_else(|| unknown(process))?;
                declared.name = name.to_string();
                let mut program = self.program.clone();
                scope::rename_process(&mut program, &former, &name);
                self.settle_program(program, self.carried.clone())
                    .map_err(|error| (at_process(process), Kind::InvalidProgram(error)))
            }
            WorkflowEdit::SetProcessSignature {
                process,
                mut params,
                return_ty,
            } => {
                let declared =
                    process_mut(&mut self.working, process).ok_or_else(|| unknown(process))?;
                // A lifted process's captures follow its authored
                // parameters. They are derived, so the edit keeps them.
                if let crate::ProcessOrigin::Lifted { hidden_params, .. } = &declared.origin {
                    let authored = declared
                        .params
                        .len()
                        .saturating_sub(*hidden_params as usize);
                    params.extend_from_slice(&declared.params[authored..]);
                }
                if let Some(wrapper) = &mut declared.wrapper
                    && passes_params_through(wrapper, &declared.params)
                {
                    wrapper.params = params.iter().map(|param| param.name.clone()).collect();
                    wrapper.arguments = params
                        .iter()
                        .map(|param| Expr::Variable(param.name.clone()))
                        .collect();
                }
                if let crate::ProcessOrigin::Lifted {
                    declared_return_ty, ..
                } = &mut declared.origin
                {
                    declared_return_ty.clone_from(&return_ty);
                }
                declared.params = params;
                declared.return_ty = return_ty;
                self.settled(at_process(process))
            }
            WorkflowEdit::SetProcessWrapper { process, wrapper } => {
                process_mut(&mut self.working, process)
                    .ok_or_else(|| unknown(process))?
                    .wrapper = wrapper.map(Box::new);
                self.settled(at_process(process))
            }
            WorkflowEdit::InsertFunction { function } => {
                let name = function.name.clone();
                self.working
                    .declarations
                    .push(WorkflowDeclaration::Function(function));
                self.settled(WorkflowEditLocation::Function {
                    name,
                    slot: WorkflowSlotPath::default(),
                })
            }
            WorkflowEdit::ReplaceFunction { function } => {
                let name = function.name.clone();
                let location = WorkflowEditLocation::Function {
                    name: name.clone(),
                    slot: WorkflowSlotPath::default(),
                };
                let declared = self
                    .working
                    .declarations
                    .iter_mut()
                    .find_map(|declaration| match declaration {
                        WorkflowDeclaration::Function(declared) if declared.name == name => {
                            Some(declared)
                        }
                        _ => None,
                    })
                    .ok_or((location.clone(), Kind::UnknownFunction { name }))?;
                *declared = function;
                self.settled(location)
            }
            WorkflowEdit::RemoveFunction { name } => {
                let before = self.working.declarations.len();
                self.working.declarations.retain(|declaration| {
                    !matches!(declaration, WorkflowDeclaration::Function(declared) if declared.name == name)
                });
                if self.working.declarations.len() == before {
                    return Err((
                        WorkflowEditLocation::Function {
                            name: name.clone(),
                            slot: WorkflowSlotPath::default(),
                        },
                        Kind::UnknownFunction { name },
                    ));
                }
                self.settled(WorkflowEditLocation::Document)
            }
            WorkflowEdit::SetPrivateBindings { bindings } => {
                self.working.private_bindings = bindings;
                self.settled(WorkflowEditLocation::Document)
            }
        }
    }

    fn settled(&mut self, location: WorkflowEditLocation) -> Result<(), Refused> {
        self.settle().map_err(|kind| (location, kind))
    }

    /// Places `node` in `body` ahead of `before`, or at the body's end.
    fn insert(
        &mut self,
        body: &WorkflowBodyRef,
        before: Option<WorkflowDraftHandle>,
        node: WorkflowNode,
    ) -> Result<(), Refused> {
        let target = body_mut(&mut self.working, body).ok_or((at_body(body), Kind::UnknownBody))?;
        let node = WorkflowBodyItem::Node(Box::new(node));
        let items = target.body.items_mut();
        match before {
            None => items.push(node),
            Some(anchor) => place_before(items, &anchor.id(), node, true)
                .map_err(|_| (at_body(body), Kind::AnchorOutsideBody { anchor }))?,
        }
        Ok(())
    }

    /// Takes `node` out of its body. The group that held it keeps its
    /// completion value.
    fn take(&mut self, node: WorkflowDraftHandle) -> Result<WorkflowNode, Refused> {
        let (body, _) = parent_of(&self.working, node).ok_or_else(|| unknown(node))?;
        let body = body_mut(&mut self.working, &body).ok_or_else(|| unknown(node))?;
        take_from(body.body.items_mut(), &node.id()).ok_or_else(|| unknown(node))
    }

    /// Gives a copied node and everything under it handles of their own.
    fn rehandle(&mut self, node: &mut WorkflowNode) {
        if let Some(of) = WorkflowDraftHandle::of(&node.id) {
            node.id = self.mint(WorkflowNodeSource::Clone { of }).id();
        }
        if let WorkflowNodeKind::Container(container) = &mut node.kind {
            for (_, child) in container.child_subgraphs_mut() {
                for node in child.nodes_mut() {
                    self.rehandle(node);
                }
            }
        }
    }

    fn rename_binding(
        &mut self,
        binding: WorkflowBindingRef,
        to: AstString,
        ambient: &Ambient,
    ) -> Result<(), Refused> {
        let lexical = Lexical::of(&self.program);
        let (frame, from, location) = match binding {
            WorkflowBindingRef::Variable { at, name } => {
                let frame = match self.process_frame(at) {
                    Some(root) => lexical.frames.iter().position(|frame| frame.root == root),
                    None => self.address(at).and_then(|path| lexical.frame_at(path)),
                };
                let location = if self.process_frame(at).is_some() {
                    at_process(at)
                } else {
                    at_node(at)
                };
                (frame.ok_or_else(|| unknown(at))?, name, location)
            }
            WorkflowBindingRef::Nested {
                node,
                function,
                name,
            } => {
                let location = WorkflowEditLocation::Node {
                    node,
                    slot: function.clone(),
                };
                let path = self.address(node).ok_or_else(|| unknown(node))?;
                let root = expr_at(&self.program, path)
                    .and_then(|statement| statement.child_steps(function.slots()))
                    .map(|steps| {
                        let mut root = path.clone();
                        root.steps.extend(steps);
                        FrameRoot::Expr(root)
                    });
                let frame = root
                    .and_then(|root| lexical.frames.iter().position(|frame| frame.root == root))
                    .ok_or((location.clone(), Kind::UnknownSlot))?;
                (frame, name, location)
            }
            WorkflowBindingRef::Function { function, name } => {
                let location = WorkflowEditLocation::Function {
                    name: function.clone(),
                    slot: WorkflowSlotPath::default(),
                };
                let frame = (0u32..)
                    .zip(&self.program.declarations)
                    .find_map(|(index, declaration)| match declaration {
                        Declaration::Function(declared) if declared.name == function => {
                            Some(FrameRoot::Declaration(index))
                        }
                        _ => None,
                    })
                    .and_then(|root| lexical.frames.iter().position(|frame| frame.root == root))
                    .ok_or((location.clone(), Kind::UnknownFunction { name: function }))?;
                (frame, name, location)
            }
        };
        let frame = scope::owning_frame(&lexical, &self.program, frame, &from);
        let bound = lexical.frames[frame].params.contains(&from)
            || lexical.occurrences.iter().any(|occurrence| {
                occurrence.frame == frame
                    && occurrence.role == Role::Bind
                    && occurrence.name == from
            });
        if !bound {
            return Err((location, Kind::UnknownBinding { name: from }));
        }
        let declared = self.program.declarations.iter().any(|declaration| {
            matches!(declaration, Declaration::Function(function) if function.name == to)
        });
        let ambient_name = self
            .owner(lexical.top(frame))
            .and_then(|owner| ambient.get(&owner))
            .is_some_and(|names| names.contains(&to));
        let taken =
            declared
                || ambient_name
                || lexical.processes.contains(&to)
                || (*lexical.top(frame) != FrameRoot::Main
                    && matches!(to.as_str(), "input" | "inputs"))
                || lexical.frames.iter().enumerate().any(|(index, inner)| {
                    lexical.within(index, frame) && inner.params.contains(&to)
                })
                || lexical.occurrences.iter().any(|occurrence| {
                    occurrence.name == to
                        && occurrence.role != Role::Process
                        && lexical.within(occurrence.frame, frame)
                });
        if taken {
            return Err((location, Kind::BindingNameTaken { name: to }));
        }
        let mut program = self.program.clone();
        scope::rename(&mut program, &lexical.frames[frame].root, &from, &to);
        self.settle_program(program, self.carried.clone())
            .map_err(|error| (location, Kind::InvalidProgram(error)))
    }

    /// The frame of the process container `handle`, when it is one: its
    /// declaration, or the literal that carries it.
    fn process_frame(&self, handle: WorkflowDraftHandle) -> Option<FrameRoot> {
        let id = self.ids.get(&handle)?;
        let process = super::processes(&self.document).find(|process| process.id == *id)?;
        let declared = (0u32..)
            .zip(&self.program.declarations)
            .find_map(|(index, declaration)| match declaration {
                Declaration::Process(declared) if declared.name == process.name.as_str() => {
                    Some(FrameRoot::Declaration(index))
                }
                _ => None,
            });
        declared.or_else(|| match &process.origin {
            crate::ProcessOrigin::Lifted { site, .. } => Some(FrameRoot::Expr(site.clone())),
            crate::ProcessOrigin::Declared => None,
        })
    }

    pub(super) fn edge_drag(
        &self,
        drag: &super::WorkflowEdgeDrag,
    ) -> Result<WorkflowEdit, Refused> {
        match drag {
            super::WorkflowEdgeDrag::Sequence { from, to } => {
                let (body, index) =
                    parent_of(&self.working, *from).ok_or_else(|| unknown(*from))?;
                node_ref(&self.working, *to).ok_or_else(|| unknown(*to))?;
                let before = self::body(&self.working, &body).and_then(|body| {
                    body.nodes()
                        .into_iter()
                        .skip(index + 1)
                        .filter_map(|node| WorkflowDraftHandle::of(&node.id))
                        .find(|next| next != to)
                });
                Ok(WorkflowEdit::MoveNode {
                    node: *to,
                    body,
                    before,
                })
            }
            super::WorkflowEdgeDrag::Data { from, to, slot } => {
                let producer = node_ref(&self.working, *from).ok_or_else(|| unknown(*from))?;
                let bound = match &producer.kind {
                    WorkflowNodeKind::Data { binding, .. }
                    | WorkflowNodeKind::Call { binding, .. }
                    | WorkflowNodeKind::Computation { binding, .. } => {
                        binding.as_ref().map(|target| &target.root)
                    }
                    WorkflowNodeKind::Effect(effect) => effect.binding().map(|target| &target.root),
                    WorkflowNodeKind::StateUpdate(write) => Some(write.root()),
                    WorkflowNodeKind::Container(container) => {
                        container.binding().map(|target| &target.root)
                    }
                    WorkflowNodeKind::Terminal(_) | WorkflowNodeKind::Throw { .. } => None,
                };
                let bound = bound.ok_or((
                    at_node(*from),
                    Kind::EditDoesNotApply {
                        expected: "a node that binds its value",
                    },
                ))?;
                Ok(WorkflowEdit::ReplaceExpression {
                    target: WorkflowExpressionRef::Node(*to),
                    slot: slot.clone(),
                    expression: Expr::Variable(bound.clone()),
                })
            }
        }
    }
}

/// The run call's ordinary slots, restricted to its argument expressions.
fn wrapper_argument_mut<'w>(
    wrapper: &'w mut WorkflowProcessWrapper,
    slots: &[ExprSlot],
) -> Result<&'w mut Expr, Kind> {
    let (argument, rest) = match slots {
        [ExprSlot::Arg(index), rest @ ..] => (wrapper.arguments.get_mut(*index as usize), rest),
        [ExprSlot::Callee, ExprSlot::Arg(index), rest @ ..] if *index > 0 => (
            wrapper
                .driver
                .as_mut()
                .and_then(|driver| driver.arguments.get_mut((*index - 1) as usize)),
            rest,
        ),
        [ExprSlot::Callee, ExprSlot::Body, ..] | [ExprSlot::Callee, ExprSlot::Arg(0), ..] => {
            return Err(Kind::SlotInChildBody);
        }
        _ => return Err(Kind::UnknownSlot),
    };
    argument
        .and_then(|argument| argument.at_slots_mut(rest))
        .ok_or(Kind::UnknownSlot)
}

/// Places `new` ahead of the statement `anchor` in `items` or a group under
/// them. Ahead of a group's first statement it stays outside the group:
/// `Err(true)` hands it back to the list that holds the group, and
/// `Err(false)` means the anchor is not here. `outermost` is whether `items`
/// is the body's own list, which has no list to hand back to.
fn place_before(
    items: &mut Vec<WorkflowBodyItem>,
    anchor: &WorkflowNodeId,
    mut new: WorkflowBodyItem,
    outermost: bool,
) -> Result<(), (WorkflowBodyItem, bool)> {
    for index in 0..items.len() {
        match &mut items[index] {
            WorkflowBodyItem::Node(node) if node.id == *anchor => {}
            WorkflowBodyItem::Node(_) => continue,
            WorkflowBodyItem::Group { items: inner, .. } => {
                match place_before(inner, anchor, new, false) {
                    Ok(()) => return Ok(()),
                    Err((returned, true)) => new = returned,
                    Err((returned, false)) => {
                        new = returned;
                        continue;
                    }
                }
            }
        }
        if index == 0 && !outermost {
            return Err((new, true));
        }
        items.insert(index, new);
        return Ok(());
    }
    Err((new, false))
}

/// Takes the statement `id` out of `items` or a group under them.
fn take_from(items: &mut Vec<WorkflowBodyItem>, id: &WorkflowNodeId) -> Option<WorkflowNode> {
    for index in 0..items.len() {
        match &mut items[index] {
            WorkflowBodyItem::Node(node) if node.id == *id => {
                return match items.remove(index) {
                    WorkflowBodyItem::Node(node) => Some(*node),
                    WorkflowBodyItem::Group { .. } => None,
                };
            }
            WorkflowBodyItem::Node(_) => {}
            WorkflowBodyItem::Group { items: inner, .. } => {
                if let Some(node) = take_from(inner, id) {
                    return Some(node);
                }
            }
        }
    }
    None
}

/// A node that spells exactly `statement`. Normalization gives it the kind
/// the projector reads the statement as.
fn statement_node(handle: WorkflowDraftHandle, statement: Expr) -> WorkflowNode {
    let (label, expression) = match statement {
        Expr::LabelAnnotated { label, expr } => (Some(label), *expr),
        statement => (None, statement),
    };
    WorkflowNode {
        id: handle.id(),
        name: String::new(),
        label,
        kind: WorkflowNodeKind::Computation {
            binding: None,
            expression,
        },
        available_variables: Vec::new(),
        type_facets: None,
        outputs: Vec::new(),
        execution_sites: Vec::new(),
    }
}

/// Replaces the expression at `slots` from the node's statement.
fn replace_expression(
    node: &mut WorkflowNode,
    slots: &[ExprSlot],
    expression: Expr,
) -> Result<(), Kind> {
    let labelled = node.label.is_some();
    let WorkflowNodeKind::Container(container) = &mut node.kind else {
        // A statement with no child bodies is edited as the expression it
        // is, then read back the way an inserted statement is.
        let Some(handle) = WorkflowDraftHandle::of(&node.id) else {
            return Err(Kind::UnknownSlot);
        };
        let mut statement = workflow_node_statement(node);
        *statement.at_slots_mut(slots).ok_or(Kind::UnknownSlot)? = expression;
        *node = statement_node(handle, statement);
        return Ok(());
    };
    // A container's statement is its label, its binding and its header
    // around child bodies, so the path is read against those fields and the
    // bodies keep their nodes.
    let mut slots = slots;
    if labelled {
        slots = slots
            .strip_prefix(&[ExprSlot::Inner])
            .ok_or(Kind::UnknownSlot)?;
    }
    let binding = match container {
        WorkflowContainer::If { binding, .. }
        | WorkflowContainer::For { binding, .. }
        | WorkflowContainer::While { binding, .. }
        | WorkflowContainer::Try { binding, .. }
        | WorkflowContainer::Scope { binding, .. } => binding,
    };
    if let Some(target) = binding {
        match slots.split_first() {
            Some((ExprSlot::AssignIndex(index), rest)) => {
                let step = target
                    .steps
                    .iter_mut()
                    .filter_map(|step| match step {
                        AssignPathStep::Index(index) => Some(index),
                        AssignPathStep::Field(_) => None,
                    })
                    .nth(*index as usize)
                    .ok_or(Kind::UnknownSlot)?;
                *step.at_slots_mut(rest).ok_or(Kind::UnknownSlot)? = expression;
                return Ok(());
            }
            Some((ExprSlot::Value, rest)) => slots = rest,
            _ => return Err(Kind::UnknownSlot),
        }
    }
    let (first, rest) = slots.split_first().ok_or(Kind::UnknownSlot)?;
    let header = match (container, first) {
        (
            WorkflowContainer::If { condition, .. } | WorkflowContainer::While { condition, .. },
            ExprSlot::Condition,
        ) => condition,
        (WorkflowContainer::For { iterable, .. }, ExprSlot::Iterable) => iterable,
        (
            WorkflowContainer::For {
                bind: Some(bind), ..
            },
            ExprSlot::Bind,
        ) => bind,
        (
            _,
            ExprSlot::Then
            | ExprSlot::Else
            | ExprSlot::Body
            | ExprSlot::Catch
            | ExprSlot::Finally
            | ExprSlot::Inner,
        ) => return Err(Kind::SlotInChildBody),
        _ => return Err(Kind::UnknownSlot),
    };
    *header.at_slots_mut(rest).ok_or(Kind::UnknownSlot)? = expression;
    Ok(())
}

fn set_binding(node: &mut WorkflowNode, binding: Option<AssignTarget>) -> Result<(), Kind> {
    match &mut node.kind {
        WorkflowNodeKind::Data {
            binding: current, ..
        }
        | WorkflowNodeKind::Call {
            binding: current, ..
        }
        | WorkflowNodeKind::Computation {
            binding: current, ..
        }
        | WorkflowNodeKind::Container(
            WorkflowContainer::If {
                binding: current, ..
            }
            | WorkflowContainer::For {
                binding: current, ..
            }
            | WorkflowContainer::While {
                binding: current, ..
            }
            | WorkflowContainer::Try {
                binding: current, ..
            }
            | WorkflowContainer::Scope {
                binding: current, ..
            },
        ) => *current = binding,
        WorkflowNodeKind::Effect(effect) => match (effect.binding_mut(), binding) {
            (Some(current), binding) => *current = binding,
            (None, None) => {}
            // A `break` or a `continue` has no value; one that is bound is
            // spelled as the expression it is.
            (None, binding) => {
                node.kind = WorkflowNodeKind::Computation {
                    binding,
                    expression: effect.to_ir(),
                };
            }
        },
        WorkflowNodeKind::StateUpdate(WorkflowStateWrite::Plain { target, value }) => match binding
        {
            Some(binding) => *target = binding,
            None => {
                node.kind = WorkflowNodeKind::Computation {
                    binding: None,
                    expression: value.clone(),
                };
            }
        },
        WorkflowNodeKind::StateUpdate(WorkflowStateWrite::Member { root, step, .. }) => {
            let member = Kind::EditDoesNotApply {
                expected: "a target that is the same kind of member step of a variable",
            };
            let Some(AssignTarget { root: to, steps }) = binding else {
                return Err(Kind::EditDoesNotApply {
                    expected: "a node that can bind its value",
                });
            };
            let mut steps = steps.into_iter();
            match (step, steps.next(), steps.next()) {
                (WorkflowMemberStep::Field { field }, Some(AssignPathStep::Field(to)), None) => {
                    *field = to;
                }
                (
                    WorkflowMemberStep::Index { index, .. },
                    Some(AssignPathStep::Index(to)),
                    None,
                ) => {
                    *index = to;
                }
                _ => return Err(member),
            }
            *root = to;
        }
        WorkflowNodeKind::Terminal(_) | WorkflowNodeKind::Throw { .. } => {
            return Err(Kind::EditDoesNotApply {
                expected: "a node that can bind its value",
            });
        }
    }
    Ok(())
}

/// Whether a wrapper passes exactly the process's parameters, by name, to
/// its run function's parameters of the same names.
fn passes_params_through(wrapper: &WorkflowProcessWrapper, params: &[ProcessParam]) -> bool {
    wrapper.params.len() == params.len()
        && wrapper.arguments.len() == params.len()
        && params
            .iter()
            .zip(&wrapper.params)
            .zip(&wrapper.arguments)
            .all(|((param, run), argument)| {
                param.name == *run
                    && matches!(argument, Expr::Variable(passed) if *passed == param.name)
            })
}

pub(super) fn body<'g>(
    graph: &'g WorkflowGraph,
    body: &WorkflowBodyRef,
) -> Option<&'g WorkflowSubgraph> {
    match body {
        WorkflowBodyRef::Main => Some(&graph.main),
        WorkflowBodyRef::Process(process) => {
            let id = process.id();
            super::processes(graph)
                .find(|process| process.id == id)
                .map(|process| &process.body)
        }
        WorkflowBodyRef::Child { node, slot } => match &node_ref(graph, *node)?.kind {
            WorkflowNodeKind::Container(container) => child_body(container, *slot),
            _ => None,
        },
    }
}

fn body_mut<'g>(
    graph: &'g mut WorkflowGraph,
    body: &WorkflowBodyRef,
) -> Option<&'g mut WorkflowSubgraph> {
    match body {
        WorkflowBodyRef::Main => Some(&mut graph.main),
        WorkflowBodyRef::Process(process) => {
            process_mut(graph, *process).map(|process| &mut process.body)
        }
        WorkflowBodyRef::Child { node, slot } => match &mut node_mut(graph, *node)?.kind {
            WorkflowNodeKind::Container(container) => child_body_mut(container, *slot),
            _ => None,
        },
    }
}

fn process_mut(
    graph: &mut WorkflowGraph,
    handle: WorkflowDraftHandle,
) -> Option<&mut WorkflowProcess> {
    let id = handle.id();
    graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) if process.id == id => Some(process),
            _ => None,
        })
}

fn container_mut(
    graph: &mut WorkflowGraph,
    handle: WorkflowDraftHandle,
) -> Result<&mut WorkflowContainer, Refused> {
    match &mut node_mut(graph, handle).ok_or_else(|| unknown(handle))?.kind {
        WorkflowNodeKind::Container(container) => Ok(container),
        _ => Err((
            at_node(handle),
            Kind::EditDoesNotApply {
                expected: "a container",
            },
        )),
    }
}

fn node_ref(graph: &WorkflowGraph, handle: WorkflowDraftHandle) -> Option<&WorkflowNode> {
    let id = handle.id();
    graph.nodes().find(|node| node.id == id)
}

/// The node `handle` at or under `node`.
fn node_ref_in(node: &WorkflowNode, handle: WorkflowDraftHandle) -> Option<&WorkflowNode> {
    if node.id == handle.id() {
        return Some(node);
    }
    children(node)
        .into_iter()
        .flat_map(|(_, child)| child.nodes())
        .find_map(|node| node_ref_in(node, handle))
}

fn node_mut(graph: &mut WorkflowGraph, handle: WorkflowDraftHandle) -> Option<&mut WorkflowNode> {
    fn within<'g>(
        body: &'g mut WorkflowSubgraph,
        id: &crate::WorkflowNodeId,
    ) -> Option<&'g mut WorkflowNode> {
        for node in body.nodes_mut() {
            if node.id == *id {
                return Some(node);
            }
            if let WorkflowNodeKind::Container(container) = &mut node.kind {
                for (_, child) in container.child_subgraphs_mut() {
                    if let Some(found) = within(child, id) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }
    let id = handle.id();
    let WorkflowGraph {
        main, declarations, ..
    } = graph;
    std::iter::once(main)
        .chain(
            declarations
                .iter_mut()
                .filter_map(|declaration| match declaration {
                    WorkflowDeclaration::Process(process) => Some(&mut process.body),
                    WorkflowDeclaration::Function(_) => None,
                }),
        )
        .find_map(|body| within(body, &id))
}

/// The body that holds the node `handle`, and the node's position in it.
pub(super) fn parent_of(
    graph: &WorkflowGraph,
    handle: WorkflowDraftHandle,
) -> Option<(WorkflowBodyRef, usize)> {
    fn within(
        body: &WorkflowSubgraph,
        at: WorkflowBodyRef,
        id: &crate::WorkflowNodeId,
    ) -> Option<(WorkflowBodyRef, usize)> {
        let nodes = body.nodes();
        if let Some(index) = nodes.iter().position(|node| node.id == *id) {
            return Some((at, index));
        }
        nodes.into_iter().find_map(|node| {
            let WorkflowNodeKind::Container(container) = &node.kind else {
                return None;
            };
            let parent = WorkflowDraftHandle::of(&node.id)?;
            body_slots(container).into_iter().find_map(|slot| {
                within(
                    child_body(container, slot)?,
                    WorkflowBodyRef::Child { node: parent, slot },
                    id,
                )
            })
        })
    }
    let id = handle.id();
    within(&graph.main, WorkflowBodyRef::Main, &id).or_else(|| {
        super::processes(graph).find_map(|process| {
            within(
                &process.body,
                WorkflowBodyRef::Process(WorkflowDraftHandle::of(&process.id)?),
                &id,
            )
        })
    })
}

/// What a body's frame hangs from: `main` or a process container.
fn root_of(graph: &WorkflowGraph, body: &WorkflowBodyRef) -> Option<Owner> {
    let mut body = body.clone();
    loop {
        body = match body {
            WorkflowBodyRef::Main => return Some(Owner::Main),
            WorkflowBodyRef::Process(process) => return Some(Owner::Process(process)),
            WorkflowBodyRef::Child { node, .. } => parent_of(graph, node)?.0,
        };
    }
}
