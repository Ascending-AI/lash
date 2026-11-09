//! Sealing the heap (cutover contract §6): every object the run holds
//! gets a directory row under an [`ObjectId`], and the directory keeps only
//! what the roots reach.
//!
//! Seal writes the rows of the objects that changed since the last seal:
//! the new ones the roots reach and every dirty object with an id. It
//! descends only into those, so an unchanged object and a cold proxy cost
//! nothing; their rows are already in the directory. Ids are given in one
//! canonical order: depth first and pre-order from the roots, then from
//! each dirty object in id order. Pruning traces the rows' edge sets, never
//! a payload.
//!
//! The arena marks nothing clean until the directory has sealed, so a seal
//! that fails leaves every changed object dirty for the next one.

use std::collections::HashSet;
use std::sync::Arc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body, Chunks, ErrorBody};
use lash_kernel_doc::heap::{EntityId, FamilyId};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::directory::Row;
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::objects::{ClosureCode, Heads, ObjectKind, Record, RunValue};
use lash_kernel_state::paged::run::root::HeapRoot;

use super::open::entity;
use crate::paged::SealError;

fn fault(problem: impl Into<String>) -> SealError {
    SealError::Fault {
        problem: problem.into(),
    }
}

/// Writes every object `roots` reach that changed since the last seal into
/// `directory`, minting ids from `high_water`, prunes what they no longer
/// reach and seals it.
///
/// # Errors
///
/// [`SealError::Heap`] when the directory's pages cannot be read or
/// written; [`SealError::Fault`] when the ids run out or the arena breaks
/// an invariant (a stale handle, a resident body with a cold chunk).
pub(crate) fn seal(
    arena: &mut Arena,
    roots: &[Value],
    directory: &mut Directory,
    high_water: &mut HighWater,
) -> Result<HeapRoot, SealError> {
    let mut walk = Walk {
        arena,
        high_water,
        seen: HashSet::new(),
        written: Vec::new(),
    };
    let handles: Vec<ArenaHandle> = roots.iter().filter_map(Value::handle).collect();
    walk.from(handles)?;
    for (_, handle) in walk.arena.dirty() {
        if walk.seen.insert(handle.index()) {
            walk.written.push(handle);
            let members = members(walk.arena, handle)?;
            walk.from(members)?;
        }
    }
    let Walk { arena, written, .. } = walk;

    for &handle in &written {
        let id = id_of(arena, handle)?;
        let mut members = Vec::new();
        lash_kernel_state::arena::each_member(
            arena
                .body(handle)
                .map_err(|error| fault(error.to_string()))?,
            &mut |value| members.push(*value),
        );
        let row = Row::of(record(arena, handle)?, heads(arena, &members)?);
        directory.put(id, row).map_err(SealError::Heap)?;
    }
    let heads = heads(arena, roots)?;
    let roots = roots
        .iter()
        .map(|value| run_value(arena, value))
        .collect::<Result<Vec<_>, _>>()?;
    directory
        .prune(roots.iter().filter_map(RunValue::id))
        .map_err(SealError::Heap)?;
    let directory = directory.seal().map_err(SealError::Heap)?;

    for &handle in &written {
        arena
            .clean(handle)
            .map_err(|error| fault(error.to_string()))?;
    }
    let _ = arena.take_dirty();
    Ok(HeapRoot {
        high_water: *high_water,
        directory,
        roots,
        heads,
    })
}

/// The heads of the immutables `values` name, once each by id. Each is
/// fixed as its object's at its first row, so every row and root naming
/// the object names it by the same head, and a seal that fails and is
/// retried writes the same one.
fn heads(arena: &Arena, values: &[Value]) -> Result<Heads, SealError> {
    let mut heads = std::collections::BTreeMap::new();
    for value in values {
        let immutable = matches!(
            value,
            Value::Function(_)
                | Value::BigInt(_)
                | Value::LongText(_)
                | Value::Bytes(_)
                | Value::BigTimestamp(_)
                | Value::Tuple(_)
                | Value::Error(_)
                | Value::Handle(_)
        );
        let Some(handle) = value.handle().filter(|_| immutable) else {
            continue;
        };
        let id = id_of(arena, handle)?;
        if heads.contains_key(&id) {
            continue;
        }
        let head = arena
            .head(handle)
            .map_err(|error| fault(error.to_string()))?;
        arena
            .certify(handle, head.clone())
            .map_err(|error| fault(error.to_string()))?;
        heads.insert(id, head);
    }
    Ok(Heads(heads.into_iter().collect()))
}

