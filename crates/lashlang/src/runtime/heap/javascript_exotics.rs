use super::guest_coercion::{GuestPrimitive, PrimitiveHint};
use super::*;
use crate::runtime::{ProjectedFuture, javascript_to_number, javascript_to_string};
use std::collections::BTreeSet;

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

impl From<crate::runtime::EcmaErrorClass> for ErrorKind {
    fn from(class: crate::runtime::EcmaErrorClass) -> Self {
        use crate::runtime::EcmaErrorClass;
        match class {
            EcmaErrorClass::TypeError => Self::TypeError,
            EcmaErrorClass::RangeError => Self::RangeError,
            EcmaErrorClass::SyntaxError => Self::SyntaxError,
            EcmaErrorClass::ReferenceError => Self::ReferenceError,
            EcmaErrorClass::URIError => Self::URIError,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ErrorObject {
    pub(crate) kind: ErrorKind,
    /// The own `message` data property. `Some` when the constructor received
    /// a non-`undefined` argument (ToString'ed at install, per ECMA-262) or a
    /// write installed one; `None` when the argument was absent or `undefined`,
    /// or a `delete` removed the property. A read of an absent `message`
    /// answers `""`, the value `Error.prototype.message` supplies in Node.
    ///
    /// The slot is text-only because both durable wires carry `message` as a
    /// bare string: a write of a non-string refuses rather than persisting a
    /// shape the wire cannot hold.
    pub(crate) message: Option<String>,
    /// The own `cause` data property: `Some` exactly when the constructor's
    /// `options` argument carried a `cause` property (InstallErrorCause) or a
    /// write installed one — `Some(Value::Undefined)` for `{cause: undefined}`.
    pub(crate) cause: Option<Value>,
    /// The own `errors` data property. `Some` only for AggregateError — the
    /// constructor installs it and it is always a JavaScript List value; a
    /// `delete` can remove it, so `None` on an AggregateError means deleted.
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

    #[expect(
        clippy::expect_used,
        reason = "import_values returns exactly one value per pushed member, matched by has_cause and has_errors above, per each message"
    )]
    pub(crate) fn allocate_error(
        &mut self,
        kind: ErrorKind,
        message: Option<String>,
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
        Ok(self.get(id)?.is_function()
            || self.is_builtin_object(id)
            || self.is_javascript_exotic(id)?)
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

    pub(crate) fn set_regexp_last_index(
        &mut self,
        id: HeapId,
        last_index: u64,
    ) -> Result<(), RuntimeError> {
        self.regexp_last_index_overrides.remove(&id);
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
            .entries
            .get_mut(&id)
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

    #[expect(
        clippy::expect_used,
        reason = "import_values imports exactly the two pushed values, one key and one value, per each message"
    )]
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
        // A wholesale element replacement is a guest rewrite of the slot
        // list; every recorded hole is gone with the old elements.
        self.list_holes.remove(&id);
        self.update_object(id, |object| {
            let HeapObject::List(current) = object else {
                return false;
            };
            *current = values;
            true
        })
    }

    /// Whether `index` of the `List` at `id` is an array-literal hole — a
    /// slot ECMA never stored, distinct from a stored `undefined`.
    pub(crate) fn is_list_hole(&self, id: HeapId, index: usize) -> bool {
        self.list_holes
            .get(&id)
            .is_some_and(|holes| holes.contains(&index))
    }

    pub(crate) fn mark_list_holes(&mut self, id: HeapId, holes: BTreeSet<usize>) {
        if holes.is_empty() {
            return;
        }
        self.list_holes.insert(id, holes);
    }

    /// A store to `index` fills the position: it is a real element now.
    pub(crate) fn clear_list_hole(&mut self, id: HeapId, index: usize) {
        if let Some(holes) = self.list_holes.get_mut(&id) {
            holes.remove(&index);
            if holes.is_empty() {
                self.list_holes.remove(&id);
            }
        }
    }

    /// `lastIndex` as the guest stored it: the raw written value when the
    /// durable `u64` slot could not represent it, else the coerced slot.
    pub(crate) fn regexp_last_index_value(
        &self,
        id: HeapId,
    ) -> Result<Option<Value>, RuntimeError> {
        Ok(match self.get(id)? {
            HeapObject::RegExp(regexp) => Some(
                self.regexp_last_index_overrides
                    .get(&id)
                    .cloned()
                    .unwrap_or(Value::Number(regexp.last_index as f64)),
            ),
            _ => None,
        })
    }

