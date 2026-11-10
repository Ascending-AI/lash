//! Kernel-owned equality domain for PLAN-INDEP and CONT-EXACT laws.
//!
//! This is an eager diagnostic reader, never an execution or persistence path.
//! Continuation bytes stay exact. The graph walk retains identity relationships,
//! sharing, cycles and insertion order, but excludes allocator and census state,
//! unreachable objects, tree packing and residency.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use crate::kernel_internals::PagedHeap;
use crate::kernel_internals::tree::{Cell, Tree};
use crate::paged::*;
use lash_kernel_doc::heap::*;
use {lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::encode_body};

/// A semantic checkpoint, independent of its storage layout.
#[derive(Clone, Debug, Eq)]
pub struct Checkpoint {
    continuation: Vec<Vec<u8>>,
    graph: Vec<Vec<u8>>,
    /// The durable ids of the objects the graph reaches, in the order the
    /// graph first reaches them. Outside the equality domain: the graph
    /// numbers objects by that order instead.
    ids: Vec<EntityId>,
}

impl Checkpoint {
    /// Exact source continuation rows, including pending phases and ordered debt.
    pub fn continuation(&self) -> &[Vec<u8>] {
        &self.continuation
    }

    /// Canonical graph rows, preserving reference multiplicity and insertion order.
    pub fn graph(&self) -> &[Vec<u8>] {
        &self.graph
    }

    /// The durable ids of the objects the checkpoint reaches, in the order
    /// its graph first reaches them: what PLAN-INDEP over ids compares
    /// (seam C of `docs/design/kernel-end-state.md`).
    pub fn ids(&self) -> &[EntityId] {
        &self.ids
    }
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
    mut source: Box<dyn PageSource>,
) -> Result<Checkpoint, HeapFault> {
    let kind = source
        .read(&[root.heap])?
        .pop()
        .ok_or_else(|| HeapFault::Malformed("the page port answered nothing".into()))??
        .kind;
    if kind == PageKind::RUN_ROOT {
        return run::checkpoint(root, source);
    }
    let mut heap = PagedHeap::open_header(root.heap, source, EditEpoch(0))?;
    heap.set_family_base(Box::new(
        crate::kernel_internals::family_impl::FrozenBase::default(),
    ));
    let mut reader = Reader {
        heap,
        graph: Vec::new(),
        seen: BTreeMap::new(),
        ids: Vec::new(),
    };
    reader.put(&root.kernel);
    reader.put(&root.document);
    reader.put(&root.link);
    reader.put(&root.closure);
    reader.put(&root.helper_library);
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
    reader.plain(
        &Tree::<lash_kernel_value::sites::WaitCancellation>::of_sequence(&root.unreported),
    )?;
    reader.plain(&Tree::<u64>::of_keyed(&root.withdrawn))?;
    reader.tree(
        &Tree::<lash_kernel_value::OperationUse>::of_keyed(&root.reads),
        |r, use_| {
            r.put(use_);
            Ok(())
        },
    )?;
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
        r.put(&task.observed);
        r.put(&task.passed);
        // Retain the state tag and all scalar state, whether an ended result
        // is retained among it, then visit its value edges.
        let mut state = task.state.clone();
        let held = match &mut state {
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                ..
            } => Some(v),
            TaskStateRecord::Ended(ended) => ended.retained_mut(),
            _ => None,
        };
        if let Some(v) = held {
            *v = PageValue::Null
        }
        r.put(&state);
        match &task.state {
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                ..
            } => r.value(v)?,
            TaskStateRecord::Ended(ended) => {
                if let Some(v) = ended.retained() {
                    r.value(v)?
                }
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
            // The continuation keeps the pending point's scalar state; its
            // role and result values are graph edges, numbered below by
            // first reach rather than by durable id.
            let pending = frame
                .pending
                .as_ref()
                .map(|pending| {
                    let mut scalar = pending.clone();
                    scalar.roles = crate::paged::RolesRecord::Flat(
                        pending
                            .roles(r.heap.pages())?
                            .into_iter()
                            .map(|role| crate::paged::RoleRecord {
                                coordinate: role.coordinate,
                                value: PageValue::Null,
                            })
                            .collect(),
                    );
                    if scalar.pending.is_some() {
                        scalar.pending = Some(PageValue::Null);
                    }
                    Ok::<_, HeapFault>(encode_body(&scalar))
                })
                .transpose()?;
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
            r.put(&frame.link);
            r.put(&frame.capture_names);
            r.put(&frame.created.is_some());
            if let Some(created) = &frame.created {
                r.put(created.resolve(r.heap.pages())?.as_ref());
            }
            r.put(&frame.convention);
            r.put(&frame.public_result);
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
                r.put(&pending.occurrence);
                r.put(&pending.invocation);
                r.put(&pending.unpaid);
                r.put(&pending.departure);
                r.put(&pending.ordinal);
                r.put(&pending.debt);
                r.put(&pending.refusal);
                r.put(&pending.pending.is_some());
                if let Some(value) = &pending.pending {
                    r.value(value)?;
                }
                let roles = pending.roles(r.heap.pages())?;
                r.put(&(roles.len() as u64));
                for role in roles {
                    r.put(&role.coordinate);
                    r.value(&role.value)?;
                }
            }
            Ok(())
        })
    })?;
    Ok(Checkpoint {
        continuation,
        graph: reader.graph,
        ids: reader.ids,
    })
}

