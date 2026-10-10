//! Independent recorded native outcomes and charges. The same JSON shards
//! can be fed to lash-kernel-conformance's NativeShard loader by an embedder.

use std::collections::HashSet;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorValue, FunctionId, FunctionRegistry, Measure, NativeCall, NativeError, NativeHeap,
    Object, ObjectId, Operand, Value, WorkCounter,
};
use num_traits::ToPrimitive;

use crate::register_numbers;
use crate::tests::Heap;

const NATIVE_SHARDS: &[&str] = &[
    include_str!("../corpus/native/numbers/K-BND-001.json"),
    include_str!("../corpus/native/numbers/K-FN-007.json"),
    include_str!("../corpus/native/numbers/K-LIB-006.json"),
    include_str!("../corpus/native/numbers/K-NUM-001.json"),
    include_str!("../corpus/native/numbers/K-NUM-002.json"),
    include_str!("../corpus/native/numbers/K-NUM-003.json"),
    include_str!("../corpus/native/numbers/K-NUM-004.json"),
    include_str!("../corpus/native/numbers/K-NUM-005.json"),
    include_str!("../corpus/native/numbers/K-NUM-006.json"),
    include_str!("../corpus/native/numbers/K-VAL-001.json"),
    include_str!("../corpus/native/numbers/K-VAL-020.json"),
    include_str!("../corpus/native/numbers/K-VAL-021.json"),
    include_str!("../corpus/native/numbers/K-VAL-022.json"),
    include_str!("../corpus/native/numbers/K-VAL-023.json"),
    include_str!("../corpus/native/numbers/K-VAL-024.json"),
    include_str!("../corpus/native/numbers/K-VAL-025.json"),
    include_str!("../corpus/native/numbers/K-VAL-027.json"),
    include_str!("../corpus/native/numbers/K-VAL-029.json"),
    include_str!("../corpus/native/numbers/K-VAL-030.json"),
    include_str!("../corpus/native/numbers/K-VAL-031.json"),
    include_str!("../corpus/native/numbers/K-VAL-032.json"),
    include_str!("../corpus/native/numbers/K-VAL-033.json"),
];

const DOCUMENT_SHARDS: &[&str] = &[
    include_str!("../corpus/numbers/K-KEY-002.json"),
    include_str!("../corpus/numbers/K-KEY-003.json"),
    include_str!("../corpus/numbers/K-KEY-004.json"),
    include_str!("../corpus/numbers/K-KEY-005.json"),
    include_str!("../corpus/numbers/K-VAL-001.json"),
    include_str!("../corpus/numbers/K-VAL-026.json"),
];

fn decode(datum: Datum, heap: &mut dyn NativeHeap) -> Value {
    match datum {
        Datum::Null => Value::Null,
        Datum::Absent => Value::Absent,
        Datum::Bool(value) => Value::Bool(value),
        Datum::Int(value) => Value::Int(value),
        Datum::Float(value) => Value::Float(value),
        Datum::Text(value) => Value::text(value),
        Datum::Bytes(value) => Value::Bytes(value),
        Datum::Timestamp(value) => Value::Timestamp(value),
        Datum::Tuple(values) => Value::Tuple(
            values
                .into_iter()
                .map(|value| decode(value, heap))
                .collect(),
        ),
        Datum::List(values) => {
            let values = values
                .into_iter()
                .map(|value| decode(value, heap))
                .collect();
            Value::List(heap.allocate(Object::List(values)).unwrap())
        }
        Datum::Set(values) => {
            let values = values
                .into_iter()
                .map(|value| decode(value, heap))
                .collect();
            Value::Set(heap.allocate(Object::Set(values)).unwrap())
        }
        Datum::Map(values) => {
            let values = values
                .into_iter()
                .map(|(key, value)| (decode(key, heap), decode(value, heap)))
                .collect();
            Value::Map(heap.allocate(Object::Map(values)).unwrap())
        }
        Datum::Record(values) => {
            let values = values
                .into_iter()
                .map(|(name, value)| (name, decode(value, heap)))
                .collect();
            Value::Record(heap.allocate(Object::Record(values)).unwrap())
        }
        Datum::Error(value) => Value::Error(Arc::new(ErrorValue {
            kind: value.kind,
            message: value.message,
            data: decode(value.data, heap),
        })),
        Datum::Function(value) => Value::Function(value),
        Datum::Handle(value) => Value::Handle(Arc::new(value)),
        Datum::Number(_) => panic!("recorded cases carry already decoded numbers"),
    }
}

fn magnitude(value: &Value) -> u64 {
    match value {
        Value::Int(value) if !value.is_negative() => value.to_u64().unwrap_or(u64::MAX),
        Value::Float(value) if value.get().is_finite() && value.get() >= 0.0 => value.get() as u64,
        _ => 0,
    }
}

