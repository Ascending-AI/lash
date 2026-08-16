use std::collections::BTreeSet;

use super::super::{
    ErrorKind, canonical_regexp_flags, ensure_javascript_string_size, javascript_to_string,
};
use super::javascript::{ecma_record_entries, js_stdlib_error};
use super::javascript_json::javascript_json_stringify;
use super::*;

impl<H: ExecutionHost> Vm<'_, H> {
    fn require_typescript_intrinsic(&self, operation: &str) -> Result<(), RuntimeError> {
        if self.reference_semantics {
            Ok(())
        } else {
            Err(RuntimeError::ValidationFailed {
                reason: format!(
                    "TYPESCRIPT_REFERENCE_SEMANTICS_REQUIRED: {operation} is unavailable in Lashlang"
                ),
            })
        }
    }

    pub(super) fn execute_dynamic_call(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("dynamic calls")?;
        let arguments = self.pop_stack()?;
        let function = self.pop_stack()?;
        let arguments = match arguments {
            Value::Ref(id) => match self.heap.get(id)? {
                HeapObject::List(values) | HeapObject::Tuple(values) => values.clone(),
                object => {
                    return Err(RuntimeError::ShapingListRequired {
                        builtin: "dynamic call".into(),
                        actual: object.kind_name().to_string(),
                    });
                }
            },
            Value::List(values) | Value::Tuple(values) => values.to_vec(),
            value => {
                return Err(RuntimeError::ShapingListRequired {
                    builtin: "dynamic call".into(),
                    actual: super::super::value_type_name(&value).to_string(),
                });
            }
        };
        self.begin_direct_function_call(function, arguments)
    }

    pub(super) fn execute_async_map(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("async map")?;
        let function = self.pop_stack()?;
        let receiver = self.pop_stack()?;
        let items = match &receiver {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(values) | HeapObject::Tuple(values) => values.clone(),
                object => {
                    return Err(RuntimeError::ShapingListRequired {
                        builtin: "async map".into(),
                        actual: object.kind_name().to_string(),
                    });
                }
            },
            Value::List(values) | Value::Tuple(values) => values.to_vec(),
            value => {
                return Err(RuntimeError::ShapingListRequired {
                    builtin: "async map".into(),
                    actual: super::super::value_type_name(value).to_string(),
                });
            }
        };
        // Deliberately schedule one callback body to completion before starting
        // the next. Every effect boundary remains resumable and journaled; WP-A
        // consumes the settled results in input order. This deterministic v1
        // policy differs from JavaScript's interleaving of async callbacks.
        let calls = items
            .into_iter()
            .enumerate()
            .map(|(index, value)| vec![value, Value::Number(index as f64), receiver.clone()])
            .collect();
        self.begin_callback_driver(function, calls, true, true)
    }

    pub(super) fn execute_javascript_heap_new(&mut self, argc: usize) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("JavaScript heap constructors")?;
        let mut values = Vec::with_capacity(argc);
        for _ in 0..argc {
            values.push(self.pop_stack()?);
        }
        values.reverse();
        let Some((Value::String(kind), args)) = values.split_first() else {
            return Err(js_stdlib_error("missing heap constructor discriminator"));
        };
        if let Some(error_kind) = ErrorKind::from_name(kind) {
            let (errors, message_index) = if error_kind == ErrorKind::AggregateError {
                let Some(errors) = args.first() else {
                    return Err(js_stdlib_error(
                        "AggregateError requires an errors iterable",
                    ));
                };
                let errors = self
                    .heap
                    .allocate_list(heap_sequence(&self.heap, errors)?)?;
                (Some(errors), 1)
            } else {
                (None, 0)
            };
            let message = match args.get(message_index) {
                None | Some(Value::Undefined) => String::new(),
                Some(value) => self.heap.javascript_to_string(value)?,
            };
            let cause = args
                .get(message_index + 1)
                .and_then(|options| javascript_error_cause(&self.heap, options));
            let value = self
                .heap
                .allocate_error(error_kind, message, cause, errors)?;
            self.stack.push(value);
            return Ok(());
        }
        let value = match (kind.as_str(), args) {
            ("URL", [input]) => {
                let input = self.heap.javascript_to_string(input)?;
                ensure_javascript_string_size(input.len())?;
                self.heap.allocate_url(&input, None)?
            }
            ("URL", [input, base]) => {
                let input = self.heap.javascript_to_string(input)?;
                ensure_javascript_string_size(input.len())?;
                if matches!(base, Value::Undefined) {
                    self.heap.allocate_url(&input, None)?
                } else {
                    let base = self.heap.javascript_to_string(base)?;
                    ensure_javascript_string_size(base.len())?;
                    self.heap.allocate_url(&input, Some(&base))?
                }
            }
            ("URLSearchParams", []) | ("URLSearchParams", [Value::Undefined | Value::Null]) => {
                self.heap.allocate_url_search_params(Vec::new())?
            }
            ("URLSearchParams", [initial]) => {
                let entries = url_search_params_initial(&self.heap, initial)?;
                self.heap.allocate_url_search_params(entries)?
            }
            ("RegExp", [pattern, flags]) => {
                let pattern = scalar_javascript_string(pattern)?;
                let flags = canonical_regexp_flags(&scalar_javascript_string(flags)?)
                    .map_err(js_stdlib_error)?;
                self.heap.allocate_regexp(pattern, flags)?
            }
            ("Map", []) | ("Map", [Value::Undefined]) => self.heap.allocate_map(Vec::new())?,
            ("Map", [entries]) => {
                let mut map_entries = Vec::new();
                for entry in heap_sequence(&self.heap, entries)? {
                    let pair = heap_sequence(&self.heap, &entry)?;
                    if pair.len() < 2 {
                        return Err(js_stdlib_error(
                            "Map constructor entry has fewer than two values",
                        ));
                    }
                    map_entries.push((pair[0].clone(), pair[1].clone()));
                }
                self.heap.allocate_map(map_entries)?
            }
            ("Set", []) | ("Set", [Value::Undefined]) => self.heap.allocate_set(Vec::new())?,
            ("Set", [values]) => self.heap.allocate_set(heap_sequence(&self.heap, values)?)?,
            ("Date", values) => self.construct_javascript_date(values)?,
            _ => {
                return Err(js_stdlib_error(format!(
                    "TS_CONSTRUCTOR_UNSUPPORTED: {kind} with {} argument(s)",
                    args.len()
                )));
            }
        };
        self.stack.push(value);
        Ok(())
    }

    pub(super) fn execute_javascript_instanceof(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("JavaScript instanceof")?;
        let constructor = self.pop_stack()?;
        let value = self.pop_stack()?;
        let Value::String(constructor) = constructor else {
            return Err(js_stdlib_error(
                "instanceof constructor discriminator must be a string",
            ));
        };
        self.stack.push(Value::Bool(
            self.heap.javascript_instanceof(&value, &constructor)?,
        ));
        Ok(())
    }

    pub(super) fn execute_javascript_heap_delete_member(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("JavaScript member deletion")?;
        let key = self.pop_stack()?;
        let receiver = self.pop_stack()?;
        let deleted = self.heap.delete_javascript_member(&receiver, &key)?;
        self.stack.push(Value::Bool(deleted));
        Ok(())
    }

    pub(super) fn execute_javascript_global_delete(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("global deletion")?;
        let name = self.pop_stack()?;
        let Value::String(name) = name else {
            return Err(js_stdlib_error("global deletion name must be a string"));
        };
        reject_reserved_global_name(&name)?;
        let slot = self
            .chunk
            .slot_names
            .iter()
            .position(|candidate| candidate.text.as_ref() == name.as_str());
        let slots = if self.active_function.is_some() {
            &mut self
                .frames
                .first_mut()
                .expect("an active function has a root caller frame")
                .slots
        } else {
            &mut self.slots
        };
        let deleted = if let Some(slot) = slot {
            slots.ensure_assignable(slot, &self.chunk.slot_names)?;
            slots.values[slot].take().is_some()
        } else {
            slots.extras.remove(name.as_str()).is_some()
        };
        self.assigned_globals.insert(name.to_string());
        self.stack.push(Value::Bool(deleted));
        Ok(())
    }

    pub(super) fn execute_javascript_global_has(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("global presence query")?;
        let name = self.pop_stack()?;
        let Value::String(name) = name else {
            return Err(js_stdlib_error("global presence name must be a string"));
        };
        reject_reserved_global_name(&name)?;
        let slot = self
            .chunk
            .slot_names
            .iter()
            .position(|candidate| candidate.text.as_ref() == name.as_str());
        let slots = if self.active_function.is_some() {
            &self
                .frames
                .first()
                .expect("an active function has a root caller frame")
                .slots
        } else {
            &self.slots
        };
        let present = slot.map_or_else(
            || slots.extras.get(name.as_str()).is_some(),
            |slot| slots.values[slot].is_some(),
        );
        self.stack.push(Value::Bool(present));
        Ok(())
    }

    pub(super) fn execute_javascript_global_set(&mut self) -> Result<(), RuntimeError> {
        self.require_typescript_intrinsic("global assignment")?;
        let value = self.pop_stack()?;
        let name = self.pop_stack()?;
        let Value::String(name) = name else {
            return Err(js_stdlib_error("global assignment name must be a string"));
        };
        reject_reserved_global_name(&name)?;
        let slot = self
            .chunk
            .slot_names
            .iter()
            .position(|candidate| candidate.text.as_ref() == name.as_str());
        let slots = if self.active_function.is_some() {
            &mut self
                .frames
                .first_mut()
                .expect("an active function has a root caller frame")
                .slots
        } else {
            &mut self.slots
        };
        if let Some(slot) = slot {
            slots.ensure_assignable(slot, &self.chunk.slot_names)?;
            slots.values[slot] = Some(value.clone());
        } else {
            slots.extras.insert(name.to_string(), value.clone());
        }
        self.assigned_globals.insert(name.to_string());
        self.stack.push(value);
        Ok(())
    }
}

