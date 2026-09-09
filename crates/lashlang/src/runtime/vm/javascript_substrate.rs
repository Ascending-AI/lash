use std::collections::BTreeSet;

use crate::runtime::heap::ensure_value_depth;

use super::super::{
    ErrorKind, ensure_javascript_string_size, javascript_to_string, to_json_direct,
};
use super::javascript::{ecma_record_entries, js_stdlib_error};
use super::javascript_json::javascript_json_stringify;
use super::*;

/// The stdlib method name that renders a `console.*` argument list.
///
/// The lowerer writes this name into the compiled artifact, so it is program
/// identity: a cached artifact compiled before a rename would still spell the
/// old name. The lowerer spells the same literal by hand rather than sharing a
/// constant, so a one-character drift cannot stay self-consistent — the
/// substrate stops recognising the call and `lash-internal-typescript`'s
/// `console_observation` suite fails loudly instead of quietly coercing again.
pub(super) const CONSOLE_OBSERVATION_TEXT: &str = "__consoleObservationText";

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn require_typescript_intrinsic(&self, operation: &str) -> Result<(), RuntimeError> {
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
        // The discriminator is always a compiler-emitted literal, so the minted
        // brands are reachable here — the dialect's `new` allowlist is what
        // keeps `new EffectError(...)` out of guest source.
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
            ("RegExp", args) => self.construct_regexp(args)?,
            ("Map", []) | ("Map", [Value::Undefined | Value::Null]) => {
                self.heap.allocate_map(Vec::new())?
            }
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
            ("Set", []) | ("Set", [Value::Undefined | Value::Null]) => {
                self.heap.allocate_set(Vec::new())?
            }
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

fn heap_sequence(heap: &Heap, value: &Value) -> Result<Vec<Value>, RuntimeError> {
    Ok(match value {
        Value::Ref(id) => match heap.get(*id)? {
            HeapObject::List(values) | HeapObject::Tuple(values) => values.clone(),
            // An exec/`matchAll` result is an array in ECMA, and `new Map` of a
            // `matchAll` reads its first two slots exactly as it would any
            // other entry pair.
            HeapObject::RegExpMatch(result) => result.items.clone(),
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
                HeapObject::RegExpMatch(result) => {
                    let values = result
                        .items
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

/// Renders a `console.*` argument list as one observation line.
///
/// The RLM prompt tells a cell to inspect values with `console.log`, so this
/// text is the model's only view of what it just computed. ECMAScript's own
/// string coercion answers `"[object Object]"` for every plain object and
/// comma-joins arrays into an equally opaque line, which is exactly the shape a
/// cell reaches for. Objects and arrays therefore render as JSON — the same
/// body `JSON.stringify` and the host's print projector produce, so the two
/// inspect paths agree — and every other value keeps JavaScript's coercion,
/// which is already the useful answer for numbers (`NaN`, `Infinity`,
/// exponent form), booleans, `null`, `undefined`, dates, regexps and errors.
///
/// This is the single seam that owns console observation text: the lowerer
/// hands over the argument values untouched, and `"" + value`, template
/// literals and `String(value)` keep ECMAScript's answer everywhere else.
pub(super) fn javascript_console_observation_text(
    heap: &Heap,
    values: &[Value],
) -> Result<String, RuntimeError> {
    let mut text = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            push_console_text(&mut text, " ")?;
        }
        write_console_value(heap, value, &mut BTreeSet::new(), 1, true, &mut text)?;
    }
    Ok(text)
}

/// Appends `text` to the observation, refusing the moment the byte budget is
/// exceeded.
///
/// Every write in this walk goes through here, so the refusal lands while the
/// string is still bounded rather than after it has been built. That is not a
/// nicety: the `active` set below closes true cycles but pops on the way out,
/// so a shared object graph — `a = { l: a, r: a }` repeated — re-expands
/// exponentially in the output while staying shallow enough that
/// `ensure_value_depth` never fires. Checking once at the end would let such a
/// value allocate gigabytes before anything refused it. The error is the same
/// `MemoryLimitExceeded` a post-hoc `ensure_javascript_string_size` produced,
/// so callers see no new failure mode; only `attempted` differs, being the
/// first size over the budget rather than the size the walk would have reached.
fn push_console_text(out: &mut String, text: &str) -> Result<(), RuntimeError> {
    ensure_javascript_string_size(out.len() + text.len())?;
    out.push_str(text);
    Ok(())
}

/// Writes one value in observation form.
///
/// The shape is the compact JSON the host's print projector produces, so a
/// TypeScript observation and a Lashlang one describe the same value the same
/// way. Two rules differ from `JSON.stringify`, both because an inspect step
/// must never fail the cell it is describing: a value with no JSON body of its
/// own (a `Map`, a `Date`, a function) keeps its JavaScript string instead of
/// refusing, and a cycle closes with `[Circular]` instead of throwing.
///
/// `depth` is the nesting level of `value` itself, bounded exactly as every
/// other coercion in this file is: the `active` set beside it only closes
/// cycles, and a finite but deeply nested container would otherwise recurse
/// until the thread stack is gone. The bound is the durable boundary's, so a
/// value this refuses could never have been persisted either. The output size
/// is bounded independently, inside the walk, by `push_console_text`: depth
/// alone does not bound a shared graph.
fn write_console_value(
    heap: &Heap,
    value: &Value,
    active: &mut BTreeSet<HeapId>,
    depth: usize,
    top_level: bool,
    out: &mut String,
) -> Result<(), RuntimeError> {
    ensure_value_depth(depth)?;
    match value {
        Value::Null => push_console_text(out, "null")?,
        // Only a bare `console.log(undefined)` can say `undefined`: inside a
        // container JSON has no spelling for it, and the containers below drop
        // or null it exactly as `JSON.stringify` does.
        Value::Undefined => push_console_text(out, if top_level { "undefined" } else { "null" })?,
        Value::Bool(value) => push_console_text(out, if *value { "true" } else { "false" })?,
        Value::Number(_) => push_console_text(out, &javascript_to_string(value))?,
        Value::String(value) => {
            if top_level {
                push_console_text(out, value)?;
            } else {
                write_json_string(value, out)?;
            }
        }
        Value::Image(_) | Value::Resource(_) => push_console_text(
            out,
            &serde_json::to_string(&to_json_direct(value))
                .map_err(|error| js_stdlib_error(format!("console rendering: {error}")))?,
        )?,
        // A projected handle is a host-side view of a value, not an object of
        // its own: describe what is behind it.
        Value::Projected(projected) => {
            write_console_value(
                heap,
                &projected.materialize(),
                active,
                depth,
                top_level,
                out,
            )?;
        }
        Value::List(values) | Value::Tuple(values) => {
            write_console_sequence(heap, values, active, depth, out)?;
        }
        Value::Record(record) => write_console_record(heap, record, active, depth, out)?,
        Value::Ref(id) => {
            if !active.insert(*id) {
                push_console_text(out, "\"[Circular]\"")?;
                return Ok(());
            }
            let result = write_console_heap_object(heap, *id, value, active, depth, top_level, out);
            active.remove(id);
            result?;
        }
    }
    Ok(())
}

fn write_console_heap_object(
    heap: &Heap,
    id: HeapId,
    value: &Value,
    active: &mut BTreeSet<HeapId>,
    depth: usize,
    top_level: bool,
    out: &mut String,
) -> Result<(), RuntimeError> {
    match heap.get(id)? {
        HeapObject::List(values) | HeapObject::Tuple(values) => {
            write_console_sequence(heap, values, active, depth, out)
        }
        HeapObject::RegExpMatch(result) => {
            write_console_sequence(heap, &result.items, active, depth, out)
        }
        HeapObject::Record(record) => write_console_record(heap, record, active, depth, out),
        // Everything else — `Map`, `Set`, `Date`, `RegExp`, `Error`, `URL` — has
        // no JSON body, so its JavaScript string is the most informative text
        // available; it at least names the type the cell has to convert.
        // A function has no JavaScript string at this boundary at all.
        HeapObject::Closure { .. } => {
            let text = "[Function]";
            if top_level {
                push_console_text(out, text)
            } else {
                write_json_string(text, out)
            }
        }
        _ => {
            let text = heap.javascript_to_string(value)?;
            if top_level {
                push_console_text(out, &text)
            } else {
                write_json_string(&text, out)
            }
        }
    }
}

fn write_console_sequence(
    heap: &Heap,
    values: &[Value],
    active: &mut BTreeSet<HeapId>,
    depth: usize,
    out: &mut String,
) -> Result<(), RuntimeError> {
    push_console_text(out, "[")?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            push_console_text(out, ",")?;
        }
        write_console_value(heap, value, active, depth + 1, false, out)?;
    }
    push_console_text(out, "]")?;
    Ok(())
}

fn write_console_record(
    heap: &Heap,
    record: &Record,
    active: &mut BTreeSet<HeapId>,
    depth: usize,
    out: &mut String,
) -> Result<(), RuntimeError> {
    push_console_text(out, "{")?;
    let mut written = 0usize;
    for (key, value) in ecma_record_entries(record) {
        if matches!(value, Value::Undefined) {
            continue;
        }
        if written > 0 {
            push_console_text(out, ",")?;
        }
        write_json_string(key, out)?;
        push_console_text(out, ":")?;
        write_console_value(heap, value, active, depth + 1, false, out)?;
        written += 1;
    }
    push_console_text(out, "}")?;
    Ok(())
}

fn write_json_string(value: &str, out: &mut String) -> Result<(), RuntimeError> {
    push_console_text(
        out,
        &serde_json::to_string(value).expect("strings are JSON strings"),
    )
}
