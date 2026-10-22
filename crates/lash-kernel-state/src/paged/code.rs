//! Code retained by authenticated live values, independent of heap garbage.
use super::directory::{Body, Entry};
use super::heap_impl::PagedHeap;
use super::tree::{Cell, Tree};
use super::*;
use lash_kernel_doc::heap::PageReader as _;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue, TupleRef};
use lash_vocabulary::sites::Unit;
use lash_vocabulary::{CallableId, LinkId};
use std::{collections::BTreeSet, sync::Arc};

/// Defining code still owed by a parked execution's frames and function values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetainedCode {
    pub callables: BTreeSet<CallableId>,
    pub frames: BTreeSet<(LinkId, Unit)>,
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
                scan.code.frames.insert((
                    frame.link,
                    frame.statement.resolve(heap.pages())?.unit.clone(),
                ));
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
                            if let TaskStateRecord::Ended(ended) = row.state {
                                if let Some(value) = ended.retained() {
                                    pending.push(Work::Value(value.clone()));
                                }
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