struct Walk<'a> {
    arena: &'a mut Arena,
    high_water: &'a mut HighWater,
    /// The slots walked, by index: live handles never share one.
    seen: HashSet<u32>,
    /// The objects whose rows the seal writes, in the order they took ids.
    written: Vec<ArenaHandle>,
}

impl Walk<'_> {
    /// Depth first and pre-order from `start`, first member first: mints an
    /// id for each object without one and descends into it and into each
    /// dirty object; stops at clean objects and cold proxies.
    fn from(&mut self, start: Vec<ArenaHandle>) -> Result<(), SealError> {
        let mut pending: Vec<ArenaHandle> = start.into_iter().rev().collect();
        while let Some(handle) = pending.pop() {
            if self.seen.contains(&handle.index()) {
                continue;
            }
            let facts = self
                .arena
                .facts(handle)
                .map_err(|error| fault(error.to_string()))?;
            let descend = match facts.entity {
                _ if facts.cold => {
                    if facts.entity.is_none() {
                        return Err(fault("a cold proxy has no id"));
                    }
                    false
                }
                Some(_) => facts.dirty,
                None => {
                    let id = self
                        .high_water
                        .mint()
                        .map_err(|error| fault(error.to_string()))?;
                    self.arena
                        .assign(handle, entity(id))
                        .map_err(|error| fault(error.to_string()))?;
                    true
                }
            };
            self.seen.insert(handle.index());
            if descend {
                self.written.push(handle);
                let mut members = members(self.arena, handle)?;
                members.reverse();
                pending.extend(members);
            }
        }
        Ok(())
    }
}

/// The objects `handle`'s body references, in member order, captures last.
fn members(arena: &Arena, handle: ArenaHandle) -> Result<Vec<ArenaHandle>, SealError> {
    let body = arena
        .body(handle)
        .map_err(|error| fault(error.to_string()))?;
    let mut members = Vec::new();
    lash_kernel_state::arena::each_member(body, &mut |value| members.extend(value.handle()));
    if let Body::Closure { captures, .. } = body {
        members.extend(captures.iter().copied());
    }
    Ok(members)
}

fn id_of(arena: &Arena, handle: ArenaHandle) -> Result<ObjectId, SealError> {
    let facts = arena
        .facts(handle)
        .map_err(|error| fault(error.to_string()))?;
    match facts.entity {
        Some(EntityId {
            family: FamilyId::PRIMARY,
            ordinal,
        }) => Ok(ObjectId(ordinal)),
        Some(_) => Err(fault("an object holds a non-primary id")),
        None => Err(fault("a sealed object references one without an id")),
    }
}

fn run_value(arena: &Arena, value: &Value) -> Result<RunValue, SealError> {
    let object =
        |kind: ObjectKind, handle: &ArenaHandle| Ok(RunValue::Object(kind, id_of(arena, *handle)?));
    match value {
        Value::Null => Ok(RunValue::Null),
        Value::Absent => Ok(RunValue::Absent),
        Value::Bool(flag) => Ok(RunValue::Bool(*flag)),
        Value::Int(int) => Ok(RunValue::Int(*int)),
        Value::Float(float) => Ok(RunValue::Float(*float)),
        Value::Text(text) => Ok(RunValue::Text(Arc::from(text.as_str()))),
        Value::Timestamp(nanoseconds) => Ok(RunValue::Timestamp(*nanoseconds)),
        Value::Task(task) => Ok(RunValue::Task(task.0)),
        Value::TaskRef(task) => Ok(RunValue::TaskRef(task.0)),
        Value::Function(handle) => object(ObjectKind::Function, handle),
        Value::BigInt(handle) => object(ObjectKind::BigInt, handle),
        Value::LongText(handle) => object(ObjectKind::LongText, handle),
        Value::Bytes(handle) => object(ObjectKind::Bytes, handle),
        Value::BigTimestamp(handle) => object(ObjectKind::Timestamp, handle),
        Value::Tuple(handle) => object(ObjectKind::Tuple, handle),
        Value::Error(handle) => object(ObjectKind::Error, handle),
        Value::Handle(handle) => object(ObjectKind::Handle, handle),
        Value::List(handle) => object(ObjectKind::List, handle),
        Value::Map(handle) => object(ObjectKind::Map, handle),
        Value::Set(handle) => object(ObjectKind::Set, handle),
        Value::Record(handle) => object(ObjectKind::Record, handle),
        Value::Closure(handle) => object(ObjectKind::Closure, handle),
        Value::ObjectRef(handle) => Ok(RunValue::Ref(id_of(arena, *handle)?)),
    }
}

