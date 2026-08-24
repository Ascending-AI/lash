//! Field- and index-access on `Value`s, plus the assignment-path machinery.
//! Every read of `record.field` / `tuple[index]` / `list[index]` and every
//! write of `record.field = …` / `list[index] = …` flows through these helpers.
//!
//! The `*_direct` and `*_ref_direct` variants are the sync fast paths the VM
//! uses for concrete operands; projected operands are resolved inline by the
//! VM via `ProjectedValue::get_field` / `get_index`. `assign_path` and
//! `assign_path_steps` walk a `CompiledAssignPath` to mutate nested
//! structures in place.

use std::sync::Arc;

use compact_str::ToCompactString;

use super::instruction::Name;
use super::*;

pub(crate) fn read_field_ref_direct(value: &Value, field: &Name) -> Result<Value, RuntimeError> {
    match value {
        Value::Record(record) => Ok(record
            .get_symbol(field.symbol)
            .cloned()
            .unwrap_or(Value::Null)),
        Value::Image(image) => read_image_field(image, field),
        Value::Null => Ok(Value::Null),
        _ => Err(RuntimeError::CannotReadField {
            field: field.text.to_string(),
            actual: value_type_name(value).to_string(),
        }),
    }
}

pub(crate) fn unwrap_tool_result(value: Value) -> Result<Value, RuntimeError> {
    let Value::Record(record) = value else {
        return Err(RuntimeError::ToolResultExpected {
            actual: value_type_name(&value).to_string(),
        });
    };

    let result_names = result_wrapper_names();
    match record.get_symbol(result_names.ok.symbol) {
        Some(Value::Bool(true)) => record
            .get_symbol(result_names.value.symbol)
            .cloned()
            .ok_or(RuntimeError::ToolResultMissingValue),
        Some(Value::Bool(false)) => {
            let message = record
                .get_symbol(result_names.error.symbol)
                .map(Value::to_string)
                .unwrap_or_else(|| "unknown error".to_string());
            Err(RuntimeError::UnwrappedToolResultFailed { message })
        }
        _ => Err(RuntimeError::ToolResultInvalidOk),
    }
}

pub(crate) fn is_process_handle_record(record: &Record) -> bool {
    record.get("__handle__").is_some() || record.get("handle").is_some()
}

pub(crate) fn read_field_direct(value: Value, field: &Name) -> Result<Value, RuntimeError> {
    match value {
        Value::Record(record) => Ok(record
            .get_symbol(field.symbol)
            .cloned()
            .unwrap_or(Value::Null)),
        Value::Image(image) => read_image_field(&image, field),
        Value::Null => Ok(Value::Null),
        _ => Err(RuntimeError::CannotReadField {
            field: field.text.to_string(),
            actual: value_type_name(&value).to_string(),
        }),
    }
}

pub(crate) fn read_image_field(image: &ImageValue, field: &Name) -> Result<Value, RuntimeError> {
    match field.text.as_ref() {
        "id" => Ok(Value::String(image.id.clone().into())),
        "label" => Ok(Value::String(image.label.clone().into())),
        "size" => Ok(Value::Number(image.size as f64)),
        "width" => Ok(image
            .width
            .map(|width| Value::Number(width as f64))
            .unwrap_or(Value::Null)),
        "height" => Ok(image
            .height
            .map(|height| Value::Number(height as f64))
            .unwrap_or(Value::Null)),
        _ => Ok(Value::Null),
    }
}

pub(crate) fn read_index_direct(target: Value, index: Value) -> Result<Value, RuntimeError> {
    read_index_ref_direct(&target, &index)
}

pub(crate) fn read_index_ref_direct(target: &Value, index: &Value) -> Result<Value, RuntimeError> {
    match target {
        Value::Tuple(values) => {
            let idx = resolve_index(index, values.len())?;
            Ok(idx
                .and_then(|idx| values.get(idx).cloned())
                .unwrap_or(Value::Null))
        }
        Value::List(values) => {
            let idx = resolve_index(index, values.len())?;
            Ok(idx
                .and_then(|idx| values.get(idx).cloned())
                .unwrap_or(Value::Null))
        }
        Value::String(value) => {
            let idx = resolve_index(index, value.chars().count())?;
            Ok(idx
                .and_then(|idx| value.chars().nth(idx))
                .map(|ch| Value::String(ch.to_compact_string()))
                .unwrap_or(Value::Null))
        }
        Value::Record(record) => Ok(record
            .get(coerce_string(index)?.as_ref())
            .cloned()
            .unwrap_or(Value::Null)),
        Value::Null => Ok(Value::Null),
        _ => Err(RuntimeError::CannotIndex {
            actual: value_type_name(target).to_string(),
        }),
    }
}

