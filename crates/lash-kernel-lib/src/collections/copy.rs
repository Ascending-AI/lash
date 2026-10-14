//! Deep copies preserve sharing, refuse cycles, and keep opaque identities.
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use lash_kernel_doc::{Element, EntityId, NativeError, NativeHeap, Object, Value};

use super::{native::snapshot, raise};
use crate::numbers::KIND_NAMES;
use lash_kernel_doc::heap::Payload;

pub(super) fn deep(value: &Value, heap: &mut dyn NativeHeap) -> Result<Value, NativeError> {
    Copy {
        heap,
        active: BTreeSet::new(),
        copied: BTreeMap::new(),
    }
    .value(value, 0)
}
struct Copy<'a> {
    heap: &'a mut dyn NativeHeap,
    active: BTreeSet<EntityId>,
    copied: BTreeMap<EntityId, Value>,
}
impl Copy<'_> {
    fn value(&mut self, value: &Value, depth: usize) -> Result<Value, NativeError> {
        if depth > self.heap.value_depth_limit().min(128) {
            return Err(raise(
                "too_deep",
                &format!(
                    "deep copy exceeds the configured {} levels",
                    self.heap.value_depth_limit().min(128)
                ),
            ));
        }
        match value {
            Value::Tuple(tuple) => {
                let mut values = Vec::new();
                self.heap.tuple_visit(tuple, &mut |v| {
                    values.push(v.clone());
                    ControlFlow::Continue(())
                })?;
                let values = self.items(&values, depth)?;
                Ok(Value::Tuple(self.heap.tuple(values)?))
            }
            Value::Error(error) => {
                let mut copied = (**error).clone();
                copied.family = None;
                let data = self.heap.error_data(error)?;
                copied.data = Payload::Resident(std::sync::Arc::new(self.value(&data, depth + 1)?));
                if let Payload::Resident(data) = &copied.data {
                    copied.task_edges = lash_kernel_doc::heap_values::task_count(data);
                    copied.cost = lash_kernel_doc::heap::LogicalBytes(
                        copied.kind.summary.utf8_bytes
                            + copied.message.summary.utf8_bytes
                            + lash_kernel_doc::heap_values::local_cost(data),
                    );
                }
                Ok(Value::Error(std::sync::Arc::new(copied)))
            }
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                self.object(value, *id, depth)
            }
            // A ref names its original object; closures, task handles and host handles
            // are opaque identities, not collection contents to duplicate.
            other => Ok(other.clone()),
        }
    }
    fn items(&mut self, items: &[Value], depth: usize) -> Result<Vec<Value>, NativeError> {
        items.iter().map(|x| self.value(x, depth + 1)).collect()
    }
    fn object(&mut self, value: &Value, id: EntityId, depth: usize) -> Result<Value, NativeError> {
        if self.active.contains(&id) {
            return Err(raise(
                "cyclic_value",
                "deep copy refuses a collection cycle",
            ));
        }
        if let Some(copied) = self.copied.get(&id) {
            return Ok(copied.clone());
        }
        self.active.insert(id);
        let object = match snapshot(value, self.heap)? {
            Object::List(items) => Object::List(self.items(&items, depth)?),
            Object::Set(items) => Object::Set(self.items(&items, depth)?),
            Object::Map(entries) => Object::Map(
                entries
                    .iter()
                    .map(|(key, value)| {
                        Ok((self.value(key, depth + 1)?, self.value(value, depth + 1)?))
                    })
                    .collect::<Result<_, NativeError>>()?,
            ),
            Object::Record(fields) => Object::Record(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), self.value(value, depth + 1)?)))
                    .collect::<Result<_, NativeError>>()?,
            ),
        };
        let copy = self.heap.allocate(object)?;
        let copied = match value {
            Value::List(_) => Value::List(copy),
            Value::Map(_) => Value::Map(copy),
            Value::Set(_) => Value::Set(copy),
            Value::Record(_) => Value::Record(copy),
            _ => unreachable!("object snapshot selected the four collection kinds"),
        };
        self.active.remove(&id);
        self.copied.insert(id, copied.clone());
        Ok(copied)
    }
}

/// Whether neither `value` nor anything a deep copy of it reads has a kind
/// `refused` marks, no collection holds itself and nothing nests more
/// than a deep copy takes. It reads what [`deep`] reads, in its order, at
/// its depths and each collection once, so [`deep`] copies a value it
/// passes without raising. Each collection it reads reserves its place
/// in what was read.
pub(super) fn holds_none(
    value: &Value,
    refused: &[bool; KIND_NAMES.len()],
    heap: &mut dyn NativeHeap,
) -> Result<bool, NativeError> {
    enum Visit {
        Value(Value, usize),
        Leave(EntityId),
    }
    let mut active = BTreeSet::new();
    let mut read = BTreeSet::new();
    let mut pending = vec![Visit::Value(value.clone(), 0)];
    while let Some(visit) = pending.pop() {
        let (value, depth) = match visit {
            Visit::Value(value, depth) => (value, depth),
            Visit::Leave(id) => {
                active.remove(&id);
                read.insert(id);
                continue;
            }
        };
        if depth > heap.value_depth_limit().min(128) || refused[value.kind() as usize] {
            return Ok(false);
        }
        // The members, in the order a copy reads them, are pushed in
        // reverse so the first is read first.
        let from = pending.len();
        match &value {
            Value::Tuple(tuple) => heap.tuple_visit(tuple, &mut |item| {
                pending.push(Visit::Value(item.clone(), depth + 1));
                ControlFlow::Continue(())
            })?,
            Value::Error(error) => pending.push(Visit::Value(heap.error_data(error)?, depth + 1)),
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                if active.contains(id) {
                    return Ok(false);
                }
                if read.contains(id) {
                    continue;
                }
                heap.reserve(1, 0)?;
                active.insert(*id);
                pending.push(Visit::Leave(*id));
                let from = pending.len();
                heap.visit(*id, &mut |element| {
                    match element {
                        Element::Item(item) => pending.push(Visit::Value(item.clone(), depth + 1)),
                        Element::Entry { key, value } => {
                            pending.push(Visit::Value(key.clone(), depth + 1));
                            pending.push(Visit::Value(value.clone(), depth + 1));
                        }
                        Element::Field { value, .. } => {
                            pending.push(Visit::Value(value.clone(), depth + 1));
                        }
                    }
                    ControlFlow::Continue(())
                })?;
                pending[from..].reverse();
                continue;
            }
            _ => {}
        }
        pending[from..].reverse();
    }
    Ok(true)
}
