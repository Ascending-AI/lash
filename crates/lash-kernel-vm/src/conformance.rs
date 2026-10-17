//! Kernel-owned equality domain for PLAN-INDEP and CONT-EXACT laws.
//!
//! This is an eager diagnostic reader, never an execution or persistence path.
//! Continuation bytes stay exact. The graph walk retains identity relationships,
//! sharing, cycles and insertion order, but excludes allocator and census state,
//! unreachable objects, tree packing and residency.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use lash_kernel_doc::graph_tokens::{TokenCodec, encode_body};
use lash_kernel_doc::heap::*;
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::tree::{Cell, Tree};
use lash_kernel_state::paged::*;

/// A semantic checkpoint, independent of its storage layout.
#[derive(Clone, Debug, Eq)]
pub struct Checkpoint {
    continuation: Vec<Vec<u8>>,
    graph: Vec<Vec<u8>>,
}

impl PartialEq for Checkpoint {
    fn eq(&self, other: &Self) -> bool {
        same_checkpoint(self, other)
    }
}

/// The one comparison used by laws claiming exact continuation/graph equality.
pub fn same_checkpoint(a: &Checkpoint, b: &Checkpoint) -> bool {
    a.continuation == b.continuation && a.graph == b.graph
}

/// Reads a sealed checkpoint's semantic state. Faults remain infrastructure.
///
/// # Errors
/// Any fault reading a reachable page.
pub fn checkpoint(
    root: &CheckpointRoot,
    source: Box<dyn PageSource>,
) -> Result<Checkpoint, HeapFault> {
    let mut heap = PagedHeap::open_header(root.heap, source, EditEpoch(0))?;
    heap.set_family_base(Box::new(
        lash_kernel_state::kernel_internals::family_impl::FrozenBase::default(),
    ));
    let mut reader = Reader {
        heap,
        graph: Vec::new(),
        seen: BTreeMap::new(),
    };
    reader.put(&root.kernel);
    reader.put(&root.document);
    reader.put(&root.release);
    reader.put(&root.owner);
    reader.put(&root.target);
    match &root.status {
        RootStatus::Parked => reader.put(&0u64),
        RootStatus::Ended(ended) => match ended {
            EndedRecord::Returned(v) => {
                reader.put(&1u64);
                reader.frozen(v)?;
            }
            EndedRecord::Finished(v) => {
                reader.put(&2u64);
                reader.frozen(v)?;
            }
            EndedRecord::Failed(v) => {
                reader.put(&3u64);
                reader.frozen(v)?;
            }
            EndedRecord::Stopped => reader.put(&4u64),
        },
    }
    reader.put(&root.host);
    reader.put(&root.meters);
    reader.bindings(&Tree::of_keyed(&root.session))?;
    reader.functions(&Tree::of_keyed(&root.functions))?;
    reader.plain(&Tree::<lash_kernel_doc::TaskId>::of_sequence(&root.ready))?;
    reader.plain(&Tree::<u64>::of_sequence(&root.unreported))?;
    reader.plain(&Tree::<u64>::of_keyed(&root.withdrawn))?;
    reader.tree(&Tree::<NameRef>::of_keyed(&root.reads), |r, name| {
        r.put(&name.resolve(r.heap.pages())?);
        Ok(())
    })?;
    reader.tree(&Tree::<WaitRecord>::of_keyed(&root.waits), |r, wait| {
        r.put(&wait.task);
        r.put(&wait.kind);
        match &wait.phase {
            WaitPhase::Issued { request } => {
                r.put(&0u64);
                r.frozen(request)?;
            }
            WaitPhase::Admitted => r.put(&1u64),
            WaitPhase::Committed(outcome) => match outcome {
                CommittedOutcomeRecord::Completed(v) => {
                    r.put(&2u64);
                    r.frozen(v)?;
                }
                CommittedOutcomeRecord::Failed(v) => {
                    r.put(&3u64);
                    r.frozen(v)?;
                }
                CommittedOutcomeRecord::Rejected(v) => {
                    r.put(&4u64);
                    r.frozen(v)?;
                }
                CommittedOutcomeRecord::Elapsed => r.put(&5u64),
            },
        }
        Ok(())
    })?;
    reader.tree(&Tree::<ListJoinRecord>::of_keyed(&root.joins), |r, join| {
        r.put(&join.joiner);
        r.put(&join.mode);
        r.plain(&join.members)
    })?;
    let mut continuation = Vec::new();
    let task_rows = rows(
        &Tree::<TaskRecord>::of_keyed(&root.tasks),
        reader.heap.pages(),
    )?;
    let task_keys = task_rows
        .iter()
        .map(|(key, _)| {
            key.as_slice()
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| HeapFault::Malformed("task number key width".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut task_keys = task_keys.into_iter();
    reader.tree(&Tree::<TaskRecord>::of_keyed(&root.tasks), |r, task| {
        r.put(&task.identity.resolve(r.heap.pages())?);
        r.put(&task.result_retained);
        r.put(&task.observed);
        r.put(&task.passed);
        r.put(&task.failed);
        // Retain the state tag and all scalar state, then visit its value edges.
        let mut state = task.state.clone();
        match &mut state {
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                ..
            }
            | TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v)) => {
                *v = Value::Null
            }
            _ => (),
        }
        r.put(&state);
        match &task.state {
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                ..
            }
            | TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v)) => {
                r.value(v)?
            }
            _ => (),
        }
        r.plain(&task.joiners)?;
        r.plain(&task.occurrences)?;
        let task_id = task_keys
            .next()
            .ok_or_else(|| HeapFault::Malformed("missing task key".into()))?;
        let mut frame_index = 0usize;
        r.tree(&task.frames, |r, frame| {
            let site = frame.statement.resolve(r.heap.pages())?;
            let pending = frame.pending.as_ref().map(encode_body);
            let mut loops = Vec::new();
            for (_, row) in rows(&frame.loops, r.heap.pages())? {
                loops.push(encode_body(&row));
            }
            // Preserve the existing canonical continuation grammar exactly:
            // task/frame order is supplied by the surrounding keyed trees.
            continuation.push(
                serde_json::to_vec(&(task_id, frame_index, site.as_ref(), pending, loops))
                    .map_err(|e| HeapFault::Malformed(e.to_string()))?,
            );
            frame_index += 1;
            r.put(site.as_ref());
            r.bindings(&frame.bindings)?;
            r.plain(&frame.loops)?;
            r.plain(&frame.finally)?;
            r.tree(&frame.iterated, |r, v| r.value(&v.0))?;
            r.tree(&frame.departing, Reader::value)?;
            r.put(&frame.arguments.is_some());
            if let Some(args) = &frame.arguments {
                r.tree(args, Reader::value)?;
            }
            r.put(&frame.pending.is_some());
            if let Some(pending) = &frame.pending {
                r.put(&pending.site);
                r.put(&pending.phase);
                let roles = pending.roles(r.heap.pages())?;
                r.put(&(roles.len() as u64));
                for role in roles {
                    r.put(&role.site);
                    r.value(&role.value)?;
                }
            }
            Ok(())
        })
    })?;
    Ok(Checkpoint {
        continuation,
        graph: reader.graph,
    })
}

