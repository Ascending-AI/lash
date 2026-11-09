//! Opening the heap (cutover contract §6): the run's directory, and the
//! decoding of one row into a resident body.
//!
//! A load decodes exactly one row: the object's own record. Every object
//! the record references becomes a proxy, a cold placeholder of its kind
//! the arena answers [`lash_kernel_doc::Retry::Cold`] for, or the resident
//! object already holding its id. One load therefore reads one directory
//! path, whatever the record's referents hold.

use std::sync::Arc;

use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{
    ArenaHandle, Body, Chunks, ClosureIdentity, Sequence, ShortText, Table,
};
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, PageId};
use lash_kernel_state::arena::{Arena, ProxyKind};
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::objects::{ObjectKind, Record, RunValue};

/// The run's durable heap: the directory its objects are written to and
/// read from, and the ids its lineage has minted.
pub(crate) struct Durable {
    pub(crate) directory: Directory,
    pub(crate) high_water: HighWater,
}

impl Durable {
    /// What the directory holds that the arena cannot use: a store
    /// integrity failure, reported against the directory's root page.
    pub(crate) fn corrupt(&self, reason: impl Into<String>) -> HeapFault {
        HeapFault::Corrupt {
            page: self
                .directory
                .page()
                .map_or(PageId([0; 32]), |page| page.id),
            reason: reason.into(),
        }
    }
}

/// The arena's name for object `id`.
pub(crate) fn entity(id: ObjectId) -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: id.0,
    }
}

/// The id of the object `handle`, which has one.
pub(crate) fn id_of(arena: &Arena, handle: ArenaHandle) -> Result<ObjectId, String> {
    match arena
        .facts(handle)
        .map_err(|error| error.to_string())?
        .entity
    {
        Some(EntityId {
            family: FamilyId::PRIMARY,
            ordinal,
        }) => Ok(ObjectId(ordinal)),
        Some(_) => Err("an object holds a non-primary id".into()),
        None => Err("a cold object has no id".into()),
    }
}

/// The proxy kind of a referent of `kind`. An immutable referent waits on
/// its head in the referrer's record (lead ruling A, pending review).
fn proxy_kind(kind: ObjectKind) -> Result<ProxyKind, String> {
    Ok(match kind {
        ObjectKind::List => ProxyKind::List,
        ObjectKind::Map => ProxyKind::Map,
        ObjectKind::Set => ProxyKind::Set,
        ObjectKind::Record => ProxyKind::Record,
        ObjectKind::Closure => ProxyKind::Closure,
        _ => return Err(format!("an immutable {kind:?} referent has no head")),
    })
}

fn value_of(kind: ObjectKind, handle: ArenaHandle) -> Value {
    match kind {
        ObjectKind::List => Value::List(handle),
        ObjectKind::Map => Value::Map(handle),
        ObjectKind::Set => Value::Set(handle),
        ObjectKind::Record => Value::Record(handle),
        ObjectKind::Closure => Value::Closure(handle),
        ObjectKind::Tuple => Value::Tuple(handle),
        ObjectKind::Error => Value::Error(handle),
        ObjectKind::BigInt => Value::BigInt(handle),
        ObjectKind::Timestamp => Value::BigTimestamp(handle),
        ObjectKind::LongText => Value::LongText(handle),
        ObjectKind::Bytes => Value::Bytes(handle),
        ObjectKind::Function => Value::Function(handle),
        ObjectKind::Handle => Value::Handle(handle),
    }
}