fn scalar_javascript_string(value: &Value) -> Result<String, RuntimeError> {
    if matches!(value, Value::Ref(_)) {
        return Err(js_stdlib_error(
            "heap constructor scalar argument cannot be an object",
        ));
    }
    Ok(javascript_to_string(value))
}

fn heap_sequence(heap: &Heap, value: &Value) -> Result<Vec<Value>, RuntimeError> {
    Ok(match value {
        Value::Ref(id) => match heap.get(*id)? {
            HeapObject::List(values) | HeapObject::Tuple(values) => values.clone(),
            object => {
                return Err(js_stdlib_error(format!(
                    "{} is not an iterable constructor input",
                    object.kind_name()
                )));
            }
        },
        Value::List(values) | Value::Tuple(values) => values.to_vec(),
        _ => {
            return Err(js_stdlib_error(
                "constructor input is not an iterable value",
            ));
        }
    })
}

fn url_search_params_initial(
    heap: &Heap,
    initial: &Value,
) -> Result<Vec<(String, String)>, RuntimeError> {
    match initial {
        Value::String(value) => Ok(crate::runtime::heap::parse_params_string(value)),
        Value::Record(record) => ecma_record_entries(record)
            .into_iter()
            .map(|(name, value)| Ok((name.to_string(), heap.javascript_to_string(value)?)))
            .collect(),
        Value::Ref(id) => match heap.get(*id)? {
            HeapObject::UrlSearchParams(params) => Ok(params.entries.clone()),
            HeapObject::Record(record) => ecma_record_entries(record)
                .into_iter()
                .map(|(name, value)| Ok((name.to_string(), heap.javascript_to_string(value)?)))
                .collect(),
            HeapObject::List(_) | HeapObject::Tuple(_) => {
                url_search_params_pairs(heap, heap_sequence(heap, initial)?)
            }
            _ => {
                let string = heap.javascript_to_string(initial)?;
                Ok(crate::runtime::heap::parse_params_string(&string))
            }
        },
        Value::List(_) | Value::Tuple(_) => {
            url_search_params_pairs(heap, heap_sequence(heap, initial)?)
        }
        value => {
            let string = heap.javascript_to_string(value)?;
            Ok(crate::runtime::heap::parse_params_string(&string))
        }
    }
}

