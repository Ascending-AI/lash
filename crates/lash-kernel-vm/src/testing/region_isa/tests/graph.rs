//! Amendment 2 fixtures. The source interpreter uses the existing canonical
//! ledger for itemized retention, releases, balance and peak. The fake lease
//! uses its own local reference counts and incremental member index.
use super::*;
use lash_kernel_doc::heap::{EntityId, FamilyId, Identity as HeapIdentity, PageValue};
use lash_kernel_doc::{Float, LogicalHeldV1 as Cost};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::logical::{Change, Holding, Ledger};
use lash_kernel_state::paged::EditEpoch;
use lash_kernel_state::paged::heap::SemanticHeap;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

mod laws;
mod local;
mod oracle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    List,
    Map,
    Set,
    Record,
    Cell,
    Closure,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Int(i64),
    Float(i64),
    Text(Arc<str>),
}

impl Key {
    fn value(&self) -> Val {
        match self {
            Self::Int(n) => Val::Int(*n),
            Self::Float(n) => Val::Float(*n),
            Self::Text(s) => Val::Text(Arc::clone(s)),
        }
    }
    fn native(&self) -> lash_kernel_doc::Value {
        match self {
            Self::Int(n) => lash_kernel_doc::Value::Int(*n),
            Self::Float(n) => lash_kernel_doc::Value::Float(Float::new(*n as f64)),
            Self::Text(s) => {
                lash_kernel_doc::Value::Text(lash_kernel_doc::arena::ShortText::new(s).unwrap())
            }
        }
    }
    fn local_key(&self) -> Normal {
        match self {
            Self::Int(n) | Self::Float(n) => Normal::Number(*n),
            Self::Text(s) => Normal::Text(Arc::clone(s)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Normal {
    Number(i64),
    Text(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Val {
    Null,
    Absent,
    Int(i64),
    Float(i64),
    Text(Arc<str>),
    Bytes(Arc<[u8]>),
    Ref(u64),
    Tuple(Arc<[Val]>),
}

impl Val {
    fn refs(&self, visit: &mut impl FnMut(u64)) {
        match self {
            Self::Ref(id) => visit(*id),
            Self::Tuple(items) => {
                for item in &**items {
                    item.refs(visit);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    key: Key,
    value: Val,
    seq: u64,
}

#[derive(Clone, Debug)]
struct Body {
    kind: Kind,
    entries: Vec<Entry>,
    next_seq: u64,
    version: u64,
    structural: u64,
    cold: bool,
}

#[derive(Clone, Debug)]
struct Object {
    body: Body,
    born: usize,
    invocation: u64,
}

#[derive(Clone, Debug)]
struct Root {
    value: Val,
    producer: usize,
    invocation: u64,
    input: Option<u64>,
}

#[derive(Clone, Debug)]
struct World {
    objects: BTreeMap<u64, Object>,
    roots: BTreeMap<u32, Root>,
    calls: Vec<u64>,
    existing_count: u64,
    next_id: u64,
    next: usize,
    balance: u64,
    peak: u64,
    charges: Vec<ChargeEntry>,
    released: Vec<u64>,
}

type Members = Arc<[(Key, Val)]>;
type InitialObject = (Kind, Vec<(Key, Val)>, bool);

#[derive(Clone, Debug)]
enum Op {
    Allocate {
        to: u32,
        kind: Kind,
        members: Members,
    },
    Root {
        to: u32,
        value: Option<Val>,
    },
    Member {
        object: u64,
        key: Key,
        next: Option<Val>,
    },
    Read {
        object: u64,
        key: Key,
        to: u32,
    },
    Walk {
        object: u64,
        after: Option<u64>,
        to: u32,
    },
    Grow {
        object: u64,
        additional: usize,
    },
    Enter,
    Leave,
    Callback {
        object: u64,
        key: Key,
        to: u32,
        external: bool,
    },
}

#[derive(Clone, Debug)]
struct Scene {
    initial: Vec<InitialObject>,
    roots: Vec<(u32, Val)>,
    ops: Vec<Op>,
    charge: u64,
    memory: u64,
    depth: u32,
}

impl Scene {
    fn new(initial: Vec<InitialObject>, roots: Vec<(u32, Val)>, ops: Vec<Op>) -> Self {
        Self {
            initial,
            roots,
            ops,
            charge: u64::MAX,
            memory: u64::MAX,
            depth: u32::MAX,
        }
    }
}

fn source_equal(keys: &Arena, a: &Key, b: &Key) -> bool {
    keys.key(&a.native()).unwrap() == keys.key(&b.native()).unwrap()
}

fn source_value_cost(value: &Val) -> u64 {
    match value {
        Val::Null => Cost::local(&PageValue::Null),
        Val::Absent => Cost::local(&PageValue::Absent),
        Val::Int(n) => Cost::local(&PageValue::Int((*n).into())),
        Val::Float(n) => Cost::local(&PageValue::Float(Float::new(*n as f64))),
        Val::Text(s) => Cost::REFERENCE + s.len() as u64,
        Val::Bytes(s) => Cost::REFERENCE + s.len() as u64,
        // All mutable object occurrences have the same canonical price.
        Val::Ref(id) => Cost::local(&PageValue::List(entity(*id))),
        Val::Tuple(items) => Cost::REFERENCE + items.iter().map(source_value_cost).sum::<u64>(),
    }
}

fn source_body_cost(body: &Body) -> u64 {
    Cost::OBJECT
        + body
            .entries
            .iter()
            .map(|entry| {
                let value = source_value_cost(&entry.value);
                match body.kind {
                    Kind::Map | Kind::Set => source_value_cost(&entry.key.value()) + value,
                    Kind::Closure => Cost::ENTITY_EDGE,
                    Kind::Record => source_value_cost(&entry.key.value()) - Cost::REFERENCE + value,
                    _ => value,
                }
            })
            .sum::<u64>()
}

fn entity(id: u64) -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: id,
    }
}

fn source_holding(value: &Val, heap: &PagedHeap) -> Holding {
    let mut refs = Vec::new();
    value.refs(&mut |id| refs.push(HeapIdentity::Object(entity(id))));
    Holding::with_edges(source_value_cost(value), refs, heap.pages()).unwrap()
}

fn source_body_holding(body: &Body, heap: &PagedHeap) -> Holding {
    let mut refs = Vec::new();
    for entry in &body.entries {
        entry
            .value
            .refs(&mut |id| refs.push(HeapIdentity::Object(entity(id))));
    }
    Holding::with_edges(source_body_cost(body), refs, heap.pages()).unwrap()
}

fn graph_coordinate(ordinal: usize, invocation: u64) -> Coordinate {
    Coordinate {
        site: ordinal as u64,
        source_occurrence: ordinal as u64,
        invocation: id(IdentityKind::Invocation, invocation),
        occurrence: id(IdentityKind::Occurrence, ordinal as u64),
        phase: 0,
    }
}

fn graph_identity(world: &World, object: u64, salt: u64) -> Identity {
    if object < world.existing_count {
        id(IdentityKind::ExistingEntity, object)
    } else {
        id(IdentityKind::Entity, object + salt)
    }
}

fn semantic(value: &Val, world: &World, salt: u64) -> SemanticValue {
    match value {
        Val::Ref(object) => SemanticValue::Object {
            kind: world.objects[object].body.kind as u32,
            identity: graph_identity(world, *object, salt),
        },
        Val::Tuple(items) => {
            SemanticValue::Tuple(items.iter().map(|v| semantic(v, world, salt)).collect())
        }
        _ => {
            let mut bytes = Vec::new();
            encode(value, &mut bytes, &mut |_| {});
            SemanticValue::Scalar(bytes)
        }
    }
}

// References are encoded as positions and reported separately as typed edges.
// Physical handle numbers must never leak into canonical scalar/body bytes.
fn encode(value: &Val, out: &mut Vec<u8>, refs: &mut impl FnMut(u64)) {
    match value {
        Val::Null => out.push(0),
        Val::Absent => out.push(7),
        Val::Int(n) => {
            out.push(1);
            out.extend(n.to_be_bytes());
        }
        Val::Float(n) => {
            out.push(2);
            out.extend((*n as f64).to_bits().to_be_bytes());
        }
        Val::Text(s) => {
            out.push(3);
            out.extend((s.len() as u64).to_be_bytes());
            out.extend(s.as_bytes());
        }
        Val::Bytes(s) => {
            out.push(4);
            out.extend((s.len() as u64).to_be_bytes());
            out.extend(&**s);
        }
        Val::Ref(id) => {
            out.push(5);
            refs(*id);
        }
        Val::Tuple(items) => {
            out.push(6);
            out.extend((items.len() as u64).to_be_bytes());
            for v in &**items {
                encode(v, out, refs);
            }
        }
    }
}

fn graph_snapshot(
    world: &World,
    ordinal: usize,
    failure: Option<&str>,
    salt: u64,
) -> PrefixSnapshot {
    let invocation = world.calls.last().copied().unwrap_or(0);
    let at = graph_coordinate(ordinal, invocation);
    let mut values = Vec::new();
    let mut origins = Vec::new();
    let mut roots = vec![RootFact {
        owner: Owner::Frame {
            task: TaskId(0),
            invocation: 0,
        },
        direct_bytes: Cost::FRAME,
        edges: vec![],
    }];
    for call in &world.calls {
        roots.push(RootFact {
            owner: Owner::Frame {
                task: TaskId(0),
                invocation: *call,
            },
            direct_bytes: Cost::FRAME,
            edges: vec![],
        });
    }
    for (slot, root) in &world.roots {
        let producer = id(
            IdentityKind::Provenance,
            1_000 + root.producer as u64 * 256 + u64::from(*slot),
        );
        values.push((binding(*slot), semantic(&root.value, world, salt), producer));
        let mut inputs = Vec::new();
        if let Some(object) = root.input {
            inputs.push(id(IdentityKind::Provenance, 1_000_000 + object));
        } else {
            root.value
                .refs(&mut |object| inputs.push(id(IdentityKind::Provenance, 1_000_000 + object)));
        }
        origins.push(ValueOrigin {
            value: producer,
            at: graph_coordinate(root.producer, root.invocation),
            kernel: b"graph-fixture/root".to_vec(),
            inputs,
        });
        let mut counts = BTreeMap::new();
        root.value.refs(&mut |object| {
            *counts
                .entry(graph_identity(world, object, salt))
                .or_insert(0_u64) += 1
        });
        roots.push(RootFact {
            owner: binding(*slot),
            direct_bytes: source_value_cost(&root.value),
            edges: counts.into_iter().collect(),
        });
    }
    let mut retained = Vec::new();
    let mut edges = Vec::new();
    let mut dependencies = Vec::new();
    for (object, node) in world.objects.iter() {
        let object_id = graph_identity(world, *object, salt);
        origins.push(ValueOrigin {
            value: id(IdentityKind::Provenance, 1_000_000 + object),
            at: graph_coordinate(node.born, node.invocation),
            kernel: format!("graph-fixture/{:?}", node.body.kind).into_bytes(),
            inputs: vec![],
        });
        let mut bytes = vec![node.body.kind as u8];
        bytes.extend(node.body.next_seq.to_be_bytes());
        bytes.extend(node.body.version.to_be_bytes());
        bytes.extend(node.body.structural.to_be_bytes());
        let mut entries: Vec<_> = node.body.entries.iter().collect();
        entries.sort_by_key(|entry| entry.seq);
        let mut member = 0_u32;
        for entry in entries {
            bytes.extend(entry.seq.to_be_bytes());
            encode(&entry.key.value(), &mut bytes, &mut |_| {});
            encode(&entry.value, &mut bytes, &mut |target| {
                edges.push((object_id, member, graph_identity(world, target, salt), 1));
                member += 1;
            });
        }
        retained.push((object_id, bytes));
        dependencies.push((object_id, 1, node.body.version));
    }
    let mut continuation = (world.next as u64).to_be_bytes().to_vec();
    continuation.extend(world.next_id.to_be_bytes());
    PrefixSnapshot::observed(PrefixFacts {
        at: at.clone(),
        values,
        origins,
        roots,
        edges,
        retained,
        released: world
            .released
            .iter()
            .map(|object| graph_identity(world, *object, salt))
            .collect(),
        dependencies,
        balance: world.balance,
        peak: world.peak,
        charges: world.charges.clone(),
        debt_cursor: world.charges.len() as u64,
        debt: world
            .calls
            .iter()
            .map(|call| ChargeEntry {
                at: graph_coordinate(*call as usize, *call),
                kind: 90,
                units: 1,
            })
            .collect(),
        depth: world.calls.len() as u32,
        call_offsets: world
            .calls
            .iter()
            .map(|call| (id(IdentityKind::Invocation, *call), world.next as u64))
            .collect(),
        failure: failure.map(|reason| (reason.into(), at)),
        stop_payload: failure.map(|reason| SemanticValue::Scalar(reason.as_bytes().to_vec())),
        operands: vec![],
        pending: None,
        continuation,
        interruption: None,
    })
}

fn source_units(op: &Op) -> u64 {
    match op {
        Op::Allocate { members, .. } => 3 + members.len() as u64,
        Op::Member { next, .. } => 2 + u64::from(next.is_some()),
        Op::Grow { .. } => 1,
        Op::Enter => 5,
        Op::Leave => 1,
        _ => 2,
    }
}

fn key(text: &str) -> Key {
    Key::Text(text.into())
}
fn tuple(values: Vec<Val>) -> Val {
    Val::Tuple(values.into())
}
fn members(values: Vec<(Key, Val)>) -> Members {
    values.into()
}

fn body(kind: Kind, members: &[(Key, Val)], keys: &Arena, cold: bool) -> Body {
    let mut body = Body {
        kind,
        entries: vec![],
        next_seq: 0,
        version: 0,
        structural: 0,
        cold,
    };
    for (key, value) in members {
        if let Some(entry) = body
            .entries
            .iter_mut()
            .find(|entry| source_equal(keys, &entry.key, key))
        {
            entry.value = value.clone();
        } else {
            let seq = body.next_seq;
            body.next_seq += 1;
            body.entries.push(Entry {
                key: key.clone(),
                value: value.clone(),
                seq,
            });
        }
    }
    body
}
