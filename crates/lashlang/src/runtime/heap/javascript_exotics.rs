use super::*;
use crate::runtime::{javascript_to_number, javascript_to_string};

pub(crate) const MAX_JAVASCRIPT_LENGTH: u64 = 9_007_199_254_740_991;

/// Non-durable slot for WP-C's compiled matcher.
///
/// The substrate deliberately carries no regex engine. WP-C replaces the
/// empty marker with its compiled program while retaining the important wire
/// rule: this slot is absent from every persistence wire and empty after
/// restore.
#[derive(Clone, Debug)]
pub(crate) struct RegExpProgramCache;

#[derive(Debug)]
pub(crate) struct RegExpObject {
    pub(crate) pattern: String,
    pub(crate) flags: String,
    pub(crate) last_index: u64,
    pub(crate) compiled_program: Option<Box<RegExpProgramCache>>,
}

impl Clone for RegExpObject {
    fn clone(&self) -> Self {
        Self {
            pattern: self.pattern.clone(),
            flags: self.flags.clone(),
            last_index: self.last_index,
            // Heap clones are in-process transactional copies. Persistence is
            // controlled by the explicit wire conversion, which omits this.
            compiled_program: self.compiled_program.clone(),
        }
    }
}

impl PartialEq for RegExpObject {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
            && self.flags == other.flags
            && self.last_index == other.last_index
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MapObject {
    pub(crate) entries: Vec<(Value, Value)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SetObject {
    pub(crate) values: Vec<Value>,
}

/// Immutable because the TypeScript dialect intentionally exposes no Date
/// setters. WP-D may add reads, but every mutation remains absent by design.
#[derive(Clone, Debug)]
pub(crate) struct DateObject {
    pub(crate) milliseconds: f64,
}

impl PartialEq for DateObject {
    fn eq(&self, other: &Self) -> bool {
        self.milliseconds == other.milliseconds
            || (self.milliseconds.is_nan() && other.milliseconds.is_nan())
    }
}

impl Heap {
    pub(crate) fn allocate_regexp(
        &mut self,
        pattern: String,
        flags: String,
    ) -> Result<Value, RuntimeError> {
        self.allocate_object(HeapObject::RegExp(RegExpObject {
            pattern,
            flags,
            last_index: 0,
            compiled_program: None,
        }))
    }

    pub(crate) fn allocate_map(
        &mut self,
        entries: Vec<(Value, Value)>,
    ) -> Result<Value, RuntimeError> {
        let entry_count = entries.len();
        let values = entries
            .into_iter()
            .flat_map(|(key, value)| [key, value])
            .collect::<Vec<_>>();
        let mut values = self.import_values(values, 0)?.into_iter();
        let mut normalized = Vec::<(Value, Value)>::with_capacity(entry_count);
        while let (Some(key), Some(value)) = (values.next(), values.next()) {
            let key = normalize_same_value_zero_storage(key);
            if let Some((_, stored)) = normalized
                .iter_mut()
                .find(|(candidate, _)| same_value_zero(candidate, &key))
            {
                *stored = value;
            } else {
                normalized.push((key, value));
            }
        }
        self.allocate_object(HeapObject::Map(MapObject {
            entries: normalized,
        }))
    }

    pub(crate) fn allocate_set(&mut self, values: Vec<Value>) -> Result<Value, RuntimeError> {
        let values = self.import_values(values, 0)?;
        let mut normalized = Vec::with_capacity(values.len());
        for value in values {
            let value = normalize_same_value_zero_storage(value);
            if !normalized
                .iter()
                .any(|candidate| same_value_zero(candidate, &value))
            {
                normalized.push(value);
            }
        }
        self.allocate_object(HeapObject::Set(SetObject { values: normalized }))
    }

    pub(crate) fn allocate_date(&mut self, milliseconds: f64) -> Result<Value, RuntimeError> {
        self.allocate_object(HeapObject::Date(DateObject { milliseconds }))
    }

    pub(crate) fn is_javascript_exotic(&self, id: HeapId) -> Result<bool, RuntimeError> {
        Ok(matches!(
            self.get(id)?,
            HeapObject::RegExp(_) | HeapObject::Map(_) | HeapObject::Set(_) | HeapObject::Date(_)
        ))
    }

    pub(crate) fn regexp_last_index(&self, id: HeapId) -> Result<Option<u64>, RuntimeError> {
        Ok(match self.get(id)? {
            HeapObject::RegExp(regexp) => Some(regexp.last_index),
            _ => None,
        })
    }

    pub(crate) fn set_regexp_last_index(
        &mut self,
        id: HeapId,
        last_index: u64,
    ) -> Result<(), RuntimeError> {
        self.update_object(id, |object| {
            let HeapObject::RegExp(regexp) = object else {
                return false;
            };
            regexp.last_index = last_index;
            true
        })
    }

    pub(crate) fn map_entries(
        &self,
        id: HeapId,
    ) -> Result<Option<Vec<(Value, Value)>>, RuntimeError> {
        Ok(match self.get(id)? {
            HeapObject::Map(map) => Some(map.entries.clone()),
            _ => None,
        })
    }

    pub(crate) fn set_values(&self, id: HeapId) -> Result<Option<Vec<Value>>, RuntimeError> {
        Ok(match self.get(id)? {
            HeapObject::Set(set) => Some(set.values.clone()),
            _ => None,
        })
    }

    pub(crate) fn date_milliseconds(&self, id: HeapId) -> Result<Option<f64>, RuntimeError> {
        Ok(match self.get(id)? {
            HeapObject::Date(date) => Some(date.milliseconds),
            _ => None,
        })
    }

    pub(crate) fn map_get(&self, id: HeapId, key: &Value) -> Result<Option<Value>, RuntimeError> {
        let HeapObject::Map(map) = self.get(id)? else {
            return Ok(None);
        };
        Ok(map
            .entries
            .iter()
            .find(|(candidate, _)| same_value_zero(candidate, key))
            .map(|(_, value)| value.clone()))
    }

    pub(crate) fn map_has(&self, id: HeapId, key: &Value) -> Result<bool, RuntimeError> {
        let HeapObject::Map(map) = self.get(id)? else {
            return Ok(false);
        };
        Ok(map
            .entries
            .iter()
            .any(|(candidate, _)| same_value_zero(candidate, key)))
    }

    pub(crate) fn map_set(
        &mut self,
        id: HeapId,
        key: Value,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let mut imported = self.import_values(vec![key, value], 0)?.into_iter();
        let key = normalize_same_value_zero_storage(imported.next().expect("Map key imported"));
        let value = imported.next().expect("Map value imported");
        self.update_object(id, |object| {
            let HeapObject::Map(map) = object else {
                return false;
            };
            if let Some((_, stored)) = map
                .entries
                .iter_mut()
                .find(|(candidate, _)| same_value_zero(candidate, &key))
            {
                *stored = value;
            } else {
                map.entries.push((key, value));
            }
            true
        })
    }

    pub(crate) fn map_delete(&mut self, id: HeapId, key: &Value) -> Result<bool, RuntimeError> {
        let mut deleted = false;
        self.update_object(id, |object| {
            let HeapObject::Map(map) = object else {
                return false;
            };
            if let Some(index) = map
                .entries
                .iter()
                .position(|(candidate, _)| same_value_zero(candidate, key))
            {
                map.entries.remove(index);
                deleted = true;
            }
            true
        })?;
        Ok(deleted)
    }

    pub(crate) fn map_clear(&mut self, id: HeapId) -> Result<(), RuntimeError> {
        self.update_object(id, |object| {
            let HeapObject::Map(map) = object else {
                return false;
            };
            map.entries.clear();
            true
        })
    }

    pub(crate) fn set_has(&self, id: HeapId, value: &Value) -> Result<bool, RuntimeError> {
        let HeapObject::Set(set) = self.get(id)? else {
            return Ok(false);
        };
        Ok(set
            .values
            .iter()
            .any(|candidate| same_value_zero(candidate, value)))
    }

    pub(crate) fn set_add(&mut self, id: HeapId, value: Value) -> Result<(), RuntimeError> {
        let value = self.import_values(vec![value], 0)?.remove(0);
        let value = normalize_same_value_zero_storage(value);
        self.update_object(id, |object| {
            let HeapObject::Set(set) = object else {
                return false;
            };
            if !set
                .values
                .iter()
                .any(|candidate| same_value_zero(candidate, &value))
            {
                set.values.push(value);
            }
            true
        })
    }

    pub(crate) fn set_delete(&mut self, id: HeapId, value: &Value) -> Result<bool, RuntimeError> {
        let mut deleted = false;
        self.update_object(id, |object| {
            let HeapObject::Set(set) = object else {
                return false;
            };
            if let Some(index) = set
                .values
                .iter()
                .position(|candidate| same_value_zero(candidate, value))
            {
                set.values.remove(index);
                deleted = true;
            }
            true
        })?;
        Ok(deleted)
    }

    pub(crate) fn set_clear(&mut self, id: HeapId) -> Result<(), RuntimeError> {
        self.update_object(id, |object| {
            let HeapObject::Set(set) = object else {
                return false;
            };
            set.values.clear();
            true
        })
    }

    fn update_object(
        &mut self,
        id: HeapId,
        update: impl FnOnce(&mut HeapObject) -> bool,
    ) -> Result<(), RuntimeError> {
        let mut object = self.get(id)?.clone();
        if !update(&mut object) {
            return Err(RuntimeError::ValidationFailed {
                reason: "TS_METHOD_UNSUPPORTED: receiver has the wrong heap kind".to_string(),
            });
        }
        self.commit_object_update(id, object)
    }

    fn commit_object_update(&mut self, id: HeapId, object: HeapObject) -> Result<(), RuntimeError> {
        let slot = self
            .id_to_slot
            .get(&id)
            .copied()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let old_bytes = self
            .slots
            .get(slot)
            .and_then(Option::as_ref)
            .map(|entry| entry.logical_bytes)
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let new_bytes = object.logical_bytes();
        let next_live = self
            .live_logical_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        let entry = self.slots[slot]
            .as_mut()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let old_children = entry.object.child_refs();
        let new_children = object.child_refs();
        entry.object = object;
        entry.logical_bytes = new_bytes;
        self.live_logical_bytes = next_live;
        self.retarget_parent_edges(id, &old_children, &new_children);
        self.invalidate_materialized_reaching(id);
        self.debug_assert_byte_accounting();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn replace_object(
        &mut self,
        id: HeapId,
        object: HeapObject,
    ) -> Result<(), RuntimeError> {
        self.commit_object_update(id, object)
    }

    /// Applies JavaScript's object-to-primitive conversion without detaching a
    /// heap object or recursing through `Value::Ref` unchanged.
    pub(crate) fn javascript_to_primitive_string_or_number(
        &self,
        value: &Value,
    ) -> Result<Value, RuntimeError> {
        self.javascript_to_primitive_inner(value, &mut BTreeSet::new())
    }

    pub(crate) fn javascript_to_number(&self, value: &Value) -> Result<f64, RuntimeError> {
        let primitive = self.javascript_to_primitive_string_or_number(value)?;
        Ok(javascript_to_number(&primitive))
    }

    fn javascript_to_primitive_inner(
        &self,
        value: &Value,
        active: &mut BTreeSet<HeapId>,
    ) -> Result<Value, RuntimeError> {
        let object = match value {
            Value::Ref(id) => {
                if !active.insert(*id) {
                    return Err(RuntimeError::ValidationFailed {
                        reason: "TS_CYCLIC_COERCION_UNSUPPORTED: cyclic object coercion"
                            .to_string(),
                    });
                }
                Some((*id, self.get(*id)?))
            }
            _ => None,
        };
        let primitive = match object.map(|(_, object)| object) {
            Some(HeapObject::Tuple(values) | HeapObject::List(values)) => {
                Value::String(self.javascript_sequence_string(values, active)?.into())
            }
            Some(HeapObject::Record(_)) => Value::String("[object Object]".into()),
            Some(HeapObject::Date(date)) => Value::Number(date.milliseconds),
            Some(HeapObject::Map(_)) => Value::String("[object Map]".into()),
            Some(HeapObject::Set(_)) => Value::String("[object Set]".into()),
            Some(HeapObject::RegExp(_)) => Value::String("[object RegExp]".into()),
            Some(HeapObject::Closure { .. }) => {
                return Err(RuntimeError::FunctionValueAtHostBoundary);
            }
            None => match value {
                Value::Tuple(values) | Value::List(values) => {
                    Value::String(self.javascript_sequence_string(values, active)?.into())
                }
                Value::Record(_) | Value::Image(_) | Value::Resource(_) => {
                    Value::String("[object Object]".into())
                }
                other => other.clone(),
            },
        };
        if let Value::Ref(id) = value {
            active.remove(id);
        }
        Ok(primitive)
    }

    fn javascript_sequence_string(
        &self,
        values: &[Value],
        active: &mut BTreeSet<HeapId>,
    ) -> Result<String, RuntimeError> {
        values
            .iter()
            .map(|value| match value {
                Value::Null | Value::Undefined => Ok(String::new()),
                other => self
                    .javascript_to_primitive_inner(other, active)
                    .map(|primitive| javascript_to_string(&primitive)),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|items| items.join(","))
    }
}

fn normalize_same_value_zero_storage(value: Value) -> Value {
    match value {
        Value::Number(number) if number == 0.0 && number.is_sign_negative() => Value::Number(0.0),
        value => value,
    }
}

pub(super) fn host_boundary_error(object: &HeapObject) -> RuntimeError {
    RuntimeError::JavaScriptExoticAtHostBoundary {
        kind: object.kind_name().to_string(),
    }
}

pub(crate) fn same_value_zero(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            left == right || (left.is_nan() && right.is_nan())
        }
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        // Every JavaScript object that reaches Map/Set storage is heap-backed;
        // reference identity is therefore exactly HeapId identity.
        (Value::Ref(left), Value::Ref(right)) => left == right,
        (Value::Resource(left), Value::Resource(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn canonical_regexp_flags(flags: &str) -> Result<String, &'static str> {
    if flags
        .chars()
        .any(|flag| !matches!(flag, 'g' | 'i' | 'm' | 's' | 'u' | 'y'))
    {
        return Err("invalid RegExp flags");
    }
    let mut canonical = String::new();
    for flag in ['g', 'i', 'm', 's', 'u', 'y'] {
        if flags.contains(flag) {
            if flags.chars().filter(|candidate| *candidate == flag).count() != 1 {
                return Err("duplicate RegExp flag");
            }
            canonical.push(flag);
        }
    }
    Ok(canonical)
}