fn url_search_params_pairs(
    heap: &Heap,
    values: Vec<Value>,
) -> Result<Vec<(String, String)>, RuntimeError> {
    values
        .into_iter()
        .map(|entry| {
            let pair = heap_sequence(heap, &entry)?;
            if pair.len() != 2 {
                return Err(js_stdlib_error(
                    "URLSearchParams constructor pair must contain exactly two values",
                ));
            }
            Ok((
                heap.javascript_to_string(&pair[0])?,
                heap.javascript_to_string(&pair[1])?,
            ))
        })
        .collect()
}

fn javascript_error_cause(heap: &Heap, options: &Value) -> Option<Value> {
    match options {
        Value::Record(record) => record.get("cause").cloned(),
        Value::Ref(id) => match heap.get(*id).ok()? {
            HeapObject::Record(record) => record.get("cause").cloned(),
            _ => None,
        },
        _ => None,
    }
}

fn reject_reserved_global_name(name: &str) -> Result<(), RuntimeError> {
    if matches!(name, "undefined" | "NaN" | "Infinity") {
        return Err(js_stdlib_error(format!(
            "TS_RESERVED_GLOBAL_NAME: `{name}` cannot be used as session state"
        )));
    }
    Ok(())
}

pub(super) fn javascript_json_stringify_with_options(
    heap: &Heap,
    value: &Value,
    replacer: Option<&Value>,
    space: Option<&Value>,
) -> Result<Option<String>, RuntimeError> {
    if matches!(value, Value::Undefined) {
        return Ok(None);
    }
    let whitelist = replacer
        .filter(|value| !matches!(value, Value::Null | Value::Undefined))
        .map(|value| json_property_whitelist(heap, value))
        .transpose()?;
    let gap = match space {
        Some(value) => match heap.javascript_to_primitive_string_or_number(value)? {
            Value::Number(value) => " ".repeat(if value.is_nan() || value <= 0.0 {
                0
            } else {
                value.trunc().min(10.0) as usize
            }),
            Value::String(value) => value.chars().take(10).collect(),
            _ => String::new(),
        },
        None => String::new(),
    };
    javascript_json_stringify_with_errors(
        heap,
        value,
        &mut BTreeSet::new(),
        whitelist.as_deref(),
        &gap,
        0,
        false,
    )
    .map(Some)
}