/// Reads a terminal cell's carry in the same graph equality domain.
///
/// # Errors
/// Any fault reading a reachable page.
pub fn session(root: &SessionRoot, source: Box<dyn PageSource>) -> Result<Checkpoint, HeapFault> {
    if let Some(page) = root.run_page() {
        return run::session(page, source);
    }
    let mut heap = PagedHeap::open_header(
        crate::kernel_internals::session_heap(root),
        source,
        EditEpoch(0),
    )?;
    heap.set_family_base(Box::new(
        crate::kernel_internals::family_impl::FrozenBase::default(),
    ));
    let mut reader = Reader {
        heap,
        graph: Vec::new(),
        seen: BTreeMap::new(),
        ids: Vec::new(),
    };
    reader.bindings(&Tree::of_keyed(&crate::kernel_internals::session_bindings(
        root,
    )))?;
    reader.functions(&Tree::of_keyed(
        &crate::kernel_internals::session_functions(root),
    ))?;
    Ok(Checkpoint {
        continuation: Vec::new(),
        graph: reader.graph,
        ids: reader.ids,
    })
}

struct Reader {
    heap: PagedHeap,
    graph: Vec<Vec<u8>>,
    seen: BTreeMap<EntityId, u64>,
    ids: Vec<EntityId>,
}