fn size(value: &Value, heap: &dyn NativeHeap) -> u64 {
    match value {
        Value::Int(value) => 1 + value.bits().div_ceil(64),
        Value::Text(value) => 1 + value.len() as u64,
        Value::Bytes(value) => 1 + value.as_slice().len() as u64,
        Value::Tuple(values) => 1 + values.len() as u64,
        Value::List(id) | Value::Set(id) | Value::Record(id) => 1 + heap.len(*id) as u64,
        Value::Map(id) => 1 + 2 * heap.len(*id) as u64,
        Value::Error(value) => 1 + (value.kind.len() + value.message.len()) as u64,
        _ => 1,
    }
}

fn deep_size(value: &Value, heap: &dyn NativeHeap) -> u64 {
    let mut pending = vec![value.clone()];
    let mut seen = HashSet::<ObjectId>::new();
    let mut total = 0u64;
    while let Some(value) = pending.pop() {
        if let Some(id) = value.object()
            && !seen.insert(id)
        {
            continue;
        }
        total = total.saturating_add(size(&value, heap));
        match value {
            Value::Tuple(values) => pending.extend(values.iter().cloned()),
            Value::Error(error) => pending.push(error.data.clone()),
            Value::List(id) | Value::Set(id) | Value::Map(id) | Value::Record(id) => {
                heap.visit(id, &mut |element| {
                    match element {
                        lash_kernel_doc::Element::Item(value)
                        | lash_kernel_doc::Element::Field { value, .. } => {
                            pending.push(value.clone())
                        }
                        lash_kernel_doc::Element::Entry { key, value } => {
                            pending.push(key.clone());
                            pending.push(value.clone());
                        }
                    }
                    std::ops::ControlFlow::Continue(())
                });
            }
            _ => {}
        }
    }
    total
}

#[test]
fn k_lib_006_recorded_native_edges_and_charges_match() {
    let mut registry = FunctionRegistry::new();
    register_numbers(&mut registry).unwrap();
    let mut count = 0;
    for shard in NATIVE_SHARDS {
        let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
        for case in shard["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let function: FunctionId = serde_json::from_value(case["function"].clone()).unwrap();
            let registered = registry
                .get(&function)
                .unwrap_or_else(|| panic!("{name}: missing function {function}"));
            let definition = &registered.definition;
            let mut heap = Heap::default();
            let args: Vec<Value> = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| decode(serde_json::from_value(arg.clone()).unwrap(), &mut heap))
                .collect();
            let limit = definition
                .guard
                .as_ref()
                .map(|guard| guard.limit.evaluate(&mut |_, _| 0));
            let mut counter = WorkCounter::new(limit);
            let result = registered.native.as_ref().unwrap().call(NativeCall {
                args: &args,
                heap: &mut heap,
                counter: &mut counter,
            });
            let outcome = &case["expected"]["outcome"];
            if let Some(datum) = outcome.get("returned") {
                let expected = decode(serde_json::from_value(datum.clone()).unwrap(), &mut heap);
                assert_eq!(result.as_ref().unwrap(), &expected, "{name}");
            } else if let Some(error) = outcome.get("raised") {
                let NativeError::Raised(actual) = result.as_ref().unwrap_err() else {
                    panic!("{name}: expected raised error");
                };
                let expected = decode(
                    Datum::Error(Box::new(serde_json::from_value(error.clone()).unwrap())),
                    &mut heap,
                );
                assert_eq!(Value::Error(Arc::new(actual.clone())), expected, "{name}");
            } else {
                let NativeError::Guard(actual) = result.as_ref().unwrap_err() else {
                    panic!("{name}: expected guard failure");
                };
                assert_eq!(
                    actual.limit,
                    outcome["guard"]["limit"].as_u64().unwrap(),
                    "{name}"
                );
            }
            let charged = if matches!(result, Err(NativeError::Guard(_))) {
                0
            } else {
                definition.charge.evaluate(&mut |operand, measure| {
                    let value = match operand {
                        Operand::Result => result.as_ref().ok(),
                        Operand::Param(name) => definition
                            .signature
                            .params
                            .iter()
                            .position(|param| param.name == *name)
                            .map(|index| &args[index]),
                    };
                    value.map_or(0, |value| match measure {
                        Measure::Size => size(value, &heap),
                        Measure::DeepSize => deep_size(value, &heap),
                        Measure::Magnitude => magnitude(value),
                    })
                })
            };
            assert_eq!(
                charged,
                case["expected"]["charged"].as_u64().unwrap(),
                "{name} charge"
            );
            assert_eq!(
                counter.spent(),
                case["expected"]["work"].as_u64().unwrap(),
                "{name} work"
            );
            count += 1;
        }
    }
    println!("recorded native rule cases executed: {count}");
}

#[test]
fn k_lib_009_document_corpus_names_registered_definitions() {
    let mut registry = FunctionRegistry::new();
    register_numbers(&mut registry).unwrap();
    for shard in DOCUMENT_SHARDS {
        let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
        for case in shard["cases"].as_array().unwrap() {
            let document =
                lash_kernel_doc::parse_document(case["document"].as_str().unwrap()).unwrap();
            lash_kernel_doc::validate_document(&document, &registry).unwrap();
        }
    }
}