pub(crate) fn read_javascript_field_direct(
    value: Value,
    field: &Name,
) -> Result<Value, RuntimeError> {
    match value {
        Value::Record(record) => Ok(record
            .get_symbol(field.symbol)
            .cloned()
            .unwrap_or(Value::Undefined)),
        Value::List(values) | Value::Tuple(values) if field.text.as_ref() == "length" => {
            Ok(Value::Number(values.len() as f64))
        }
        Value::String(value) if field.text.as_ref() == "length" => {
            Ok(Value::Number(value.encode_utf16().count() as f64))
        }
        Value::Null | Value::Undefined => Err(RuntimeError::CannotReadField {
            field: field.text.to_string(),
            actual: value_type_name(&value).to_string(),
        }),
        _ => Ok(Value::Undefined),
    }
}

pub(crate) fn read_javascript_index_direct(
    target: Value,
    index: Value,
) -> Result<Value, RuntimeError> {
    let key = javascript_to_string(&index);
    read_javascript_index_direct_with_key(target, &key)
}

/// Property names whose ECMA meaning is the prototype chain.
///
/// The value model is dense records with no prototypes, so none of these names
/// has anything to read or mutate here. Every statically written form is
/// rejected by the TypeScript adapter; a computed key is only knowable at the
/// access, and the two honest answers there are `undefined` — silently
/// divergent from node, where `o[k]` with `k === '__proto__'` yields the
/// prototype and `o[k] = v` changes what the object inherits — or a named
/// rejection. It rejects.
pub(crate) fn is_prototype_chain_key(key: &str) -> bool {
    ensure_no_prototype_chain_wire_key(key).is_err()
}

/// Refuses a prototype-chain data key while decoding persisted value wires.
///
/// Nothing this runtime encodes can carry one of these keys any more —
/// `JSON.parse` and every host result refuse them — so a wire that does is
/// forged or predates the guard. Restoring it would recreate a stranded key
/// that `Object.keys` lists but nothing can read or serialize.
pub(crate) fn ensure_no_prototype_chain_wire_key(name: &str) -> Result<(), &'static str> {
    match name {
        "__proto__" => Err(
            "record key `__proto__` names the prototype chain, which this value model does not have",
        ),
        "__defineGetter__" => Err(
            "record key `__defineGetter__` names the prototype chain, which this value model does not have",
        ),
        "__defineSetter__" => Err(
            "record key `__defineSetter__` names the prototype chain, which this value model does not have",
        ),
        "__lookupGetter__" => Err(
            "record key `__lookupGetter__` names the prototype chain, which this value model does not have",
        ),
        "__lookupSetter__" => Err(
            "record key `__lookupSetter__` names the prototype chain, which this value model does not have",
        ),
        _ => Ok(()),
    }
}

pub(crate) fn prototype_chain_key_error(key: &str) -> Option<RuntimeError> {
    is_prototype_chain_key(key).then(|| RuntimeError::ValidationFailed {
        reason: format!(
            "TS_PROTOTYPE_MUTATION_UNSUPPORTED: `{key}` reaches the prototype chain, which this value model does not have"
        ),
    })
}

/// Refuses a prototype-chain name carried as a data key by a value entering the
/// runtime from outside the guest — parsed JSON, a host or tool result, a
/// decoded wire heap.
///
/// The read guard above is only half of it. Nothing inside the guest can build
/// such a key, but an external value could arrive already carrying one, and
/// then `Object.keys` listed it while every read and `JSON.stringify` refused
/// it: an enumerable key nothing could read and nothing could serialize,
/// reachable from ordinary untrusted-JSON round-tripping. Refusing at entry
/// makes the over-rejection uniform instead of stranding a value in a shape
/// with no way out. Node parses `"__proto__"` as an ordinary data property, so
/// this is a registered divergence rather than a conformance claim.
pub(crate) fn prototype_chain_data_key_error(value: &Value) -> Option<RuntimeError> {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Record(record) => {
                for entry in record.entries.iter() {
                    if is_prototype_chain_key(&entry.name) {
                        return Some(RuntimeError::ValidationFailed {
                            reason: format!(
                                "TS_PROTOTYPE_MUTATION_UNSUPPORTED: `{}` names the prototype chain, which this value model does not have, so a value entering from JSON.parse or a host result cannot carry it as a data key",
                                entry.name
                            ),
                        });
                    }
                    pending.push(&entry.value);
                }
            }
            Value::List(values) | Value::Tuple(values) => pending.extend(values.iter()),
            _ => {}
        }
    }
    None
}

