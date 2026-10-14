//! Strict native replay over the actual paged heap and S7 census.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{
    Integer, Measure, NativeCall, NativeError, Operand, PreparedNative, RegisteredFunction, Value,
    WorkCounter,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    ops::ControlFlow,
};
#[path = "probe/fixtures.rs"]
pub(crate) mod fixtures;
#[path = "probe/recording.rs"]
mod recording;
use fixtures::{RecordedObject, RecordedValue, Replay};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub args: Vec<RecordedValue>,
    pub objects: Vec<RecordedObject>,
    pub work: u64,
    pub room: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer {
    pub outcome: String,
    pub spent: u64,
    pub charge: u64,
    pub reservations: Vec<(u64, u64)>,
    pub reserved: u64,
    pub prepared_difference: Option<String>,
}
pub(crate) fn answer(
    registered: &RegisteredFunction,
    input: &Input,
    prepared: Option<PreparedNative>,
) -> Answer {
    let replay =
        Replay::load(input).unwrap_or_else(|error| panic!("invalid native fixture: {error}"));
    answer_loaded(registered, input, prepared, replay)
}
pub(crate) fn answer_loaded(
    registered: &RegisteredFunction,
    input: &Input,
    prepared: Option<PreparedNative>,
    replay: Replay,
) -> Answer {
    let native = registered
        .native
        .as_ref()
        .unwrap_or_else(|| panic!("a recorded entry has no native"));
    let Replay {
        mut heap,
        args,
        identities,
    } = replay;
    let mut counter = WorkCounter::new(Some(input.work));
    let mut instrument = recording::Recording {
        heap: &mut heap,
        log: Vec::new(),
    };
    let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        native.call(NativeCall {
            args: &args,
            heap: &mut instrument,
            counter: &mut counter,
        })
    }));
    let reservations = instrument.log.iter().map(|r| (r.values, r.bytes)).collect();
    let reserved = instrument
        .log
        .iter()
        .filter(|r| r.accepted)
        .fold(0u64, |sum, r| {
            sum.saturating_add(r.values.saturating_mul(16))
                .saturating_add(r.bytes)
        });
    drop(instrument);
    let mut outcome = String::new();
    let result = match called {
        Ok(Ok(value)) => {
            outcome.push_str("ok ");
            write_value(&heap, &identities, &value, &mut outcome);
            value
        }
        Ok(Err(NativeError::Raised(error))) => {
            let _ = write!(
                outcome,
                "raised {:?} {:?} ",
                text(&heap, &error.kind),
                text(&heap, &error.message)
            );
            let data = heap
                .error_data(&error)
                .unwrap_or_else(|error| panic!("native error payload: {error}"));
            write_value(&heap, &identities, &data, &mut outcome);
            Value::Absent
        }
        Ok(Err(NativeError::Guard(guard))) => {
            let _ = write!(outcome, "guard {}", guard.limit);
            Value::Absent
        }
        Ok(Err(NativeError::Memory)) => {
            outcome.push_str("memory");
            Value::Absent
        }
        Ok(Err(NativeError::Heap(error))) => panic!("native replay infrastructure fault: {error}"),
        Err(_) => {
            outcome.push_str("panicked");
            Value::Absent
        }
    };
    let charge = registered
        .definition
        .charge
        .evaluate(&mut |operand, measure| {
            let value = match operand {
                Operand::Result => &result,
                Operand::Param(name) => registered
                    .definition
                    .signature
                    .params
                    .iter()
                    .position(|p| &p.name == name)
                    .and_then(|i| args.get(i))
                    .unwrap_or(&Value::Absent),
            };
            measure_value(&heap, value, measure)
        });
    let prepared_difference = prepared
        .filter(|_| !outcome.starts_with("guard "))
        .and_then(|prepared| {
            let called =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (prepared.entry)(&args)));
            let mut observed = String::new();
            match called {
                Ok(Some(value)) => {
                    observed.push_str("ok ");
                    write_value(&heap, &identities, &value, &mut observed);
                }
                Ok(None) => return None,
                Err(_) => observed.push_str("panicked"),
            };
            (observed != outcome).then_some(observed)
        });
    Answer {
        outcome,
        spent: counter.spent(),
        charge,
        reservations,
        reserved,
        prepared_difference,
    }
}
fn text(heap: &dyn NativeHeap, value: &TextRef) -> String {
    let mut out = String::new();
    heap.payload_visit(
        &Value::Text(value.clone()),
        0..value.summary.utf8_bytes,
        &mut |chunk| {
            let Chunk::Text(chunk) = chunk else {
                panic!("text descriptor yielded octets")
            };
            out.push_str(chunk);
            ControlFlow::Continue(())
        },
    )
    .unwrap_or_else(|error| panic!("native replay text: {error}"));
    out
}
fn integer(heap: &dyn NativeHeap, value: &IntRef) -> Integer {
    match value {
        IntRef::Inline(value) => value.clone(),
        IntRef::Paged { summary, .. } => {
            let mut digits = Vec::new();
            heap.payload_visit(
                &Value::Int(value.clone()),
                0..summary.bits.div_ceil(8),
                &mut |chunk| {
                    let Chunk::Bytes(chunk) = chunk else {
                        panic!("integer descriptor yielded text")
                    };
                    digits.extend_from_slice(chunk);
                    ControlFlow::Continue(())
                },
            )
            .unwrap_or_else(|error| panic!("native replay integer: {error}"));
            lash_kernel_doc::heap_values::integer_from_digits(summary.negative, &digits)
        }
    }
}
fn write_value(
    heap: &dyn NativeHeap,
    ids: &BTreeMap<EntityId, u64>,
    value: &Value,
    out: &mut String,
) {
    out.push_str(&json(heap, ids, value, &mut BTreeSet::new()).to_string());
}
fn json(
    heap: &dyn NativeHeap,
    ids: &BTreeMap<EntityId, u64>,
    value: &Value,
    path: &mut BTreeSet<EntityId>,
) -> serde_json::Value {
    use serde_json::json;
    match value {
        Value::Null => json!("null"),
        Value::Absent => json!("absent"),
        Value::Bool(v) => json!({"bool":v}),
        Value::Int(v) => json!({"int":integer(heap,v)}),
        Value::Float(v) => json!({"float":v}),
        Value::Text(v) => json!({"text":text(heap,v)}),
        Value::Function(v) => json!({"function":text(heap,v)}),
        Value::Bytes(v) => {
            let mut bytes = Vec::new();
            heap.payload_visit(value, 0..v.len, &mut |chunk| {
                let Chunk::Bytes(chunk) = chunk else {
                    panic!("byte descriptor yielded text")
                };
                bytes.extend_from_slice(chunk);
                ControlFlow::Continue(())
            })
            .unwrap_or_else(|e| panic!("native replay bytes: {e}"));
            json!({"bytes":lash_kernel_doc::Bytes::new(bytes)})
        }
        Value::Timestamp(v) => json!({"timestamp":v}),
        Value::Handle(v) => json!({"handle":v}),
        Value::Task(v) => json!({"task":v}),
        Value::Ref(lash_kernel_doc::Identity::Task(v)) => json!({"ref":{"task":v}}),
        Value::Ref(lash_kernel_doc::Identity::Object(v)) => {
            json!({"ref":{"object":ids.get(v).unwrap_or_else(||panic!("unrecorded identity reference"))}})
        }
        Value::Tuple(v) => {
            let mut values = Vec::new();
            heap.tuple_visit(v, &mut |v| {
                values.push(json(heap, ids, v, path));
                ControlFlow::Continue(())
            })
            .unwrap_or_else(|e| panic!("native replay tuple: {e}"));
            json!({"tuple":values})
        }
        Value::Error(v) => {
            let data = heap
                .error_data(v)
                .unwrap_or_else(|e| panic!("native replay error: {e}"));
            json!({"error":{"kind":text(heap,&v.kind),"message":text(heap,&v.message),"data":json(heap,ids,&data,path)}})
        }
        Value::List(id) | Value::Set(id) | Value::Map(id) | Value::Record(id) => {
            if !path.insert(*id) {
                return json!({"cyclic_object":ids.get(id).unwrap_or_else(||panic!("unrecorded cyclic identity"))});
            }
            let mut items = Vec::new();
            let mut entries = Vec::new();
            let mut fields = Vec::new();
            heap.visit(*id, &mut |element| {
                match element {
                    Element::Item(v) => items.push(json(heap, ids, v, path)),
                    Element::Entry { key, value } => {
                        entries.push([json(heap, ids, key, path), json(heap, ids, value, path)])
                    }
                    Element::Field { name, value } => {
                        fields.push((text(heap, name), json(heap, ids, value, path)))
                    }
                };
                ControlFlow::Continue(())
            })
            .unwrap_or_else(|e| panic!("native replay object: {e}"));
            path.remove(id);
            match value {
                Value::List(_) => json!({"list":items}),
                Value::Set(_) => json!({"set":items}),
                Value::Map(_) => json!({"map":entries}),
                _ => json!({"record":fields}),
            }
        }
        Value::Closure(_) => panic!("native corpus contains no closure fixtures"),
    }
}
fn size(heap: &dyn NativeHeap, value: &Value) -> u64 {
    1 + match value {
        Value::Int(IntRef::Inline(v)) => v.bits().div_ceil(64),
        Value::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(64),
        Value::Text(v) => v.summary.utf8_bytes,
        Value::Bytes(v) => v.len,
        Value::Tuple(v) => v.summary.len,
        Value::Error(v) => v.kind.summary.utf8_bytes + v.message.summary.utf8_bytes,
        Value::Map(id) => {
            2 * heap
                .len(*id)
                .unwrap_or_else(|e| panic!("native measure: {e}"))
        }
        Value::List(id) | Value::Set(id) | Value::Record(id) => heap
            .len(*id)
            .unwrap_or_else(|e| panic!("native measure: {e}")),
        _ => 0,
    }
}
fn measure_value(heap: &dyn NativeHeap, value: &Value, measure: Measure) -> u64 {
    match measure {
        Measure::Size => size(heap, value),
        Measure::Magnitude => match value {
            Value::Int(v) => {
                let v = integer(heap, v);
                if v.is_negative() {
                    0
                } else {
                    v.to_string().parse().unwrap_or(u64::MAX)
                }
            }
            Value::Float(v) if v.get().is_finite() && v.get() >= 0.0 => v.get() as u64,
            _ => 0,
        },
        Measure::DeepSize | Measure::NestedSize => {
            let mut pending = vec![value.clone()];
            let mut seen = BTreeSet::new();
            let mut total = 0u64;
            let mut root = true;
            while let Some(value) = pending.pop() {
                let nested = measure == Measure::NestedSize;
                if let Some(id) = value.object() {
                    if nested {
                        if !root {
                            total = total.saturating_add(1)
                        };
                        root = false;
                        continue;
                    }
                    if !seen.insert(id) {
                        continue;
                    }
                    heap.visit(id, &mut |element| {
                        match element {
                            Element::Item(v) | Element::Field { value: v, .. } => {
                                pending.push(v.clone())
                            }
                            Element::Entry { key, value } => {
                                pending.push(key.clone());
                                pending.push(value.clone())
                            }
                        };
                        ControlFlow::Continue(())
                    })
                    .unwrap_or_else(|e| panic!("native measure object: {e}"));
                }
                if !nested || !root {
                    total = total.saturating_add(size(heap, &value));
                }
                root = false;
                match value {
                    Value::Tuple(v) => heap
                        .tuple_visit(&v, &mut |v| {
                            pending.push(v.clone());
                            ControlFlow::Continue(())
                        })
                        .unwrap_or_else(|e| panic!("native measure tuple: {e}")),
                    Value::Error(v) => pending.push(
                        heap.error_data(&v)
                            .unwrap_or_else(|e| panic!("native measure error: {e}")),
                    ),
                    _ => (),
                }
            }
            total
        }
    }
}
