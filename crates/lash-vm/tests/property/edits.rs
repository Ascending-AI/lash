//! Random sequences of typed workflow edits (FIG-5578).
//!
//! A script of choices drives transactions against a draft. Each edit names
//! its subjects among the handles the draft has when the edit is built, with
//! a few drawn from handles it no longer has, and states its content as IR
//! the generator builds. Every edit kind is reachable, and nothing steers the
//! script towards edits that apply: the law is what a transaction leaves
//! whether it applies or is refused.

use std::collections::{BTreeMap, BTreeSet};

use lash_vm::testing::ast_builders as b;
use lash_vm::testing::workflow_edits::workflow_edit_kind;
use lash_vm::{
    AssignTarget, AstString, CoercingBinaryOp, Declaration, Expr, ExprSlot, ExprSlotVisitor,
    Program, TypeExpr, WorkflowBindingRef, WorkflowBodyForm, WorkflowBodyRef, WorkflowBodySlot,
    WorkflowContainer, WorkflowCorrespondence, WorkflowCorrespondenceEntry as Entry,
    WorkflowDeclaration, WorkflowDraft, WorkflowDraftHandle, WorkflowDraftRevision,
    WorkflowEdgeDrag, WorkflowEdit, WorkflowEditTransaction, WorkflowNodeId, WorkflowNodeKind,
    WorkflowProcessWrapper, WorkflowSlotPath, walk_expr_slots, workflow_graph_from_program,
    workflow_node_statement, workflow_program_from_graph,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::ir_gen;

/// What one script did to one draft.
#[derive(Default)]
pub struct Fuzzed {
    /// The kind of every edit of an applied transaction.
    pub applied: Vec<&'static str>,
    pub expression_targets: Vec<&'static str>,
    /// The diagnostic code of every refusal, with the kind of the edit it
    /// names (`transaction` for a check of the whole result).
    pub refused: Vec<(&'static str, &'static str)>,
    /// Every program an applied transaction left.
    pub programs: Vec<Program>,
}

const BODY_SLOTS: [WorkflowBodySlot; 7] = [
    WorkflowBodySlot::Then,
    WorkflowBodySlot::Else,
    WorkflowBodySlot::LoopBody,
    WorkflowBodySlot::TryBody,
    WorkflowBodySlot::Catch,
    WorkflowBodySlot::Finally,
    WorkflowBodySlot::Scope,
];

struct Script<'s> {
    words: &'s [u16],
    at: usize,
    names: usize,
    /// Handles the draft had once, which a later edit may still name.
    retired: Vec<WorkflowDraftHandle>,
}

/// What a draft holds when an edit is built.
struct View {
    nodes: Vec<WorkflowDraftHandle>,
    processes: Vec<WorkflowDraftHandle>,
    bodies: Vec<WorkflowBodyRef>,
    functions: Vec<AstString>,
    process_names: Vec<String>,
    /// The `if` and `while` nodes, which have a condition.
    conditional: Vec<WorkflowDraftHandle>,
    /// The `for` nodes, each with the element it binds.
    loops: Vec<(WorkflowDraftHandle, AstString)>,
    tries: Vec<WorkflowDraftHandle>,
}

