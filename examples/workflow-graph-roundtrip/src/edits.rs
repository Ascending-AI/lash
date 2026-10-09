//! The typed edit transactions this host applies to a workflow.
//!
//! Every change to a saved workflow goes through
//! [`lash::workflow::WorkflowDraft`]. The form editor saves a whole document:
//! [`apply_document`] turns it into edits by node identity (a node the
//! document still names keeps its handle; one it names for the first time is
//! inserted; one it stopped naming is removed). The generic structured editor
//! sends [`EditOperation`]s, one for each [`WorkflowEdit`], with their IR as
//! JSON. Nothing here prints or parses a source dialect.

use std::collections::{BTreeMap, BTreeSet};

use lash::vm::ir::{
    AssignTarget, AstString, Expr, ExprSlot, FunctionDecl, LabelMetadata, ProcessParam, TypeExpr,
    WorkflowBodySlot, WorkflowContainer, WorkflowDeclaration, WorkflowNode, WorkflowNodeId,
    WorkflowNodeKind, WorkflowNodeNameSource, WorkflowProcess, WorkflowSlotPath, WorkflowSubgraph,
};
use lash::workflow::{
    WorkflowBindingRef, WorkflowBodyForm, WorkflowBodyRef, WorkflowDraft, WorkflowDraftHandle,
    WorkflowEdit, WorkflowEditRefusal, WorkflowEditTransaction, WorkflowGraph, WorkflowGraphError,
    WorkflowProcessWrapper, workflow_node_statement,
};
use serde::Deserialize;

/// Why a set of edits was not applied. The saved workflow is unchanged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EditError {
    #[error(transparent)]
    Refused(#[from] WorkflowEditRefusal),
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

/// A draft with a document's edits applied, and the handle of every node
/// and process container the document named.
pub(crate) struct AppliedDocument {
    pub(crate) draft: WorkflowDraft,
    pub(crate) handles: BTreeMap<String, WorkflowDraftHandle>,
}

/// An ordered body of a draft, as a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Body {
    Main,
    Process(WorkflowDraftHandle),
    Child(WorkflowDraftHandle, u8),
}

const SLOTS: [WorkflowBodySlot; 7] = [
    WorkflowBodySlot::Then,
    WorkflowBodySlot::Else,
    WorkflowBodySlot::LoopBody,
    WorkflowBodySlot::TryBody,
    WorkflowBodySlot::Catch,
    WorkflowBodySlot::Finally,
    WorkflowBodySlot::Scope,
];

impl Body {
    fn child(node: WorkflowDraftHandle, slot: WorkflowBodySlot) -> Self {
        let index = SLOTS.iter().position(|known| *known == slot).unwrap_or(0);
        Self::Child(node, u8::try_from(index).unwrap_or(0))
    }