/// Every value `chunks` holds; a body being written is whole.
fn resident(chunks: &Chunks) -> Result<Vec<&Value>, SealError> {
    let mut values = Vec::new();
    for chunk in 0..chunks.count() {
        let held = chunks
            .resident(chunk)
            .ok_or_else(|| fault("a changed object holds a cold chunk"))?;
        values.extend(held);
    }
    Ok(values)
}

fn record(arena: &Arena, handle: ArenaHandle) -> Result<Record, SealError> {
    let value = |value: &Value| run_value(arena, value);
    let values = |chunks: &Chunks| -> Result<Vec<RunValue>, SealError> {
        resident(chunks)?.into_iter().map(value).collect()
    };
    let pairs = |chunks: &Chunks| -> Result<Vec<(RunValue, RunValue)>, SealError> {
        resident(chunks)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, member]| Ok((value(key)?, value(member)?)))
            .collect()
    };
    let body = arena
        .body(handle)
        .map_err(|error| fault(error.to_string()))?;
    Ok(match body {
        Body::Free => return Err(fault("a freed slot reached the seal")),
        Body::List(sequence) => Record::List(values(&sequence.chunks)?),
        Body::Tuple(sequence) => Record::Tuple(values(&sequence.chunks)?),
        Body::Set(table) => Record::Set(
            resident(&table.chunks)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, _]| value(key))
                .collect::<Result<_, _>>()?,
        ),
        Body::Map(table) => Record::Map(pairs(&table.chunks)?),
        Body::Record(table) => Record::Record(pairs(&table.chunks)?),
        Body::Closure {
            identity, captures, ..
        } => Record::Closure {
            code: ClosureCode {
                capture_names: identity.capture_names.clone(),
                callable: identity.callable,
                link: identity.site.link,
                site: identity.site.site.clone(),
            },
            captures: captures
                .iter()
                .map(|cell| id_of(arena, *cell))
                .collect::<Result<_, _>>()?,
        },
        Body::Cell(held) => Record::Cell(value(held)?),
        Body::Error(ErrorBody {
            kind,
            message,
            data,
        }) => Record::Error {
            kind: value(kind)?,
            message: value(message)?,
            data: value(data)?,
        },
        Body::BigInt(int) => Record::BigInt(int.clone()),
        Body::Timestamp(int) => Record::Timestamp(int.clone()),
        Body::LongText(text) => {
            let mut whole = String::with_capacity(text.summary.utf8_bytes as usize);
            for chunk in text.chunks.iter() {
                match chunk {
                    lash_kernel_doc::arena::Chunk::Resident(part) => whole.push_str(part),
                    lash_kernel_doc::arena::Chunk::Cold { .. } => {
                        return Err(fault("a changed text holds a cold chunk"));
                    }
                }
            }
            if text.moved() {
                return Err(fault("a moved text reached the seal"));
            }
            Record::LongText(Arc::from(whole))
        }
        Body::Bytes(bytes) => {
            let mut whole = Vec::with_capacity(bytes.len as usize);
            for chunk in bytes.chunks.iter() {
                match chunk {
                    lash_kernel_doc::arena::Chunk::Resident(part) => whole.extend_from_slice(part),
                    lash_kernel_doc::arena::Chunk::Cold { .. } => {
                        return Err(fault("changed bytes hold a cold chunk"));
                    }
                }
            }
            Record::Bytes(Arc::from(whole))
        }
        Body::Function(callable) => Record::Function(**callable),
        Body::Handle(host) => Record::Handle(host.clone()),
        Body::Cold(_) => return Err(fault("a cold immutable reached the seal")),
    })
}
