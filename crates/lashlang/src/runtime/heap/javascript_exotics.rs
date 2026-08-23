use super::*;
use crate::runtime::{javascript_to_number, javascript_to_string};

pub(crate) const MAX_JAVASCRIPT_LENGTH: u64 = 9_007_199_254_740_991;

/// The brand an error object carries: the `name` it reports, and — for the ECMA
/// kinds — the one constructor besides `Error` that `instanceof` answers true
/// for.
///
/// The first eight are ECMA constructors a guest can call. [`Self::EffectError`]
/// and [`Self::RuntimeError`] are brands only the substrate mints, and no
/// constructor names them, so they answer `instanceof Error` and nothing
/// narrower. They are the shape a JavaScript library would write as
/// `class EffectError extends Error`, which this value model expresses as a
/// brand because a dense record has no prototype to subclass and no own slot to
/// write `name` into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum ErrorKind {
    Error,
    TypeError,
    RangeError,
    SyntaxError,
    ReferenceError,
    URIError,
    EvalError,
    AggregateError,
    EffectError,
    RuntimeError,
}

impl ErrorKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::SyntaxError => "SyntaxError",
            Self::ReferenceError => "ReferenceError",
            Self::URIError => "URIError",
            Self::EvalError => "EvalError",
            Self::AggregateError => "AggregateError",
            Self::EffectError => "EffectError",
            Self::RuntimeError => "RuntimeError",
        }
    }

    /// Resolves a brand from a name.
    ///
    /// Every caller reads a compiler-emitted discriminator or a live error
    /// object's own brand, never guest text: `new EffectError(...)` is refused
    /// by the dialect's `new` allowlist, which is where every constructor the
    /// dialect withholds is withheld.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Error" => Self::Error,
            "TypeError" => Self::TypeError,
            "RangeError" => Self::RangeError,
            "SyntaxError" => Self::SyntaxError,
            "ReferenceError" => Self::ReferenceError,
            "URIError" => Self::URIError,
            "EvalError" => Self::EvalError,
            "AggregateError" => Self::AggregateError,
            "EffectError" => Self::EffectError,
            "RuntimeError" => Self::RuntimeError,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ErrorObject {
    pub(crate) kind: ErrorKind,
    pub(crate) message: String,
    pub(crate) cause: Option<Value>,
    /// Present only for AggregateError and always a JavaScript List value.
    pub(crate) errors: Option<Value>,
}

/// Non-durable slot for WP-C's compiled matcher.
///
/// The substrate deliberately carries no regex engine. WP-C replaces the
/// empty marker with its compiled program while retaining the important wire
/// rule: this slot is absent from every persistence wire and empty after
/// restore.
#[derive(Clone, Debug)]
pub(crate) struct RegExpProgramCache {
    pub(crate) program: lash_regress::Regex,
}

#[derive(Debug)]
pub(crate) struct RegExpObject {
    pub(crate) pattern: String,
    pub(crate) flags: String,
    pub(crate) last_index: u64,
    pub(crate) compiled_program: Option<Box<RegExpProgramCache>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RegExpMatchObject {
    pub(crate) items: Vec<Value>,
    pub(crate) index: Value,
    pub(crate) input: Value,
    pub(crate) groups: Value,
}

impl RegExpMatchObject {
    pub(crate) fn enumerable_keys(&self) -> Vec<String> {
        (0..self.items.len())
            .map(|index| index.to_string())
            .chain(["index", "input", "groups"].map(str::to_string))
            .collect()
    }

    pub(crate) fn enumerable_values(&self) -> Vec<Value> {
        self.items
            .iter()
            .cloned()
            .chain([self.index.clone(), self.input.clone(), self.groups.clone()])
            .collect()
    }

