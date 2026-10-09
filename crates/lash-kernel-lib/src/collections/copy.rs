//! Deep copies preserve sharing, refuse cycles, and keep opaque identities.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{ErrorValue, NativeError, NativeHeap, Object, ObjectId, Value};

use super::{native::snapshot, raise};

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
    active: BTreeSet<ObjectId>,
    copied: BTreeMap<ObjectId, Value>,
}
impl Copy<'_> {
    fn value(&mut self, value: &Value, depth: usize) -> Result<Value, NativeError> {
        if depth > 128 {
            return Err(raise("too_deep", "deep copy exceeds 128 levels"));
        }
        match value {
            Value::Tuple(items) => Ok(Value::Tuple(self.items(items, depth)?.into())),
            Value::Error(error) => Ok(Value::Error(std::sync::Arc::new(ErrorValue {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self.value(&error.data, depth + 1)?,
            }))),
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
    fn object(&mut self, value: &Value, id: ObjectId, depth: usize) -> Result<Value, NativeError> {
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
            _ => unreachable!(),
        };
        let copy = self.heap.allocate(object)?;
        let copied = match value {
            Value::List(_) => Value::List(copy),
            Value::Map(_) => Value::Map(copy),
            Value::Set(_) => Value::Set(copy),
            Value::Record(_) => Value::Record(copy),
            _ => unreachable!(),
        };
        self.active.remove(&id);
        self.copied.insert(id, copied.clone());
        Ok(copied)
    }
}
