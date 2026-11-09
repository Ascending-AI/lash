//! Opening the heap (cutover contract §6): the run's directory, and the
//! decoding of one row into a resident body.
//!
//! A load decodes exactly one row: the object's own record. Every object
//! the record references becomes a proxy, a cold placeholder of its kind
//! the arena answers [`lash_kernel_doc::Retry::Cold`] for, or the resident
//! object already holding its id. An immutable referent opens from the
//! head the row holds for it, so its length, summaries and key answer
//! without its row. One load therefore reads one directory path, whatever
//! the record's referents hold.

use std::sync::Arc;

use lash_kernel_doc::TaskId;
use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{
    ArenaHandle, Body, Bytes, Chunk, Chunks, ClosureIdentity, ErrorBody, LongText, Sequence,
    ShortText, Table,
};
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, PageId};
use lash_kernel_state::arena::{Arena, ProxyKind};
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::objects::{Head, Heads, ObjectKind, Record, RunValue};

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

/// How a referent opens: a mutable one as a proxy of its kind, an
/// immutable one from its head.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Opening<'a> {
    Proxy(ProxyKind),
    Head(&'a Head),
}

impl Opening<'_> {
    /// What the referent holds once opened, before a load of its row.
    pub(crate) fn placeholder(self) -> Body {
        match self {
            Opening::Proxy(kind) => kind.placeholder(),
            Opening::Head(head) => lash_kernel_state::arena::opened(head).0,
        }
    }
}

/// How the referent `id` of `kind` opens, the row holding `heads`.
fn opening(kind: ObjectKind, id: ObjectId, heads: &Heads) -> Result<Opening<'_>, String> {
    Ok(match kind {
        ObjectKind::List => Opening::Proxy(ProxyKind::List),
        ObjectKind::Map => Opening::Proxy(ProxyKind::Map),
        ObjectKind::Set => Opening::Proxy(ProxyKind::Set),
        ObjectKind::Record => Opening::Proxy(ProxyKind::Record),
        ObjectKind::Closure => Opening::Proxy(ProxyKind::Closure),
        _ => Opening::Head(
            heads
                .get(id)
                .ok_or_else(|| format!("immutable {} has no head", id.0))?,
        ),
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

/// Every object `record` references, once each in member order, with how
/// it opens.
pub(crate) fn referents<'a>(
    record: &Record,
    heads: &'a Heads,
) -> Result<Vec<(ObjectId, Opening<'a>)>, String> {
    let mut referents = Vec::new();
    for value in record.values() {
        match value {
            RunValue::Object(kind, id) => referents.push((*id, opening(*kind, *id, heads)?)),
            RunValue::Ref(id) => referents.push((*id, Opening::Proxy(ProxyKind::Unknown))),
            _ => {}
        }
    }
    if let Record::Closure { captures, .. } = record {
        referents.extend(
            captures
                .iter()
                .map(|cell| (*cell, Opening::Proxy(ProxyKind::Cell))),
        );
    }
    let mut seen = std::collections::HashSet::new();
    referents.retain(|(id, _)| seen.insert(*id));
    Ok(referents)
}

/// The value `value` names: an immediate, or the object holding its id,
/// opened as a proxy, or from its head in `heads`, when the arena has none.
pub(crate) fn value(arena: &mut Arena, value: &RunValue, heads: &Heads) -> Result<Value, String> {
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
        RunValue::Object(kind, id) => {
            let handle = match opening(*kind, *id, heads)? {
                Opening::Proxy(proxy) => arena.proxy(entity(*id), proxy),
                Opening::Head(head) => arena
                    .open_immutable(entity(*id), head)
                    .map_err(|error| error.to_string())?,
            };
            value_of(*kind, handle)
        }
        RunValue::Ref(id) => Value::ObjectRef(arena.proxy(entity(*id), ProxyKind::Unknown)),
    })
}

/// The resident body `record` decodes to, its referents opened by
/// [`value`].
pub(crate) fn body(arena: &mut Arena, record: &Record, heads: &Heads) -> Result<Body, String> {
    let mut values = |members: &[RunValue]| -> Result<Vec<Value>, String> {
        members
            .iter()
            .map(|member| value(arena, member, heads))
            .collect()
    };
    Ok(match record {
        Record::List(members) => {
            let members = values(members)?;
            Body::List(Sequence {
                len: members.len() as u64,
                chunks: Chunks::whole(members),
            })
        }
        Record::Tuple(members) => {
            let members = values(members)?;
            Body::Tuple(Sequence {
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
                flat.push(value(arena, key, heads)?);
                flat.push(value(arena, member, heads)?);
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
        Record::Cell(member) => Body::Cell(value(arena, member, heads)?),
        Record::Error {
            kind,
            message,
            data,
        } => Body::Error(ErrorBody {
            kind: value(arena, kind, heads)?,
            message: value(arena, message, heads)?,
            data: value(arena, data, heads)?,
        }),
        Record::BigInt(int) => Body::BigInt(int.clone()),
        Record::Timestamp(nanoseconds) => Body::Timestamp(nanoseconds.clone()),
        Record::LongText(text) => Body::LongText(LongText {
            summary: lash_kernel_doc::heap_values::text_summary(text),
            chunks: Box::new([Chunk::Resident(text.to_string())]),
        }),
        Record::Bytes(bytes) => Body::Bytes(Bytes {
            len: bytes.len() as u64,
            chunks: Box::new([Chunk::Resident(Box::from(&**bytes))]),
        }),
        Record::Function(callable) => Body::Function(Box::new(*callable)),
        Record::Handle(host) => Body::Handle(host.clone()),
    })
}
