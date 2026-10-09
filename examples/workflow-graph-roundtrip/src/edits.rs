//! Typed edit transactions for both form and IR editors.
//! Forms lower their edited fields into operations against the existing draft;
//! no save builds a target graph or infers structure from a document.

use std::collections::{BTreeMap, BTreeSet};

use lash::vm::ir::{
    AssignTarget, AstString, Expr, ExprSlot, FunctionDecl, LabelMetadata, ProcessParam, TypeExpr,
    WorkflowBodySlot, WorkflowContainer, WorkflowNode, WorkflowNodeKind, WorkflowNodeNameSource,
    WorkflowSlotPath, WorkflowSubgraph,
};
use lash::workflow::{
    WorkflowBindingRef, WorkflowBodyForm, WorkflowBodyRef, WorkflowDraft, WorkflowDraftHandle,
    WorkflowEdit, WorkflowEditRefusal, WorkflowEditTransaction, WorkflowGraphError,
    WorkflowProcessWrapper, workflow_node_statement,
};
use serde::Deserialize;

/// Why a set of edits was not applied. The saved workflow is unchanged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EditError {
    #[error(transparent)]
    Refused(#[from] WorkflowEditRefusal),
    #[error("form fields were refused")]
    Form(Box<crate::RenderErrorResponse>),
    #[error("node `{node}` does not spell a statement: {error}")]
    Statement {
        node: String,
        error: WorkflowGraphError,
    },
    #[error("`{id}` names no node or process of the workflow")]
    UnknownNode { id: String },
    #[error("`{slot}` is not a child body of node `{node}`")]
    UnknownSlot { node: String, slot: String },
}

/// The typed slot of the child body a container names `name`.
pub(crate) fn body_slot(container: &WorkflowContainer, name: &str) -> Option<WorkflowBodySlot> {
    Some(match (container, name) {
        (WorkflowContainer::If { .. }, "then") => WorkflowBodySlot::Then,
        (WorkflowContainer::If { .. }, "else") => WorkflowBodySlot::Else,
        (WorkflowContainer::For { .. } | WorkflowContainer::While { .. }, "body") => {
            WorkflowBodySlot::LoopBody
        }
        (WorkflowContainer::Try { .. }, "body") => WorkflowBodySlot::TryBody,
        (WorkflowContainer::Try { .. }, "catch") => WorkflowBodySlot::Catch,
        (WorkflowContainer::Try { .. }, "finally") => WorkflowBodySlot::Finally,
        (WorkflowContainer::Scope { .. }, "body") => WorkflowBodySlot::Scope,
        _ => return None,
    })
}

fn statement(node: &WorkflowNode) -> Result<Expr, EditError> {
    workflow_node_statement(node).map_err(|error| EditError::Statement {
        node: node.id.to_string(),
        error,
    })
}

/// A node's own statement: what it spells with its child bodies emptied.
fn own_statement(node: &WorkflowNode) -> Result<Expr, EditError> {
    let mut own = node.clone();
    if let WorkflowNodeKind::Container(container) = &mut own.kind {
        for (_, child) in container.child_subgraphs_mut() {
            *child = WorkflowSubgraph::default();
        }
    }
    statement(&own)
}

fn label(
    source: WorkflowNodeNameSource,
    title: &str,
    description: Option<&String>,
) -> Option<LabelMetadata> {
    matches!(source, WorkflowNodeNameSource::Label).then(|| LabelMetadata {
        title: title.into(),
        description: description.map(|description| description.as_str().into()),
    })
}

/// The path from a container node's statement to the container expression
/// itself, through the assignment and the label around it.
fn container_path(statement: &Expr) -> Vec<ExprSlot> {
    let mut path = Vec::new();
    let mut current = statement;
    loop {
        let slot = match current {
            Expr::LabelAnnotated { .. } | Expr::Role { .. } => ExprSlot::Inner,
            Expr::Assign { .. } => ExprSlot::Value,
            _ => return path,
        };
        match current.slot(slot) {
            Some(inner) => {
                path.push(slot);
                current = inner;
            }
            None => return path,
        }
    }
}

