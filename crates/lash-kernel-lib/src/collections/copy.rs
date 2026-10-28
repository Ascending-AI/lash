//! Deep copies preserve sharing, refuse cycles, and keep opaque identities.
use std::collections::HashMap;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{Args, NativeError, NewObject, Nursery, Value};

use super::native::{entries, items, snapshot};
use super::raise;
use crate::numbers::KIND_NAMES;

/// A handle's slot, as a call-local map key: what a copy has met, never
/// anything the run observes.
fn slot(handle: ArenaHandle) -> (u32, u32) {
    (handle.index(), handle.generation())
}

/// What a deep copy reads nests no deeper than this.
fn depth_limit(args: &Args<'_>) -> usize {
    args.limits().value_depth().min(128)
}

pub(super) fn deep<'call>(
    value: &Value,
    args: &Args<'call>,
    new: &mut Nursery<'call>,
) -> Result<Value, NativeError> {
    Copy {
        args,
        new,
        active: Vec::new(),
        copied: HashMap::new(),
    }
    .value(value, 0)
}
struct Copy<'a, 'call> {
    args: &'a Args<'call>,
    new: &'a mut Nursery<'call>,
    active: Vec<ArenaHandle>,
    copied: HashMap<(u32, u32), Value>,
}
impl Copy<'_, '_> {
    fn value(&mut self, value: &Value, depth: usize) -> Result<Value, NativeError> {
        #[cfg(test)]
        let _frame = crate::native_contract_laws::scratch(4 * 16);
        let limit = depth_limit(self.args);
        if depth > limit {
            return Err(raise(
                "too_deep",
                format!("deep copy exceeds the configured {limit} levels"),
            ));
        }
        match value {
            Value::Tuple(_) => {
                let values = items(self.args, value)?;
                let values = self.items(&values, depth)?;
                Ok(self.new.allocate(NewObject::Tuple(values)))
            }
            Value::Error(_) => {
                let Some(error) = self.args.error(value)? else {
                    return Err(NativeError::Malformed("an error names another body"));
                };
                let (kind, message) = (error.kind, error.message);
                let data = self.value(&error.data, depth + 1)?;
                Ok(self.new.allocate(NewObject::Error {
                    kind,
                    message,
                    data,
                }))
            }
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                self.object(value, *id, depth)
            }
            // A ref names its original object; closures, task handles and host handles
            // are opaque identities, not collection contents to duplicate.
            other => Ok(*other),
        }
    }
    fn items(&mut self, items: &[Value], depth: usize) -> Result<Vec<Value>, NativeError> {
        #[cfg(test)]
        let _members = crate::native_contract_laws::scratch_values(*self.args, items);
        #[cfg(test)]
        let mut _completed = Vec::new();
        #[cfg(test)]
        let mut _prefix = crate::native_contract_laws::scratch(0);
        items
            .iter()
            .map(|x| {
                let value = self.value(x, depth + 1)?;
                #[cfg(test)]
                {
                    _completed.push(value);
                    _prefix.resize(crate::native_contract_laws::new_values_bytes(
                        *self.args,
                        self.new,
                        &_completed,
                    ));
                }
                Ok(value)
            })
            .collect()
    }
    fn pairs(
        &mut self,
        pairs: &[(Value, Value)],
        depth: usize,
        keys: bool,
    ) -> Result<Vec<(Value, Value)>, NativeError> {
        #[cfg(test)]
        let _pairs = crate::native_contract_laws::scratch_values(
            *self.args,
            &pairs.iter().flat_map(|(a, b)| [*a, *b]).collect::<Vec<_>>(),
        );
        #[cfg(test)]
        let mut _completed = Vec::new();
        #[cfg(test)]
        let mut _prefix = crate::native_contract_laws::scratch(0);
        pairs
            .iter()
            .map(|(key, value)| {
                let key = if keys {
                    self.value(key, depth + 1)?
                } else {
                    *key
                };
                #[cfg(test)]
                {
                    _completed.push(key);
                    _prefix.resize(crate::native_contract_laws::new_values_bytes(
                        *self.args,
                        self.new,
                        &_completed,
                    ));
                }
                let value = self.value(value, depth + 1)?;
                #[cfg(test)]
                {
                    _completed.push(value);
                    _prefix.resize(crate::native_contract_laws::new_values_bytes(
                        *self.args,
                        self.new,
                        &_completed,
                    ));
                }
                Ok((key, value))
            })
            .collect()
    }
    fn object(
        &mut self,
        value: &Value,
        id: ArenaHandle,
        depth: usize,
    ) -> Result<Value, NativeError> {
        if self.active.contains(&id) {
            return Err(raise(
                "cyclic_value",
                "deep copy refuses a collection cycle",
            ));
        }
        if let Some(copied) = self.copied.get(&slot(id)) {
            return Ok(*copied);
        }
        self.active.push(id);
        let object = match snapshot(self.args, value)? {
            NewObject::List(items) => NewObject::List(self.items(&items, depth)?),
            NewObject::Set(items) => NewObject::Set(self.items(&items, depth)?),
            NewObject::Map(entries) => NewObject::Map(self.pairs(&entries, depth, true)?),
            NewObject::Record(fields) => NewObject::Record(self.pairs(&fields, depth, false)?),
            _ => unreachable!("object snapshot selected the four collection kinds"),
        };
        let copied = self.new.allocate(object);
        self.active.pop();
        self.copied.insert(slot(id), copied);
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
    args: &Args<'_>,
    new: &mut Nursery<'_>,
) -> Result<bool, NativeError> {
    enum Visit {
        Value(Value, usize),
        Leave(ArenaHandle),
    }
    let limit = depth_limit(args);
    let mut active: Vec<ArenaHandle> = Vec::new();
    let mut read = std::collections::HashSet::new();
    let mut pending = vec![Visit::Value(*value, 0)];
    while !pending.is_empty() {
        #[cfg(test)]
        let _frontier = crate::native_contract_laws::scratch_values(
            *args,
            &pending
                .iter()
                .filter_map(|visit| match visit {
                    Visit::Value(value, _) => Some(*value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
        #[cfg(test)]
        let _visited =
            crate::native_contract_laws::scratch((active.len() + read.len()) as u64 * 16);
        let visit = pending
            .pop()
            .unwrap_or_else(|| unreachable!("nonempty frontier"));
        let (value, depth) = match visit {
            Visit::Value(value, depth) => (value, depth),
            Visit::Leave(id) => {
                active.retain(|held| *held != id);
                read.insert(slot(id));
                continue;
            }
        };
        if depth > limit || refused[value.kind() as usize] {
            return Ok(false);
        }
        // The members, in the order a copy reads them, are pushed in
        // reverse so the first is read first.
        let from = pending.len();
        match &value {
            Value::Tuple(_) => pending.extend(
                items(args, &value)?
                    .into_iter()
                    .map(|item| Visit::Value(item, depth + 1)),
            ),
            Value::Error(_) => {
                let Some(error) = args.error(&value)? else {
                    return Err(NativeError::Malformed("an error names another body"));
                };
                pending.push(Visit::Value(error.data, depth + 1));
            }
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                if active.contains(id) {
                    return Ok(false);
                }
                if read.contains(&slot(*id)) {
                    continue;
                }
                new.reserve(1, 0)?;
                active.push(*id);
                pending.push(Visit::Leave(*id));
                let from = pending.len();
                match &value {
                    Value::List(_) => pending.extend(
                        items(args, &value)?
                            .into_iter()
                            .map(|item| Visit::Value(item, depth + 1)),
                    ),
                    Value::Set(_) => pending.extend(
                        entries(args, &value)?
                            .into_iter()
                            .map(|(key, _)| Visit::Value(key, depth + 1)),
                    ),
                    Value::Map(_) => {
                        for (key, value) in entries(args, &value)? {
                            pending.push(Visit::Value(key, depth + 1));
                            pending.push(Visit::Value(value, depth + 1));
                        }
                    }
                    _ => pending.extend(
                        entries(args, &value)?
                            .into_iter()
                            .map(|(_, value)| Visit::Value(value, depth + 1)),
                    ),
                }
                pending[from..].reverse();
                continue;
            }
            _ => {}
        }
        pending[from..].reverse();
    }
    Ok(true)
}
