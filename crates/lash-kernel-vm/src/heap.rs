//! The run's heap: the mutable kinds, with identity, sharing and memory
//! accounting (`K-VAL-011`, `K-BND-003`).
//!
//! An object is named by an [`ObjectId`] taken in allocation order. A map
//! and a set keep their entries under a sequence number, so a loop over one
//! holds a position that insertions and removals do not move
//! (`K-ITER-003`).

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use lash_kernel_doc::{
    Bytes, Element, ErrorValue, Identity, NativeError, NativeHeap, Object, ObjectId, TaskId, Value,
};
use num_bigint::BigInt;
use num_traits::FromPrimitive;

use crate::compile::CodeId;

/// The deepest an immutable value may nest tuples and errors, and the
/// deepest a value copied out of the run may nest at all (`K-VAL-034`).
pub(crate) const MAX_VALUE_DEPTH: usize = 128;

/// What an object costs before its contents.
const OBJECT_BYTES: u64 = 32;
/// What a value costs where it is held, before its payload.
const VALUE_BYTES: u64 = 16;
/// The heap is not collected below this much accounted memory.
const MIN_COLLECT_BYTES: u64 = 1 << 20;

/// A legal key in its normal form: two keys are the same key exactly when
/// their normal forms are equal (`K-KEY-002`). An integral float is its
/// integer, `-0.0` is zero and every NaN is one key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Null,
    Bool(bool),
    Int(BigInt),
    /// A finite float with a fraction, or an infinity, by its bits.
    Float(u64),
    Nan,
    Text(Arc<str>),
    Bytes(Bytes),
    Timestamp(BigInt),
    Ref(Identity),
    Tuple(Vec<Key>),
}

impl Key {
    /// The key a value is, or `None` when it is not a legal key
    /// (`K-KEY-001`).
    pub(crate) fn of(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Null => Self::Null,
            Value::Bool(flag) => Self::Bool(*flag),
            Value::Int(integer) => Self::Int(integer.as_bigint().clone()),
            Value::Float(float) => {
                let float = float.get();
                if float.is_nan() {
                    Self::Nan
                } else if float.is_finite() && float.fract() == 0.0 {
                    Self::Int(BigInt::from_f64(float)?)
                } else {
                    Self::Float(float.to_bits())
                }
            }
            Value::Text(text) => Self::Text(Arc::clone(text)),
            Value::Bytes(bytes) => Self::Bytes(bytes.clone()),
            Value::Timestamp(timestamp) => {
                Self::Timestamp(timestamp.nanoseconds.as_bigint().clone())
            }
            Value::Ref(identity) => Self::Ref(*identity),
            Value::Tuple(members) => {
                Self::Tuple(members.iter().map(Self::of).collect::<Option<_>>()?)
            }
            _ => return None,
        })
    }
}

/// The entries of a map or a set, in insertion order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Table {
    index: BTreeMap<Key, u64>,
    /// Each entry under its sequence number: the key as first written, and
    /// the value (null for a set).
    entries: BTreeMap<u64, (Value, Value)>,
    next: u64,
}

impl Table {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(&self, key: &Key) -> Option<&Value> {
        let sequence = self.index.get(key)?;
        self.entries.get(sequence).map(|(_, value)| value)
    }

    pub(crate) fn contains(&self, key: &Key) -> bool {
        self.index.contains_key(key)
    }

    /// Writes `value` under `key`. A key the table holds keeps its first
    /// spelling and its position (`K-KEY-003`).
    pub(crate) fn insert(&mut self, key: Key, written: Value, value: Value) {
        if let Some(sequence) = self.index.get(&key) {
            if let Some(entry) = self.entries.get_mut(sequence) {
                entry.1 = value;
            }
            return;
        }
        self.index.insert(key, self.next);
        self.entries.insert(self.next, (written, value));
        self.next += 1;
    }

    pub(crate) fn remove(&mut self, key: &Key) {
        if let Some(sequence) = self.index.remove(key) {
            self.entries.remove(&sequence);
        }
    }

    /// The first entry after sequence number `last`, with its number.
    pub(crate) fn after(&self, last: Option<u64>) -> Option<(u64, &Value)> {
        let from = last.map_or(0, |last| last.saturating_add(1));
        self.entries
            .range(from..)
            .next()
            .map(|(sequence, (key, _))| (*sequence, key))
    }