pub(crate) fn read_javascript_index_direct_with_key(
    target: Value,
    key: &str,
) -> Result<Value, RuntimeError> {
    if let Some(error) = prototype_chain_key_error(key) {
        return Err(error);
    }
    match target {
        Value::List(values) | Value::Tuple(values) => Ok(javascript_array_index_key(key)
            .and_then(|index| values.get(index).cloned())
            .unwrap_or(Value::Undefined)),
        Value::String(value) => {
            let Some(index) = javascript_array_index_key(key) else {
                return Ok(Value::Undefined);
            };
            let Some(unit) = value.encode_utf16().nth(index) else {
                return Ok(Value::Undefined);
            };
            char::from_u32(unit.into())
                .map(|ch| Value::String(ch.to_compact_string()))
                .ok_or_else(|| RuntimeError::ValidationFailed {
                    reason: "TS_LONE_SURROGATE_UNSUPPORTED: string indexing produced an unrepresentable lone surrogate".to_string(),
                })
        }
        Value::Record(record) => Ok(record.get(key).cloned().unwrap_or(Value::Undefined)),
        Value::Null | Value::Undefined => Err(RuntimeError::CannotIndex {
            actual: value_type_name(&target).to_string(),
        }),
        _ => Ok(Value::Undefined),
    }
}

pub(crate) fn read_javascript_heap_field(
    heap: &Heap,
    id: HeapId,
    field: &Name,
) -> Result<Value, RuntimeError> {
    if field.text.as_ref() == "lastIndex"
        && let Some(last_index) = heap.regexp_last_index(id)?
    {
        return Ok(Value::Number(last_index as f64));
    }
    Ok(match heap.get(id)? {
        HeapObject::Record(record) => record
            .get_symbol(field.symbol)
            .cloned()
            .unwrap_or(Value::Undefined),
        HeapObject::List(values) | HeapObject::Tuple(values) if field.text.as_ref() == "length" => {
            Value::Number(values.len() as f64)
        }
        HeapObject::RegExp(regexp) => match field.text.as_ref() {
            "source" => Value::String(regexp_source(regexp).into()),
            "flags" => Value::String(regexp.flags.as_str().into()),
            "global" => Value::Bool(regexp.flags.contains('g')),
            "ignoreCase" => Value::Bool(regexp.flags.contains('i')),
            "multiline" => Value::Bool(regexp.flags.contains('m')),
            "sticky" => Value::Bool(regexp.flags.contains('y')),
            "unicode" => Value::Bool(regexp.flags.contains('u')),
            _ => Value::Undefined,
        },
        HeapObject::RegExpMatch(result) => match field.text.as_ref() {
            "length" => Value::Number(result.items.len() as f64),
            "index" => result.index.clone(),
            "input" => result.input.clone(),
            "groups" => result.groups.clone(),
            _ => Value::Undefined,
        },
        HeapObject::Map(map) if field.text.as_ref() == "size" => {
            Value::Number(map.entries.len() as f64)
        }
        HeapObject::Set(set) if field.text.as_ref() == "size" => {
            Value::Number(set.values.len() as f64)
        }
        HeapObject::Error(error) => match field.text.as_ref() {
            "name" => Value::String(error.kind.name().into()),
            "message" => Value::String(error.message.as_str().into()),
            "cause" => error.cause.clone().unwrap_or(Value::Undefined),
            "errors" if error.kind == ErrorKind::AggregateError => {
                error.errors.clone().unwrap_or(Value::Undefined)
            }
            _ => Value::Undefined,
        },
        HeapObject::Url(_) => heap
            .url_property(id, field.text.as_ref())?
            .unwrap_or(Value::Undefined),
        HeapObject::UrlSearchParams(params) if field.text.as_ref() == "size" => {
            Value::Number(params.entries.len() as f64)
        }
        _ => Value::Undefined,
    })
}