    /// `lastIndex` as `exec` consumes it: the stored value through ToLength —
    /// non-numeric stores coerce to `0`, matching Node.
    pub(crate) fn regexp_last_index_coerced(&self, id: HeapId) -> Result<u64, RuntimeError> {
        let HeapObject::RegExp(regexp) = self.get(id)? else {
            return Ok(0);
        };
        let number = match self.regexp_last_index_overrides.get(&id) {
            Some(value) => self.javascript_to_number(value)?,
            None => regexp.last_index as f64,
        };
        // ToLength: NaN and non-positive numbers become 0; +Infinity and
        // anything past the safe-integer cap saturate to it.
        if number.is_nan() || number <= 0.0 {
            return Ok(0);
        }
        Ok((number as u64).min(MAX_JAVASCRIPT_LENGTH))
    }

    /// `re.lastIndex = value` stores the raw value, as ECMA's writable data
    /// property does; `exec` coerces at use. A nonnegative integer the
    /// durable slot holds exactly writes through it; anything else rides the
    /// in-memory override while the slot keeps the value's ToLength floor,
    /// which is where a restored process resumes from.
    pub(crate) fn set_regexp_last_index_raw(
        &mut self,
        id: HeapId,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let exact = match &value {
            Value::Number(number)
                if number.is_finite()
                    && number.fract() == 0.0
                    && *number >= 0.0
                    && *number <= u64::MAX as f64 =>
            {
                *number as u64
            }
            _ => {
                // `as` saturates: negative and NaN become 0, +Infinity and
                // overflow become u64::MAX, fractions truncate — exactly the
                // ToLength-shaped index a restored process should see.
                let durable = match &value {
                    Value::Number(number) => (*number as u64).min(MAX_JAVASCRIPT_LENGTH),
                    _ => 0,
                };
                self.set_regexp_last_index(id, durable)?;
                self.regexp_last_index_overrides.insert(id, value);
                return Ok(());
            }
        };
        self.regexp_last_index_overrides.remove(&id);
        self.set_regexp_last_index(id, exact)
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
        let old_bytes = self
            .entries
            .get(&id)
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
        let entry = self.entry_mut(id)?;
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

    /// Whether ECMA's OrdinaryToPrimitive would run a guest-written method on
    /// `value`: a plain object carrying its own callable `toString` or
    /// `valueOf` (FIG-3652).
    pub(crate) fn has_guest_primitive_hooks(&self, value: &Value) -> Result<bool, RuntimeError> {
        let record = match value {
            Value::Record(record) => Some(record.as_ref()),
            Value::Ref(id) => match self.get(*id)? {
                HeapObject::Record(record) => Some(record.as_ref()),
                _ => None,
            },
            _ => None,
        };
        let Some(record) = record else {
            return Ok(false);
        };
        for name in ["toString", "valueOf"] {
            if let Some(member) = record.get(name)
                && let Value::Ref(id) = member
                && self.get(*id)?.is_function()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// OrdinaryToPrimitive's `TypeError`. A plain object whose own `toString`
    /// is not callable shadows `Object.prototype.toString`, and the
    /// `valueOf` it inherits answers the object itself, so no step yields a
    /// primitive — unless an own callable `valueOf` does, which the guest
    /// hooks then run.
    fn ensure_record_has_primitive(&self, record: &Record) -> Result<(), RuntimeError> {
        let callable = |name: &str| -> Result<bool, RuntimeError> {
            Ok(match record.get(name) {
                Some(Value::Ref(id)) => self.get(*id)?.is_function(),
                _ => false,
            })
        };
        if record.get("toString").is_some() && !callable("toString")? && !callable("valueOf")? {
            return Err(RuntimeError::type_error(
                "Cannot convert object to primitive value",
            ));
        }
        Ok(())
    }

    /// ECMA-262 ToString as `String(value)` and a template substitution run
    /// it: the string hint.
    pub(crate) fn javascript_to_string_for_output(
        &self,
        value: &Value,
    ) -> Result<String, RuntimeError> {
        let primitive = self.javascript_to_primitive_inner(
            value,
            &mut BTreeSet::new(),
            1,
            PrimitiveHint::String,
        )?;
        Ok(javascript_to_string(&primitive))
    }

    /// ToPrimitive with an explicit hint, answering ECMA's type tag for an
    /// object with no string of its own.
    pub(crate) fn javascript_to_primitive_with_hint(
        &self,
        value: &Value,
        hint: PrimitiveHint,
    ) -> Result<Value, RuntimeError> {
        self.javascript_to_primitive_inner(value, &mut BTreeSet::new(), 1, hint)
    }

    /// ToPrimitive with an explicit hint of a value that is (or reaches) a
    /// projected host binding.
    pub(crate) fn javascript_to_primitive_with_hint_async<'a>(
        &'a self,
        value: &'a Value,
        hint: PrimitiveHint,
    ) -> ProjectedFuture<'a, Result<Value, RuntimeError>> {
        Box::pin(async move {
            self.javascript_to_primitive_inner_async(value, &mut BTreeSet::new(), 1, hint)
                .await
        })
    }

    /// A plain object's primitive: its guest hooks' answer when it has any,
    /// else its type tag.
    fn plain_object_primitive(
        &self,
        value: &Value,
        hint: PrimitiveHint,
    ) -> Result<Value, RuntimeError> {
        if self.has_guest_primitive_hooks(value)? {
            match self.guest_primitive(value, hint)? {
                GuestPrimitive::Value(primitive) => return Ok(primitive),
                GuestPrimitive::Tag => {}
            }
        }
        Ok(Value::String("[object Object]".into()))
    }

    /// ECMA-262 ToPrimitive with the default hint: an object with no string
    /// of its own answers its type tag (`"[object Object]"`), as
    /// `Object.prototype.toString` does.
    pub(crate) fn javascript_to_primitive_string_or_number(
        &self,
        value: &Value,
    ) -> Result<Value, RuntimeError> {
        self.javascript_to_primitive_inner(value, &mut BTreeSet::new(), 1, PrimitiveHint::Default)
    }

    pub(crate) fn javascript_coercion_contains_projected(
        &self,
        value: &Value,
    ) -> Result<bool, RuntimeError> {
        self.javascript_coercion_contains_projected_inner(value, &mut BTreeSet::new(), 1)
    }

    fn javascript_coercion_contains_projected_inner(
        &self,
        value: &Value,
        active: &mut BTreeSet<HeapId>,
        depth: usize,
    ) -> Result<bool, RuntimeError> {
        super::ensure_value_depth(depth)?;
        let values = match value {
            Value::Projected(_) => return Ok(true),
            Value::Tuple(values) | Value::List(values) => values.as_ref(),
            Value::Ref(id) if active.insert(*id) => match self.get(*id)? {
                HeapObject::Tuple(values) | HeapObject::List(values) => values.as_slice(),
                HeapObject::RegExpMatch(result) => result.items.as_slice(),
                _ => {
                    active.remove(id);
                    return Ok(false);
                }
            },
            Value::Ref(_) => return Ok(false),
            _ => return Ok(false),
        };
        let contains = values.iter().try_fold(false, |contains, value| {
            if contains {
                Ok(true)
            } else {
                self.javascript_coercion_contains_projected_inner(value, active, depth + 1)
            }
        })?;
        if let Value::Ref(id) = value {
            active.remove(id);
        }
        Ok(contains)
    }

    fn javascript_to_primitive_inner_async<'a>(
        &'a self,
        value: &'a Value,
        active: &'a mut BTreeSet<HeapId>,
        depth: usize,
        hint: PrimitiveHint,
    ) -> ProjectedFuture<'a, Result<Value, RuntimeError>> {
        Box::pin(async move {
            super::ensure_value_depth(depth)?;
            match value {
                Value::Projected(projected) => {
                    let materialized = projected.materialize_async().await?;
                    self.javascript_to_primitive_inner_async(&materialized, active, depth, hint)
                        .await
                }
                Value::Tuple(values) | Value::List(values) => Ok(Value::String(
                    self.javascript_sequence_string_async(values, active, depth)
                        .await?
                        .into(),
                )),
                Value::Ref(id) => {
                    let values = match self.get(*id)? {
                        HeapObject::Tuple(values) | HeapObject::List(values) => values.as_slice(),
                        HeapObject::RegExpMatch(result) => result.items.as_slice(),
                        _ => {
                            return self.javascript_to_primitive_inner(value, active, depth, hint);
                        }
                    };
                    if !active.insert(*id) {
                        return Err(RuntimeError::ValidationFailed {
                            reason: "TS_CYCLIC_COERCION_UNSUPPORTED: cyclic object coercion"
                                .to_string(),
                        });
                    }
                    let result = self
                        .javascript_sequence_string_async(values, active, depth)
                        .await
                        .map(|value| Value::String(value.into()));
                    active.remove(id);
                    result
                }
                _ => self.javascript_to_primitive_inner(value, active, depth, hint),
            }
        })
    }

    fn javascript_sequence_string_async<'a>(
        &'a self,
        values: &'a [Value],
        active: &'a mut BTreeSet<HeapId>,
        depth: usize,
    ) -> ProjectedFuture<'a, Result<String, RuntimeError>> {
        Box::pin(async move {
            let mut strings = Vec::with_capacity(values.len());
            for value in values {
                let string = match value {
                    Value::Null | Value::Undefined => String::new(),
                    Value::Ref(id) if matches!(self.get(*id)?, HeapObject::Date(_)) => {
                        let HeapObject::Date(date) = self.get(*id)? else {
                            unreachable!()
                        };
                        crate::runtime::vm::javascript_date::date_to_string(date.milliseconds)
                    }
                    other => {
                        let primitive = self
                            .javascript_to_primitive_inner_async(
                                other,
                                active,
                                depth + 1,
                                PrimitiveHint::String,
                            )
                            .await?;
                        javascript_to_string(&primitive)
                    }
                };
                strings.push(string);
            }
            Ok(strings.join(","))
        })
    }

    pub(crate) fn javascript_to_number(&self, value: &Value) -> Result<f64, RuntimeError> {
        let primitive = self.javascript_to_primitive_inner(
            value,
            &mut BTreeSet::new(),
            1,
            PrimitiveHint::Number,
        )?;
        Ok(javascript_to_number(&primitive))
    }

    pub(crate) fn javascript_to_string(&self, value: &Value) -> Result<String, RuntimeError> {
        if let Value::Ref(id) = value
            && let HeapObject::Date(date) = self.get(*id)?
        {
            return Ok(crate::runtime::vm::javascript_date::date_to_string(
                date.milliseconds,
            ));
        }
        let primitive = self.javascript_to_primitive_inner(
            value,
            &mut BTreeSet::new(),
            1,
            PrimitiveHint::String,
        )?;
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
        hint: PrimitiveHint,
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
            Some(HeapObject::Record(record)) => {
                self.ensure_record_has_primitive(record)?;
                self.plain_object_primitive(value, hint)?
            }
            // A Date's default ToPrimitive hint is string — unlike every other
            // object — so the `+`/`String()`/template conversion answers its
            // DateString, while the number-hinted coercion keeps the epoch.
            Some(HeapObject::Date(date)) => match hint {
                PrimitiveHint::Number => Value::Number(date.milliseconds),
                PrimitiveHint::Default | PrimitiveHint::String => Value::String(
                    crate::runtime::vm::javascript_date::date_to_string(date.milliseconds).into(),
                ),
            },
            Some(HeapObject::Map(_)) => Value::String("[object Map]".into()),
            Some(HeapObject::Set(_)) => Value::String("[object Set]".into()),
            Some(HeapObject::RegExp(regexp)) => Value::String(regexp_string(regexp).into()),
            Some(HeapObject::Error(error)) => Value::String(
                match error
                    .message
                    .as_deref()
                    .filter(|message| !message.is_empty())
                {
                    None => error.kind.name().to_string(),
                    Some(message) => format!("{}: {}", error.kind.name(), message),
                }
                .into(),
            ),
            Some(HeapObject::Url(url)) => Value::String(url.href.as_str().into()),
            Some(HeapObject::UrlSearchParams(params)) => {
                Value::String(super::url_objects::serialize_params(&params.entries).into())
            }
            // A function's string is its source text
            // (Function.prototype.toString), which the dialect does not keep.
            Some(HeapObject::Closure { .. }) => {
                return Err(RuntimeError::ValidationFailed {
                    reason: "TS_FUNCTION_STRING_COERCION: converting a function to a primitive needs its source text, which this runtime does not keep; call the function or compare it by identity".to_string(),
                });
            }
            // A built-in function value used in a coercion is a function: the
            // same refusal applies (FIG-3652, FIG-3701).
            Some(HeapObject::BuiltinFunction(_)) => {
                return Err(RuntimeError::ValidationFailed {
                    reason: "TS_FUNCTION_STRING_COERCION: converting a function to a primitive needs its source text, which this runtime does not keep; call the function or compare it by identity".to_string(),
                });
            }
            None => match value {
                Value::Tuple(values) | Value::List(values) => Value::String(
                    self.javascript_sequence_string(values, active, depth)?
                        .into(),
                ),
                Value::Record(record) => {
                    self.ensure_record_has_primitive(record)?;
                    self.plain_object_primitive(value, hint)?
                }
                Value::Image(_) | Value::Resource(_) => Value::String("[object Object]".into()),
                // A projected handle is a host-side view of a value, not an
                // object of its own: coerce what is behind it.
                Value::Projected(projected) => {
                    return self.javascript_to_primitive_inner(
                        &projected.materialize()?,
                        active,
                        depth,
                        hint,
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
                    let HeapObject::Date(date) = self.get(*id)? else {
                        unreachable!()
                    };
                    Ok(crate::runtime::vm::javascript_date::date_to_string(
                        date.milliseconds,
                    ))
                }
                other => self
                    .javascript_to_primitive_inner(other, active, depth + 1, PrimitiveHint::String)
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
        Value::String(error.message.clone().unwrap_or_default().into()),
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