fn json_property_whitelist(heap: &Heap, value: &Value) -> Result<Vec<String>, RuntimeError> {
    let values = match value {
        Value::Ref(id) => match heap.get(*id)? {
            HeapObject::List(values) | HeapObject::Tuple(values) => values.as_slice(),
            HeapObject::Closure { .. } => {
                return Err(js_stdlib_error(
                    "TS_JSON_REPLACER_FUNCTION_INTERNAL: function replacers must stay in the VM",
                ));
            }
            _ => {
                return Err(js_stdlib_error(
                    "TypeError: JSON.stringify replacer must be null, an array, or a function",
                ));
            }
        },
        Value::List(values) | Value::Tuple(values) => values.as_ref(),
        _ => {
            return Err(js_stdlib_error(
                "TypeError: JSON.stringify replacer must be null, an array, or a function",
            ));
        }
    };
    let mut result = Vec::new();
    for value in values {
        if matches!(value, Value::String(_) | Value::Number(_)) {
            let key = heap.javascript_to_string(value)?;
            if !result.contains(&key) {
                result.push(key);
            }
        }
    }
    Ok(result)
}

fn javascript_json_stringify_with_errors(
    heap: &Heap,
    value: &Value,
    active: &mut BTreeSet<HeapId>,
    whitelist: Option<&[String]>,
    gap: &str,
    depth: usize,
    array_element: bool,
) -> Result<String, RuntimeError> {
    match value {
        Value::Ref(id) => {
            if !active.insert(*id) {
                return Err(js_stdlib_error(
                    "TypeError: Converting circular structure to JSON",
                ));
            }
            let result = match heap.get(*id)? {
                HeapObject::Error(_) | HeapObject::UrlSearchParams(_) => Ok("{}".to_string()),
                HeapObject::Url(url) => serde_json::to_string(&url.href)
                    .map_err(|error| js_stdlib_error(format!("JSON.stringify: {error}"))),
                HeapObject::List(values) | HeapObject::Tuple(values) => {
                    let values = values
                        .iter()
                        .map(|value| {
                            javascript_json_stringify_with_errors(
                                heap,
                                value,
                                active,
                                whitelist,
                                gap,
                                depth + 1,
                                true,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(join_json_container('[', ']', values, gap, depth))
                }
                HeapObject::Record(record) => {
                    stringify_heap_record(heap, record, active, whitelist, gap, depth)
                }
                object => Err(js_stdlib_error(format!(
                    "JSON.stringify received unsupported {} object",
                    object.kind_name()
                ))),
            };
            active.remove(id);
            result
        }
        Value::Tuple(values) | Value::List(values) => {
            let values = values
                .iter()
                .map(|value| {
                    javascript_json_stringify_with_errors(
                        heap,
                        value,
                        active,
                        whitelist,
                        gap,
                        depth + 1,
                        true,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(join_json_container('[', ']', values, gap, depth))
        }
        Value::Record(record) => stringify_heap_record(heap, record, active, whitelist, gap, depth),
        Value::Undefined if array_element => Ok("null".to_string()),
        value => javascript_json_stringify(value),
    }
}

fn stringify_heap_record(
    heap: &Heap,
    record: &Record,
    active: &mut BTreeSet<HeapId>,
    whitelist: Option<&[String]>,
    gap: &str,
    depth: usize,
) -> Result<String, RuntimeError> {
    let ordered_entries = whitelist.map_or_else(
        || ecma_record_entries(record),
        |keys| {
            keys.iter()
                .filter_map(|key| record.get(key).map(|value| (key.as_str(), value)))
                .collect()
        },
    );
    let entries = ordered_entries
        .into_iter()
        .filter(|(_, value)| !matches!(value, Value::Undefined))
        .map(|(key, value)| {
            let separator = if gap.is_empty() { ":" } else { ": " };
            Ok(format!(
                "{}{separator}{}",
                serde_json::to_string(key).expect("record keys are JSON strings"),
                javascript_json_stringify_with_errors(
                    heap,
                    value,
                    active,
                    whitelist,
                    gap,
                    depth + 1,
                    false,
                )?
            ))
        })
        .collect::<Result<Vec<_>, RuntimeError>>()?;
    Ok(join_json_container('{', '}', entries, gap, depth))
}

fn join_json_container(
    open: char,
    close: char,
    entries: Vec<String>,
    gap: &str,
    depth: usize,
) -> String {
    if entries.is_empty() {
        return format!("{open}{close}");
    }
    if gap.is_empty() {
        return format!("{open}{}{close}", entries.join(","));
    }
    let current = gap.repeat(depth);
    let nested = gap.repeat(depth + 1);
    format!(
        "{open}\n{nested}{}\n{current}{close}",
        entries.join(&format!(",\n{nested}"))
    )
}