/// Reads a terminal cell's carry in the same graph equality domain.
///
/// # Errors
/// Any fault reading a reachable page.
pub fn session(root: &SessionRoot, source: Box<dyn PageSource>) -> Result<Checkpoint, HeapFault> {
    let mut heap = PagedHeap::open_header(root.heap, source, EditEpoch(0))?;
    heap.set_family_base(Box::new(
        lash_kernel_state::kernel_internals::family_impl::FrozenBase::default(),
    ));
    let mut reader = Reader {
        heap,
        graph: Vec::new(),
        seen: BTreeMap::new(),
    };
    reader.bindings(&Tree::of_keyed(&root.bindings))?;
    reader.functions(&Tree::of_keyed(&root.functions))?;
    Ok(Checkpoint {
        continuation: Vec::new(),
        graph: reader.graph,
    })
}

struct Reader {
    heap: PagedHeap,
    graph: Vec<Vec<u8>>,
    seen: BTreeMap<EntityId, u64>,
}

fn rows<T: Cell>(
    tree: &Tree<T>,
    access: &lash_kernel_state::kernel_internals::tree::Access<'_>,
) -> Result<Vec<(Vec<u8>, T)>, HeapFault> {
    let mut rows = Vec::new();
    tree.visit(access, &mut |key, row| {
        rows.push((key.to_vec(), row.clone()));
        true
    })?;
    Ok(rows)
}