    /// How many entries sit at or before sequence number `last`: a loop's
    /// position as a count, which outlives the numbering.
    pub(crate) fn passed(&self, last: Option<u64>) -> u64 {
        last.map_or(0, |last| self.entries.range(..=last).count() as u64)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Value, &Value)> {
        self.entries.values().map(|(key, value)| (key, value))
    }
}

/// A closure: its code and the variables it shares, in the order the code
/// lists its captures.
#[derive(Clone, Debug)]
pub(crate) struct ClosureObj {
    pub(crate) code: CodeId,
    pub(crate) captures: Vec<ObjectId>,
}

#[derive(Clone, Debug)]
pub(crate) enum Obj {
    List(Vec<Value>),
    Map(Table),
    Set(Table),
    Record(Vec<(String, Value)>),
    Closure(ClosureObj),
    /// A variable a closure shares with its defining scope.
    Variable(Value),
}

/// The bytes a value is accounted where it is held.
pub(crate) fn value_bytes(value: &Value) -> u64 {
    let payload = match value {
        Value::Int(integer) => integer.as_bigint().bits().div_ceil(8),
        Value::Timestamp(timestamp) => timestamp.nanoseconds.as_bigint().bits().div_ceil(8),
        Value::Text(text) => text.len() as u64,
        Value::Bytes(bytes) => bytes.as_slice().len() as u64,
        Value::Function(name) => name.as_str().len() as u64,
        Value::Handle(handle) => (handle.kind.len() + handle.id.len()) as u64,
        Value::Tuple(members) => members.iter().map(value_bytes).sum(),
        Value::Error(error) => error_bytes(error),
        _ => 0,
    };
    VALUE_BYTES.saturating_add(payload)
}

fn error_bytes(error: &ErrorValue) -> u64 {
    ((error.kind.len() + error.message.len()) as u64).saturating_add(value_bytes(&error.data))
}

/// The bytes an object is accounted, contents included.
pub(crate) fn object_bytes(object: &Obj) -> u64 {
    let contents: u64 = match object {
        Obj::List(items) => items.iter().map(value_bytes).sum(),
        Obj::Map(table) | Obj::Set(table) => table
            .iter()
            .map(|(key, value)| value_bytes(key).saturating_add(value_bytes(value)))
            .sum(),
        Obj::Record(fields) => fields
            .iter()
            .map(|(name, value)| (name.len() as u64).saturating_add(value_bytes(value)))
            .sum(),
        Obj::Closure(closure) => closure.captures.len() as u64 * 8,
        Obj::Variable(value) => value_bytes(value),
    };
    OBJECT_BYTES.saturating_add(contents)
}

/// Whether a value nests tuples and errors no deeper than `budget` levels.
pub(crate) fn within_depth(value: &Value, budget: usize) -> bool {
    match value {
        Value::Tuple(members) => {
            budget > 0
                && members
                    .iter()
                    .all(|member| within_depth(member, budget - 1))
        }
        Value::Error(error) => budget > 0 && within_depth(&error.data, budget - 1),
        _ => true,
    }
}

/// Adds the heap objects and the tasks a value names, through tuples and
/// errors.
pub(crate) fn refs(value: &Value, out: &mut Vec<Identity>) {
    match value {
        Value::Tuple(members) => members.iter().for_each(|member| refs(member, out)),
        Value::Error(error) => refs(&error.data, out),
        Value::Ref(identity) => out.push(*identity),
        Value::Task(task) => out.push(Identity::Task(*task)),
        other => out.extend(other.object().map(Identity::Object)),
    }
}

/// An object and when it was last written.
#[derive(Debug)]
struct Held {
    object: Obj,
    /// The heap's write clock at the object's last write: a save rewrites
    /// the fragment of an object whose stamp has moved.
    written: u64,
}

#[derive(Debug)]
pub(crate) struct Heap {
    objects: BTreeMap<u64, Held>,
    next: u64,
    /// Counts writes: every allocation and every mutable borrow.
    clock: u64,
    /// The accounted memory: exact after a collection, an upper bound of
    /// what is live between two.
    pub(crate) memory: u64,
    /// The accounted memory at which the next collection runs.
    pub(crate) collect_at: u64,
}

impl Heap {
    pub(crate) fn new(bound: u64) -> Self {
        Self {
            objects: BTreeMap::new(),
            next: 0,
            clock: 0,
            memory: 0,
            collect_at: bound.min(MIN_COLLECT_BYTES),
        }
    }

    pub(crate) fn get(&self, id: ObjectId) -> Option<&Obj> {
        self.objects.get(&id.0).map(|held| &held.object)
    }

