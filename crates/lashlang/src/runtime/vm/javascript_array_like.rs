//! Generic `Array.prototype` methods on arbitrary receivers (FIG-3787).
//!
//! ECMA-262 writes the array methods over `ToObject(this)`: they read
//! `length`, then Get/HasProperty/CreateDataProperty/DeletePropertyOrThrow the
//! canonical integer keys under it. None of the steps inspect the receiver's
//! class, so `Array.prototype.push.call(record)` and the re-attach
//! `record.push = Array.prototype.push; record.push(...)` run the same steps
//! over a record, a list, a string wrapper or a built-in object.
//!
//! The operations a method performs are spelled as `array_like_get`,
//! `array_like_has`, `array_like_set` and `array_like_delete`, dispatched on
//! the receiver's heap kind:
//!
//! - `Record` receivers own their numeric keys as ordinary fields, so the
//!   engine is sparse: a huge `length` names positions, not memory.
//! - `List` receivers keep hole tracking (`is_list_hole`); `Set` fills the
//!   slot, `Delete` marks it, and a `length` write truncates or hole-pads.
//! - `String` receivers answer their indexed code units and `length`; writes
//!   hit the string wrapper's non-writable indices and throw the strict-mode
//!   `TypeError` V8 throws.
//! - `BuiltinFunction` receivers — `Array.prototype` itself included — read
//!   and write through the built-in surface (`builtin_read`/`builtin_assign`),
//!   so `Array.prototype.pop()` mutates the one prototype object.
//! - `Map`, `Set`, `Date`, `RegExp`, `URL`, `URLSearchParams`, errors and
//!   closures have no element store: reads answer `undefined` (ECMA's
//!   absent-own-property answer) and writes are a named refusal rather than
//!   a silently dropped expando.
//! - `Number`/`Boolean` receivers get the ephemeral wrapper ECMA builds and
//!   discards: reads miss, writes land nowhere — which is exactly what Node
//!   does with `Array.prototype.push.call(5, "x")`.
//! - `Tuple` receivers read like lists but are fixed-arity; writes refuse.
//!
//! A `length` of `2**53` therefore costs nothing until a method actually
//! visits a position: the mutators that move the tail enumerate present keys,
//! never the range.

use std::collections::BTreeSet;

use super::super::ensure_javascript_string_size;
use super::javascript_stdlib::{array_index_property, utf16_value};
use super::*;

/// `2**53 - 1`, the `ToLength` ceiling: the largest `length` a generic method
/// can be handed.
const MAX_ARRAY_LIKE_LENGTH: u64 = 9_007_199_254_740_991;

/// The methods whose iteration calls a guest callback — they drive frames, so
/// `call_array_like_method` answers them rather than `array_like_method`.
const CALLBACK_METHODS: &[&str] = &[
    "every",
    "filter",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "flatMap",
    "forEach",
    "map",
    "reduce",
    "reduceRight",
];

/// Whether `method` is one the callback driver has to run. `sort` joins them
/// only when a comparator argument is present; without one it is a plain
/// string-keyed ordering the engine answers directly.
pub(super) fn is_driven_array_method(method: &str, args: &[Value]) -> bool {
    if CALLBACK_METHODS.contains(&method) {
        return true;
    }
    matches!(method, "sort" | "toSorted") && !matches!(args.first(), None | Some(Value::Undefined))
}

/// `ToLength`: clamp the `length` read to `[0, 2**53 - 1]`.
fn to_length(number: f64) -> u64 {
    if number.is_nan() || number <= 0.0 {
        0
    } else {
        (number as u64).min(MAX_ARRAY_LIKE_LENGTH)
    }
}

/// `ToUint32` on a `length` write, the bound the `Array` exotic checks: a
/// non-canonical or over-range length is `RangeError: Invalid array length`.
fn to_array_length(number: f64) -> Result<u64, RuntimeError> {
    if number.is_nan() || number < 0.0 || number.fract() != 0.0 || number > u32::MAX as f64 {
        return Err(RuntimeError::range_error("Invalid array length"));
    }
    Ok(number as u64)
}

/// `fromIndex` for `indexOf`/`includes`: negative counts back from `length`.
fn clamp_relative_index_u64(from: f64, length: u64) -> u64 {
    if from.is_nan() {
        0
    } else if from >= 0.0 {
        (from as u64).min(length)
    } else {
        (length as f64 + from.trunc()).max(0.0) as u64
    }
}

/// `lastIndexOf`'s exclusive end: `None` when the search range is empty.
fn last_index_exclusive_u64(from: f64, length: u64) -> Option<u64> {
    if length == 0 {
        return None;
    }
    if from.is_nan() {
        return Some(length);
    }
    if from >= 0.0 {
        Some((from.trunc() as u64 + 1).min(length))
    } else {
        let end = length as f64 + from.trunc() + 1.0;
        (end > 0.0).then_some(end.min(length as f64) as u64)
    }
}

/// A canonical integer-index key past `array_index_property`'s `u32` bound:
/// the property `ToString(k)` names for `k` in `0..2**53-1`. Records and the
/// built-in objects carry such keys as ordinary fields, which is how a
/// `length` of `2**53 - 1` stays sparse.
fn integer_index_key(key: &str) -> Option<u64> {
    if key.is_empty() || key.len() > 16 {
        return None;
    }
    let index: u64 = key.parse().ok()?;
    (index < MAX_ARRAY_LIKE_LENGTH && index.to_string() == key).then_some(index)
}