fn transaction(draft: &mut WorkflowDraft, edits: Vec<WorkflowEdit>) -> Result<(), EditError> {
    if edits.is_empty() {
        return Ok(());
    }
    draft.apply(WorkflowEditTransaction {
        base: draft.revision(),
        edits,
    })?;
    Ok(())
}

/// A body of the workflow, named by the ids of the served document.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum BodyRef {
    Main,
    Process { process: String },
    Child { node: String, slot: String },
}

/// A variable of the workflow, named where the document binds it.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum BindingRef {
    Variable {
        at: String,
        name: AstString,
    },
    Nested {
        node: String,
        function: WorkflowSlotPath,
        name: AstString,
    },
    Function {
        function: AstString,
        name: AstString,
    },
}

/// One typed edit of the generic structured editor: a
/// [`lash::workflow::WorkflowEdit`] whose nodes are named by the ids of the
/// served document and whose content is Lash's IR as JSON.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum EditOperation {
    /// Edit one form's fields; structure is addressed by separate operations.
    SetForm {
        node: String,
        data: Box<crate::NodeData>,
    },
    /// Insert a form node under a request-local name used by later operations.
    InsertForm {
        id: String,
        body: BodyRef,
        #[serde(default)]
        before: Option<String>,
        data: Box<crate::NodeData>,
    },
    InsertFormProcess {
        id: String,
        data: Box<crate::NodeData>,
    },
    InsertNode {
        body: BodyRef,
        #[serde(default)]
        before: Option<String>,
        statement: Expr,
    },
    CloneNode {
        node: String,
        body: BodyRef,
        #[serde(default)]
        before: Option<String>,
    },
    RemoveNode {
        node: String,
    },
    MoveNode {
        node: String,
        body: BodyRef,
        #[serde(default)]
        before: Option<String>,
    },
    ReplaceNode {
        node: String,
        statement: Expr,
    },
    ReplaceExpression {
        node: String,
        slot: WorkflowSlotPath,
        expression: Expr,
    },
    SetBinding {
        node: String,
        #[serde(default)]
        binding: Option<AssignTarget>,
    },
    RenameBinding {
        binding: BindingRef,
        name: AstString,
    },
    SetCondition {
        node: String,
        condition: Expr,
    },
    SetLabel {
        target: String,
        #[serde(default)]
        label: Option<LabelMetadata>,
    },
    SetLoopBinding {
        node: String,
        element: AstString,
        #[serde(default)]
        authored_element: Option<AstString>,
        #[serde(default)]
        bind: Option<Expr>,
    },
    SetCatch {
        node: String,
        #[serde(default)]
        binding: Option<AstString>,
    },
    SetFinally {
        node: String,
        present: bool,
    },
    SetBodyForm {
        body: BodyRef,
        form: WorkflowBodyForm,
    },
    InsertProcess {
        name: AstString,
        #[serde(default)]
        params: Vec<ProcessParam>,
        #[serde(default)]
        return_ty: Option<TypeExpr>,
    },
    RemoveProcess {
        process: String,
    },
    RenameProcess {
        process: String,
        name: AstString,
    },
    SetProcessSignature {
        process: String,
        #[serde(default)]
        params: Vec<ProcessParam>,
        #[serde(default)]
        return_ty: Option<TypeExpr>,
    },
    SetProcessWrapper {
        process: String,
        #[serde(default)]
        wrapper: Option<WorkflowProcessWrapper>,
    },
    InsertFunction {
        function: FunctionDecl,
    },
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

/// The nodes of a draft by the ids the host served them under.
type Named = BTreeMap<String, WorkflowDraftHandle>;