    /// Borrows an object to write it, and stamps it written.
    pub(crate) fn get_mut(&mut self, id: ObjectId) -> Option<&mut Obj> {
        let held = self.objects.get_mut(&id.0)?;
        self.clock += 1;
        held.written = self.clock;
        Some(&mut held.object)
    }

    /// Stores an object. The caller has accounted its bytes.
    pub(crate) fn insert(&mut self, object: Obj) -> ObjectId {
        let id = self.next;
        self.next += 1;
        self.clock += 1;
        let written = self.clock;
        self.objects.insert(id, Held { object, written });
        ObjectId(id)
    }

    /// Puts back an object of a parked run under the identity it had,
    /// unwritten: stamp 0 is what a loaded baseline holds for it. Returns
    /// whether the identity was free.
    pub(crate) fn restore(&mut self, id: ObjectId, object: Obj) -> bool {
        let held = Held { object, written: 0 };
        self.objects.insert(id.0, held).is_none()
    }

    /// How many objects the run has allocated: the next identity.
    pub(crate) fn allocated(&self) -> u64 {
        self.next
    }

    /// Sets the next identity, when a parked run is put back. Returns
    /// whether every object the heap holds is below it.
    pub(crate) fn set_allocated(&mut self, allocated: u64) -> bool {
        self.next = allocated;
        self.objects
            .keys()
            .next_back()
            .is_none_or(|last| *last < allocated)
    }