impl<H: ExecutionHost> Vm<'_, H> {
    /// `Get(O, key)` where `key` is a canonical array index or `"length"` —
    /// the only property names the generic methods read.
    pub(super) fn array_like_get(
        &mut self,
        receiver: &Value,
        key: &str,
    ) -> Result<Value, RuntimeError> {
        match receiver {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => {
                    Ok(record.get(key).cloned().unwrap_or(Value::Undefined))
                }
                HeapObject::List(items) | HeapObject::Tuple(items) => {
                    if key == "length" {
                        return Ok(Value::Number(items.len() as f64));
                    }
                    Ok(match array_index_property(key) {
                        Some(index) => items
                            .get(index as usize)
                            .cloned()
                            .unwrap_or(Value::Undefined),
                        None => Value::Undefined,
                    })
                }
                HeapObject::RegExpMatch(result) => Ok(match key {
                    "length" => Value::Number(result.items.len() as f64),
                    "index" => result.index.clone(),
                    "input" => result.input.clone(),
                    "groups" => result.groups.clone(),
                    _ => match array_index_property(key) {
                        Some(index) => result
                            .items
                            .get(index as usize)
                            .cloned()
                            .unwrap_or(Value::Undefined),
                        None => Value::Undefined,
                    },
                }),
                HeapObject::BuiltinFunction(_) => self.heap.builtin_read(*id, key),
                HeapObject::Closure { name, length, .. } => Ok(match key {
                    "length" => length.clone().unwrap_or(Value::Undefined),
                    "name" => name.clone().unwrap_or(Value::Undefined),
                    _ => Value::Undefined,
                }),
                _ => Ok(Value::Undefined),
            },
            Value::Record(record) => Ok(record.get(key).cloned().unwrap_or(Value::Undefined)),
            Value::List(items) | Value::Tuple(items) => {
                if key == "length" {
                    return Ok(Value::Number(items.len() as f64));
                }
                Ok(match array_index_property(key) {
                    Some(index) => items
                        .get(index as usize)
                        .cloned()
                        .unwrap_or(Value::Undefined),
                    None => Value::Undefined,
                })
            }
            Value::String(text) => {
                if key == "length" {
                    return Ok(Value::Number(text.encode_utf16().count() as f64));
                }
                match array_index_property(key) {
                    Some(index) => text
                        .encode_utf16()
                        .nth(index as usize)
                        .map_or(Ok(Value::Undefined), |unit| utf16_value(vec![unit])),
                    None => Ok(Value::Undefined),
                }
            }
            _ => Ok(Value::Undefined),
        }
    }

    /// `HasProperty(O, key)`. The prototype chains these receivers ride have
    /// no integer-indexed members, so `has` is the own check plus `"length"`
    /// where the kind carries it.
    fn array_like_has(&mut self, receiver: &Value, key: &str) -> Result<bool, RuntimeError> {
        match receiver {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => Ok(record.get(key).is_some()),
                HeapObject::List(items) | HeapObject::Tuple(items) => {
                    if key == "length" {
                        return Ok(true);
                    }
                    Ok(match array_index_property(key) {
                        Some(index) => {
                            let index = index as usize;
                            index < items.len() && !self.heap.is_list_hole(*id, index)
                        }
                        None => false,
                    })
                }
                HeapObject::RegExpMatch(result) => Ok(match key {
                    "length" | "index" | "input" | "groups" => true,
                    _ => array_index_property(key)
                        .is_some_and(|index| (index as usize) < result.items.len()),
                }),
                HeapObject::BuiltinFunction(_) => self.heap.builtin_has_property(*id, key),
                HeapObject::Closure { .. } => Ok(matches!(key, "length" | "name")),
                _ => Ok(false),
            },
            Value::Record(record) => Ok(record.get(key).is_some()),
            Value::List(items) | Value::Tuple(items) => Ok(key == "length"
                || array_index_property(key).is_some_and(|index| (index as usize) < items.len())),
            Value::String(text) => Ok(key == "length"
                || array_index_property(key)
                    .is_some_and(|index| (index as usize) < text.encode_utf16().count())),
            _ => Ok(false),
        }
    }

    /// `Set(O, key, value, true)` — the throw-on-failure form the generic
    /// methods use (ECMA writes them in strict mode).
    fn array_like_set(
        &mut self,
        receiver: &Value,
        key: &str,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let Value::Ref(id) = receiver else {
            return match receiver {
                Value::Record(_) | Value::List(_) | Value::Tuple(_) => {
                    Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_ARRAY_LIKE_VALUE_UNSUPPORTED: writing `{key}` needs a heap-backed receiver"
                        ),
                    })
                }
                Value::String(_) => Err(RuntimeError::type_error(format!(
                    "Cannot create property '{key}' on {}",
                    match receiver {
                        Value::String(text) => format!("string '{text}'"),
                        _ => "object".to_string(),
                    }
                ))),
                // A primitive wrapper takes the write and is discarded
                // immediately — `Array.prototype.push.call(5, "x")` answers 1
                // in Node and `x` lands on nothing.
                _ => Ok(()),
            };
        };
        let id = *id;
        match self.heap.get(id)? {
            HeapObject::Record(_) => {
                let mut object = self.heap.get(id)?.clone();
                let HeapObject::Record(record) = &mut object else {
                    unreachable!("record receiver kind was checked")
                };
                record.insert_str(key, value);
                self.heap.commit_object_update(id, object)
            }
            HeapObject::List(items) => {
                if key == "length" {
                    let length = to_array_length(self.heap.javascript_to_number(&value)?)?;
                    return self.set_list_length(id, length as usize);
                }
                let Some(index) = array_index_property(key) else {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_ARRAY_LIKE_NAMED_UNSUPPORTED: arrays do not carry a `{key}` property"
                        ),
                    });
                };
                let index = index as usize;
                let mut items = items.clone();
                let mut holes: BTreeSet<usize> = (0..items.len())
                    .filter(|hole| self.heap.is_list_hole(id, *hole))
                    .collect();
                if index >= items.len() {
                    let old_len = items.len();
                    items.resize(index + 1, Value::Undefined);
                    holes.extend(old_len..index);
                }
                items[index] = value;
                holes.remove(&index);
                self.heap.replace_javascript_list(id, items)?;
                self.heap.mark_list_holes(id, holes);
                Ok(())
            }
            HeapObject::Tuple(_) => Err(RuntimeError::ValidationFailed {
                reason: format!(
                    "TS_ARRAY_LIKE_TUPLE_UNSUPPORTED: tuples are fixed-arity; cannot write `{key}`"
                ),
            }),
            HeapObject::RegExpMatch(_) => Err(RuntimeError::ValidationFailed {
                reason: "TS_ARRAY_LIKE_MATCH_UNSUPPORTED: match arrays do not take writes"
                    .to_string(),
            }),
            HeapObject::BuiltinFunction(_) => self.heap.builtin_assign(id, key, value),
            object => Err(RuntimeError::ValidationFailed {
                reason: format!(
                    "TS_ARRAY_LIKE_EXOTIC_UNSUPPORTED: {} does not take expando writes",
                    object.kind_name()
                ),
            }),
        }
    }

    /// `DeletePropertyOrThrow(O, key)`.
    fn array_like_delete(&mut self, receiver: &Value, key: &str) -> Result<(), RuntimeError> {
        let Value::Ref(id) = receiver else {
            return match receiver {
                Value::Record(_) | Value::List(_) | Value::Tuple(_) => {
                    Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_ARRAY_LIKE_VALUE_UNSUPPORTED: deleting `{key}` needs a heap-backed receiver"
                        ),
                    })
                }
                Value::String(_) => Err(RuntimeError::type_error(format!(
                    "Cannot delete property '{key}' of object"
                ))),
                _ => Ok(()),
            };
        };
        let id = *id;
        match self.heap.get(id)? {
            HeapObject::Record(_) => {
                let mut object = self.heap.get(id)?.clone();
                let HeapObject::Record(record) = &mut object else {
                    unreachable!("record receiver kind was checked")
                };
                record.remove(key);
                self.heap.commit_object_update(id, object)
            }
            HeapObject::List(items) => {
                if key == "length" {
                    return Err(RuntimeError::type_error(
                        "Cannot delete property 'length' of array",
                    ));
                }
                let Some(index) = array_index_property(key) else {
                    return Ok(());
                };
                let index = index as usize;
                if index >= items.len() {
                    return Ok(());
                }
                let mut items = items.clone();
                let mut holes: BTreeSet<usize> = (0..items.len())
                    .filter(|hole| self.heap.is_list_hole(id, *hole))
                    .collect();
                items[index] = Value::Undefined;
                holes.insert(index);
                self.heap.replace_javascript_list(id, items)?;
                self.heap.mark_list_holes(id, holes);
                Ok(())
            }
            HeapObject::BuiltinFunction(_) => {
                self.heap.builtin_delete(id, key)?;
                Ok(())
            }
            // Deleting a key that was never there answers `true`.
            _ => Ok(()),
        }
    }

    /// `O.length` through `ToLength` — the coercion runs the receiver's own
    /// `valueOf`/`toString` hooks when it carries them, replayed by the
    /// suspending instruction that owns this call (FIG-3652).
    pub(super) fn array_like_length(&mut self, receiver: &Value) -> Result<u64, RuntimeError> {
        let length = self.array_like_get(receiver, "length")?;
        Ok(to_length(self.heap.javascript_to_number(&length)?))
    }

    /// The `length` write the shrinking and growing methods do once their
    /// element writes are done. For a `List` receiver the write is the vector
    /// resize itself; for everything else it is an ordinary `Set`.
    fn array_like_set_length(&mut self, receiver: &Value, length: u64) -> Result<(), RuntimeError> {
        if let Value::Ref(id) = receiver
            && matches!(self.heap.get(*id)?, HeapObject::List(_))
        {
            return self.set_list_length(*id, length.min(u32::MAX as u64) as usize);
        }
        self.array_like_set(receiver, "length", Value::Number(length as f64))
    }

    /// Resize a heap list to `length`, truncating or padding with holes.
    fn set_list_length(&mut self, id: HeapId, length: usize) -> Result<(), RuntimeError> {
        let HeapObject::List(items) = self.heap.get(id)? else {
            return Err(RuntimeError::ValidationFailed {
                reason: "TS_ARRAY_LIKE_KIND: receiver is not a list".to_string(),
            });
        };
        let old_len = items.len();
        let mut holes: BTreeSet<usize> = (0..old_len.min(length))
            .filter(|hole| self.heap.is_list_hole(id, *hole))
            .collect();
        let mut items = items.clone();
        if length > old_len {
            self.heap.ensure_list_allocation_len(length)?;
            items.resize(length, Value::Undefined);
            holes.extend(old_len..length);
        } else {
            items.truncate(length);
        }
        self.heap.replace_javascript_list(id, items)?;
        self.heap.mark_list_holes(id, holes);
        Ok(())
    }

    /// The canonical-index keys present on the receiver, sorted — the sparse
    /// enumeration the tail-moving methods use so a `length` of `2**53` costs
    /// its present keys, not its range.
    fn array_like_present_indices(&mut self, receiver: &Value) -> Result<Vec<u64>, RuntimeError> {
        let mut indices: Vec<u64> = Vec::new();
        match receiver {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => {
                    for (key, _) in record.iter() {
                        if let Some(index) = integer_index_key(key) {
                            indices.push(index);
                        }
                    }
                }
                HeapObject::List(items) | HeapObject::Tuple(items) => {
                    for index in 0..items.len() {
                        if !self.heap.is_list_hole(*id, index) {
                            indices.push(index as u64);
                        }
                    }
                }
                HeapObject::RegExpMatch(result) => {
                    indices.extend(0..result.items.len() as u64);
                }
                // A built-in object can carry integer-keyed expandos
                // (`Array.prototype.push` writes them onto `Array.prototype`);
                // the canonical-name surface carries no indices.
                HeapObject::BuiltinFunction(_) => {
                    for key in self.heap.builtin_enumerable_keys(*id)? {
                        if let Some(index) = integer_index_key(&key) {
                            indices.push(index);
                        }
                    }
                }
                _ => {}
            },
            Value::Record(record) => {
                for (key, _) in record.iter() {
                    if let Some(index) = integer_index_key(key) {
                        indices.push(index);
                    }
                }
            }
            Value::List(items) | Value::Tuple(items) => indices.extend(0..items.len() as u64),
            Value::String(text) => indices.extend(0..text.encode_utf16().count() as u64),
            _ => {}
        }
        indices.sort_unstable();
        indices.dedup();
        Ok(indices)
    }

    /// Read `start..end` of the receiver into a dense vector, charging the
    /// whole allocation first exactly as `Array(n)` does: past the ECMA array
    /// limit it is `RangeError: Invalid array length`.
    pub(super) fn array_like_dense(
        &mut self,
        receiver: &Value,
        start: u64,
        end: u64,
    ) -> Result<Vec<Value>, RuntimeError> {
        let count = end.saturating_sub(start);
        if count > u32::MAX as u64 {
            return Err(RuntimeError::range_error("Invalid array length"));
        }
        self.heap.ensure_list_allocation_len(count as usize)?;
        self.charge_intrinsic_work(count as usize);
        let mut items = Vec::with_capacity(count as usize);
        for index in start..end {
            items.push(self.array_like_get(receiver, &index.to_string())?);
        }
        Ok(items)
    }

    /// The `ToIntegerOrInfinity`-relative bound a start/end/index argument
    /// resolves to against `length`.
    fn array_like_relative_bound(
        &mut self,
        value: Option<&Value>,
        length: u64,
        default: u64,
    ) -> Result<u64, RuntimeError> {
        let value = match value {
            None | Some(Value::Undefined) => return Ok(default),
            Some(value) => value,
        };
        let number = self.heap.javascript_to_number(value)?;
        Ok(if number.is_nan() || number == f64::NEG_INFINITY {
            0
        } else if number == f64::INFINITY {
            length
        } else if number < 0.0 {
            (length as f64 + number.trunc()).clamp(0.0, length as f64) as u64
        } else {
            (number.trunc() as u64).min(length)
        })
    }

    /// Whether the receiver can take `Set`/`Delete`: heap-backed records,
    /// lists and the built-in objects can; every other kind either refuses
    /// honestly (the exotic kinds a write would need an expando for) or
    /// throws the strict-mode `TypeError` a string wrapper raises.
    fn array_like_require_writable(
        &mut self,
        receiver: &Value,
        method: &str,
    ) -> Result<(), RuntimeError> {
        let writable = match receiver {
            Value::Ref(id) => matches!(
                self.heap.get(*id)?,
                HeapObject::Record(_) | HeapObject::List(_) | HeapObject::BuiltinFunction(_)
            ),
            // A string's index writes throw in `array_like_set`, exactly as
            // V8's string wrapper refuses them. Every other primitive's
            // writes land on the ephemeral wrapper `ToObject` made and are
            // discarded with it — `Array.prototype.push.call(5, "x")`
            // answers 1 and `x` lands on nothing.
            Value::Record(_) | Value::List(_) | Value::Tuple(_) => false,
            _ => true,
        };
        if writable {
            return Ok(());
        }
        Err(RuntimeError::ValidationFailed {
            reason: format!(
                "TS_ARRAY_LIKE_RECEIVER_UNSUPPORTED: Array.prototype.{method} cannot mutate this receiver"
            ),
        })
    }

    /// The object a `return O` step answers: the receiver itself when it is
    /// one, or the ephemeral wrapper `ToObject` built around a primitive —
    /// fresh on every call, exactly as ECMA's transient boxing is.
    pub(super) fn array_like_receiver_object(&self, receiver: &Value) -> Value {
        match receiver {
            Value::Ref(_) | Value::Record(_) | Value::List(_) | Value::Tuple(_) => receiver.clone(),
            _ => Value::Record(std::sync::Arc::new(Default::default())),
        }
    }

    /// `Array.prototype.<method>` for the methods that answer without driving
    /// a guest callback. `receiver` has already passed the nullish check and
    /// its argument coercion ran in `detached_builtin_result`; the dispatch
    /// below is each method's own ECMA steps over `array_like_*` ops.
    pub(super) fn array_like_method(
        &mut self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        match method {
            "push" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                if length + args.len() as u64 > MAX_ARRAY_LIKE_LENGTH {
                    return Err(RuntimeError::type_error(format!(
                        "Pushing {} elements on an array-like of length {length} is disallowed, as the total surpasses 2**53-1",
                        args.len()
                    )));
                }
                let mut position = length;
                for argument in args {
                    self.array_like_set(receiver, &position.to_string(), argument.clone())?;
                    position += 1;
                }
                self.array_like_set_length(receiver, position)?;
                Ok(Value::Number(position as f64))
            }
            "pop" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                if length == 0 {
                    self.array_like_set_length(receiver, 0)?;
                    return Ok(Value::Undefined);
                }
                let last = length - 1;
                let value = self.array_like_get(receiver, &last.to_string())?;
                self.array_like_delete(receiver, &last.to_string())?;
                self.array_like_set_length(receiver, last)?;
                Ok(value)
            }
            "shift" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                if length == 0 {
                    self.array_like_set_length(receiver, 0)?;
                    return Ok(Value::Undefined);
                }
                let first = self.array_like_get(receiver, "0")?;
                // ECMA walks the indices; a sparse receiver's present keys are
                // the whole walk, so positions that were absent need no work —
                // `Delete` on a missing key is a no-op.
                let present = self.array_like_present_indices(receiver)?;
                let mut written: BTreeSet<u64> = BTreeSet::new();
                for index in &present {
                    if *index >= 1 && *index < length {
                        let value = self.array_like_get(receiver, &index.to_string())?;
                        self.array_like_set(receiver, &(index - 1).to_string(), value)?;
                        written.insert(index - 1);
                    }
                }
                for index in &present {
                    if *index < length && !written.contains(index) {
                        self.array_like_delete(receiver, &index.to_string())?;
                    }
                }
                self.array_like_delete(receiver, &(length - 1).to_string())?;
                self.array_like_set_length(receiver, length - 1)?;
                Ok(first)
            }
            "unshift" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                let insert = args.len() as u64;
                if insert > 0 {
                    if length + insert > MAX_ARRAY_LIKE_LENGTH {
                        return Err(RuntimeError::type_error("Invalid array length"));
                    }
                    let present = self.array_like_present_indices(receiver)?;
                    let mut written: BTreeSet<u64> = BTreeSet::new();
                    for index in present.iter().rev() {
                        if *index < length {
                            let value = self.array_like_get(receiver, &index.to_string())?;
                            self.array_like_set(receiver, &(index + insert).to_string(), value)?;
                            written.insert(index + insert);
                        }
                    }
                    for index in &present {
                        if *index < length && !written.contains(index) {
                            self.array_like_delete(receiver, &index.to_string())?;
                        }
                    }
                    for (offset, argument) in args.iter().enumerate() {
                        self.array_like_set(receiver, &offset.to_string(), argument.clone())?;
                    }
                }
                self.array_like_set_length(receiver, length + insert)?;
                Ok(Value::Number((length + insert) as f64))
            }
            "reverse" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                let present: BTreeSet<u64> = self
                    .array_like_present_indices(receiver)?
                    .into_iter()
                    .filter(|index| *index < length)
                    .collect();
                // Only positions that are present — or mirror one — can
                // change; every other pair swaps two absences.
                let mut lowers: BTreeSet<u64> = BTreeSet::new();
                for index in &present {
                    let mirror = length - 1 - *index;
                    if *index < length.div_ceil(2) {
                        lowers.insert(*index);
                    }
                    if mirror < length.div_ceil(2) {
                        lowers.insert(mirror);
                    }
                }
                for lower in lowers {
                    let upper = length - lower - 1;
                    let lower_key = lower.to_string();
                    let upper_key = upper.to_string();
                    let lower_has = self.array_like_has(receiver, &lower_key)?;
                    let upper_has = self.array_like_has(receiver, &upper_key)?;
                    let lower_value = if lower_has {
                        self.array_like_get(receiver, &lower_key)?
                    } else {
                        Value::Undefined
                    };
                    let upper_value = if upper_has {
                        self.array_like_get(receiver, &upper_key)?
                    } else {
                        Value::Undefined
                    };
                    match (lower_has, upper_has) {
                        (true, true) => {
                            self.array_like_set(receiver, &lower_key, upper_value)?;
                            self.array_like_set(receiver, &upper_key, lower_value)?;
                        }
                        (true, false) => {
                            self.array_like_delete(receiver, &lower_key)?;
                            self.array_like_set(receiver, &upper_key, lower_value)?;
                        }
                        (false, true) => {
                            self.array_like_set(receiver, &lower_key, upper_value)?;
                            self.array_like_delete(receiver, &upper_key)?;
                        }
                        (false, false) => {}
                    }
                }
                Ok(self.array_like_receiver_object(receiver))
            }
            "copyWithin" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                let to = self.array_like_relative_bound(args.first(), length, 0)?;
                let from = self.array_like_relative_bound(args.get(1), length, 0)?;
                let end = match args.get(2) {
                    None | Some(Value::Undefined) => length,
                    Some(_) => self.array_like_relative_bound(args.get(2), length, length)?,
                };
                let count = end.saturating_sub(from).min(length.saturating_sub(to));
                if count == 0 || to == from {
                    return Ok(self.array_like_receiver_object(receiver));
                }
                // Snapshot the source range first: the snapshot makes the
                // overlap-ordering question ECMA answers with direction moot —
                // a value moved this call never re-reads a moved position.
                let mut moved: Vec<(u64, Value)> = Vec::new();
                for index in self.array_like_present_indices(receiver)? {
                    if index >= from && index < from + count {
                        let value = self.array_like_get(receiver, &index.to_string())?;
                        // `to` may sit below `from`; the shift is signed, so
                        // add before the unsigned conversion. `index >= from`
                        // and `to >= 0` keep the target non-negative.
                        let target = index as i64 + (to as i64 - from as i64);
                        moved.push((target as u64, value));
                    }
                }
                let written: BTreeSet<u64> = moved.iter().map(|(index, _)| *index).collect();
                for index in self.array_like_present_indices(receiver)? {
                    if index >= to && index < to + count && !written.contains(&index) {
                        self.array_like_delete(receiver, &index.to_string())?;
                    }
                }
                for (index, value) in moved {
                    self.array_like_set(receiver, &index.to_string(), value)?;
                }
                Ok(self.array_like_receiver_object(receiver))
            }
            "fill" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                let start = self.array_like_relative_bound(args.get(1), length, 0)?;
                let end = match args.get(2) {
                    None | Some(Value::Undefined) => length,
                    Some(_) => self.array_like_relative_bound(args.get(2), length, length)?,
                };
                let end = end.max(start);
                // Every position `start..end` becomes a real element — the
                // write count is the work ECMA charges; the intrinsic counter
                // bounds a hostile `length` the same way.
                self.charge_intrinsic_work((end - start).min(usize::MAX as u64) as usize);
                for index in start..end {
                    self.array_like_set(receiver, &index.to_string(), value.clone())?;
                }
                Ok(self.array_like_receiver_object(receiver))
            }
            "splice" => {
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                let start = self.array_like_relative_bound(args.first(), length, 0)?;
                let delete = if args.is_empty() {
                    0
                } else if args.len() == 1 {
                    length - start
                } else {
                    to_length(self.heap.javascript_to_number(&args[1])?).min(length - start)
                };
                let insert = args.len().saturating_sub(2) as u64;
                if length + insert - delete > MAX_ARRAY_LIKE_LENGTH {
                    return Err(RuntimeError::type_error("Invalid array length"));
                }
                // The removed elements are a real, fresh Array — dense reads.
                let removed = self.array_like_dense(receiver, start, start + delete)?;
                let shift = insert as i64 - delete as i64;
                if shift != 0 {
                    let tail_start = start + delete;
                    let present = self.array_like_present_indices(receiver)?;
                    let mut written: BTreeSet<u64> = BTreeSet::new();
                    if shift < 0 {
                        for index in &present {
                            if *index >= tail_start && *index < length {
                                let target = (*index as i64 + shift) as u64;
                                let value = self.array_like_get(receiver, &index.to_string())?;
                                self.array_like_set(receiver, &target.to_string(), value)?;
                                written.insert(target);
                            }
                        }
                    } else {
                        for index in present.iter().rev() {
                            if *index >= tail_start && *index < length {
                                let target = index + shift as u64;
                                let value = self.array_like_get(receiver, &index.to_string())?;
                                self.array_like_set(receiver, &target.to_string(), value)?;
                                written.insert(target);
                            }
                        }
                    }
                    // Every present index the shift did not write leaves the
                    // window: for a shrink that is the tail-delete too, and
                    // `start`'s own slot was overwritten only when its source
                    // existed — a hole there deletes, it does not keep the
                    // removed element.
                    let delete_below = if shift < 0 { length } else { length - delete };
                    for index in &present {
                        if *index >= start && *index < delete_below && !written.contains(index) {
                            self.array_like_delete(receiver, &index.to_string())?;
                        }
                    }
                }
                for (offset, argument) in args.iter().skip(2).enumerate() {
                    self.array_like_set(
                        receiver,
                        &(start + offset as u64).to_string(),
                        argument.clone(),
                    )?;
                }
                self.array_like_set_length(receiver, length + insert - delete)?;
                self.heap.allocate_list(removed)
            }
            "slice" => {
                let length = self.array_like_length(receiver)?;
                let start = self.array_like_relative_bound(args.first(), length, 0)?;
                let end = match args.get(1) {
                    None | Some(Value::Undefined) => length,
                    Some(_) => self.array_like_relative_bound(args.get(1), length, length)?,
                };
                let items = self.array_like_dense(receiver, start, end.max(start))?;
                self.heap.allocate_list(items)
            }
            "concat" => {
                // IsConcatSpreadable spreads actual arrays; a plain object —
                // the receiver included — appends as one element.
                let mut items = Vec::new();
                for value in std::iter::once(receiver).chain(args.iter()) {
                    match value {
                        Value::List(list) | Value::Tuple(list) => {
                            items.extend(list.iter().cloned());
                        }
                        Value::Ref(id) => match self.heap.get(*id)? {
                            HeapObject::List(list) | HeapObject::Tuple(list) => {
                                items.extend(list.iter().cloned());
                            }
                            HeapObject::RegExpMatch(result) => {
                                items.extend(result.items.iter().cloned());
                            }
                            _ => items.push(value.clone()),
                        },
                        _ => items.push(value.clone()),
                    }
                }
                self.heap.allocate_list(items)
            }
            "indexOf" | "lastIndexOf" => {
                // `HasProperty` gates each visit, so the receiver's present
                // keys are the whole search space — a `2**53 - 1` `length`
                // with two stored elements scans those two. `start` is
                // inclusive for `indexOf`; `end` exclusive for `lastIndexOf`,
                // which visits the in-range keys descending.
                let length = self.array_like_length(receiver)?;
                if length == 0 {
                    return Ok(Value::Number(-1.0));
                }
                let needle = args.first().cloned().unwrap_or(Value::Undefined);
                let forward = method == "indexOf";
                let bound = if args.len() < 2 {
                    if forward { 0 } else { length }
                } else {
                    let from = self.heap.javascript_to_number(&args[1])?;
                    if forward {
                        clamp_relative_index_u64(from, length)
                    } else {
                        match last_index_exclusive_u64(from, length) {
                            Some(end) => end,
                            None => return Ok(Value::Number(-1.0)),
                        }
                    }
                };
                let present = self.array_like_present_indices(receiver)?;
                let ordered: Box<dyn Iterator<Item = u64>> = if forward {
                    Box::new(present.into_iter())
                } else {
                    Box::new(present.into_iter().rev())
                };
                for index in ordered {
                    // A key stored past `length` is not an element at all.
                    if index >= length || (forward && index < bound) || (!forward && index >= bound)
                    {
                        continue;
                    }
                    if crate::runtime::javascript::javascript_strict_equal(
                        &self.array_like_get(receiver, &index.to_string())?,
                        &needle,
                    ) {
                        return Ok(Value::Number(index as f64));
                    }
                }
                Ok(Value::Number(-1.0))
            }
            "includes" => {
                // `includes` reads through `Get` only — a hole reads
                // `undefined`, so `[,].includes(undefined)` is true. The
                // present keys carry every possible non-`undefined` match;
                // one index left unread means an `undefined` needle matched.
                let length = self.array_like_length(receiver)?;
                if length == 0 {
                    return Ok(Value::Bool(false));
                }
                let needle = args.first().cloned().unwrap_or(Value::Undefined);
                let start = if args.len() < 2 {
                    0
                } else {
                    let from = self.heap.javascript_to_number(&args[1])?;
                    clamp_relative_index_u64(from, length)
                };
                let mut visited = 0u64;
                for index in self.array_like_present_indices(receiver)? {
                    if index < start || index >= length {
                        continue;
                    }
                    visited += 1;
                    if same_value_zero(&self.array_like_get(receiver, &index.to_string())?, &needle)
                    {
                        return Ok(Value::Bool(true));
                    }
                }
                let absent_reads_undefined =
                    visited < length.saturating_sub(start) && matches!(needle, Value::Undefined);
                Ok(Value::Bool(absent_reads_undefined))
            }
            "at" => {
                let length = self.array_like_length(receiver)?;
                let relative = self
                    .heap
                    .javascript_to_number(args.first().unwrap_or(&Value::Undefined))?;
                let index = if relative.is_nan() {
                    0.0
                } else {
                    relative.trunc()
                };
                let index = if index < 0.0 {
                    length as f64 + index
                } else {
                    index
                };
                if index < 0.0 || index >= length as f64 {
                    return Ok(Value::Undefined);
                }
                self.array_like_get(receiver, &(index as u64).to_string())
            }
            "join" => {
                let length = self.array_like_length(receiver)?;
                if length == 0 {
                    return Ok(Value::String("".into()));
                }
                let separator = match args.first() {
                    None | Some(Value::Undefined) => ",".to_string(),
                    Some(value) => self.heap.javascript_to_string(value)?,
                };
                // The output is at least `length - 1` separators long, so the
                // size check answers before the first element is read.
                if length > 1 {
                    ensure_javascript_string_size(
                        ((length - 1).saturating_mul(separator.len() as u64))
                            .try_into()
                            .unwrap_or(usize::MAX),
                    )?;
                }
                self.charge_intrinsic_work(length.min(usize::MAX as u64) as usize);
                let mut parts = String::new();
                for index in 0..length {
                    if index > 0 {
                        parts.push_str(&separator);
                    }
                    let element = self.array_like_get(receiver, &index.to_string())?;
                    match element {
                        Value::Null | Value::Undefined => {}
                        element => {
                            let text = self.heap.javascript_to_string(&element)?;
                            parts.push_str(&text);
                        }
                    }
                }
                ensure_javascript_string_size(parts.len())?;
                Ok(Value::String(parts.into()))
            }
            // `toString` without a callable own `join` falls to
            // `Object.prototype.toString`'s tag for the receiver; the
            // call-level path handles the guest-`join` delegation.
            "toString" => {
                let join = self.array_like_get(receiver, "join")?;
                match self.builtin_callee(&join)? {
                    Some(function) => self.detached_builtin_result(function, receiver, &[]),
                    None => Ok(Value::String(self.object_tag(receiver)?.into())),
                }
            }
            "valueOf" => Ok(self.array_like_receiver_object(receiver)),
            "keys" => {
                let length = self.array_like_length(receiver)?;
                if length > u32::MAX as u64 {
                    return Err(RuntimeError::range_error("Invalid array length"));
                }
                self.heap.ensure_list_allocation_len(length as usize)?;
                self.heap.allocate_list(
                    (0..length)
                        .map(|index| Value::Number(index as f64))
                        .collect(),
                )
            }
            "values" => {
                let length = self.array_like_length(receiver)?;
                let items = self.array_like_dense(receiver, 0, length)?;
                self.heap.allocate_list(items)
            }
            "entries" => {
                let length = self.array_like_length(receiver)?;
                if length > u32::MAX as u64 {
                    return Err(RuntimeError::range_error("Invalid array length"));
                }
                self.heap.ensure_list_allocation_len(length as usize)?;
                let mut items = Vec::with_capacity(length as usize);
                for index in 0..length {
                    items.push(Value::List(
                        vec![
                            Value::Number(index as f64),
                            self.array_like_get(receiver, &index.to_string())?,
                        ]
                        .into(),
                    ));
                }
                self.heap.allocate_list(items)
            }
            "flat" => {
                let length = self.array_like_length(receiver)?;
                let depth = match args.first() {
                    None | Some(Value::Undefined) => 1u64,
                    Some(value) => {
                        let depth = self.heap.javascript_to_number(value)?;
                        if depth.is_nan() || depth <= 0.0 {
                            0
                        } else if depth.is_infinite() {
                            u64::MAX
                        } else {
                            depth.trunc() as u64
                        }
                    }
                };
                let mut output = Vec::new();
                self.array_like_flatten(receiver, length, depth, 0, &mut output)?;
                self.heap.allocate_list(output)
            }
            "toReversed" => {
                let length = self.array_like_length(receiver)?;
                self.array_like_dense_bound(length)?;
                let mut items = Vec::with_capacity(length as usize);
                for index in (0..length).rev() {
                    items.push(self.array_like_get(receiver, &index.to_string())?);
                }
                self.heap.allocate_list(items)
            }
            "toSpliced" => {
                let length = self.array_like_length(receiver)?;
                let start = self.array_like_relative_bound(args.first(), length, 0)?;
                let delete = if args.is_empty() {
                    0
                } else if args.len() == 1 {
                    length - start
                } else {
                    to_length(self.heap.javascript_to_number(&args[1])?).min(length - start)
                };
                let insert = args.len().saturating_sub(2) as u64;
                let final_length = length + insert - delete;
                self.array_like_dense_bound(final_length)?;
                let mut items = Vec::with_capacity(final_length as usize);
                for index in 0..start {
                    items.push(self.array_like_get(receiver, &index.to_string())?);
                }
                items.extend(args.iter().skip(2).cloned());
                for index in (start + delete)..length {
                    items.push(self.array_like_get(receiver, &index.to_string())?);
                }
                self.heap.allocate_list(items)
            }
            // The no-comparator sorts: the string-keyed ordering. A comparator
            // argument makes `sort` a driven call handled at the call level.
            "toSorted" => {
                let length = self.array_like_length(receiver)?;
                let items = self.array_like_present_elements(receiver, length)?;
                let sorted = self.array_like_string_sort(items)?;
                self.array_like_dense_bound(length)?;
                let mut output: Vec<Value> = sorted;
                output.resize(length as usize, Value::Undefined);
                self.heap.allocate_list(output)
            }
            "sort" => {
                // A comparator turns `sort` into a driven call; inside a
                // builtin result there is no call path, so refuse honestly.
                if args
                    .first()
                    .is_some_and(|comparator| !matches!(comparator, Value::Undefined))
                {
                    return Err(RuntimeError::ValidationFailed {
                        reason:
                            "TS_NESTED_DRIVER_UNSUPPORTED: a comparator sort needs the call path"
                                .to_string(),
                    });
                }
                self.array_like_require_writable(receiver, method)?;
                let length = self.array_like_length(receiver)?;
                let items = self.array_like_present_elements(receiver, length)?;
                let sorted = self.array_like_string_sort(items)?;
                self.array_like_write_back(receiver, length, sorted)?;
                Ok(self.array_like_receiver_object(receiver))
            }
            "with" => {
                let length = self.array_like_length(receiver)?;
                let index = self
                    .heap
                    .javascript_to_number(args.first().unwrap_or(&Value::Undefined))?;
                let position = {
                    let index = if index.is_nan() { 0.0 } else { index.trunc() };
                    let index = if index < 0.0 {
                        length as f64 + index
                    } else {
                        index
                    };
                    (index >= 0.0 && index < length as f64).then_some(index as u64)
                };
                let Some(position) = position else {
                    return Err(RuntimeError::range_error(format!(
                        "Invalid index : {}",
                        crate::runtime::javascript::javascript_to_string(&Value::Number(
                            index.trunc()
                        ))
                    )));
                };
                let value = args.get(1).cloned().unwrap_or(Value::Undefined);
                self.array_like_dense_bound(length)?;
                let mut items = Vec::with_capacity(length as usize);
                for i in 0..length {
                    items.push(if i == position {
                        value.clone()
                    } else {
                        self.array_like_get(receiver, &i.to_string())?
                    });
                }
                self.heap.allocate_list(items)
            }
            _ => Err(RuntimeError::ValidationFailed {
                reason: format!("TS_METHOD_UNSUPPORTED: Array.prototype.{method}"),
            }),
        }
    }

    /// The call-level `Array.prototype.<method>` entry: the methods whose
    /// ECMA steps call a guest callback — `forEach`/`map`/`filter` and their
    /// siblings, `reduce`, and `sort`/`toSorted` with a comparator — plus the
    /// callable check ECMA orders ahead of iterating. `receiver` is the
    /// detached call's `this`; `thisArg` is the second argument the iterating
    /// methods hand the callback as `this`.
    pub(super) fn call_array_like_method(
        &mut self,
        receiver: Value,
        method: &str,
        args: Vec<Value>,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        if !matches!(return_target, ReturnTarget::Direct) {
            return Err(RuntimeError::ValidationFailed {
                reason: "TS_NESTED_DRIVER_UNSUPPORTED: a callback method reached as a callback cannot suspend".to_string(),
            });
        }
        // The sorts validate the comparator before they touch `this` —
        // ECMA runs the callable check ahead of `ToObject(this)`.
        if matches!(method, "sort" | "toSorted")
            && let Some(comparator) = args.first()
            && !matches!(comparator, Value::Undefined)
            && !self.is_callable(comparator)?
        {
            return Err(RuntimeError::IncompatibleReceiver {
                message: format!(
                    "The comparison function must be either a function or undefined: {}",
                    self.v8_value_text(comparator)?
                ),
            });
        }
        if matches!(receiver, Value::Undefined | Value::Null) {
            return Err(RuntimeError::IncompatibleReceiver {
                message: format!("Array.prototype.{method} called on null or undefined"),
            });
        }
        if method == "sort" || method == "toSorted" {
            // `sort` alone writes back.
            let comparator = args.first().cloned().unwrap_or(Value::Undefined);
            if method == "sort" {
                self.array_like_require_writable(&receiver, method)?;
            }
            let length = self.array_like_length(&receiver)?;
            self.array_like_dense_bound(length)?;
            let mut pending: Vec<Value> = Vec::new();
            let mut undefined_count = 0u64;
            for element in self.array_like_present_elements(&receiver, length)? {
                if matches!(element, Value::Undefined) {
                    undefined_count += 1;
                } else {
                    pending.push(element);
                }
            }
            // The pending list is visited front-to-back; reversed, the next
            // element pops off the back.
            pending.reverse();
            let Some(first) = pending.pop() else {
                // Nothing to order: write back the undefined tail.
                return self.finish_sort(
                    receiver,
                    length,
                    Vec::new(),
                    undefined_count,
                    method == "sort",
                );
            };
            let state = SortState {
                pending,
                sorted: Vec::new(),
                current: first,
                probe: 0,
                lo: 0,
                hi: 0,
                undefined_count,
                receiver,
                length,
                in_place: method == "sort",
            };
            return self.begin_sort_driver(comparator, state);
        }
        let function = args.first().cloned().unwrap_or(Value::Undefined);
        // The `length` read precedes the callable check — ECMA's
        // `LengthOfArrayLike(O)` step comes first, and its hooks still run.
        let length = self.array_like_length(&receiver)?;
        if !self.is_callable(&function)? {
            return Err(RuntimeError::type_error(format!(
                "{} is not a function",
                self.v8_value_text(&function)?
            )));
        }
        let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
        let descending = matches!(method, "findLast" | "findLastIndex" | "reduceRight");
        // The find family calls the predicate on every index — holes
        // included — while the iterators gate each visit on `HasProperty`.
        let gated = !matches!(method, "find" | "findIndex" | "findLast" | "findLastIndex");
        let mut walk = ArrayLikeWalk {
            receiver: receiver.clone(),
            next: if descending {
                if length == 0 { u64::MAX } else { length - 1 }
            } else {
                0
            },
            length,
            descending,
            gated,
            omit_receiver: false,
        };
        let completion = match method {
            "forEach" => CallbackCompletion::Discard,
            "map" => CallbackCompletion::Map { length },
            "filter" => CallbackCompletion::Filter,
            "every" => CallbackCompletion::Every,
            "some" => CallbackCompletion::Some,
            "find" | "findLast" => CallbackCompletion::Find,
            "findIndex" | "findLastIndex" => CallbackCompletion::FindIndex,
            "flatMap" => CallbackCompletion::FlatMap,
            "reduce" | "reduceRight" => {
                // The accumulator either starts as `initialValue` or is the
                // first present element — visited by `Has`, never passed to
                // the callback. An empty receiver without a seed is the
                // TypeError V8 phrases under the old shim's name.
                let accumulator = if args.len() >= 2 {
                    args[1].clone()
                } else {
                    let Some(first) = self.array_like_walk_index(&walk)? else {
                        return Err(RuntimeError::type_error(
                            "Reduce of empty array with no initial value",
                        ));
                    };
                    walk.next = if descending {
                        first.checked_sub(1).unwrap_or(u64::MAX)
                    } else {
                        first + 1
                    };
                    self.array_like_get(&receiver, &first.to_string())?
                };
                CallbackCompletion::Reduce { accumulator }
            }
            _ => {
                return Err(RuntimeError::ValidationFailed {
                    reason: format!("TS_METHOD_UNSUPPORTED: Array.prototype.{method}"),
                });
            }
        };
        // `reduce`'s `this` is always `undefined`; the iterating methods
        // hand the callback `thisArg`.
        let this_arg = if matches!(method, "reduce" | "reduceRight") {
            Value::Undefined
        } else {
            this_arg
        };
        self.begin_array_like_driver(function, walk, completion, this_arg, return_target)
    }

    /// The index a lazy array-like walk visits next — `None` when it is
    /// exhausted. A gated walk resolves the next index that is present on
    /// the receiver *now*, so a callback's own writes are observed and its
    /// deletes skip their visits, exactly ECMA's per-index `Has`/`Get`
    /// ordering.
    fn array_like_walk_index(&mut self, walk: &ArrayLikeWalk) -> Result<Option<u64>, RuntimeError> {
        // `u64::MAX` is the descending walk's exhausted marker — an index
        // comparison alone would read it as "below every index" and revisit
        // the whole range forever.
        if walk.descending && walk.next == u64::MAX {
            return Ok(None);
        }
        if !walk.gated {
            // Every index is visited (`find` family): the cursor itself.
            return Ok(if walk.descending {
                (walk.next != u64::MAX).then_some(walk.next)
            } else {
                (walk.next < walk.length).then_some(walk.next)
            });
        }
        match &walk.receiver {
            Value::Ref(id) => {
                let id = *id;
                match self.heap.get(id)? {
                    HeapObject::List(items) | HeapObject::Tuple(items) => {
                        // A list's indices are dense in memory: the scan is a
                        // walk over the hole set, amortized linear per call.
                        let bound = (items.len() as u64).min(walk.length);
                        if walk.descending {
                            let mut index = walk.next.min(bound.saturating_sub(1));
                            while index != u64::MAX && index < bound {
                                if !self.heap.is_list_hole(id, index as usize) {
                                    return Ok(Some(index));
                                }
                                index = index.checked_sub(1).unwrap_or(u64::MAX);
                            }
                            Ok(None)
                        } else {
                            let mut index = walk.next;
                            while index < bound {
                                if !self.heap.is_list_hole(id, index as usize) {
                                    return Ok(Some(index));
                                }
                                index += 1;
                            }
                            Ok(None)
                        }
                    }
                    HeapObject::RegExpMatch(result) => {
                        let bound = (result.items.len() as u64).min(walk.length);
                        Ok(if walk.descending {
                            (walk.next != u64::MAX && walk.next < bound).then_some(walk.next)
                        } else {
                            (walk.next < bound).then_some(walk.next)
                        })
                    }
                    _ => self.present_index_in_window(walk),
                }
            }
            Value::Record(_) | Value::List(_) | Value::Tuple(_) => {
                self.present_index_in_window(walk)
            }
            Value::String(text) => {
                let bound = (text.encode_utf16().count() as u64).min(walk.length);
                Ok(if walk.descending {
                    (walk.next != u64::MAX && walk.next < bound).then_some(walk.next)
                } else {
                    (walk.next < bound).then_some(walk.next)
                })
            }
            // A primitive's wrapper holds no indexed properties.
            _ => Ok(None),
        }
    }

    /// The next present integer key in the walk's window, resolved live
    /// against the receiver's own enumerable keys — ascending finds the
    /// smallest `>= next`, descending the largest `<= next`.
    fn present_index_in_window(
        &mut self,
        walk: &ArrayLikeWalk,
    ) -> Result<Option<u64>, RuntimeError> {
        let keys: Vec<u64> = match &walk.receiver {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => record
                    .iter()
                    .filter_map(|(key, _)| integer_index_key(key))
                    .collect(),
                HeapObject::BuiltinFunction(_) => self
                    .heap
                    .builtin_enumerable_keys(*id)?
                    .iter()
                    .filter_map(|key| integer_index_key(key))
                    .collect(),
                _ => Vec::new(),
            },
            Value::Record(record) => record
                .iter()
                .filter_map(|(key, _)| integer_index_key(key))
                .collect(),
            Value::List(items) | Value::Tuple(items) => {
                let bound = (items.len() as u64).min(walk.length);
                return Ok(if walk.descending {
                    (walk.next != u64::MAX && walk.next < bound).then_some(walk.next)
                } else {
                    (walk.next < bound).then_some(walk.next)
                });
            }
            _ => Vec::new(),
        };
        // The rescan is O(keys) per callback step; charge it so a walk the
        // callback keeps mutating stays under the instruction budget.
        self.charge_intrinsic_work(keys.len());
        Ok(if walk.descending {
            keys.iter()
                .filter(|index| **index <= walk.next && **index < walk.length)
                .max()
                .copied()
        } else {
            keys.iter()
                .filter(|index| **index >= walk.next && **index < walk.length)
                .min()
                .copied()
        })
    }

    /// The next invocation a lazy array-like walk produces: resolve the
    /// index against the receiver as it stands *now*, Get the element, and
    /// build the callback's argument tuple — `[acc,] element, index, O`.
    /// The produced tuple is appended to `calls` so the element-keyed
    /// completions read `calls[next_index - 1]` as they do for a queued
    /// driver.
    pub(super) fn array_like_next_call(
        &mut self,
        callback: &mut CallbackDriver,
    ) -> Result<Option<Vec<Value>>, RuntimeError> {
        let Some(walk) = &callback.array_like else {
            return Ok(None);
        };
        let walk = walk.clone();
        let Some(index) = self.array_like_walk_index(&walk)? else {
            return Ok(None);
        };
        // The cursor advances past the resolved index — before the callback
        // runs, so a callback re-checking `next` cannot revisit it.
        if let Some(walk) = &mut callback.array_like {
            walk.next = if walk.descending {
                index.checked_sub(1).unwrap_or(u64::MAX)
            } else {
                index + 1
            };
        }
        let element = self.array_like_get(&walk.receiver, &index.to_string())?;
        let mut arguments = Vec::with_capacity(4);
        if let CallbackCompletion::Reduce { accumulator } = &callback.completion {
            arguments.push(accumulator.clone());
        }
        arguments.push(element);
        arguments.push(Value::Number(index as f64));
        if !walk.omit_receiver {
            arguments.push(walk.receiver.clone());
        }
        Ok(Some(arguments))
    }

    /// The final write-back a completed sort does — sorted defined elements,
    /// then `undefined`s, then holes.
    fn finish_sort(
        &mut self,
        receiver: Value,
        length: u64,
        mut sorted: Vec<Value>,
        undefined_count: u64,
        in_place: bool,
    ) -> Result<(), RuntimeError> {
        sorted.extend(std::iter::repeat_n(
            Value::Undefined,
            undefined_count as usize,
        ));
        if in_place {
            self.array_like_write_back(&receiver, length, sorted)?;
            let receiver = self.array_like_receiver_object(&receiver);
            self.stack.push(receiver);
        } else {
            sorted.resize(length.min(usize::MAX as u64) as usize, Value::Undefined);
            let result = self.heap.allocate_list(sorted)?;
            self.stack.push(result);
        }
        Ok(())
    }

    /// The elements `0..length` — `Has` then `Get`, in index order — that the
    /// sorts reorder and the callback methods iterate.
    pub(super) fn array_like_present_elements(
        &mut self,
        receiver: &Value,
        length: u64,
    ) -> Result<Vec<Value>, RuntimeError> {
        let mut items = Vec::new();
        for index in self.array_like_present_indices(receiver)? {
            if index >= length {
                break;
            }
            items.push(self.array_like_get(receiver, &index.to_string())?);
        }
        Ok(items)
    }

    /// ECMA's default `sort` ordering: ToString each present element and
    /// compare by UTF-16 code unit; `undefined` sorts after every defined
    /// element.
    fn array_like_string_sort(&mut self, items: Vec<Value>) -> Result<Vec<Value>, RuntimeError> {
        let mut keyed = items
            .into_iter()
            .map(|value| {
                let key = if matches!(value, Value::Undefined) {
                    None
                } else {
                    Some(self.heap.javascript_to_string(&value)?)
                };
                Ok((value, key))
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        keyed.sort_by(|(_, left), (_, right)| match (left, right) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(left), Some(right)) => left.encode_utf16().cmp(right.encode_utf16()),
        });
        Ok(keyed.into_iter().map(|(value, _)| value).collect())
    }

    /// Sort's write-back: the sorted elements take positions `0..n` and the
    /// positions after them to `length` are deleted, preserving the sparse
    /// receiver's holes.
    pub(super) fn array_like_write_back(
        &mut self,
        receiver: &Value,
        length: u64,
        sorted: Vec<Value>,
    ) -> Result<(), RuntimeError> {
        let count = sorted.len() as u64;
        for (index, value) in sorted.into_iter().enumerate() {
            self.array_like_set(receiver, &index.to_string(), value)?;
        }
        for index in self.array_like_present_indices(receiver)? {
            if index >= count && index < length {
                self.array_like_delete(receiver, &index.to_string())?;
            }
        }
        Ok(())
    }

    /// A fresh dense array can name at most `u32::MAX` positions; a sparse
    /// receiver's length beyond it is `RangeError: Invalid array length` —
    /// the same ceiling `Array(n)` has.
    fn array_like_dense_bound(&self, length: u64) -> Result<(), RuntimeError> {
        if length > u32::MAX as u64 {
            return Err(RuntimeError::range_error("Invalid array length"));
        }
        self.heap.ensure_list_allocation_len(length as usize)
    }

    /// Recursive `flat` over the array-like's elements; a nested value
    /// spreads when it is array-shaped in this model.
    fn array_like_flatten(
        &mut self,
        receiver: &Value,
        length: u64,
        depth: u64,
        level: u64,
        output: &mut Vec<Value>,
    ) -> Result<(), RuntimeError> {
        for index in self.array_like_present_indices(receiver)? {
            if index >= length {
                break;
            }
            let value = self.array_like_get(receiver, &index.to_string())?;
            let spread = depth > level
                && match &value {
                    Value::List(_) | Value::Tuple(_) => true,
                    Value::Ref(id) => matches!(
                        self.heap.get(*id)?,
                        HeapObject::List(_) | HeapObject::Tuple(_) | HeapObject::RegExpMatch(_)
                    ),
                    _ => false,
                };
            if spread {
                let inner_length = self.array_like_length(&value)?;
                self.array_like_flatten(&value, inner_length, depth, level + 1, output)?;
            } else {
                output.push(value);
            }
        }
        Ok(())
    }
}
