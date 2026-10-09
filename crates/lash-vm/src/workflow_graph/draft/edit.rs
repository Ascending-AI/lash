//! The typed edits of a draft and how each changes the document.
//!
//! Every authoritative field of the document has an edit that changes it:
//! statements are inserted, cloned, removed, moved and replaced; any
//! expression of any statement is replaced through its slot path; headers,
//! labels, bindings, body forms, declarations and process wrappers each have
//! their own. An edit states its subject by handle and its content as typed
//! IR, and never as text.

use std::collections::BTreeSet;

use crate::ast::{
    AssignPathStep, AssignTarget, AstString, Declaration, Expr, ExprSlot, FunctionDecl,
    LabelMetadata, ProcessParam, TypeExpr,
};
use crate::workflow_graph::{
    WorkflowBodyForm, WorkflowCatch, WorkflowContainer, WorkflowDeclaration, WorkflowGraph,
    WorkflowNode, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowProcess,
    WorkflowProcessWrapper, WorkflowSlotPath, WorkflowSubgraph, workflow_node_statement,
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
    /// Replaces the expression at `slot`, a non-empty path from the node's
    /// statement that stays outside its child bodies.
    ReplaceExpression {
        node: WorkflowDraftHandle,
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
    /// Sets how the IR spells a body around its statements: its
    /// completion value and completion groups.
    SetBodyForm {
        body: WorkflowBodyRef,
        form: WorkflowBodyForm,
    },
    /// Declares a process with an empty body.
    InsertProcess {
        name: AstString,
        params: Vec<ProcessParam>,
        return_ty: Option<TypeExpr>,
    },
    RemoveProcess {
        process: WorkflowDraftHandle,
    },
    /// Renames a declared process and every reference to it.
    RenameProcess {
        process: WorkflowDraftHandle,
        name: AstString,
    },
    /// Sets a process's parameters and declared output. A failure wrapper
    /// that passes the parameters straight through is rewritten to pass the
    /// new ones.
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
                    for node in &child.nodes {
                        self.forget(node, false);
                    }
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::ReplaceExpression {
                node,
                slot,
                expression,
            } => {
                let refuse = |kind| {
                    (
                        WorkflowEditLocation::Node {
                            node,
                            slot: slot.clone(),
                        },
                        kind,
                    )
                };
                let slots = slot
                    .expr_slots()
                    .filter(|slots| !slots.is_empty())
                    .ok_or_else(|| refuse(Kind::UnknownSlot))?;
                let subject = node_mut(&mut self.working, node).ok_or_else(|| unknown(node))?;
                replace_expression(subject, &slots, expression).map_err(refuse)?;
                self.settled(WorkflowEditLocation::Node { node, slot })
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
                    match label {
                        Some(label) => {
                            process.display_name = label.title.to_string();
                            process.description = label.description.map(|text| text.to_string());
                            process.name_source = WorkflowNodeNameSource::Label;
                        }
                        None => {
                            process.display_name.clone_from(&process.name);
                            process.description = None;
                            process.name_source = WorkflowNodeNameSource::Derived;
                        }
                    }
                    return self.settled(at_process(target));
                }
                let node = node_mut(&mut self.working, target).ok_or_else(|| unknown(target))?;
                set_label(node, label);
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
                for node in removed.iter().flat_map(|body| &body.nodes) {
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
                for node in removed.iter().flat_map(|body| &body.nodes) {
                    self.forget(node, false);
                }
                self.settled(at_node(node))
            }
            WorkflowEdit::SetBodyForm { body, form } => {
                body_mut(&mut self.working, &body)
                    .ok_or((at_body(&body), Kind::UnknownBody))?
                    .form = form;
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
                        display_name: name.to_string(),
                        description: None,
                        name_source: WorkflowNodeNameSource::Derived,
                        params,
                        return_ty,
                        origin: crate::ProcessOrigin::Declared,
                        wrapper: None,
                        body: WorkflowSubgraph::default(),
                    }));
                self.settled(at_process(handle))
            }
            WorkflowEdit::RemoveProcess { process } => {
                let id = process.id();
                let before = self.working.declarations.len();
                self.working.declarations.retain(|declaration| {
                    !matches!(declaration, WorkflowDeclaration::Process(declared) if declared.id == id)
                });
                if self.working.declarations.len() == before {
                    return Err(unknown(process));
                }
                self.settled(WorkflowEditLocation::Document)
            }
            WorkflowEdit::RenameProcess { process, name } => {
                let declared =
                    process_mut(&mut self.working, process).ok_or_else(|| unknown(process))?;
                if declared.origin.is_lifted() {
                    return Err((at_process(process), Kind::DerivedProcess));
                }
                let former = AstString::from(declared.name.as_str());
                declared.name = name.to_string();
                if declared.name_source == WorkflowNodeNameSource::Derived {
                    declared.display_name = name.to_string();
                }
                let mut program = self.program.clone();
                scope::rename_process(&mut program, &former, &name);
                self.settle_program(program, self.carried.clone())
                    .map_err(|error| (at_process(process), Kind::InvalidProgram(error)))
            }
            WorkflowEdit::SetProcessSignature {
                process,
                params,
                return_ty,
            } => {
                let declared =
                    process_mut(&mut self.working, process).ok_or_else(|| unknown(process))?;
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
                self.settled(WorkflowEditLocation::Function { name })
            }
            WorkflowEdit::ReplaceFunction { function } => {
                let name = function.name.clone();
                let location = WorkflowEditLocation::Function { name: name.clone() };
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
                        WorkflowEditLocation::Function { name: name.clone() },
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

    /// Places `node` in `body` ahead of `before`, shifting the body's form
    /// around it.
    fn insert(
        &mut self,
        body: &WorkflowBodyRef,
        before: Option<WorkflowDraftHandle>,
        node: WorkflowNode,
    ) -> Result<(), Refused> {
        let target = body_mut(&mut self.working, body).ok_or((at_body(body), Kind::UnknownBody))?;
        let index = match before {
            None => target.nodes.len(),
            Some(anchor) => target
                .nodes
                .iter()
                .position(|node| node.id == anchor.id())
                .ok_or((at_body(body), Kind::AnchorOutsideBody { anchor }))?,
        };
        target.form = target.form.with_inserted(position(index));
        target.nodes.insert(index, node);
        Ok(())
    }

    /// Takes `node` out of its body, shifting the body's form around the
    /// gap.
    fn take(&mut self, node: WorkflowDraftHandle) -> Result<WorkflowNode, Refused> {
        let (body, index) = parent_of(&self.working, node).ok_or_else(|| unknown(node))?;
        let body = body_mut(&mut self.working, &body).ok_or_else(|| unknown(node))?;
        body.form = body.form.with_removed(position(index));
        Ok(body.nodes.remove(index))
    }

    /// Gives a copied node and everything under it handles of their own.
    fn rehandle(&mut self, node: &mut WorkflowNode) {
        if let Some(of) = WorkflowDraftHandle::of(&node.id) {
            node.id = self.mint(WorkflowNodeSource::Clone { of }).id();
        }
        if let WorkflowNodeKind::Container(container) = &mut node.kind {
            for (_, child) in container.child_subgraphs_mut() {
                for node in &mut child.nodes {
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
                let root = function
                    .expr_slots()
                    .and_then(|slots| expr_at(&self.program, path)?.child_steps(&slots))
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
                    body.nodes[index + 1..]
                        .iter()
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
                let binding = match &producer.kind {
                    WorkflowNodeKind::Data { binding, .. }
                    | WorkflowNodeKind::Call { binding, .. }
                    | WorkflowNodeKind::Effect { binding, .. }
                    | WorkflowNodeKind::Computation { binding, .. } => binding.as_ref(),
                    WorkflowNodeKind::StateUpdate { target, .. } => Some(target),
                    WorkflowNodeKind::Container(container) => container.binding(),
                    WorkflowNodeKind::Terminal { .. } | WorkflowNodeKind::Throw { .. } => None,
                };
                let binding = binding.ok_or((
                    at_node(*from),
                    Kind::EditDoesNotApply {
                        expected: "a node that binds its value",
                    },
                ))?;
                Ok(WorkflowEdit::ReplaceExpression {
                    node: *to,
                    slot: slot.clone(),
                    expression: Expr::Variable(binding.root.clone()),
                })
            }
        }
    }
}

fn position(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// A node that spells exactly `statement`. Normalization gives it the kind
/// the projector reads the statement as.
fn statement_node(handle: WorkflowDraftHandle, statement: Expr) -> WorkflowNode {
    let (label, expression) = match statement {
        Expr::LabelAnnotated { label, expr } => (Some(label), *expr),
        statement => (None, statement),
    };
    let mut node = WorkflowNode {
        id: handle.id(),
        name: String::new(),
        description: None,
        name_source: WorkflowNodeNameSource::Derived,
        kind: WorkflowNodeKind::Computation {
            binding: None,
            expression,
        },
        available_variables: Vec::new(),
        type_facets: None,
        outputs: Vec::new(),
        execution_sites: Vec::new(),
        source_span: None,
    };
    set_label(&mut node, label);
    node
}

fn set_label(node: &mut WorkflowNode, label: Option<LabelMetadata>) {
    match label {
        Some(label) => {
            node.name = label.title.to_string();
            node.description = label.description.map(|text| text.to_string());
            node.name_source = WorkflowNodeNameSource::Label;
        }
        None => {
            node.description = None;
            node.name_source = WorkflowNodeNameSource::Derived;
        }
    }
}

/// Replaces the expression at `slots` from the node's statement.
fn replace_expression(
    node: &mut WorkflowNode,
    slots: &[ExprSlot],
    expression: Expr,
) -> Result<(), Kind> {
    let labelled = node.name_source == WorkflowNodeNameSource::Label;
    let WorkflowNodeKind::Container(container) = &mut node.kind else {
        // A statement with no child bodies is edited as the expression it
        // is, then read back the way an inserted statement is.
        let Some(handle) = WorkflowDraftHandle::of(&node.id) else {
            return Err(Kind::UnknownSlot);
        };
        let mut statement = workflow_node_statement(node).map_err(Kind::InvalidDocument)?;
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
        | WorkflowNodeKind::Effect {
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
        WorkflowNodeKind::StateUpdate { target, .. } if binding.is_some() => {
            if let Some(binding) = binding {
                *target = binding;
            }
        }
        WorkflowNodeKind::StateUpdate {
            expression,
            update: None,
            pinned: None,
            ..
        } => {
            node.kind = WorkflowNodeKind::Computation {
                binding: None,
                expression: expression.clone(),
            };
        }
        WorkflowNodeKind::StateUpdate { .. }
        | WorkflowNodeKind::Terminal { .. }
        | WorkflowNodeKind::Throw { .. } => {
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
        .flat_map(|(_, child)| &child.nodes)
        .find_map(|node| node_ref_in(node, handle))
}

fn node_mut(graph: &mut WorkflowGraph, handle: WorkflowDraftHandle) -> Option<&mut WorkflowNode> {
    fn within<'g>(
        body: &'g mut WorkflowSubgraph,
        id: &crate::WorkflowNodeId,
    ) -> Option<&'g mut WorkflowNode> {
        for node in &mut body.nodes {
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
        if let Some(index) = body.nodes.iter().position(|node| node.id == *id) {
            return Some((at, index));
        }
        body.nodes.iter().find_map(|node| {
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
