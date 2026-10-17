//! Independent recorded native outcomes and charges.

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::{
    Datum, FunctionId, FunctionRegistry, Measure, NativeError, Operand, Value, WorkCounter,
};
use num_traits::ToPrimitive;

use crate::register_numbers;
use crate::testsupport::{self, Bench};

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

fn decode(datum: Datum, heap: &mut Bench) -> Value {
    testsupport::decode(heap, datum)
}

fn magnitude(value: &Value, heap: &Bench) -> u64 {
    match value {
        Value::Int(_) | Value::BigInt(_) => {
            let value = testsupport::integer(heap, value);
            if value.is_negative() {
                0
            } else {
                value.to_u64().unwrap_or(u64::MAX)
            }
        }
        Value::Float(value) if value.get().is_finite() && value.get() >= 0.0 => value.get() as u64,
        _ => 0,
    }
}

fn len(handle: ArenaHandle, heap: &Bench) -> u64 {
    match heap.arena().body(handle).unwrap() {
        Body::List(sequence) | Body::Tuple(sequence) => sequence.len,
        Body::Map(table) | Body::Set(table) | Body::Record(table) => table.len,
        _ => panic!("a collection has another body"),
    }
}

fn error_parts(handle: ArenaHandle, heap: &Bench) -> (Value, Value, Value) {
    let Body::Error(error) = heap.arena().body(handle).unwrap() else {
        panic!("an error has another body")
    };
    (error.kind, error.message, error.data)
}

fn size(value: &Value, heap: &Bench) -> u64 {
    match value {
        Value::Int(_) | Value::BigInt(_) => {
            1 + testsupport::integer(heap, value).bits().div_ceil(64)
        }
        Value::Text(_) | Value::LongText(_) => 1 + testsupport::text(heap, value).len() as u64,
        Value::Bytes(_) => {
            1 + heap
                .arena()
                .semantics()
                .bytes(value.handle().unwrap())
                .unwrap()
                .len() as u64
        }
        Value::Tuple(values) => 1 + len(*values, heap),
        Value::List(id) | Value::Set(id) | Value::Record(id) => 1 + len(*id, heap),
        Value::Map(id) => 1 + 2 * len(*id, heap),
        Value::Error(error) => {
            let (kind, message, _) = error_parts(*error, heap);
            1 + (testsupport::text(heap, &kind).len() + testsupport::text(heap, &message).len())
                as u64
        }
        _ => 1,
    }
}

fn members(value: &Value, heap: &Bench) -> Vec<Value> {
    let semantics = heap.arena().semantics();
    match value {
        Value::Tuple(id) | Value::List(id) => semantics.sequence(*id).unwrap().to_vec(),
        Value::Set(id) => semantics
            .entries(*id)
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| pair[0])
            .collect(),
        Value::Map(id) => semantics.entries(*id).unwrap().to_vec(),
        Value::Record(id) => semantics
            .entries(*id)
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| pair[1])
            .collect(),
        Value::Error(error) => vec![error_parts(*error, heap).2],
        _ => Vec::new(),
    }
}

fn nested_size(value: &Value, heap: &Bench) -> u64 {
    if !matches!(value, Value::Tuple(_) | Value::Error(_)) {
        return 0;
    }
    members(value, heap)
        .into_iter()
        .map(|value| match value {
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => 1,
            _ => size(&value, heap) + nested_size(&value, heap),
        })
        .sum()
}

fn deep_size(value: &Value, heap: &Bench) -> u64 {
    let mut pending = vec![*value];
    let mut seen = Vec::<ArenaHandle>::new();
    let mut total = 0u64;
    while let Some(value) = pending.pop() {
        if let Value::List(id) | Value::Set(id) | Value::Map(id) | Value::Record(id) = value {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
        }
        total = total.saturating_add(size(&value, heap));
        pending.extend(members(&value, heap));
    }
    total
}