fn fail(error: &dyn std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

/// Every node and process container of the draft's document, by handle.
fn live(
    draft: &WorkflowDraft,
) -> Result<BTreeMap<WorkflowDraftHandle, WorkflowNodeId>, TestCaseError> {
    let document = draft.document();
    let ids = document
        .nodes()
        .map(|node| &node.id)
        .chain(
            document
                .declarations
                .iter()
                .filter_map(|declaration| match declaration {
                    WorkflowDeclaration::Process(process) => Some(&process.id),
                    WorkflowDeclaration::Function(_) => None,
                }),
        );
    let mut handles = BTreeMap::new();
    for id in ids {
        let Some(handle) = draft.handle(id) else {
            return Err(TestCaseError::fail(format!("`{id}` has no handle")));
        };
        prop_assert_eq!(draft.node_id(handle), Some(id));
        prop_assert!(
            handles.insert(handle, id.clone()).is_none(),
            "two nodes share {} at {}",
            handle,
            id
        );
    }
    Ok(handles)
}

fn view(draft: &WorkflowDraft) -> View {
    let document = draft.document();
    let mut nodes = Vec::new();
    let mut conditional = Vec::new();
    let mut loops = Vec::new();
    let mut tries = Vec::new();
    for node in document.nodes() {
        let Some(handle) = draft.handle(&node.id) else {
            continue;
        };
        nodes.push(handle);
        match &node.kind {
            WorkflowNodeKind::Container(
                WorkflowContainer::If { .. } | WorkflowContainer::While { .. },
            ) => conditional.push(handle),
            WorkflowNodeKind::Container(WorkflowContainer::For { element, .. }) => {
                loops.push((handle, AstString::from(element.as_str())));
            }
            WorkflowNodeKind::Container(WorkflowContainer::Try { .. }) => tries.push(handle),
            _ => {}
        }
    }
    let mut processes = Vec::new();
    let mut functions = Vec::new();
    let mut process_names = Vec::new();
    for declaration in &document.declarations {
        match declaration {
            WorkflowDeclaration::Process(process) => {
                processes.extend(draft.handle(&process.id));
                process_names.push(process.name.clone());
            }
            WorkflowDeclaration::Function(function) => functions.push(function.name.clone()),
        }
    }
    let mut bodies = vec![WorkflowBodyRef::Main];
    bodies.extend(processes.iter().copied().map(WorkflowBodyRef::Process));
    for node in &nodes {
        for slot in BODY_SLOTS {
            let body = WorkflowBodyRef::Child { node: *node, slot };
            if draft.body(&body).is_some() {
                bodies.push(body);
            }
        }
    }
    View {
        nodes,
        processes,
        bodies,
        functions,
        process_names,
        conditional,
        loops,
        tries,
    }
}

/// Every name the statement of `handle` reads or binds.
fn names_of(draft: &WorkflowDraft, handle: WorkflowDraftHandle) -> Vec<AstString> {
    fn collect(expr: &Expr, names: &mut BTreeSet<AstString>) {
        match expr {
            Expr::Variable(name) => {
                names.insert(name.clone());
            }
            Expr::Assign { target, .. } => {
                names.insert(target.root.clone());
            }
            Expr::For { binding, .. } => {
                names.insert(binding.clone());
            }
            Expr::Try(region) => {
                names.extend(region.catch.iter().map(|catch| catch.binding.clone()));
            }
            _ => {}
        }
        for child in expr.children() {
            collect(child, names);
        }
    }
    let mut names = BTreeSet::new();
    if let Some(statement) = draft
        .node(handle)
        .and_then(|node| workflow_node_statement(node).ok())
    {
        collect(&statement, &mut names);
    }
    names.into_iter().collect()
}

/// Every slot path of the statement of `handle`.
fn slots_of(draft: &WorkflowDraft, handle: WorkflowDraftHandle) -> Vec<Vec<ExprSlot>> {
    struct Paths(Vec<Vec<ExprSlot>>);
    impl ExprSlotVisitor for Paths {
        fn visit_slot(&mut self, path: &[ExprSlot], _expr: &Expr) {
            self.0.push(path.to_vec());
        }
    }
    let mut paths = Paths(Vec::new());
    if let Some(statement) = draft
        .node(handle)
        .and_then(|node| workflow_node_statement(node).ok())
    {
        walk_expr_slots(&mut paths, &statement);
    }
    paths.0
}

fn expression_slots(expression: &Expr) -> Vec<Vec<ExprSlot>> {
    struct Paths(Vec<Vec<ExprSlot>>);
    impl ExprSlotVisitor for Paths {
        fn visit_slot(&mut self, path: &[ExprSlot], _expr: &Expr) {
            self.0.push(path.to_vec());
        }
    }
    let mut paths = Paths(vec![Vec::new()]);
    walk_expr_slots(&mut paths, expression);
    paths.0
}

impl Script<'_> {
    fn pick(&mut self, options: usize) -> usize {
        let word = self.words.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        usize::from(word) % options.max(1)
    }

    fn spent(&self) -> bool {
        self.at >= self.words.len()
    }

    /// The next few choices, for a generator of IR.
    fn take(&mut self, count: usize) -> Vec<u16> {
        let words = (0..count)
            .map(|offset| self.words.get(self.at + offset).copied().unwrap_or(0))
            .collect();
        self.at += count;
        words
    }

    fn fresh(&mut self) -> AstString {
        self.names += 1;
        AstString::from(format!("n{}", self.names))
    }

    /// A handle of `preferred` most of the time, when it has one: the nodes
    /// an edit of one kind of node applies to.
    fn prefer(
        &mut self,
        preferred: &[WorkflowDraftHandle],
        otherwise: WorkflowDraftHandle,
    ) -> WorkflowDraftHandle {
        if preferred.is_empty() || self.pick(4) == 3 {
            otherwise
        } else {
            preferred[self.pick(preferred.len())]
        }
    }

    /// A handle of `pool`, or now and then one the draft retired.
    fn handle(&mut self, pool: &[WorkflowDraftHandle]) -> Option<WorkflowDraftHandle> {
        if !self.retired.is_empty() && self.pick(12) == 11 {
            let index = self.pick(self.retired.len());
            return Some(self.retired[index]);
        }
        (!pool.is_empty()).then(|| pool[self.pick(pool.len())])
    }

    fn body(&mut self, view: &View) -> WorkflowBodyRef {
        view.bodies[self.pick(view.bodies.len())].clone()
    }

    /// A statement of `body` to place another before, or its end.
    fn anchor(
        &mut self,
        draft: &WorkflowDraft,
        body: &WorkflowBodyRef,
    ) -> Option<WorkflowDraftHandle> {
        let statements = draft.body(body).unwrap_or_default();
        match self.pick(statements.len() + 1) {
            0 => None,
            index => Some(statements[index - 1]),
        }
    }

    fn statement(&mut self) -> Expr {
        let words = self.take(12);
        self.names += 1;
        ir_gen::closed_statement(&words, &format!("e{}_", self.names))
    }

    fn expression(&mut self) -> Expr {
        let words = self.take(6);
        ir_gen::closed_expression(&words)
    }

    fn name(&mut self, taken: &[AstString]) -> AstString {
        if !taken.is_empty() && self.pick(5) == 4 {
            taken[self.pick(taken.len())].clone()
        } else {
            self.fresh()
        }
    }

    /// One edit against what `draft` holds now.
    fn edit(&mut self, draft: &WorkflowDraft, view: &View) -> WorkflowEdit {
        let insert = |script: &mut Self| {
            let body = script.body(view);
            let before = script.anchor(draft, &body);
            WorkflowEdit::InsertNode {
                body,
                before,
                statement: script.statement(),
            }
        };
        let kind = self.pick(28);
        let node = self.handle(&view.nodes);
        let process = self.handle(&view.processes);
        match (kind, node, process) {
            (0 | 1, _, _) => insert(self),
            (2, Some(node), _) => {
                let body = self.body(view);
                let before = self.anchor(draft, &body);
                WorkflowEdit::CloneNode { node, body, before }
            }
            (3, Some(node), _) => WorkflowEdit::RemoveNode { node },
            (4 | 5, Some(node), _) => {
                let body = self.body(view);
                let before = self.anchor(draft, &body);
                WorkflowEdit::MoveNode { node, body, before }
            }
            (6, Some(node), _) => WorkflowEdit::ReplaceNode {
                node,
                statement: self.statement(),
            },
            (7 | 8, Some(node), _) => {
                let slots = slots_of(draft, node);
                let slot = match slots.len() {
                    0 => vec![ExprSlot::Operand],
                    count => slots[self.pick(count)].clone(),
                };
                WorkflowEdit::ReplaceExpression {
                    target: lash_vm::WorkflowExpressionRef::Node(node),
                    slot: WorkflowSlotPath::structural(slot),
                    expression: self.expression(),
                }
            }
            (26, _, _) if !view.functions.is_empty() => {
                let name = view.functions[self.pick(view.functions.len())].clone();
                let Some(function) =
                    draft.document().declarations.iter().find_map(
                        |declaration| match declaration {
                            WorkflowDeclaration::Function(function) if function.name == name => {
                                Some(function)
                            }
                            _ => None,
                        },
                    )
                else {
                    return insert(self);
                };
                let paths = expression_slots(&function.body);
                WorkflowEdit::ReplaceExpression {
                    target: lash_vm::WorkflowExpressionRef::Function(name),
                    slot: WorkflowSlotPath::structural(paths[self.pick(paths.len())].clone()),
                    expression: self.expression(),
                }
            }
            (27, _, Some(process)) => {
                let mut paths = Vec::new();
                if let Some(wrapper) = draft
                    .process(process)
                    .and_then(|process| process.wrapper.as_ref())
                {
                    for (index, argument) in (0u32..).zip(&wrapper.arguments) {
                        for mut path in expression_slots(argument) {
                            path.insert(0, ExprSlot::Arg(index));
                            paths.push(path);
                        }
                    }
                    if let Some(driver) = &wrapper.driver {
                        for (index, argument) in (1u32..).zip(&driver.arguments) {
                            for mut path in expression_slots(argument) {
                                path.splice(0..0, [ExprSlot::Callee, ExprSlot::Arg(index)]);
                                paths.push(path);
                            }
                        }
                    }
                }
                let slot = if paths.is_empty() {
                    vec![ExprSlot::Arg(0)]
                } else {
                    paths[self.pick(paths.len())].clone()
                };
                WorkflowEdit::ReplaceExpression {
                    target: lash_vm::WorkflowExpressionRef::ProcessWrapper(process),
                    slot: WorkflowSlotPath::structural(slot),
                    expression: self.expression(),
                }
            }
            (9, Some(node), _) => WorkflowEdit::SetBinding {
                node,
                binding: (self.pick(3) != 0).then(|| AssignTarget::variable(self.fresh())),
            },
            (10 | 11, Some(node), _) => {
                let names = names_of(draft, node);
                let from = match names.len() {
                    0 => self.fresh(),
                    count => names[self.pick(count)].clone(),
                };
                WorkflowEdit::RenameBinding {
                    binding: WorkflowBindingRef::Variable {
                        at: node,
                        name: from,
                    },
                    name: self.name(&names),
                }
            }
            (12, Some(node), _) => WorkflowEdit::SetCondition {
                node: self.prefer(&view.conditional, node),
                condition: b::binary(
                    self.expression(),
                    CoercingBinaryOp::StrictEqual,
                    self.expression(),
                ),
            },
            (13, node, process) => {
                let target = match (self.pick(2), node, process) {
                    (0, Some(node), _) | (_, Some(node), None) => node,
                    (_, _, Some(process)) => process,
                    (_, None, None) => return insert(self),
                };
                WorkflowEdit::SetLabel {
                    target,
                    label: (self.pick(3) != 0)
                        .then(|| b::label("Edited", (self.pick(2) == 1).then_some("by a host"))),
                }
            }
            (14, Some(node), _) => {
                // The element a loop already binds keeps its body's reads
                // bound; a new one leaves them to the binding check.
                let bound = (!view.loops.is_empty() && self.pick(4) != 3)
                    .then(|| view.loops[self.pick(view.loops.len())].clone());
                let (node, element) = match bound {
                    Some((node, element)) if self.pick(2) == 0 => (node, element),
                    Some((node, _)) => (node, self.fresh()),
                    None => (node, self.fresh()),
                };
                WorkflowEdit::SetLoopBinding {
                    node,
                    element,
                    authored_element: (self.pick(2) == 1).then(|| self.fresh()),
                    bind: None,
                }
            }
            (15, Some(node), _) => WorkflowEdit::SetCatch {
                node: self.prefer(&view.tries, node),
                binding: (self.pick(3) != 0).then(|| self.fresh()),
            },
            (16, Some(node), _) => WorkflowEdit::SetFinally {
                node: self.prefer(&view.tries, node),
                present: self.pick(2) == 1,
            },
            (17, _, _) => WorkflowEdit::SetBodyForm {
                body: self.body(view),
                form: match self.pick(3) {
                    0 => WorkflowBodyForm::default(),
                    1 => WorkflowBodyForm::Completion {
                        value: Box::new(Expr::Absent),
                        groups: Vec::new(),
                    },
                    _ => WorkflowBodyForm::Statement,
                },
            },
            (18, _, _) => {
                let taken = view
                    .process_names
                    .iter()
                    .map(|name| AstString::from(name.as_str()))
                    .collect::<Vec<_>>();
                WorkflowEdit::InsertProcess {
                    name: self.name(&taken),
                    params: vec![b::param("value", TypeExpr::Any)],
                    return_ty: (self.pick(2) == 1).then_some(TypeExpr::Any),
                }
            }
            (19, _, Some(process)) => WorkflowEdit::RemoveProcess { process },
            (20, _, Some(process)) => {
                let taken = view
                    .process_names
                    .iter()
                    .map(|name| AstString::from(name.as_str()))
                    .collect::<Vec<_>>();
                WorkflowEdit::RenameProcess {
                    process,
                    name: self.name(&taken),
                }
            }
            (21, _, Some(process)) => WorkflowEdit::SetProcessSignature {
                process,
                params: [
                    vec![b::param("value", TypeExpr::Any)],
                    vec![
                        b::param("value", TypeExpr::Any),
                        b::param("extra", TypeExpr::Any),
                    ],
                    Vec::new(),
                ][self.pick(3)]
                .clone(),
                return_ty: (self.pick(2) == 1).then_some(TypeExpr::Any),
            },
            (22, _, Some(process)) => {
                let params = draft
                    .process(process)
                    .map(|process| {
                        process
                            .params
                            .iter()
                            .map(|param| param.name.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                WorkflowEdit::SetProcessWrapper {
                    process,
                    wrapper: (self.pick(3) != 0).then(|| WorkflowProcessWrapper {
                        name: None,
                        js_name: None,
                        receiver: None,
                        arguments: params.iter().map(|name| b::var(name)).collect(),
                        params,
                        captures: Vec::new(),
                        driver: None,
                        catch_binding: "__process_error".into(),
                    }),
                }
            }
            (23, _, _) => {
                let name = self.name(&view.functions);
                let declared = view.functions.contains(&name);
                let Declaration::Function(function) = b::function_decl(
                    &name,
                    vec![b::function_param("value", TypeExpr::Any)],
                    TypeExpr::Any,
                    b::binary(b::var("value"), CoercingBinaryOp::Add, self.expression()),
                ) else {
                    unreachable!("the builder declares a function")
                };
                if declared && self.pick(2) == 0 {
                    WorkflowEdit::ReplaceFunction { function }
                } else if declared && self.pick(2) == 0 {
                    WorkflowEdit::RemoveFunction { name }
                } else {
                    WorkflowEdit::InsertFunction { function }
                }
            }
            (24, _, _) => {
                let mut bindings = draft.document().private_bindings.clone();
                match self.pick(3) {
                    0 => bindings.clear(),
                    1 => {
                        bindings.insert(self.fresh());
                    }
                    _ => {
                        let first = bindings.iter().next().cloned();
                        if let Some(first) = first {
                            bindings.remove(&first);
                        }
                    }
                }
                WorkflowEdit::SetPrivateBindings { bindings }
            }
            (25, Some(from), _) => {
                let Some(to) = self.handle(&view.nodes) else {
                    return insert(self);
                };
                let drag = if self.pick(2) == 0 {
                    WorkflowEdgeDrag::Sequence { from, to }
                } else {
                    let slots = slots_of(draft, to);
                    let slot = match slots.len() {
                        0 => vec![ExprSlot::Operand],
                        count => slots[self.pick(count)].clone(),
                    };
                    WorkflowEdgeDrag::Data {
                        from,
                        to,
                        slot: WorkflowSlotPath::structural(slot),
                    }
                };
                // A drag that names no edit is the insert below: what an
                // edge drag refuses is pinned by the draft's own laws.
                draft
                    .edit_for_edge_drag(&drag)
                    .unwrap_or_else(|_| insert(self))
            }
            (kind, _, _) if kind % 2 == 0 => WorkflowEdit::RemoveFunction {
                name: self.name(&view.functions),
            },
            _ => insert(self),
        }
    }
}

/// Checks that `correspondence` accounts for every handle of `base` and of
/// `now`, once, with the ids each had.
fn correspondence_is_total(
    correspondence: &WorkflowCorrespondence,
    base: &BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
    now: &BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
) -> Result<(), TestCaseError> {
    let mut from_base = BTreeSet::new();
    let mut to_now = BTreeSet::new();
    let left = |handle: &WorkflowDraftHandle,
                from: &WorkflowNodeId,
                seen: &mut BTreeSet<WorkflowDraftHandle>|
     -> Result<(), TestCaseError> {
        prop_assert_eq!(base.get(handle), Some(from), "{} in the base", handle);
        prop_assert!(seen.insert(*handle), "{} has two outcomes", handle);
        Ok(())
    };
    let reached = |handle: &WorkflowDraftHandle,
                   to: &WorkflowNodeId,
                   seen: &mut BTreeSet<WorkflowDraftHandle>|
     -> Result<(), TestCaseError> {
        prop_assert_eq!(now.get(handle), Some(to), "{} in the result", handle);
        seen.insert(*handle);
        Ok(())
    };
    for entry in &correspondence.entries {
        match entry {
            Entry::Retained { handle, from, to } | Entry::Moved { handle, from, to } => {
                left(handle, from, &mut from_base)?;
                reached(handle, to, &mut to_now)?;
            }
            Entry::Inserted { handle, to, .. } => {
                prop_assert!(!base.contains_key(handle), "{} was in the base", handle);
                reached(handle, to, &mut to_now)?;
            }
            Entry::Deleted { handle, from } | Entry::Unmatched { handle, from } => {
                left(handle, from, &mut from_base)?;
                prop_assert!(!now.contains_key(handle), "{} is still there", handle);
            }
            Entry::Split { handle, from, into } => {
                left(handle, from, &mut from_base)?;
                prop_assert!(!now.contains_key(handle), "{} is still there", handle);
                for (piece, to) in into {
                    reached(piece, to, &mut to_now)?;
                }
            }
            other => {
                return Err(TestCaseError::fail(format!(
                    "an outcome this law does not know: {other:?}"
                )));
            }
        }
    }
    prop_assert_eq!(
        from_base,
        base.keys().copied().collect::<BTreeSet<_>>(),
        "every base node has an outcome"
    );
    prop_assert_eq!(
        to_now,
        now.keys().copied().collect::<BTreeSet<_>>(),
        "every node of the result has an outcome"
    );
    Ok(())
}

/// The edit law: whatever a script of edits asks, each transaction either
/// is refused whole (the draft keeps its document, its revision and its
/// handles, and every diagnostic is typed) or applies and leaves a document
/// that is a program again: it reconstructs to valid IR, that IR projects
/// back to the same document, the document opens as the same draft, and the
/// transaction's correspondence accounts for every node before and after.
pub fn fuzz(program: &Program, words: &[u16]) -> Result<Fuzzed, TestCaseError> {
    let opened_document = workflow_graph_from_program(program);
    let mut draft = WorkflowDraft::open(&opened_document).map_err(|error| fail(&error))?;
    prop_assert_eq!(draft.document(), &opened_document);
    let opened = draft
        .opened()
        .map(|(handle, id)| (handle, id.clone()))
        .collect::<BTreeMap<_, _>>();
    prop_assert_eq!(
        &opened,
        &live(&draft).map_err(|error| TestCaseError::fail(format!(
            "opening the generated document: {error}"
        )))?
    );
    let mut script = Script {
        words,
        at: 0,
        names: 0,
        retired: Vec::new(),
    };
    let mut fuzzed = Fuzzed::default();
    let mut previous: Option<WorkflowDraftRevision> = None;
    while !script.spent() {
        let before = live(&draft)?;
        let document = draft.document().clone();
        let revision = draft.revision();
        let shape = view(&draft);
        let edits = (0..1 + script.pick(3))
            .map(|_| script.edit(&draft, &shape))
            .collect::<Vec<_>>();
        let kinds = edits.iter().map(workflow_edit_kind).collect::<Vec<_>>();
        let expression_targets = edits
            .iter()
            .filter_map(|edit| match edit {
                WorkflowEdit::ReplaceExpression { target, .. } => Some(match target {
                    lash_vm::WorkflowExpressionRef::Node(_) => "node",
                    lash_vm::WorkflowExpressionRef::Function(_) => "function",
                    lash_vm::WorkflowExpressionRef::ProcessWrapper(_) => "process_wrapper",
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let stale = previous.filter(|_| script.pick(16) == 15);
        match draft.apply(WorkflowEditTransaction {
            base: stale.unwrap_or(revision),
            edits,
        }) {
            Err(refusal) => {
                prop_assert_eq!(draft.document(), &document, "a refusal changes nothing");
                prop_assert_eq!(draft.revision(), revision);
                prop_assert_eq!(&live(&draft)?, &before);
                prop_assert!(!refusal.diagnostics.is_empty());
                for diagnostic in &refusal.diagnostics {
                    let code = diagnostic.kind.code();
                    prop_assert!(!code.is_empty() && !diagnostic.kind.to_string().is_empty());
                    prop_assert_eq!(stale.is_some(), code == "stale_revision");
                    let edit = diagnostic.edit.map_or("transaction", |index| kinds[index]);
                    fuzzed.refused.push((edit, code));
                }
            }
            Ok(correspondence) => {
                fuzzed.expression_targets.extend(expression_targets);
                prop_assert!(stale.is_none(), "a stale transaction applied");
                prop_assert_eq!(correspondence.base, revision);
                prop_assert_eq!(correspondence.revision, draft.revision());
                prop_assert_ne!(draft.revision(), revision);
                let now = live(&draft).map_err(|error| {
                    TestCaseError::fail(format!("after edit kinds {kinds:?}: {error}"))
                })?;
                correspondence_is_total(&correspondence, &before, &now)?;
                correspondence_is_total(&draft.correspondence_since_open(), &opened, &now)?;

                let spelled =
                    workflow_program_from_graph(draft.document()).map_err(|error| fail(&error))?;
                lash_vm::validate_ast(&spelled).map_err(|error| fail(&error))?;
                prop_assert_eq!(
                    &workflow_graph_from_program(&spelled),
                    draft.document(),
                    "the edited document is the projection of the program it spells"
                );
                let reopened =
                    WorkflowDraft::open(draft.document()).map_err(|error| fail(&error))?;
                prop_assert_eq!(reopened.document(), draft.document());

                script.retired.extend(
                    before
                        .keys()
                        .filter(|handle| !now.contains_key(handle))
                        .copied(),
                );
                previous = Some(revision);
                fuzzed.applied.extend(kinds);
                fuzzed.programs.push(spelled);
            }
        }
    }
    Ok(fuzzed)
}