fn handle(named: &Named, id: &str) -> Result<WorkflowDraftHandle, EditError> {
    named
        .get(id)
        .copied()
        .ok_or_else(|| EditError::UnknownNode { id: id.to_string() })
}

fn anchor(
    named: &Named,
    before: Option<&String>,
) -> Result<Option<WorkflowDraftHandle>, EditError> {
    before.map(|id| handle(named, id)).transpose()
}

fn body_ref(
    draft: &WorkflowDraft,
    named: &Named,
    body: &BodyRef,
) -> Result<WorkflowBodyRef, EditError> {
    Ok(match body {
        BodyRef::Main => WorkflowBodyRef::Main,
        BodyRef::Process { process } => WorkflowBodyRef::Process(handle(named, process)?),
        BodyRef::Child { node, slot } => {
            let handle = handle(named, node)?;
            let typed = draft.node(handle).and_then(|node| match &node.kind {
                WorkflowNodeKind::Container(container) => body_slot(container, slot),
                _ => None,
            });
            WorkflowBodyRef::Child {
                node: handle,
                slot: typed.ok_or_else(|| EditError::UnknownSlot {
                    node: node.clone(),
                    slot: slot.clone(),
                })?,
            }
        }
    })
}

fn typed_edit(
    draft: &WorkflowDraft,
    named: &Named,
    operation: EditOperation,
) -> Result<WorkflowEdit, EditError> {
    use EditOperation as Op;
    Ok(match operation {
        Op::SetForm { node, .. }
        | Op::InsertForm { id: node, .. }
        | Op::InsertFormProcess { id: node, .. } => {
            return Err(EditError::UnknownNode { id: node });
        }
        Op::InsertNode {
            body,
            before,
            statement,
        } => WorkflowEdit::InsertNode {
            body: body_ref(draft, named, &body)?,
            before: anchor(named, before.as_ref())?,
            statement,
        },
        Op::CloneNode { node, body, before } => WorkflowEdit::CloneNode {
            node: handle(named, &node)?,
            body: body_ref(draft, named, &body)?,
            before: anchor(named, before.as_ref())?,
        },
        Op::RemoveNode { node } => WorkflowEdit::RemoveNode {
            node: handle(named, &node)?,
        },
        Op::MoveNode { node, body, before } => WorkflowEdit::MoveNode {
            node: handle(named, &node)?,
            body: body_ref(draft, named, &body)?,
            before: anchor(named, before.as_ref())?,
        },
        Op::ReplaceNode { node, statement } => WorkflowEdit::ReplaceNode {
            node: handle(named, &node)?,
            statement,
        },
        Op::ReplaceExpression {
            node,
            slot,
            expression,
        } => WorkflowEdit::ReplaceExpression {
            target: lash::workflow::WorkflowExpressionRef::Node(handle(named, &node)?),
            slot,
            expression,
        },
        Op::SetBinding { node, binding } => WorkflowEdit::SetBinding {
            node: handle(named, &node)?,
            binding,
        },
        Op::RenameBinding { binding, name } => WorkflowEdit::RenameBinding {
            binding: match binding {
                BindingRef::Variable { at, name } => WorkflowBindingRef::Variable {
                    at: handle(named, &at)?,
                    name,
                },
                BindingRef::Nested {
                    node,
                    function,
                    name,
                } => WorkflowBindingRef::Nested {
                    node: handle(named, &node)?,
                    function,
                    name,
                },
                BindingRef::Function { function, name } => {
                    WorkflowBindingRef::Function { function, name }
                }
            },
            name,
        },
        Op::SetCondition { node, condition } => WorkflowEdit::SetCondition {
            node: handle(named, &node)?,
            condition,
        },
        Op::SetLabel { target, label } => WorkflowEdit::SetLabel {
            target: handle(named, &target)?,
            label,
        },
        Op::SetLoopBinding {
            node,
            element,
            authored_element,
            bind,
        } => WorkflowEdit::SetLoopBinding {
            node: handle(named, &node)?,
            element,
            authored_element,
            bind,
        },
        Op::SetCatch { node, binding } => WorkflowEdit::SetCatch {
            node: handle(named, &node)?,
            binding,
        },
        Op::SetFinally { node, present } => WorkflowEdit::SetFinally {
            node: handle(named, &node)?,
            present,
        },
        Op::SetBodyForm { body, form } => WorkflowEdit::SetBodyForm {
            body: body_ref(draft, named, &body)?,
            form,
        },
        Op::InsertProcess {
            name,
            params,
            return_ty,
        } => WorkflowEdit::InsertProcess {
            name,
            params,
            return_ty,
        },
        Op::RemoveProcess { process } => WorkflowEdit::RemoveProcess {
            process: handle(named, &process)?,
        },
        Op::RenameProcess { process, name } => WorkflowEdit::RenameProcess {
            process: handle(named, &process)?,
            name,
        },
        Op::SetProcessSignature {
            process,
            params,
            return_ty,
        } => WorkflowEdit::SetProcessSignature {
            process: handle(named, &process)?,
            params,
            return_ty,
        },
        Op::SetProcessWrapper { process, wrapper } => WorkflowEdit::SetProcessWrapper {
            process: handle(named, &process)?,
            wrapper,
        },
        Op::InsertFunction { function } => WorkflowEdit::InsertFunction { function },
        Op::ReplaceFunction { function } => WorkflowEdit::ReplaceFunction { function },
        Op::RemoveFunction { name } => WorkflowEdit::RemoveFunction { name },
        Op::SetPrivateBindings { bindings } => WorkflowEdit::SetPrivateBindings { bindings },
    })
}