pub(crate) fn read_javascript_heap_index(
    heap: &Heap,
    id: HeapId,
    index: &Value,
) -> Result<Value, RuntimeError> {
    let key = heap.javascript_to_string(index)?;
    if let Some(error) = prototype_chain_key_error(&key) {
        return Err(error);
    }
    Ok(match heap.get(id)? {
        HeapObject::List(values) | HeapObject::Tuple(values) => javascript_array_index_key(&key)
            .and_then(|index| values.get(index).cloned())
            .unwrap_or(Value::Undefined),
        HeapObject::Record(record) => record.get(&key).cloned().unwrap_or(Value::Undefined),
        HeapObject::RegExp(regexp) => match key.as_str() {
            "lastIndex" => Value::Number(regexp.last_index as f64),
            "source" => Value::String(regexp_source(regexp).into()),
            "flags" => Value::String(regexp.flags.as_str().into()),
            "global" => Value::Bool(regexp.flags.contains('g')),
            "ignoreCase" => Value::Bool(regexp.flags.contains('i')),
            "multiline" => Value::Bool(regexp.flags.contains('m')),
            "sticky" => Value::Bool(regexp.flags.contains('y')),
            "unicode" => Value::Bool(regexp.flags.contains('u')),
            _ => Value::Undefined,
        },
        HeapObject::RegExpMatch(result) => match key.as_str() {
            "length" => Value::Number(result.items.len() as f64),
            "index" => result.index.clone(),
            "input" => result.input.clone(),
            "groups" => result.groups.clone(),
            _ => javascript_array_index_key(&key)
                .and_then(|index| result.items.get(index).cloned())
                .unwrap_or(Value::Undefined),
        },
        HeapObject::Map(map) if key == "size" => Value::Number(map.entries.len() as f64),
        HeapObject::Set(set) if key == "size" => Value::Number(set.values.len() as f64),
        HeapObject::Error(error) => match key.as_str() {
            "name" => Value::String(error.kind.name().into()),
            "message" => Value::String(error.message.as_str().into()),
            "cause" => error.cause.clone().unwrap_or(Value::Undefined),
            "errors" if error.kind == ErrorKind::AggregateError => {
                error.errors.clone().unwrap_or(Value::Undefined)
            }
            _ => Value::Undefined,
        },
        HeapObject::Url(_) => heap.url_property(id, &key)?.unwrap_or(Value::Undefined),
        HeapObject::UrlSearchParams(params) if key == "size" => {
            Value::Number(params.entries.len() as f64)
        }
        _ => Value::Undefined,
    })
}

pub(crate) fn assign_path(
    root: &mut Value,
    path: &CompiledAssignPath,
    indexes: &[Value],
    value: Value,
    names: &[Name],
) -> Result<(), RuntimeError> {
    let mut index_cursor = 0;
    assign_path_steps(root, &path.steps, indexes, &mut index_cursor, value, names)
}

pub(crate) fn assign_path_steps(
    target: &mut Value,
    steps: &[CompiledAssignPathStep],
    indexes: &[Value],
    index_cursor: &mut usize,
    value: Value,
    names: &[Name],
) -> Result<(), RuntimeError> {
    let Some((step, rest)) = steps.split_first() else {
        *target = value;
        return Ok(());
    };

    match *step {
        CompiledAssignPathStep::Field(field) if rest.is_empty() => {
            assign_record_field(target, &names[field], value)
        }
        CompiledAssignPathStep::Field(field) => {
            let child = descend_record_field(target, &names[field])?;
            assign_path_steps(child, rest, indexes, index_cursor, value, names)
        }
        CompiledAssignPathStep::Index if rest.is_empty() => {
            let index = next_assign_index(indexes, index_cursor)?;
            assign_index(target, index, value)
        }
        CompiledAssignPathStep::Index => {
            let index = next_assign_index(indexes, index_cursor)?;
            let child = descend_index(target, index)?;
            assign_path_steps(child, rest, indexes, index_cursor, value, names)
        }
    }
}

pub(crate) fn next_assign_index<'a>(
    indexes: &'a [Value],
    index_cursor: &mut usize,
) -> Result<&'a Value, RuntimeError> {
    let index = indexes
        .get(*index_cursor)
        .ok_or(RuntimeError::MissingAssignmentIndex)?;
    *index_cursor += 1;
    Ok(index)
}

pub(crate) fn assign_record_field(
    target: &mut Value,
    field: &Name,
    value: Value,
) -> Result<(), RuntimeError> {
    match target {
        Value::Record(record) => {
            Arc::make_mut(record).insert_symbolized(field.symbol, field.text.clone(), value);
            Ok(())
        }
        Value::Image(_) => Err(RuntimeError::ImmutableImageFields),
        _ => Err(RuntimeError::CannotAssignField {
            field: field.text.to_string(),
            actual: value_type_name(target).to_string(),
        }),
    }
}