#[test]
fn k_lib_006_recorded_native_edges_and_charges_match() {
    let mut registry = FunctionRegistry::new();
    register_numbers(&mut registry).unwrap();
    let mut count = 0;
    let mut exercised = std::collections::BTreeSet::new();
    for shard in NATIVE_SHARDS {
        let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
        for case in shard["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let function: FunctionId = serde_json::from_value(case["function"].clone()).unwrap();
            exercised.insert(function);
            let registered = registry
                .get(&function)
                .unwrap_or_else(|| panic!("{name}: missing function {function}"));
            let definition = registered.definition();
            let mut heap = testsupport::heap();
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
            let result = testsupport::call(
                &mut heap,
                &**registered.native().unwrap(),
                &args,
                u64::MAX,
                &mut counter,
            );
            if let Some(prepared) = registered.native().unwrap().prepare()
                && let Some(prepared) = (prepared.entry)(&args)
            {
                assert_eq!(
                    testsupport::datum(&heap, &prepared),
                    testsupport::datum(&heap, result.as_ref().unwrap()),
                    "{name} prepared/general"
                );
            }
            let outcome = &case["expected"]["outcome"];
            if let Some(datum) = outcome.get("returned") {
                let expected: Datum = serde_json::from_value(datum.clone()).unwrap();
                assert_eq!(
                    testsupport::datum(&heap, result.as_ref().unwrap()),
                    expected,
                    "{name}"
                );
            } else if let Some(error) = outcome.get("raised") {
                let Some(actual) = testsupport::error_datum(&heap, result.as_ref().unwrap_err())
                else {
                    panic!("{name}: expected raised error");
                };
                let expected: lash_kernel_doc::ErrorDatum =
                    serde_json::from_value(error.clone()).unwrap();
                assert_eq!(actual, Datum::Error(Box::new(expected)), "{name}");
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
                        Measure::Magnitude => magnitude(value, &heap),
                        Measure::NestedSize => nested_size(value, &heap),
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
            let mut warm_counter = WorkCounter::new(limit);
            let warm = testsupport::call(
                &mut heap,
                &**registered.native().unwrap(),
                &args,
                u64::MAX,
                &mut warm_counter,
            );
            match (&warm, &result) {
                (Ok(a), Ok(b)) => assert_eq!(
                    testsupport::datum(&heap, a),
                    testsupport::datum(&heap, b),
                    "{name} cold/warm"
                ),
                (
                    Err(a @ (NativeError::Raised(_) | NativeError::Refused { .. })),
                    Err(b @ (NativeError::Raised(_) | NativeError::Refused { .. })),
                ) => assert_eq!(
                    testsupport::error_datum(&heap, a),
                    testsupport::error_datum(&heap, b),
                    "{name} cold/warm"
                ),
                (Err(NativeError::Guard(a)), Err(NativeError::Guard(b))) => {
                    assert_eq!(a, b, "{name} cold/warm")
                }
                (Err(NativeError::Retry(a)), Err(NativeError::Retry(b))) => {
                    assert_eq!(a, b, "{name} cold/warm")
                }
                _ => panic!("{name} cold/warm outcome differs"),
            }
            assert_eq!(
                warm_counter.spent(),
                counter.spent(),
                "{name} cold/warm work"
            );
            count += 1;
        }
    }
    // `ref` is an identity value, not a serializable Datum. Its independent
    // named case derives the answer from the object allocated as its input.
    let (id, reference) = registry
        .iter()
        .find(|(_, function)| function.definition().name.as_str() == "ref")
        .unwrap();
    let mut heap = testsupport::heap();
    let object = testsupport::decode(&mut heap, Datum::List(Vec::new()));
    let Value::List(handle) = object else {
        panic!("a list")
    };
    for _ in 0..2 {
        let mut counter = WorkCounter::new(None);
        let value = testsupport::call(
            &mut heap,
            &**reference.native().unwrap(),
            &[object],
            u64::MAX,
            &mut counter,
        )
        .unwrap();
        assert!(
            matches!(value, Value::ObjectRef(id) if id == handle),
            "ref object identity cold/warm"
        );
        assert_eq!(counter.spent(), 0);
    }
    exercised.insert(*id);
    count += 1;
    let missing: Vec<_> = registry
        .iter()
        .filter(|(id, _)| !exercised.contains(id))
        .map(|(_, function)| function.definition().name.as_str())
        .collect();
    assert_eq!(
        exercised.len(),
        registry.len(),
        "every numeric native needs a named corpus case: {missing:?}"
    );
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
            let mut machine = testsupport::start(
                lash_kernel_vm::Program {
                    release: lash_kernel_doc::heap::PageId([8; 32]),
                    document: std::sync::Arc::new(document),
                    library: lash_kernel_vm::PreparedLibrary::new(std::sync::Arc::new(
                        registry.clone(),
                    )),
                },
                lash_kernel_vm::Bounds {
                    charge: 10_000_000,
                    memory: 64 << 20,
                    call_depth: 100,
                    live_tasks: 100,
                    requests_per_park: 100,
                    join_members: 100,
                },
                lash_kernel_vm::kernel_internals::Layout::default(),
            )
            .unwrap();
            let mut world = testsupport::World::default();
            let lash_kernel_vm::Step::Ended(lash_kernel_vm::End::Finished(done)) =
                lash_kernel_vm::Machine::run(&mut machine, &mut world, u64::MAX).unwrap()
            else {
                panic!("self-contained document corpus did not finish")
            };
            let expected: Datum =
                serde_json::from_value(case["expected"]["end"]["finished"].clone()).unwrap();
            let prints: Vec<Datum> =
                serde_json::from_value(case["expected"]["prints"].clone()).unwrap();
            assert_eq!(done.result, expected, "{}", case["name"]);
            assert_eq!(world.prints, prints, "{}", case["name"]);
        }
    }
}