    /// Every object the heap holds, ascending by identity.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (ObjectId, &Obj)> {
        self.objects
            .iter()
            .map(|(id, held)| (ObjectId(*id), &held.object))
    }

    /// The write stamp of an object.
    pub(crate) fn written(&self, id: ObjectId) -> u64 {
        self.objects.get(&id.0).map_or(0, |held| held.written)
    }

    pub(crate) fn list(&self, id: ObjectId) -> Option<&Vec<Value>> {
        match self.get(id)? {
            Obj::List(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn table(&self, id: ObjectId) -> Option<&Table> {
        match self.get(id)? {
            Obj::Map(table) | Obj::Set(table) => Some(table),
            _ => None,
        }
    }

    pub(crate) fn record(&self, id: ObjectId) -> Option<&Vec<(String, Value)>> {
        match self.get(id)? {
            Obj::Record(fields) => Some(fields),
            _ => None,
        }
    }

    pub(crate) fn variable(&self, id: ObjectId) -> Option<&Value> {
        match self.get(id)? {
            Obj::Variable(value) => Some(value),
            _ => None,
        }
    }

    /// The value that names object `id`, by the object's kind.
    pub(crate) fn value_of(&self, id: ObjectId) -> Option<Value> {
        Some(match self.get(id)? {
            Obj::List(_) => Value::List(id),
            Obj::Map(_) => Value::Map(id),
            Obj::Set(_) => Value::Set(id),
            Obj::Record(_) => Value::Record(id),
            Obj::Closure(_) => Value::Closure(id),
            Obj::Variable(_) => return None,
        })
    }

    /// Frees every object `seeds` do not reach and returns the bytes of
    /// those they do. A task that is reached is handed to `task`, which
    /// adds what the task's result names.
    pub(crate) fn collect(
        &mut self,
        mut seeds: Vec<Identity>,
        task: &mut dyn FnMut(TaskId, &mut Vec<Identity>),
    ) -> u64 {
        let mut live: BTreeMap<u64, Held> = BTreeMap::new();
        let mut bytes = 0u64;
        while let Some(seed) = seeds.pop() {
            let id = match seed {
                Identity::Object(id) => id,
                Identity::Task(reached) => {
                    task(reached, &mut seeds);
                    continue;
                }
            };
            let Some(held) = self.objects.remove(&id.0) else {
                continue;
            };
            bytes = bytes.saturating_add(object_bytes(&held.object));
            match &held.object {
                Obj::List(items) => items.iter().for_each(|item| refs(item, &mut seeds)),
                Obj::Map(table) | Obj::Set(table) => {
                    for (key, value) in table.iter() {
                        refs(key, &mut seeds);
                        refs(value, &mut seeds);
                    }
                }
                Obj::Record(fields) => fields.iter().for_each(|(_, value)| refs(value, &mut seeds)),
                Obj::Closure(closure) => {
                    seeds.extend(closure.captures.iter().copied().map(Identity::Object));
                }
                Obj::Variable(value) => refs(value, &mut seeds),
            }
            live.insert(id.0, held);
        }
        self.objects = live;
        bytes
    }

    /// Sets the accounted memory after a collection and when the next one
    /// runs.
    pub(crate) fn settle(&mut self, live: u64, bound: u64) {
        self.memory = live;
        self.collect_at = bound.min(live.saturating_mul(2).max(MIN_COLLECT_BYTES));
    }
}

/// A native function's view of the heap: it reads what exists and
/// allocates what it returns, within the run's memory bound (`K-LIB-007`).
pub(crate) struct NativeView<'a> {
    pub(crate) heap: &'a mut Heap,
    pub(crate) bound: u64,
    /// What the call has reserved and not yet allocated.
    pub(crate) reserved: u64,
}

fn refused(message: &str) -> NativeError {
    NativeError::Raised(ErrorValue::new("type_error", message))
}

fn table_of(entries: Vec<(Value, Value)>) -> Result<Table, NativeError> {
    let mut table = Table::default();
    for (key, value) in entries {
        let normal = Key::of(&key).ok_or_else(|| refused("a key is not a legal key"))?;
        table.insert(normal, key, value);
    }
    Ok(table)
}

impl NativeHeap for NativeView<'_> {
    fn len(&self, object: ObjectId) -> usize {
        match self.heap.get(object) {
            Some(Obj::List(items)) => items.len(),
            Some(Obj::Map(table) | Obj::Set(table)) => table.len(),
            Some(Obj::Record(fields)) => fields.len(),
            _ => 0,
        }
    }

    fn list_get(&self, list: ObjectId, index: usize) -> Option<Value> {
        self.heap.list(list)?.get(index).cloned()
    }

    fn map_get(&self, map: ObjectId, key: &Value) -> Option<Value> {
        match self.heap.get(map)? {
            Obj::Map(table) => table.get(&Key::of(key)?).cloned(),
            _ => None,
        }
    }

    fn set_contains(&self, set: ObjectId, member: &Value) -> bool {
        match (self.heap.get(set), Key::of(member)) {
            (Some(Obj::Set(table)), Some(key)) => table.contains(&key),
            _ => false,
        }
    }

    fn record_get(&self, record: ObjectId, field: &str) -> Option<Value> {
        self.heap
            .record(record)?
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.clone())
    }

    fn visit(&self, object: ObjectId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) {
        let _ = match self.heap.get(object) {
            Some(Obj::List(items)) => items
                .iter()
                .try_for_each(|item| visitor(Element::Item(item))),
            Some(Obj::Set(table)) => table
                .iter()
                .try_for_each(|(member, _)| visitor(Element::Item(member))),
            Some(Obj::Map(table)) => table
                .iter()
                .try_for_each(|(key, value)| visitor(Element::Entry { key, value })),
            Some(Obj::Record(fields)) => fields
                .iter()
                .try_for_each(|(name, value)| visitor(Element::Field { name, value })),
            _ => ControlFlow::Continue(()),
        };
    }

    fn allocate(&mut self, object: Object) -> Result<ObjectId, NativeError> {
        let object = match object {
            Object::List(items) => Obj::List(items),
            Object::Map(entries) => Obj::Map(table_of(entries)?),
            Object::Set(members) => Obj::Set(table_of(
                members
                    .into_iter()
                    .map(|member| (member, Value::Null))
                    .collect(),
            )?),
            Object::Record(fields) => {
                let mut kept: Vec<(String, Value)> = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    match kept.iter_mut().find(|(field, _)| *field == name) {
                        Some(field) => field.1 = value,
                        None => kept.push((name, value)),
                    }
                }
                Obj::Record(kept)
            }
            Object::Closure(_) | Object::Variable(_) => {
                return Err(refused(
                    "a native function allocates lists, maps, sets and records",
                ));
            }
        };
        // An object draws on what the call reserved before it counts
        // against the room that is left.
        let bytes = object_bytes(&object);
        let reserved = self.reserved.saturating_sub(bytes);
        let memory = self.heap.memory.saturating_add(bytes);
        if memory.saturating_add(reserved) > self.bound {
            return Err(NativeError::Memory);
        }
        self.reserved = reserved;
        self.heap.memory = memory;
        Ok(self.heap.insert(object))
    }

    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let reserved = self
            .reserved
            .saturating_add(values.saturating_mul(VALUE_BYTES))
            .saturating_add(bytes);
        if self.heap.memory.saturating_add(reserved) > self.bound {
            return Err(NativeError::Memory);
        }
        self.reserved = reserved;
        Ok(())
    }
}