/// Every object `record` references, once each in member order, with the
/// kind of proxy it opens as.
pub(crate) fn referents(record: &Record) -> Result<Vec<(ObjectId, ProxyKind)>, String> {
    let mut referents = Vec::new();
    let mut value = |value: &RunValue| -> Result<(), String> {
        match value {
            RunValue::Object(kind, id) => referents.push((*id, proxy_kind(*kind)?)),
            RunValue::Ref(id) => referents.push((*id, ProxyKind::Unknown)),
            _ => {}
        }
        Ok(())
    };
    match record {
        Record::List(members) | Record::Tuple(members) | Record::Set(members) => {
            members.iter().try_for_each(&mut value)?;
        }
        Record::Map(pairs) | Record::Record(pairs) => {
            for (key, member) in pairs {
                value(key)?;
                value(member)?;
            }
        }
        Record::Cell(member) => value(member)?,
        Record::Error {
            kind,
            message,
            data,
        } => {
            value(kind)?;
            value(message)?;
            value(data)?;
        }
        Record::Closure { captures, .. } => {
            referents.extend(captures.iter().map(|cell| (*cell, ProxyKind::Cell)));
        }
        Record::BigInt(_)
        | Record::Timestamp(_)
        | Record::LongText(_)
        | Record::Bytes(_)
        | Record::Function(_)
        | Record::Handle(_) => {}
    }
    let mut seen = std::collections::HashSet::new();
    referents.retain(|(id, _)| seen.insert(*id));
    Ok(referents)
}

/// The value `value` names: an immediate, or the object holding its id,
/// opened as a proxy when the arena has none.
pub(crate) fn value(arena: &mut Arena, value: &RunValue) -> Result<Value, String> {
    Ok(match value {
        RunValue::Null => Value::Null,
        RunValue::Absent => Value::Absent,
        RunValue::Bool(flag) => Value::Bool(*flag),
        RunValue::Int(int) => Value::Int(*int),
        RunValue::Float(float) => Value::Float(*float),
        RunValue::Text(text) => {
            Value::Text(ShortText::new(text).ok_or("a long text is held as an immediate")?)
        }
        RunValue::Timestamp(nanoseconds) => Value::Timestamp(*nanoseconds),
        RunValue::Task(task) => Value::Task(TaskId(*task)),
        RunValue::TaskRef(task) => Value::TaskRef(TaskId(*task)),
        RunValue::Object(kind, id) => value_of(*kind, arena.proxy(entity(*id), proxy_kind(*kind)?)),
        RunValue::Ref(id) => Value::ObjectRef(arena.proxy(entity(*id), ProxyKind::Unknown)),
    })
}

/// The resident body `record` decodes to, its referents by proxy.
pub(crate) fn body(arena: &mut Arena, record: &Record) -> Result<Body, String> {
    let mut values = |members: &[RunValue]| -> Result<Vec<Value>, String> {
        members.iter().map(|member| value(arena, member)).collect()
    };
    Ok(match record {
        Record::List(members) => {
            let members = values(members)?;
            Body::List(Sequence {
                len: members.len() as u64,
                chunks: Chunks::whole(members),
            })
        }
        Record::Set(members) => {
            let pairs = values(members)?
                .into_iter()
                .flat_map(|key| [key, Value::Absent])
                .collect();
            Body::Set(Table::whole(pairs))
        }
        Record::Map(pairs) | Record::Record(pairs) => {
            let mut flat = Vec::with_capacity(pairs.len() * 2);
            for (key, member) in pairs {
                flat.push(value(arena, key)?);
                flat.push(value(arena, member)?);
            }
            let table = Table::whole(flat);
            if matches!(record, Record::Map(_)) {
                Body::Map(table)
            } else {
                Body::Record(table)
            }
        }
        Record::Closure { code, captures } => Body::Closure {
            identity: Arc::new(ClosureIdentity {
                capture_names: code.capture_names.clone(),
                callable: code.callable,
                site: code.linked_site(),
            }),
            captures: captures
                .iter()
                .map(|cell| arena.proxy(entity(*cell), ProxyKind::Cell))
                .collect(),
            code: None,
        },
        Record::Cell(member) => Body::Cell(value(arena, member)?),
        Record::Tuple(_)
        | Record::Error { .. }
        | Record::BigInt(_)
        | Record::Timestamp(_)
        | Record::LongText(_)
        | Record::Bytes(_)
        | Record::Function(_)
        | Record::Handle(_) => {
            return Err("an immutable object loads with its head".into());
        }
    })
}