/// The edited draft and all named handles, including request-local insertions.
pub(crate) struct AppliedEdits {
    pub(crate) draft: WorkflowDraft,
    pub(crate) handles: Named,
}

/// Apply to a private draft. A refusal publishes nothing, including operations
/// preceding the refusal. Ordinary IR edits retain one transaction.
pub(crate) fn apply_operations(
    mut draft: WorkflowDraft,
    named: &Named,
    operations: Vec<EditOperation>,
) -> Result<AppliedEdits, EditError> {
    let mut named = named.clone();
    let mut pending = Vec::new();
    for operation in operations {
        if !matches!(
            operation,
            EditOperation::SetForm { .. }
                | EditOperation::InsertForm { .. }
                | EditOperation::InsertFormProcess { .. }
        ) {
            pending.push(typed_edit(&draft, &named, operation)?);
            continue;
        }
        transaction(&mut draft, std::mem::take(&mut pending))?;
        match operation {
            EditOperation::SetForm { node, data } => {
                let handle = handle(&named, &node)?;
                let edits = form_edits(&draft, handle, &data)?;
                transaction(&mut draft, edits)?;
            }
            EditOperation::InsertForm {
                id,
                body,
                before,
                data,
            } => {
                if named.contains_key(&id) {
                    return Err(EditError::UnknownNode { id });
                }
                let body = body_ref(&draft, &named, &body)?;
                let before = anchor(&named, before.as_ref())?;
                let old = draft.body(&body).unwrap_or_default();
                let node = crate::graph::form_node(
                    &id,
                    &data,
                    None,
                    draft.document(),
                    in_process(&draft, body.clone()),
                )
                .map_err(|e| EditError::Form(Box::new(e)))?;
                transaction(
                    &mut draft,
                    vec![
                        WorkflowEdit::SetBodyForm {
                            body: body.clone(),
                            form: WorkflowBodyForm::default(),
                        },
                        WorkflowEdit::InsertNode {
                            body: body.clone(),
                            before,
                            statement: statement(&node)?,
                        },
                    ],
                )?;
                let inserted = draft
                    .body(&body)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|handle| !old.contains(handle))
                    .ok_or_else(|| EditError::UnknownNode { id: id.clone() })?;
                named.insert(id, inserted);
            }
            EditOperation::InsertFormProcess { id, data } => {
                if named.contains_key(&id) {
                    return Err(EditError::UnknownNode { id });
                }
                let process = crate::graph::form_process(&id, &data, None)
                    .map_err(|e| EditError::Form(Box::new(e)))?;
                transaction(
                    &mut draft,
                    vec![WorkflowEdit::InsertProcess {
                        name: process.name.as_str().into(),
                        params: process.params,
                        return_ty: process.return_ty,
                    }],
                )?;
                let inserted = draft
                    .document()
                    .process(&process.name)
                    .and_then(|process| draft.handle(&process.id))
                    .ok_or_else(|| EditError::UnknownNode { id: id.clone() })?;
                named.insert(id, inserted);
            }
            _ => unreachable!("form operation was classified above"),
        }
    }
    transaction(&mut draft, pending)?;
    Ok(AppliedEdits {
        draft,
        handles: named,
    })
}