pub(crate) fn descend_record_field<'a>(
    target: &'a mut Value,
    field: &Name,
) -> Result<&'a mut Value, RuntimeError> {
    match target {
        Value::Record(record) => Arc::make_mut(record)
            .get_symbol_mut(field.symbol)
            .ok_or_else(|| RuntimeError::MissingAssignmentField {
                field: field.text.to_string(),
            }),
        Value::Image(_) => Err(RuntimeError::ImmutableImageFieldsThrough),
        _ => Err(RuntimeError::CannotAssignThroughField {
            field: field.text.to_string(),
            actual: value_type_name(target).to_string(),
        }),
    }
}

pub(crate) fn assign_index(
    target: &mut Value,
    index: &Value,
    value: Value,
) -> Result<(), RuntimeError> {
    match target {
        Value::List(values) => {
            let idx = resolve_existing_list_assignment_index(index, values.len())?;
            values.make_mut()[idx] = value;
            Ok(())
        }
        Value::Tuple(_) => Err(RuntimeError::ImmutableTupleIndexes),
        Value::Record(record) => {
            let key = coerce_string(index)?;
            Arc::make_mut(record).insert_str(key.as_ref(), value);
            Ok(())
        }
        Value::Image(_) => Err(RuntimeError::ImmutableImageFields),
        _ => Err(RuntimeError::CannotAssignIndex {
            actual: value_type_name(target).to_string(),
        }),
    }
}

pub(crate) fn descend_index<'a>(
    target: &'a mut Value,
    index: &Value,
) -> Result<&'a mut Value, RuntimeError> {
    match target {
        Value::List(values) => {
            let idx = resolve_existing_list_assignment_index(index, values.len())?;
            Ok(&mut values.make_mut()[idx])
        }
        Value::Tuple(_) => Err(RuntimeError::ImmutableTupleIndexesThrough),
        Value::Record(record) => {
            let key = coerce_string(index)?;
            let record = Arc::make_mut(record);
            if let Some(value) = record.get_mut(key.as_ref()) {
                Ok(value)
            } else {
                Err(RuntimeError::MissingAssignmentKey {
                    key: key.into_owned(),
                })
            }
        }
        Value::Image(_) => Err(RuntimeError::ImmutableImageFieldsThrough),
        _ => Err(RuntimeError::CannotAssignThroughIndex {
            actual: value_type_name(target).to_string(),
        }),
    }
}

pub(crate) fn add_assign_index_number(
    target: &mut Value,
    index: &Value,
    right: f64,
) -> Result<Value, RuntimeError> {
    match target {
        Value::List(values) => {
            let idx = resolve_existing_list_assignment_index(index, values.len())?;
            add_assign_value_number(&mut values.make_mut()[idx], right)
        }
        Value::Tuple(_) => Err(RuntimeError::ImmutableTupleIndexes),
        Value::Record(record) => {
            let key = coerce_string(index)?;
            let record = Arc::make_mut(record);
            if let Some(value) = record.get_mut(key.as_ref()) {
                add_assign_value_number(value, right)
            } else {
                let value = Value::Number(right);
                record.insert_str(key.as_ref(), value.clone());
                Ok(value)
            }
        }
        Value::Image(_) => Err(RuntimeError::ImmutableImageFields),
        _ => Err(RuntimeError::CannotAssignIndex {
            actual: value_type_name(target).to_string(),
        }),
    }
}

pub(crate) fn add_assign_value_number(
    value: &mut Value,
    right: f64,
) -> Result<Value, RuntimeError> {
    match value {
        Value::Number(left) => {
            *left += right;
            Ok(Value::Number(*left))
        }
        left => {
            let value = add_values(left.clone(), Value::Number(right))?;
            *left = value.clone();
            Ok(value)
        }
    }
}

pub(crate) fn resolve_index(index: &Value, len: usize) -> Result<Option<usize>, RuntimeError> {
    let index = as_offset(index)?;
    let len = len as isize;
    let normalized = if index < 0 { len + index } else { index };
    if normalized < 0 || normalized >= len {
        return Ok(None);
    }
    Ok(Some(normalized as usize))
}

pub(crate) fn resolve_existing_list_assignment_index(
    index: &Value,
    len: usize,
) -> Result<usize, RuntimeError> {
    let Value::Number(index) = index else {
        return Err(RuntimeError::InvalidListAssignmentIndex);
    };
    if !index.is_finite() || index.fract() != 0.0 {
        return Err(RuntimeError::InvalidListAssignmentIndex);
    }
    let index = *index as isize;
    let len = len as isize;
    let normalized = if index < 0 { len + index } else { index };
    if normalized < 0 || normalized >= len {
        return Err(RuntimeError::ListAssignmentIndexOutOfBounds);
    }
    Ok(normalized as usize)
}