    pub(crate) fn enumerable_entries(&self) -> Vec<(String, Value)> {
        self.enumerable_keys()
            .into_iter()
            .zip(self.enumerable_values())
            .collect()
    }
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
    pub(crate) fn ensure_additional_logical_bytes(
        &self,
        additional: u64,
    ) -> Result<(), RuntimeError> {
        let attempted = self.live_logical_bytes.saturating_add(additional);
        if attempted > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted,
            });
        }
        Ok(())
    }

    /// Whether this value is a list, whether it is held in the heap or still a
    /// tree. RegExp match arrays extend the JavaScript list identity here.
    pub(crate) fn is_list(&self, value: &Value) -> bool {
        match value {
            Value::List(_) => true,
            Value::Ref(id) => matches!(
                self.get(*id),
                Ok(HeapObject::List(_) | HeapObject::RegExpMatch(_))
            ),
            _ => false,
        }
    }

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

    pub(crate) fn allocate_regexp_match(
        &mut self,
        items: Vec<Value>,
        index: Value,
        input: Value,
        groups: Value,
    ) -> Result<Value, RuntimeError> {
        self.allocate_object(HeapObject::RegExpMatch(RegExpMatchObject {
            items,
            index,
            input,
            groups,
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

    pub(crate) fn allocate_error(
        &mut self,
        kind: ErrorKind,
        message: String,
        cause: Option<Value>,
        errors: Option<Value>,
    ) -> Result<Value, RuntimeError> {
        let has_cause = cause.is_some();
        let has_errors = errors.is_some();
        let mut members = cause.into_iter().collect::<Vec<_>>();
        if let Some(errors) = errors {
            members.push(errors);
        }
        let mut imported = self.import_values(members, 0)?.into_iter();
        let cause = has_cause.then(|| imported.next().expect("cause member exists"));
        let errors = if kind == ErrorKind::AggregateError {
            has_errors.then(|| imported.next().expect("errors member exists"))
        } else {
            None
        };
        if kind == ErrorKind::AggregateError
            && !matches!(
                &errors,
                Some(Value::Ref(id)) if matches!(self.get(*id)?, HeapObject::List(_))
            )
        {
            return Err(RuntimeError::ValidationFailed {
                reason: "AggregateError errors must be a JavaScript list".to_string(),
            });
        }
        self.allocate_object(HeapObject::Error(ErrorObject {
            kind,
            message,
            cause,
            errors,
        }))
    }

    pub(crate) fn is_javascript_exotic(&self, id: HeapId) -> Result<bool, RuntimeError> {
        Ok(matches!(
            self.get(id)?,
            HeapObject::RegExp(_)
                | HeapObject::RegExpMatch(_)
                | HeapObject::Map(_)
                | HeapObject::Set(_)
                | HeapObject::Date(_)
                | HeapObject::Error(_)
                | HeapObject::Url(_)
                | HeapObject::UrlSearchParams(_)
        ))
    }

    pub(crate) fn is_javascript_vm_object(&self, id: HeapId) -> Result<bool, RuntimeError> {
        Ok(matches!(self.get(id)?, HeapObject::Closure { .. }) || self.is_javascript_exotic(id)?)
    }

    pub(crate) fn javascript_instanceof(
        &self,
        value: &Value,
        constructor: &str,
    ) -> Result<bool, RuntimeError> {
        let Value::Ref(id) = value else {
            return Ok(false);
        };
        Ok(match self.get(*id)? {
            HeapObject::RegExp(_) => constructor == "RegExp",
            HeapObject::RegExpMatch(_) => constructor == "Array",
            HeapObject::Map(_) => constructor == "Map",
            HeapObject::Set(_) => constructor == "Set",
            HeapObject::Date(_) => constructor == "Date",
            HeapObject::Error(error) => constructor == "Error" || constructor == error.kind.name(),
            HeapObject::Url(_) => constructor == "URL",
            HeapObject::UrlSearchParams(_) => constructor == "URLSearchParams",
            _ => false,
        })
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

    pub(crate) fn set_regexp_program(
        &mut self,
        id: HeapId,
        program: lash_regress::Regex,
    ) -> Result<(), RuntimeError> {
        let object = self
            .slots
            .get_mut(
                *self
                    .id_to_slot
                    .get(&id)
                    .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?,
            )
            .and_then(Option::as_mut)
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let HeapObject::RegExp(regexp) = &mut object.object else {
            return Err(RuntimeError::ValidationFailed {
                reason: "compiled RegExp cache target is not a RegExp".to_string(),
            });
        };
        regexp.compiled_program = Some(Box::new(RegExpProgramCache { program }));
        Ok(())
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

    pub(crate) fn replace_javascript_list(
        &mut self,
        id: HeapId,
        values: Vec<Value>,
    ) -> Result<(), RuntimeError> {
        self.update_object(id, |object| {
            let HeapObject::List(current) = object else {
                return false;
            };
            *current = values;
            true
        })
    }

    pub(crate) fn replace_javascript_record(
        &mut self,
        id: HeapId,
        record: Record,
    ) -> Result<(), RuntimeError> {
        self.update_object(id, |object| {
            let HeapObject::Record(current) = object else {
                return false;
            };
            **current = record;
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

    pub(super) fn commit_object_update(
        &mut self,
        id: HeapId,
        object: HeapObject,
    ) -> Result<(), RuntimeError> {
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
        self.javascript_to_primitive_inner(value, &mut BTreeSet::new(), 1)
    }

    pub(crate) fn javascript_to_number(&self, value: &Value) -> Result<f64, RuntimeError> {
        let primitive = self.javascript_to_primitive_string_or_number(value)?;
        Ok(javascript_to_number(&primitive))
    }

    pub(crate) fn javascript_to_string(&self, value: &Value) -> Result<String, RuntimeError> {
        if matches!(value, Value::Ref(id) if matches!(self.get(*id)?, HeapObject::Date(_))) {
            return Err(RuntimeError::ValidationFailed {
                reason: "TS_DATE_STRING_COERCION_PENDING: Date string coercion is unavailable; use .toISOString()"
                    .to_string(),
            });
        }
        let primitive = self.javascript_to_primitive_string_or_number(value)?;
        Ok(javascript_to_string(&primitive))
    }

    /// `depth` is the nesting level of `value` itself. The `active` set beside
    /// it only closes cycles; a finite but deeply nested container is not
    /// cyclic, and without a depth bound this walk and
    /// `javascript_sequence_string` recurse once per level until the thread
    /// stack is gone. The bound is the durable boundary's, so a value this
    /// refuses could never have been persisted either.
    fn javascript_to_primitive_inner(
        &self,
        value: &Value,
        active: &mut BTreeSet<HeapId>,
        depth: usize,
    ) -> Result<Value, RuntimeError> {
        super::ensure_value_depth(depth)?;
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
            Some(HeapObject::Tuple(values) | HeapObject::List(values)) => Value::String(
                self.javascript_sequence_string(values, active, depth)?
                    .into(),
            ),
            Some(HeapObject::RegExpMatch(result)) => Value::String(
                self.javascript_sequence_string(&result.items, active, depth)?
                    .into(),
            ),
            Some(HeapObject::Record(_)) => Value::String("[object Object]".into()),
            Some(HeapObject::Date(date)) => Value::Number(date.milliseconds),
            Some(HeapObject::Map(_)) => Value::String("[object Map]".into()),
            Some(HeapObject::Set(_)) => Value::String("[object Set]".into()),
            Some(HeapObject::RegExp(regexp)) => Value::String(regexp_string(regexp).into()),
            Some(HeapObject::Error(error)) => Value::String(
                if error.message.is_empty() {
                    error.kind.name().to_string()
                } else {
                    format!("{}: {}", error.kind.name(), error.message)
                }
                .into(),
            ),
            Some(HeapObject::Url(url)) => Value::String(url.href.as_str().into()),
            Some(HeapObject::UrlSearchParams(params)) => {
                Value::String(super::url_objects::serialize_params(&params.entries).into())
            }
            Some(HeapObject::Closure { .. }) => {
                return Err(RuntimeError::FunctionValueAtHostBoundary);
            }
            None => match value {
                Value::Tuple(values) | Value::List(values) => Value::String(
                    self.javascript_sequence_string(values, active, depth)?
                        .into(),
                ),
                Value::Record(_) | Value::Image(_) | Value::Resource(_) => {
                    Value::String("[object Object]".into())
                }
                // A projected handle is a host-side view of a value, not an
                // object of its own: coerce what is behind it.
                Value::Projected(projected) => {
                    return self.javascript_to_primitive_inner(
                        &projected.materialize(),
                        active,
                        depth,
                    );
                }
                other => other.clone(),
            },
        };
        if let Value::Ref(id) = value {
            active.remove(id);
        }
        Ok(primitive)
    }

    /// `depth` is the nesting level of the container these values belong to;
    /// each element sits one level below it.
    fn javascript_sequence_string(
        &self,
        values: &[Value],
        active: &mut BTreeSet<HeapId>,
        depth: usize,
    ) -> Result<String, RuntimeError> {
        values
            .iter()
            .map(|value| match value {
                Value::Null | Value::Undefined => Ok(String::new()),
                Value::Ref(id) if matches!(self.get(*id)?, HeapObject::Date(_)) => {
                    Err(RuntimeError::ValidationFailed {
                        reason: "TS_DATE_STRING_COERCION_PENDING: Date string coercion inside a container is unavailable; use .toISOString()"
                            .to_string(),
                    })
                }
                other => self
                    .javascript_to_primitive_inner(other, active, depth + 1)
                    .map(|primitive| javascript_to_string(&primitive)),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|items| items.join(","))
    }
}

pub(crate) fn regexp_string(regexp: &RegExpObject) -> String {
    let pattern = regexp_source(regexp);
    format!("/{pattern}/{}", regexp.flags)
}

pub(crate) fn regexp_source(regexp: &RegExpObject) -> String {
    if regexp.pattern.is_empty() {
        return "(?:)".to_string();
    }
    let mut source = String::new();
    let mut escaped = false;
    let mut in_class = false;
    for character in regexp.pattern.chars() {
        match character {
            '\n' => source.push_str("\\n"),
            '\r' => source.push_str("\\r"),
            '\u{2028}' => source.push_str("\\u2028"),
            '\u{2029}' => source.push_str("\\u2029"),
            '[' if !escaped => {
                in_class = true;
                source.push(character);
            }
            ']' if !escaped => {
                in_class = false;
                source.push(character);
            }
            '/' if !escaped && !in_class => source.push_str("\\/"),
            _ => source.push(character),
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    source
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

/// The detached shape of an error object: exactly the properties the guest reads
/// off it.
///
/// An Error is the one exotic that crosses a host boundary. It has no live
/// mutation surface — assigning to an error is a `TypeError` — and no internal
/// slot the guest cannot already read, so nothing is destroyed or exposed by
/// detaching it, and a caught rejection is returnable whenever its `cause` is
/// data, which is how a cell reports a tool failure. A `cause` holding another
/// exotic still refuses at the child export, as it must. Both export walks share
/// this one assembly so the shape a host sees cannot drift between them; each
/// supplies its own child export, which is the only thing the two walks disagree
/// about.
pub(super) fn error_boundary_record(
    error: &ErrorObject,
    mut export_child: impl FnMut(&Value) -> Result<Value, RuntimeError>,
) -> Result<Value, RuntimeError> {
    let mut output = record_with_capacity(4);
    output.insert("name".to_string(), Value::String(error.kind.name().into()));
    output.insert(
        "message".to_string(),
        Value::String(error.message.as_str().into()),
    );
    if let Some(cause) = &error.cause {
        output.insert("cause".to_string(), export_child(cause)?);
    }
    if let Some(errors) = &error.errors {
        output.insert("errors".to_string(), export_child(errors)?);
    }
    Ok(Value::Record(std::sync::Arc::new(output)))
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