    fn reference(self) -> WorkflowBodyRef {
        match self {
            Self::Main => WorkflowBodyRef::Main,
            Self::Process(process) => WorkflowBodyRef::Process(process),
            Self::Child(node, slot) => WorkflowBodyRef::Child {
                node,
                slot: SLOTS[usize::from(slot)],
            },
        }
    }
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

fn children(node: &WorkflowNode) -> Vec<(WorkflowBodySlot, &WorkflowSubgraph)> {
    match &node.kind {
        WorkflowNodeKind::Container(container) => container
            .child_subgraphs()
            .filter_map(|(name, body)| Some((body_slot(container, name)?, body)))
            .collect(),
        _ => Vec::new(),
    }
}

fn processes(graph: &WorkflowGraph) -> impl Iterator<Item = &WorkflowProcess> {
    graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            WorkflowDeclaration::Function(_) => None,
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

/// The target a statement node assigns its value to, for the kinds that
/// have one.
fn binding(kind: &WorkflowNodeKind) -> Option<&Option<AssignTarget>> {
    match kind {
        WorkflowNodeKind::Data { binding, .. }
        | WorkflowNodeKind::Call { binding, .. }
        | WorkflowNodeKind::Effect { binding, .. }
        | WorkflowNodeKind::Computation { binding, .. } => Some(binding),
        _ => None,
    }
}

fn binding_mut(kind: &mut WorkflowNodeKind) -> Option<&mut Option<AssignTarget>> {
    match kind {
        WorkflowNodeKind::Data { binding, .. }
        | WorkflowNodeKind::Call { binding, .. }
        | WorkflowNodeKind::Effect { binding, .. }
        | WorkflowNodeKind::Computation { binding, .. } => Some(binding),
        _ => None,
    }
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

/// Builds the transaction that makes a draft of `base` spell `target`.
struct Script<'d, 'a> {
    draft: &'d WorkflowDraft,
    /// The handle of each node the base document names.
    named: &'d BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    /// The handle of every node of the target the script kept or placed.
    handles: BTreeMap<String, WorkflowDraftHandle>,
    base_nodes: BTreeMap<&'a WorkflowNodeId, &'a WorkflowNode>,
    /// Each body as the edits so far leave it; `None` is a node the script
    /// inserted.
    bodies: BTreeMap<Body, Vec<Option<WorkflowDraftHandle>>>,
    /// Inserts, moves and the header edits that add: applied first, in
    /// order.
    structure: Vec<WorkflowEdit>,
    removals: Vec<WorkflowEdit>,
    /// Whole-statement replacements and the header edits that remove a
    /// body: applied last, once nothing still has to move out.
    replacements: Vec<WorkflowEdit>,
    kept: BTreeSet<WorkflowDraftHandle>,
    replaced: Vec<(WorkflowDraftHandle, &'a WorkflowNode)>,
    /// Each inserted node with its body, its index there and how many
    /// statements the body ends with.
    inserted: Vec<(Body, usize, usize, &'a WorkflowNode)>,
}

impl<'a> Script<'_, 'a> {
    fn load(&mut self, key: Body) {
        let Some(handles) = self.draft.body(&key.reference()) else {
            return;
        };
        for handle in &handles {
            let Some(node) = self.draft.node(*handle) else {
                continue;
            };
            for (slot, _) in children(node) {
                self.load(Body::child(*handle, slot));
            }
        }
        self.bodies
            .insert(key, handles.into_iter().map(Some).collect());
    }

    fn body(
        &mut self,
        key: Body,
        base: Option<&WorkflowSubgraph>,
        target: &'a WorkflowSubgraph,
    ) -> Result<(), EditError> {
        if base.is_some_and(|base| !base.is_statement_list()) && target.nodes.len() != 1 {
            self.structure.push(WorkflowEdit::SetBodyForm {
                body: key.reference(),
                form: WorkflowBodyForm::default(),
            });
        }
        for (index, node) in target.nodes.iter().enumerate() {
            let existing = self
                .named
                .get(&node.id)
                .copied()
                .filter(|handle| !self.kept.contains(handle));
            let current = self.bodies.entry(key).or_default();
            let before = current.get(index).copied().flatten();
            let Some(handle) = existing else {
                current.insert(index, None);
                self.structure.push(WorkflowEdit::InsertNode {
                    body: key.reference(),
                    before,
                    statement: statement(node)?,
                });
                self.inserted.push((key, index, target.nodes.len(), node));
                continue;
            };
            self.kept.insert(handle);
            self.handles.insert(node.id.to_string(), handle);
            if before != Some(handle) {
                for body in self.bodies.values_mut() {
                    body.retain(|entry| *entry != Some(handle));
                }
                self.bodies
                    .entry(key)
                    .or_default()
                    .insert(index, Some(handle));
                self.structure.push(WorkflowEdit::MoveNode {
                    node: handle,
                    body: key.reference(),
                    before,
                });
            }
            self.node(handle, node)?;
        }
        Ok(())
    }

    fn node(
        &mut self,
        handle: WorkflowDraftHandle,
        target: &'a WorkflowNode,
    ) -> Result<(), EditError> {
        let Some(base) = self.base_nodes.get(&target.id).copied() else {
            return Ok(());
        };
        if own_statement(base)? != own_statement(target)? {
            let titled = |node: &WorkflowNode| {
                label(node.name_source, &node.name, node.description.as_ref())
            };
            match (&base.kind, &target.kind) {
                (WorkflowNodeKind::Container(from), WorkflowNodeKind::Container(to))
                    if std::mem::discriminant(from) == std::mem::discriminant(to) =>
                {
                    self.header(handle, base, from, to)?;
                }
                _ => {
                    // A statement that differs only in what it binds or how
                    // it is labelled keeps its expression untouched.
                    let mut renamed = base.clone();
                    renamed.name.clone_from(&target.name);
                    renamed.description.clone_from(&target.description);
                    renamed.name_source = target.name_source;
                    let rebound = binding_mut(&mut renamed.kind)
                        .zip(binding(&target.kind))
                        .map(|(binding, target)| binding.clone_from(target))
                        .is_some();
                    if own_statement(&renamed)? != own_statement(target)? {
                        return self.replace(handle, target);
                    }
                    if rebound && binding(&base.kind) != binding(&target.kind) {
                        self.structure.push(WorkflowEdit::SetBinding {
                            node: handle,
                            binding: binding(&target.kind).cloned().flatten(),
                        });
                    }
                }
            }
            if titled(base) != titled(target) {
                self.structure.push(WorkflowEdit::SetLabel {
                    target: handle,
                    label: titled(target),
                });
            }
        }
        let base_children = children(base);
        for (slot, body) in children(target) {
            let base = base_children
                .iter()
                .find_map(|(base, body)| (*base == slot).then_some(*body));
            self.body(Body::child(handle, slot), base, body)?;
        }
        Ok(())
    }

    fn replace(
        &mut self,
        handle: WorkflowDraftHandle,
        target: &'a WorkflowNode,
    ) -> Result<(), EditError> {
        self.replacements.push(WorkflowEdit::ReplaceNode {
            node: handle,
            statement: statement(target)?,
        });
        self.replaced.push((handle, target));
        Ok(())
    }

    /// The edits of a container's own fields, child bodies aside.
    fn header(
        &mut self,
        node: WorkflowDraftHandle,
        base: &WorkflowNode,
        from: &WorkflowContainer,
        to: &WorkflowContainer,
    ) -> Result<(), EditError> {
        if from.binding() != to.binding() {
            self.structure.push(WorkflowEdit::SetBinding {
                node,
                binding: to.binding().cloned(),
            });
        }
        match (from, to) {
            (
                WorkflowContainer::If {
                    condition: from, ..
                },
                WorkflowContainer::If { condition: to, .. },
            )
            | (
                WorkflowContainer::While {
                    condition: from, ..
                },
                WorkflowContainer::While { condition: to, .. },
            ) => {
                if from != to {
                    self.structure.push(WorkflowEdit::SetCondition {
                        node,
                        condition: to.clone(),
                    });
                }
            }
            (
                WorkflowContainer::For {
                    element: from_element,
                    authored_element: from_authored,
                    iterable: from_iterable,
                    bind: from_bind,
                    ..
                },
                WorkflowContainer::For {
                    element,
                    authored_element,
                    iterable,
                    bind,
                    ..
                },
            ) => {
                if (from_element, from_authored, from_bind) != (element, authored_element, bind) {
                    self.structure.push(WorkflowEdit::SetLoopBinding {
                        node,
                        element: element.as_str().into(),
                        authored_element: authored_element
                            .as_ref()
                            .map(|name| name.as_str().into()),
                        bind: bind.clone(),
                    });
                }
                if from_iterable != iterable {
                    let mut slot = container_path(&own_statement(base)?);
                    slot.push(ExprSlot::Iterable);
                    self.structure.push(WorkflowEdit::ReplaceExpression {
                        node,
                        slot: WorkflowSlotPath::structural(slot),
                        expression: iterable.clone(),
                    });
                }
            }
            (
                WorkflowContainer::Try {
                    catch: from_catch,
                    finally: from_finally,
                    ..
                },
                WorkflowContainer::Try { catch, finally, .. },
            ) => {
                let binding = |catch: &Option<lash::workflow::WorkflowCatch>| {
                    catch
                        .as_ref()
                        .map(|catch| AstString::from(catch.binding.as_str()))
                };
                if binding(from_catch) != binding(catch) {
                    let edit = WorkflowEdit::SetCatch {
                        node,
                        binding: binding(catch),
                    };
                    match catch {
                        Some(_) => self.structure.push(edit),
                        None => self.replacements.push(edit),
                    }
                }
                if from_finally.is_some() != finally.is_some() {
                    let edit = WorkflowEdit::SetFinally {
                        node,
                        present: finally.is_some(),
                    };
                    match finally {
                        Some(_) => self.structure.push(edit),
                        None => self.replacements.push(edit),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Removes every base node the target stopped naming. A removed or
    /// replaced node takes what is still under it along.
    fn remove(&mut self, body: &WorkflowSubgraph) {
        for node in &body.nodes {
            let Some(handle) = self.named.get(&node.id).copied() else {
                continue;
            };
            if !self.kept.contains(&handle) {
                self.removals
                    .push(WorkflowEdit::RemoveNode { node: handle });
                continue;
            }
            if self
                .replaced
                .iter()
                .any(|(replaced, _)| *replaced == handle)
            {
                continue;
            }
            for (_, child) in children(node) {
                self.remove(child);
            }
        }
    }
}

/// Records the handle of `node` and, where the draft kept its child bodies
/// statement for statement, of everything under it.
fn adopt(
    draft: &WorkflowDraft,
    handle: WorkflowDraftHandle,
    node: &WorkflowNode,
    handles: &mut BTreeMap<String, WorkflowDraftHandle>,
) {
    handles.insert(node.id.to_string(), handle);
    for (slot, body) in children(node) {
        let placed = draft
            .body(&WorkflowBodyRef::Child { node: handle, slot })
            .unwrap_or_default();
        if placed.len() != body.nodes.len() {
            continue;
        }
        for (handle, node) in placed.into_iter().zip(&body.nodes) {
            adopt(draft, handle, node, handles);
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

/// Edits `draft` into `target`. `base` is the document the host served for
/// the draft and `named` the handle of each node and process container it
/// names; the surviving ones keep their ids in `target`.
pub(crate) fn apply_document(
    mut draft: WorkflowDraft,
    named: &BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    base: &WorkflowGraph,
    target: &WorkflowGraph,
) -> Result<AppliedDocument, EditError> {
    let mut handles = BTreeMap::new();
    let base_processes = processes(base)
        .map(|process| (&process.id, process))
        .collect::<BTreeMap<_, _>>();

    // A process container has to exist before anything can be put in it.
    let created = processes(target)
        .filter(|process| !base_processes.contains_key(&process.id))
        .collect::<Vec<_>>();
    transaction(
        &mut draft,
        created
            .iter()
            .map(|process| WorkflowEdit::InsertProcess {
                name: process.name.as_str().into(),
                params: process.params.clone(),
                return_ty: process.return_ty.clone(),
            })
            .collect(),
    )?;
    let mut process_handles = BTreeMap::new();
    for process in processes(target) {
        let handle = match base_processes.get(&process.id) {
            Some(base) => named.get(&base.id).copied(),
            None => draft
                .document()
                .process(&process.name)
                .and_then(|created| draft.handle(&created.id)),
        };
        let handle = handle.ok_or_else(|| EditError::UnknownNode {
            id: process.id.to_string(),
        })?;
        process_handles.insert(&process.id, handle);
        handles.insert(process.id.to_string(), handle);
    }

    let (edits, replaced, inserted) = {
        let mut script = Script {
            draft: &draft,
            named,
            handles: BTreeMap::new(),
            base_nodes: base.nodes().map(|node| (&node.id, node)).collect(),
            bodies: BTreeMap::new(),
            structure: Vec::new(),
            removals: Vec::new(),
            replacements: Vec::new(),
            kept: BTreeSet::new(),
            replaced: Vec::new(),
            inserted: Vec::new(),
        };
        script.load(Body::Main);
        for handle in process_handles.values() {
            script.load(Body::Process(*handle));
        }
        for process in processes(target) {
            let handle = process_handles[&process.id];
            let base = base_processes.get(&process.id).copied();
            let declared = |process: &WorkflowProcess| {
                label(
                    process.name_source,
                    &process.display_name,
                    process.description.as_ref(),
                )
            };
            if let Some(base) = base {
                if base.name != process.name {
                    script.structure.push(WorkflowEdit::RenameProcess {
                        process: handle,
                        name: process.name.as_str().into(),
                    });
                }
                if (&base.params, &base.return_ty) != (&process.params, &process.return_ty) {
                    // The served signature is the admitted one, with the
                    // types the linker derived. What the form left alone
                    // keeps the type the draft declares for it.
                    let authored = draft.process(handle);
                    let params = process
                        .params
                        .iter()
                        .enumerate()
                        .map(|(index, param)| {
                            let kept = authored
                                .and_then(|authored| authored.params.get(index))
                                .filter(|authored| {
                                    authored.name == param.name
                                        && base.params.get(index) == Some(param)
                                });
                            kept.unwrap_or(param).clone()
                        })
                        .collect();
                    let return_ty = if base.return_ty == process.return_ty {
                        authored.and_then(|authored| authored.return_ty.clone())
                    } else {
                        process.return_ty.clone()
                    };
                    script.structure.push(WorkflowEdit::SetProcessSignature {
                        process: handle,
                        params,
                        return_ty,
                    });
                }
            }
            if base.is_none_or(|base| base.wrapper != process.wrapper) {
                script.structure.push(WorkflowEdit::SetProcessWrapper {
                    process: handle,
                    wrapper: process.wrapper.as_deref().cloned(),
                });
            }
            if base.and_then(declared) != declared(process) {
                script.structure.push(WorkflowEdit::SetLabel {
                    target: handle,
                    label: declared(process),
                });
            }
            script.body(
                Body::Process(handle),
                base.map(|base| &base.body),
                &process.body,
            )?;
        }
        script.body(Body::Main, Some(&base.main), &target.main)?;

        let surviving = processes(target)
            .map(|process| &process.id)
            .collect::<BTreeSet<_>>();
        script.remove(&base.main);
        for process in processes(base) {
            if surviving.contains(&process.id) {
                script.remove(&process.body);
            } else if let Some(handle) = named.get(&process.id).copied() {
                script
                    .replacements
                    .push(WorkflowEdit::RemoveProcess { process: handle });
            }
        }
        handles.append(&mut script.handles);
        let mut edits = script.structure;
        edits.extend(script.removals);
        edits.extend(script.replacements);
        (
            edits,
            script.replaced,
            script
                .inserted
                .into_iter()
                .map(|(body, index, length, node)| (body.reference(), index, length, node))
                .collect::<Vec<_>>(),
        )
    };
    transaction(&mut draft, edits)?;

    for (handle, node) in replaced {
        adopt(&draft, handle, node, &mut handles);
    }
    // The script placed each inserted statement at its index, and the draft
    // lists that statement there unless normalization split a statement.
    for (body, index, length, node) in inserted {
        let placed = draft.body(&body).unwrap_or_default();
        if placed.len() == length
            && let Some(handle) = placed.get(index)
        {
            adopt(&draft, *handle, node, &mut handles);
        }
    }
    Ok(AppliedDocument { draft, handles })
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
type Named = BTreeMap<WorkflowNodeId, WorkflowDraftHandle>;

fn handle(named: &Named, id: &str) -> Result<WorkflowDraftHandle, EditError> {
    named
        .iter()
        .find_map(|(named, handle)| (named.as_str() == id).then_some(*handle))
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
            node: handle(named, &node)?,
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

/// Applies `operations` to `draft` as one transaction. `named` is the
/// handle of each node under the id the host served it with.
pub(crate) fn apply_operations(
    mut draft: WorkflowDraft,
    named: &Named,
    operations: Vec<EditOperation>,
) -> Result<WorkflowDraft, EditError> {
    let edits = operations
        .into_iter()
        .map(|operation| typed_edit(&draft, named, operation))
        .collect::<Result<Vec<_>, _>>()?;
    transaction(&mut draft, edits)?;
    Ok(draft)
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
