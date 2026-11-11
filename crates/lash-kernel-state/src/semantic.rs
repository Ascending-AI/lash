//! Kernel-owned equality domain for PLAN-INDEP and CONT-EXACT laws.
//!
//! This is an eager diagnostic reader, never an execution or persistence path.
//! Continuation bytes stay exact. The graph walk retains identity relationships,
//! sharing, cycles and insertion order, but excludes allocator and census state,
//! unreachable objects, tree packing and residency.

use crate::paged::{CheckpointRoot, PageSource, SessionRoot};
use lash_kernel_doc::heap::{EntityId, HeapFault};

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

/// Reads the run the bytecode core sealed under `root` through `access`,
/// the port its heap seals through: what a running machine reads its own
/// seal back by.
///
/// # Errors
///
/// Any fault reading a reachable page; [`HeapFault::Corrupt`] for a root
/// that is not a run root.
pub fn run_checkpoint(
    root: &CheckpointRoot,
    access: crate::paged::tree::Access<'static>,
) -> Result<Checkpoint, HeapFault> {
    run::checkpoint(root, access)
}

/// Reads a sealed checkpoint's semantic state. Faults remain infrastructure;
/// a root that is not a run root the core sealed is refused by the run
/// reader (the paged engine's header as a retired state).
///
/// # Errors
/// Any fault reading a reachable page.
pub fn checkpoint(
    root: &CheckpointRoot,
    source: Box<dyn PageSource>,
) -> Result<Checkpoint, HeapFault> {
    run::checkpoint(root, crate::paged::tree::Access::new(source))
}

/// Reads a terminal cell's carry in the same graph equality domain.
///
/// # Errors
/// Any fault reading a reachable page.
pub fn session(root: &SessionRoot, source: Box<dyn PageSource>) -> Result<Checkpoint, HeapFault> {
    run::session(root.page(), source)
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
        access: Access<'static>,
    ) -> Result<Checkpoint, HeapFault> {
        let record = crate::paged::run::continuation::read(&access, root.heap)?;
        let mut reader = Reader {
            directory: Directory::over(record.heap.directory, access),
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
        reader.put(&record.start);
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