fn rows<T: Cell>(
    tree: &Tree<T>,
    access: &crate::kernel_internals::tree::Access<'_>,
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
            r.put(&f.callable);
            r.put(&f.name.resolve(r.heap.pages())?);
            r.value(&f.meta)?;
            r.entity(f.environment)?;
            r.put(&f.closure.id);
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
                BoundRecord::Shared(id) | BoundRecord::Captured(id) => {
                    r.put(&1u64);
                    r.entity(*id)
                }
            }
        })
    }
    fn value(&mut self, value: &PageValue) -> Result<(), HeapFault> {
        self.put(&(value.kind() as u32));
        match value {
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id)
            | PageValue::Closure(id)
            | PageValue::Ref(Identity::Object(id)) => {
                self.entity(*id)?;
            }
            PageValue::Function(callable) => self.put(callable.as_ref()),
            PageValue::Int(IntRef::Inline(integer)) => {
                self.put(&integer.is_negative());
                self.graph
                    .push(lash_kernel_doc::heap_values::integer_digits(integer));
            }
            PageValue::Text(_) | PageValue::Bytes(_) | PageValue::Int(IntRef::Paged { .. }) => {
                let mut bytes = Vec::new();
                let len = match value {
                    PageValue::Text(t) => t.summary.utf8_bytes,
                    PageValue::Bytes(b) => b.len,
                    PageValue::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(8),
                    _ => unreachable!(),
                };
                let payload = value.clone();
                self.heap.payload_visit(&payload, 0..len, &mut |chunk| {
                    bytes.extend_from_slice(match chunk {
                        Chunk::Text(t) => t.as_bytes(),
                        Chunk::Bytes(b) => b,
                    });
                    ControlFlow::Continue(())
                })?;
                if let PageValue::Int(IntRef::Paged { summary, .. }) = value {
                    self.put(&summary.negative);
                }
                self.graph.push(bytes);
            }
            PageValue::Tuple(tuple) => {
                self.put(&tuple.summary.len);
                for i in 0..tuple.summary.len {
                    let v = self
                        .heap
                        .tuple_get(tuple, i)?
                        .ok_or_else(|| HeapFault::Malformed("missing tuple item".into()))?;
                    self.value(&v)?;
                }
            }
            PageValue::Error(error) => {
                self.value(&PageValue::Text(error.kind.clone()))?;
                self.value(&PageValue::Text(error.message.clone()))?;
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
        self.ids.push(id);
        let kind = self
            .heap
            .kind(id)?
            .ok_or_else(|| HeapFault::Malformed("missing reachable entity".into()))?;
        self.put(&(kind as u32));
        match kind {
            ObjectKind::Variable => self.value(&self.heap.variable_get(id)?)?,
            ObjectKind::Closure => {
                let closure = self.heap.closure(id)?;
                self.put(&closure.callable);
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
                            vec![PageValue::Text(name.clone()), value.clone()]
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

/// The bytecode core's carried session in the same equality domain: each
/// binding by name, then each saved function by name, every object it
/// reaches numbered by first reach. A text or bytes is its content and a
/// large list or table its members in order, whatever pages hold them.
mod run {
    use std::collections::BTreeMap;

    use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, PageRef};
    use lash_kernel_value::tokens::{TokenCodec, encode_body};

    use super::Checkpoint;
    use crate::paged::run::chunked::{Members, each_members};
    use crate::paged::run::directory::Directory;
    use crate::paged::run::ids::ObjectId;
    use crate::paged::run::objects::{ObjectKind, Record, RunValue};
    use crate::paged::tree::Access;
    use crate::paged::{CheckpointRoot, PageSource};

    pub(super) fn session(
        page: PageRef,
        source: Box<dyn PageSource>,
    ) -> Result<Checkpoint, HeapFault> {
        let access = Access::new(source);
        let carried = crate::paged::run::session::read(&access, page)?;
        let mut reader = Reader {
            directory: Directory::over(carried.heap.directory, access),
            graph: Vec::new(),
            seen: BTreeMap::new(),
            ids: Vec::new(),
        };
        reader.put(&(carried.names.len() as u64));
        for (at, name) in carried.names.iter().enumerate() {
            reader.put(name);
            reader.value(carried.binding(at).ok_or_else(missing)?)?;
        }
        reader.put(&(carried.functions.len() as u64));
        for (at, function) in carried.functions.iter().enumerate() {
            reader.put(&function.name);
            reader.graph.push(function.description.clone());
            reader.value(carried.function(at).ok_or_else(missing)?)?;
        }
        Ok(Checkpoint {
            continuation: Vec::new(),
            graph: reader.graph,
            ids: reader.ids,
        })
    }

    /// A run the core sealed under `root`: the root's own fields and the
    /// run's state beside its heap, whose values name the heap's roots by
    /// position, as the continuation; then every root value, objects
    /// numbered by first reach, as the graph. The directory and the ids
    /// minted stay out, as allocator history.
    pub(super) fn checkpoint(
        root: &CheckpointRoot,
        source: Box<dyn PageSource>,
    ) -> Result<Checkpoint, HeapFault> {
        let (record, directory) = crate::paged::run::continuation::open(source, root.heap)?;
        let mut reader = Reader {
            directory,
            graph: Vec::new(),
            seen: BTreeMap::new(),
            ids: Vec::new(),
        };
        reader.put(&root.kernel);
        reader.put(&root.document);
        reader.put(&root.link);
        reader.put(&root.closure);
        reader.put(&root.helper_library);
        reader.put(&root.owner);
        reader.put(&root.target);
        reader.put(&root.status);
        reader.put(&root.host);
        reader.put(&root.meters);
        reader.put(&record.entry);
        reader.put(&record.reads);
        reader.put(&record.charged);
        reader.put(&record.refused);
        reader.put(&record.globals);
        reader.put(&record.session);
        reader.put(&record.functions);
        reader.put(&record.held);
        reader.put(&record.running);
        reader.put(&record.installed);
        reader.put(&record.live);
        reader.put(&record.tasks);
        reader.put(&record.ready);
        reader.put(&record.waits);
        reader.put(&record.withdrawn);
        reader.put(&record.next_wait);
        reader.put(&record.issued);
        reader.put(&record.cancellations);
        reader.put(&record.joins);
        reader.put(&record.next_join);
        reader.put(&record.pins);
        reader.put(&record.held_code);
        let continuation = std::mem::take(&mut reader.graph);
        reader.values(record.heap.roots.iter())?;
        Ok(Checkpoint {
            continuation,
            graph: reader.graph,
            ids: reader.ids,
        })
    }

    fn missing() -> HeapFault {
        HeapFault::Malformed("a carried session misses a value".into())
    }

    struct Reader {
        directory: Directory,
        graph: Vec<Vec<u8>>,
        seen: BTreeMap<ObjectId, u64>,
        ids: Vec<EntityId>,
    }

    impl Reader {
        fn put<T: TokenCodec>(&mut self, value: &T) {
            self.graph.push(encode_body(value));
        }

        fn value(&mut self, value: &RunValue) -> Result<(), HeapFault> {
            match value {
                RunValue::Object(kind, id) => {
                    self.put(&0u64);
                    self.put(kind);
                    self.object(*id)
                }
                RunValue::Ref(id) => {
                    self.put(&1u64);
                    self.object(*id)
                }
                other => {
                    self.put(&2u64);
                    self.put(other);
                    Ok(())
                }
            }
        }

        fn values<'a>(
            &mut self,
            values: impl ExactSizeIterator<Item = &'a RunValue>,
        ) -> Result<(), HeapFault> {
            self.put(&(values.len() as u64));
            values.into_iter().try_for_each(|value| self.value(value))
        }

        fn object(&mut self, id: ObjectId) -> Result<(), HeapFault> {
            let next = self.seen.len() as u64;
            let known = self.seen.get(&id).copied();
            self.put(&known.unwrap_or(next));
            self.put(&known.is_none());
            if known.is_some() {
                return Ok(());
            }
            self.seen.insert(id, next);
            self.ids.push(EntityId {
                family: FamilyId::PRIMARY,
                ordinal: id.0,
            });
            let row = self.directory.get(id)?.ok_or_else(|| {
                HeapFault::Malformed(format!("reachable object {} has no row", id.0))
            })?;
            match row.record {
                Record::List(values) | Record::Tuple(values) | Record::Set(values) => {
                    self.values(values.iter())?;
                }
                Record::Map(pairs) | Record::Record(pairs) => {
                    let flat: Vec<&RunValue> =
                        pairs.iter().flat_map(|(key, value)| [key, value]).collect();
                    self.values(flat.into_iter())?;
                }
                Record::Closure { code, captures } => {
                    self.put(&code);
                    self.put(&(captures.len() as u64));
                    for cell in captures {
                        self.object(cell)?;
                    }
                }
                Record::Cell(value) => self.value(&value)?,
                Record::Error {
                    kind,
                    message,
                    data,
                } => {
                    self.value(&kind)?;
                    self.value(&message)?;
                    self.value(&data)?;
                }
                Record::BigInt(int) | Record::Timestamp(int) => self.put(&int),
                Record::LongText(text) => self.graph.push(text.as_bytes().to_vec()),
                Record::Bytes(bytes) => self.graph.push(bytes.to_vec()),
                Record::Paged(paged) => {
                    let whole = paged.read(self.directory.access())?;
                    self.graph.push(whole);
                }
                Record::Function(callable) => self.put(&callable),
                Record::Handle(handle) => self.put(&handle),
                Record::Chunked(chunked) => {
                    let mut items = Vec::new();
                    let mut entries = Vec::new();
                    each_members(
                        self.directory.access(),
                        &chunked,
                        &mut |members| match members {
                            Members::List(values) => items.extend(values.iter().cloned()),
                            Members::Table(found) => entries.extend(found.iter().cloned()),
                        },
                    )?;
                    entries.sort_by_key(|entry| entry.seq);
                    if chunked.kind == ObjectKind::Set {
                        items.extend(entries.into_iter().map(|entry| entry.key));
                    } else {
                        items.extend(
                            entries
                                .into_iter()
                                .flat_map(|entry| [entry.key, entry.value]),
                        );
                    }
                    self.values(items.iter())?;
                }
            }
            Ok(())
        }
    }
}