impl Reader {
    fn frozen(&mut self, frozen: &FrozenRef) -> Result<(), HeapFault> {
        self.put(&self.heap.frozen_datum(frozen)?);
        Ok(())
    }
    fn functions(&mut self, tree: &Tree<FunctionRecord>) -> Result<(), HeapFault> {
        self.tree(tree, |r, f| {
            r.put(&f.name.resolve(r.heap.pages())?);
            r.value(&f.meta)?;
            r.put(&f.release.id);
            r.put(&(f.captures.len() as u64));
            for (name, value) in &f.captures {
                r.put(name);
                r.frozen(value)?;
            }
            r.put(&(f.before.len() as u64));
            for value in &f.before {
                r.frozen(value)?;
            }
            r.put(&(f.after.len() as u64));
            for value in &f.after {
                r.frozen(value)?;
            }
            Ok(())
        })
    }
    fn put<T: TokenCodec>(&mut self, value: &T) {
        self.graph.push(encode_body(value));
    }
    fn tree<T: Cell>(
        &mut self,
        tree: &Tree<T>,
        mut visit: impl FnMut(&mut Self, &T) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.put(&tree.len());
        for (key, row) in rows(tree, self.heap.pages())? {
            self.graph.push(key);
            visit(self, &row)?;
        }
        Ok(())
    }
    fn plain<T: Cell>(&mut self, tree: &Tree<T>) -> Result<(), HeapFault> {
        self.tree(tree, |r, row| {
            r.put(row);
            Ok(())
        })
    }
    fn bindings(&mut self, tree: &Tree<BindingRecord>) -> Result<(), HeapFault> {
        self.tree(tree, |r, b| {
            r.put(&b.name.resolve(r.heap.pages())?);
            r.put(b.declared.resolve(r.heap.pages())?.as_ref());
            match &b.value {
                BoundRecord::Value(v) => {
                    r.put(&0u64);
                    r.value(v)
                }
                BoundRecord::Shared(id) => {
                    r.put(&1u64);
                    r.entity(*id)
                }
            }
        })
    }
    fn value(&mut self, value: &Value) -> Result<(), HeapFault> {
        self.put(&(value.kind() as u32));
        match value {
            Value::List(id)
            | Value::Map(id)
            | Value::Set(id)
            | Value::Record(id)
            | Value::Closure(id)
            | Value::Ref(Identity::Object(id)) => {
                self.entity(*id)?;
            }
            Value::Int(IntRef::Inline(integer)) => {
                self.put(&integer.is_negative());
                self.graph
                    .push(lash_kernel_doc::heap_values::integer_digits(integer));
            }
            Value::Text(_)
            | Value::Function(_)
            | Value::Bytes(_)
            | Value::Int(IntRef::Paged { .. }) => {
                let mut bytes = Vec::new();
                let len = match value {
                    Value::Text(t) | Value::Function(t) => t.summary.utf8_bytes,
                    Value::Bytes(b) => b.len,
                    Value::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(8),
                    _ => unreachable!(),
                };
                let payload = match value {
                    Value::Function(t) => Value::Text(t.clone()),
                    _ => value.clone(),
                };
                self.heap.payload_visit(&payload, 0..len, &mut |chunk| {
                    bytes.extend_from_slice(match chunk {
                        Chunk::Text(t) => t.as_bytes(),
                        Chunk::Bytes(b) => b,
                    });
                    ControlFlow::Continue(())
                })?;
                if let Value::Int(IntRef::Paged { summary, .. }) = value {
                    self.put(&summary.negative);
                }
                self.graph.push(bytes);
            }
            Value::Tuple(tuple) => {
                self.put(&tuple.summary.len);
                for i in 0..tuple.summary.len {
                    let v = self
                        .heap
                        .tuple_get(tuple, i)?
                        .ok_or_else(|| HeapFault::Malformed("missing tuple item".into()))?;
                    self.value(&v)?;
                }
            }
            Value::Error(error) => {
                self.value(&Value::Text(error.kind.clone()))?;
                self.value(&Value::Text(error.message.clone()))?;
                self.value(&self.heap.error_data(error)?)?;
            }
            _ => self.put(value),
        }
        Ok(())
    }
    fn entity(&mut self, id: EntityId) -> Result<(), HeapFault> {
        let next = self.seen.len() as u64;
        let known = self.seen.get(&id).copied();
        self.put(&known.unwrap_or(next));
        self.put(&known.is_none());
        if known.is_some() {
            return Ok(());
        }
        self.seen.insert(id, next);
        let kind = self
            .heap
            .kind(id)?
            .ok_or_else(|| HeapFault::Malformed("missing reachable entity".into()))?;
        self.put(&(kind as u32));
        match kind {
            ObjectKind::Variable => self.value(&self.heap.variable_get(id)?)?,
            ObjectKind::Closure => {
                let closure = self.heap.closure(id)?;
                self.put(closure.site.resolve(self.heap.pages())?.as_ref());
                self.tree(&closure.captures, |r, (name, cell)| {
                    r.put(&name.resolve(r.heap.pages())?);
                    r.entity(*cell)
                })?;
            }
            _ => {
                self.put(&self.heap.len(id)?);
                let mut elements = Vec::new();
                self.heap.visit(id, &mut |element| {
                    elements.push(match element {
                        Element::Item(v) => vec![v.clone()],
                        Element::Entry { key, value } => vec![key.clone(), value.clone()],
                        Element::Field { name, value } => {
                            vec![Value::Text(name.clone()), value.clone()]
                        }
                    });
                    ControlFlow::Continue(())
                })?;
                for element in elements {
                    for v in element {
                        self.value(&v)?;
                    }
                }
            }
        }
        Ok(())
    }
}