fn in_process(draft: &WorkflowDraft, mut body: WorkflowBodyRef) -> bool {
    loop {
        match body {
            WorkflowBodyRef::Main => return false,
            WorkflowBodyRef::Process(process) => {
                return draft
                    .process(process)
                    .is_some_and(|process| process.wrapper.is_some());
            }
            WorkflowBodyRef::Child { node, .. } => {
                let Some(parent) = draft.parent(node) else {
                    return false;
                };
                body = parent;
            }
        }
    }
}

fn form_edits(
    draft: &WorkflowDraft,
    handle: WorkflowDraftHandle,
    data: &crate::NodeData,
) -> Result<Vec<WorkflowEdit>, EditError> {
    let form_error = |e| EditError::Form(Box::new(e));
    if let Some(base) = draft.process(handle) {
        let process = crate::graph::form_process(base.id.as_str(), data, Some(base.clone()))
            .map_err(form_error)?;
        let mut edits = Vec::new();
        if base.name != process.name {
            edits.push(WorkflowEdit::RenameProcess {
                process: handle,
                name: process.name.as_str().into(),
            });
        }
        if base.params != process.params {
            // The edit sets the authored parameters; a lifted process keeps
            // the captures after them itself.
            let captures = match &process.origin {
                lash::vm::ir::ProcessOrigin::Lifted { hidden_params, .. } => {
                    *hidden_params as usize
                }
                lash::vm::ir::ProcessOrigin::Declared => 0,
            };
            let mut params = process.params;
            params.truncate(params.len().saturating_sub(captures));
            edits.push(WorkflowEdit::SetProcessSignature {
                process: handle,
                params,
                return_ty: base.return_ty.clone(),
            });
        }
        edits.push(WorkflowEdit::SetLabel {
            target: handle,
            label: label(
                process.name_source,
                &process.display_name,
                process.description.as_ref(),
            ),
        });
        return Ok(edits);
    }
    let base = draft.node(handle).ok_or_else(|| EditError::UnknownNode {
        id: format!("{handle:?}"),
    })?;
    let node = crate::graph::form_node(
        base.id.as_str(),
        data,
        Some(base),
        draft.document(),
        in_process(draft, draft.parent(handle).unwrap_or(WorkflowBodyRef::Main)),
    )
    .map_err(form_error)?;
    let mut edits = Vec::new();
    match (&base.kind, &node.kind) {
        (WorkflowNodeKind::Container(from), WorkflowNodeKind::Container(to)) => {
            if from.binding() != to.binding() {
                edits.push(WorkflowEdit::SetBinding {
                    node: handle,
                    binding: to.binding().cloned(),
                });
            }
            match to {
                WorkflowContainer::If { condition, .. }
                | WorkflowContainer::While { condition, .. } => {
                    edits.push(WorkflowEdit::SetCondition {
                        node: handle,
                        condition: condition.clone(),
                    });
                }
                WorkflowContainer::For {
                    element,
                    authored_element,
                    iterable,
                    bind,
                    ..
                } => {
                    edits.push(WorkflowEdit::SetLoopBinding {
                        node: handle,
                        element: element.as_str().into(),
                        authored_element: authored_element.as_ref().map(|v| v.as_str().into()),
                        bind: bind.clone(),
                    });
                    let mut slot = container_path(&own_statement(base)?);
                    slot.push(ExprSlot::Iterable);
                    edits.push(WorkflowEdit::ReplaceExpression {
                        target: lash::workflow::WorkflowExpressionRef::Node(handle),
                        slot: WorkflowSlotPath::structural(slot),
                        expression: iterable.clone(),
                    });
                }
                WorkflowContainer::Try { catch, finally, .. } => {
                    edits.push(WorkflowEdit::SetCatch {
                        node: handle,
                        binding: catch.as_ref().map(|c| c.binding.as_str().into()),
                    });
                    edits.push(WorkflowEdit::SetFinally {
                        node: handle,
                        present: finally.is_some(),
                    });
                }
                WorkflowContainer::Scope { .. } => {}
            }
        }
        _ => {
            if own_statement(base)? != own_statement(&node)? {
                edits.push(WorkflowEdit::ReplaceNode {
                    node: handle,
                    statement: statement(&node)?,
                });
            }
        }
    }
    edits.push(WorkflowEdit::SetLabel {
        target: handle,
        label: label(node.name_source, &node.name, node.description.as_ref()),
    });
    Ok(edits)
}

