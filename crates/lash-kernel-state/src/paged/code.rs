//! Code retained by authenticated live values, independent of heap garbage.
use super::directory::{Body, Entry};
use super::heap_impl::PagedHeap;
use super::tree::{Cell, Tree};
use super::*;
use lash_kernel_doc::heap::PageReader as _;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue, TupleRef};
use lash_kernel_value::sites::Unit;
use std::{collections::BTreeSet, sync::Arc};
use {lash_kernel_value::CallableId, lash_kernel_value::LinkId};

/// Defining code still owed by a parked execution's frames and function values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetainedCode {
    pub callables: BTreeSet<CallableId>,
    pub frames: BTreeSet<(LinkId, Unit)>,
    /// Exact transient continuation coordinates, including every caller frame.
    pub sites: BTreeSet<(LinkId, lash_kernel_value::sites::Site)>,
    /// Convention imports retained outside executable body syntax.
    pub imports: BTreeSet<(LinkId, lash_kernel_value::ImportSlot)>,
}
/// Read code identities from the existing root walk; never scan unreferenced
/// directory rows or infer identity from function names or positions.
pub fn checkpoint_code(
    checkpoint: &Checkpoint,
    source: Box<dyn PageSource>,
) -> Result<RetainedCode, HeapFault> {
    let root = &checkpoint.0;
    let mut heap = PagedHeap::open_header(root.heap, source, EditEpoch(0))?;
    heap.restore_execution_roots(root);
    heap.set_family_base(Box::<super::family_impl::FrozenBase>::default());
    let mut scan = Scan {
        heap: &heap,
        code: RetainedCode::default(),
        seen: BTreeSet::new(),
    };
    visit(
        &Tree::<FunctionRecord>::of_keyed(&root.functions),
        heap.pages(),
        |row| {
            scan.code.callables.insert(row.callable);
            scan.value(PageValue::Closure(row.environment))?;
            for frozen in row.captures.values().chain(&row.before).chain(&row.after) {
                scan.frozen(frozen)?;
            }
            Ok(())
        },
    )?;
    visit(
        &Tree::<BindingRecord>::of_keyed(&root.session),
        heap.pages(),
        |row| super::census_graph::binding_values(row, &mut |value| scan.value(value.clone())),
    )?;
    visit(
        &Tree::<ListJoinRecord>::of_keyed(&root.joins),
        heap.pages(),
        |join| {
            visit(&join.members, heap.pages(), |task| {
                scan.value(PageValue::Task(*task))
            })
        },
    )?;
    visit(
        &Tree::<TaskRecord>::of_keyed(&root.tasks),
        heap.pages(),
        |task| {
            super::census_graph::state_values(&task.state, &mut |value| scan.value(value.clone()))?;
            visit(&task.frames, heap.pages(), |frame| {
                let site = frame.statement.resolve(heap.pages())?;
                scan.code.frames.insert((frame.link, site.unit.clone()));
                scan.code.sites.insert((frame.link, site.as_ref().clone()));
                if let Some(convention) = &frame.convention {
                    for target in convention.targets() {
                        match target {
                            lash_kernel_doc::FunctionTarget::Local(function) => {
                                scan.code.callables.insert(CallableId {
                                    link: frame.link,
                                    function: *function,
                                });
                            }
                            lash_kernel_doc::FunctionTarget::Import(slot) => {
                                scan.code.imports.insert((frame.link, *slot));
                            }
                        }
                    }
                }
                super::census_graph::frame_values(frame, heap.pages(), &mut |value| {
                    scan.value(value.clone())
                })
            })
        },
    )?;
    visit(
        &Tree::<WaitRecord>::of_keyed(&root.waits),
        heap.pages(),
        |wait| match &wait.phase {
            WaitPhase::Issued { request } => scan.frozen(request),
            WaitPhase::Committed(
                CommittedOutcomeRecord::Completed(value)
                | CommittedOutcomeRecord::Failed(value)
                | CommittedOutcomeRecord::Rejected(value),
            ) => scan.frozen(value),
            WaitPhase::Admitted => Ok(()),
        },
    )?;
    Ok(scan.code)
}
fn visit<T: Cell>(
    tree: &Tree<T>,
    access: &super::tree::Access<'_>,
    mut read: impl FnMut(&T) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    for rank in 0..tree.len() {
        let (_, row) = tree
            .rank_get(rank, access)?
            .ok_or_else(|| invalid("missing code-debt row"))?;
        read(&row)?;
    }
    Ok(())
}
fn invalid(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
struct Scan<'a> {
    heap: &'a PagedHeap,
    code: RetainedCode,
    seen: BTreeSet<Identity>,
}
enum Work {
    Value(PageValue),
    Values(Tree<PageValue>, u64),
    Entries(Tree<Entry>, u64),
    Captures(Tree<(NameRef, EntityId)>, u64),
    Tuple(Arc<TupleRef>, u64),
}
impl Scan<'_> {
    fn frozen(&mut self, frozen: &FrozenRef) -> Result<(), HeapFault> {
        super::family_impl::frozen_values(self.heap, frozen, &mut |value| {
            let mut pending = vec![value.clone()];
            while let Some(value) = pending.pop() {
                match value {
                    PageValue::Function(_) | PageValue::Closure(_) => self.value(value)?,
                    PageValue::Tuple(tuple) => {
                        for at in 0..tuple.summary.len {
                            pending.push(
                                self.heap
                                    .tuple_get(&tuple, at)?
                                    .ok_or_else(|| invalid("missing frozen tuple member"))?,
                            );
                        }
                    }
                    PageValue::Error(error) => pending.push(self.heap.error_data(&error)?),
                    _ => {}
                }
            }
            Ok(())
        })
    }

    fn value(&mut self, value: PageValue) -> Result<(), HeapFault> {
        let mut pending = vec![Work::Value(value)];
        while let Some(work) = pending.pop() {
            match work {
                Work::Values(rows, rank) if rank < rows.len() => {
                    let (_, value) = rows
                        .rank_get(rank, self.heap.pages())?
                        .ok_or_else(|| invalid("missing list member"))?;
                    pending.push(Work::Values(rows, rank + 1));
                    pending.push(Work::Value(value));
                }
                Work::Entries(rows, rank) if rank < rows.len() => {
                    let (_, entry) = rows
                        .rank_get(rank, self.heap.pages())?
                        .ok_or_else(|| invalid("missing table member"))?;
                    pending.push(Work::Entries(rows, rank + 1));
                    pending.push(Work::Value(entry.value));
                    pending.push(Work::Value(entry.key));
                }
                Work::Captures(rows, rank) if rank < rows.len() => {
                    let (_, (_, entity)) = rows
                        .rank_get(rank, self.heap.pages())?
                        .ok_or_else(|| invalid("missing captured cell"))?;
                    pending.push(Work::Captures(rows, rank + 1));
                    pending.push(Work::Value(PageValue::Ref(Identity::Object(entity))));
                }
                Work::Tuple(tuple, rank) if rank < tuple.summary.len => {
                    let value = self
                        .heap
                        .tuple_get(&tuple, rank)?
                        .ok_or_else(|| invalid("missing tuple member"))?;
                    pending.push(Work::Tuple(tuple, rank + 1));
                    pending.push(Work::Value(value));
                }
                Work::Value(PageValue::Function(callable)) => {
                    self.code.callables.insert(*callable);
                }
                Work::Value(PageValue::Tuple(tuple)) => pending.push(Work::Tuple(tuple, 0)),
                Work::Value(PageValue::Error(error)) => {
                    pending.push(Work::Value(self.heap.error_data(&error)?))
                }
                Work::Value(value) => {
                    let identity = match value {
                        PageValue::List(id)
                        | PageValue::Map(id)
                        | PageValue::Set(id)
                        | PageValue::Record(id)
                        | PageValue::Closure(id) => Identity::Object(id),
                        PageValue::Task(task) => Identity::Task(task),
                        PageValue::Ref(identity) => identity,
                        _ => continue,
                    };
                    if !self.seen.insert(identity) {
                        continue;
                    }
                    match identity {
                        Identity::Object(entity) => {
                            let head = self
                                .heap
                                .current_head(entity)?
                                .ok_or_else(|| invalid("live code references a missing object"))?;
                            match &head.body {
                                Body::List { rows, .. } => {
                                    pending.push(Work::Values(rows.clone(), 0))
                                }
                                Body::Table { rows, .. } => {
                                    pending.push(Work::Entries(rows.clone(), 0))
                                }
                                Body::Variable(value) => pending.push(Work::Value(value.clone())),
                                Body::Closure {
                                    callable, captures, ..
                                } => {
                                    self.code.callables.insert(*callable);
                                    pending.push(Work::Captures(captures.clone(), 0));
                                }
                            }
                        }
                        Identity::Task(task) => {
                            let row = self
                                .heap
                                .task_root()
                                .get(&task.0.to_be_bytes(), self.heap.pages())?
                                .ok_or_else(|| invalid("live code references a missing task"))?;
                            if let TaskStateRecord::Ended(ended) = row.state
                                && let Some(value) = ended.retained()
                            {
                                pending.push(Work::Value(value.clone()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Follow only retained code. Documents supply local function bytes and exact
/// code-import bindings; helper edges come from the build's validated index.
/// Dead directory objects, unrelated exports and the rest of the library do
/// not enter this work list.
pub(super) fn restore_selection(
    live: RetainedCode,
    documents: &std::collections::BTreeMap<LinkId, lash_kernel_doc::Document>,
    code_imports: &std::collections::BTreeMap<(LinkId, lash_kernel_value::ImportSlot), CallableId>,
    build: &lash_kernel_doc::BuildHelpers,
) -> Result<lash_kernel_doc::RestoreSelection, HeapFault> {
    use lash_kernel_doc::Node;
    let mut pending: Vec<_> = live
        .frames
        .into_iter()
        .filter(|(_, unit)| !matches!(unit, Unit::Main))
        .collect();
    for (link, site) in live.sites {
        if !matches!(site.unit, Unit::Main) {
            continue;
        }
        let document = documents
            .get(&link)
            .ok_or_else(|| invalid("live code Link is not loaded"))?;
        let body = document
            .main
            .as_ref()
            .ok_or_else(|| invalid("live Main has no body"))?;
        for node in continuation(body, &site.path)? {
            enqueue(node, link, document, code_imports, &mut pending)?;
        }
    }
    pending.extend(
        live.callables
            .into_iter()
            .map(|callable| (callable.link, Unit::Library(callable.function))),
    );
    for key in live.imports {
        let callable = code_imports
            .get(&key)
            .ok_or_else(|| invalid("retained convention Import is not linked code"))?;
        pending.push((callable.link, Unit::Library(callable.function)));
    }
    let mut seen = BTreeSet::new();
    let mut pins = BTreeSet::new();
    let mut helper_contexts = BTreeSet::new();
    let mut reached = BTreeSet::new();
    while let Some((link, unit)) = pending.pop() {
        if !seen.insert((link, unit.clone())) {
            continue;
        }
        let document = documents
            .get(&link)
            .ok_or_else(|| invalid("live code Link is not loaded"))?;
        let body = match &unit {
            Unit::Main => document
                .main
                .as_ref()
                .ok_or_else(|| invalid("live Main has no body"))?,
            Unit::Function(name) => {
                let (_, body) = document
                    .units()
                    .find(|(unit, _)| unit == &Unit::Function(name.clone()))
                    .ok_or_else(|| invalid("live named function has no body"))?;
                body
            }
            Unit::Library(function) => {
                reached.insert(CallableId {
                    link,
                    function: *function,
                });
                if let Some(definition) = document.functions.get(function) {
                    reached.insert(CallableId {
                        link,
                        function: *function,
                    });
                    let Some(body) = definition.body() else {
                        continue;
                    };
                    &body.block
                } else {
                    // The continuation was admitted under this operation Link.
                    // An exact helper ID is either in its explicit manifest or
                    // in that helper image's transitive static reach.
                    if !document.library.contains(function) && !build.contains(function) {
                        // A helper removed by this build has no metadata to walk.
                        // Keep its identity for the typed membership refusal.
                        pins.insert(*function);
                        continue;
                    }
                    let mut helpers = vec![*function];
                    while let Some(helper) = helpers.pop() {
                        reached.insert(CallableId {
                            link,
                            function: helper,
                        });
                        if !helper_contexts.insert((link, helper)) {
                            continue;
                        }
                        pins.insert(helper);
                        #[cfg(any(test, feature = "perf-witness"))]
                        super::helper_compatibility_work::update(|work| work.dependency_steps += 1);
                        if let Some(callees) = build.callees(&helper) {
                            helpers.extend(callees.iter().copied());
                        }
                    }
                    continue;
                }
            }
        };
        enqueue(
            Node::Block(body),
            link,
            document,
            code_imports,
            &mut pending,
        )?;
    }
    Ok(lash_kernel_doc::RestoreSelection {
        pins,
        live: reached,
    })
}

/// Main has no content-addressed function identity: select its remaining
/// lexical suffixes. Loops can revisit their whole bodies; active try handlers
/// can still run on departure. Every frame supplies its own saved coordinate.
fn continuation<'a>(
    body: &'a lash_kernel_doc::Block,
    path: &[u32],
) -> Result<Vec<lash_kernel_doc::Node<'a>>, HeapFault> {
    use lash_kernel_doc::{Node, Stmt};
    let mut retained = Vec::new();
    let mut node = Node::Block(body);
    for (depth, index) in path.iter().copied().enumerate() {
        let index = index as usize;
        match node {
            Node::Block(block) => {
                let start = if depth + 1 == path.len() {
                    index
                } else {
                    index + 1
                };
                let suffix = block
                    .get(start..)
                    .ok_or_else(|| invalid("live Main coordinate is not a block suffix"))?;
                retained.push(Node::Block(suffix));
                if depth + 1 == path.len() {
                    return Ok(retained);
                }
            }
            Node::Stmt(Stmt::For { .. } | Stmt::While { .. }) => {
                retained.push(node);
                return Ok(retained);
            }
            Node::Stmt(Stmt::Try(scope)) => {
                if let Some(catch) = &scope.catch {
                    retained.push(Node::Block(&catch.body));
                }
                if let Some(finally) = &scope.finally {
                    retained.push(Node::Block(finally));
                }
            }
            _ => {}
        }
        node = node
            .children()
            .get(index)
            .copied()
            .ok_or_else(|| invalid("live Main coordinate is not in its document"))?;
    }
    retained.push(node);
    Ok(retained)
}

fn enqueue(
    node: lash_kernel_doc::Node<'_>,
    link: LinkId,
    document: &lash_kernel_doc::Document,
    code_imports: &std::collections::BTreeMap<(LinkId, lash_kernel_value::ImportSlot), CallableId>,
    pending: &mut Vec<(LinkId, Unit)>,
) -> Result<(), HeapFault> {
    for function in node.functions_within() {
        if !document.functions.contains_key(&function) && !document.library.contains(&function) {
            return Err(invalid(
                "live local reference is not admitted by its Document",
            ));
        }
        pending.push((link, Unit::Library(function)));
    }
    for slot in node.imports_within() {
        if let Some(callable) = code_imports.get(&(link, slot)) {
            pending.push((callable.link, Unit::Library(callable.function)));
        }
    }
    Ok(())
}