/// One expression of a node's statement that a generic editor can replace.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IrSlot {
    /// The slot path from the node's statement
    /// ([`EditOperation::ReplaceExpression`] takes it as `slot`).
    pub path: WorkflowSlotPath,
    /// The IR variant at the slot.
    pub variant: String,
    pub expression: Expr,
}

/// A node as the generic structured editor reads it: the statement it
/// spells and every expression of it that is not a child body's statement.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIr {
    pub statement: Expr,
    pub slots: Vec<IrSlot>,
}

fn variant(expression: &Expr) -> String {
    match serde_json::to_value(expression) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(serde_json::Value::Object(fields)) => fields.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

pub(crate) fn node_ir(node: &WorkflowNode) -> Result<NodeIr, EditError> {
    let statement = statement(node)?;
    // The statements of a container's child bodies are nodes of their own.
    let bodies = match &node.kind {
        WorkflowNodeKind::Container(container) => {
            let slots: &[ExprSlot] = match container {
                WorkflowContainer::If { .. } => &[ExprSlot::Then, ExprSlot::Else],
                WorkflowContainer::For { .. } | WorkflowContainer::While { .. } => {
                    &[ExprSlot::Body]
                }
                WorkflowContainer::Try { .. } => {
                    &[ExprSlot::Body, ExprSlot::Catch, ExprSlot::Finally]
                }
                WorkflowContainer::Scope { .. } => &[],
            };
            Some((container_path(&own_statement(node)?), slots))
        }
        _ => None,
    };
    fn walk(
        expression: &Expr,
        path: &mut Vec<ExprSlot>,
        bodies: Option<&(Vec<ExprSlot>, &[ExprSlot])>,
        slots: &mut Vec<IrSlot>,
    ) {
        let container = bodies.filter(|(at, _)| at == path);
        if let Some((_, [])) = container {
            // A scope's statements are its child body.
            return;
        }
        for (slot, child) in expression.slots() {
            if container.is_some_and(|(_, bodies)| bodies.contains(&slot)) {
                continue;
            }
            path.push(slot);
            slots.push(IrSlot {
                path: WorkflowSlotPath::structural(path.iter().copied()),
                variant: variant(child),
                expression: child.clone(),
            });
            walk(child, path, bodies, slots);
            path.pop();
        }
    }
    let mut slots = Vec::new();
    walk(&statement, &mut Vec::new(), bodies.as_ref(), &mut slots);
    Ok(NodeIr { statement, slots })
}
