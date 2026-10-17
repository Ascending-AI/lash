//! Independent recorded native outcomes and charges.

use std::collections::HashSet;

use lash_kernel_doc::{
    Datum, EntityId, FunctionId, FunctionRegistry, Measure, NativeCall, NativeError, NativeHeap,
    Object, Operand, Value, WorkCounter,
};
use num_traits::ToPrimitive;

use crate::register_numbers;
use crate::testsupport;
use lash_kernel_doc::heap::IntRef;

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

fn decode(datum: Datum, heap: &mut lash_kernel_state::kernel_internals::PagedHeap) -> Value {
    testsupport::decode(heap, datum)
}

fn magnitude(value: &Value, heap: &dyn NativeHeap) -> u64 {
    match value {
        Value::Int(value) => {
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

fn size(value: &Value, heap: &dyn NativeHeap) -> u64 {
    match value {
        Value::Int(value) => {
            1 + match value {
                IntRef::Inline(i) => i.bits(),
                IntRef::Paged { summary, .. } => summary.bits,
            }
            .div_ceil(64)
        }
        Value::Text(value) => 1 + value.summary.utf8_bytes,
        Value::Bytes(value) => 1 + value.len,
        Value::Tuple(values) => 1 + values.summary.len,
        Value::List(id) | Value::Set(id) | Value::Record(id) => 1 + heap.len(*id).unwrap(),
        Value::Map(id) => 1 + 2 * heap.len(*id).unwrap(),
        Value::Error(value) => {
            1 + (value.kind.summary.utf8_bytes + value.message.summary.utf8_bytes)
        }
        _ => 1,
    }
}

fn nested_size(value: &Value, heap: &dyn NativeHeap) -> u64 {
    let mut held = Vec::new();
    match value {
        Value::Tuple(values) => heap
            .tuple_visit(values, &mut |value| {
                held.push(value.clone());
                std::ops::ControlFlow::Continue(())
            })
            .unwrap(),
        Value::Error(error) => held.push(heap.error_data(error).unwrap()),
        _ => return 0,
    };
    held.into_iter()
        .map(|value| match value {
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => 1,
            _ => size(&value, heap) + nested_size(&value, heap),
        })
        .sum()
}

fn deep_size(value: &Value, heap: &dyn NativeHeap) -> u64 {
    let mut pending = vec![value.clone()];
    let mut seen = HashSet::<EntityId>::new();
    let mut total = 0u64;
    while let Some(value) = pending.pop() {
        if let Some(id) = value.object()
            && !seen.insert(id)
        {
            continue;
        }
        total = total.saturating_add(size(&value, heap));
        match value {
            Value::Tuple(values) => heap
                .tuple_visit(&values, &mut |value| {
                    pending.push(value.clone());
                    std::ops::ControlFlow::Continue(())
                })
                .unwrap(),
            Value::Error(error) => pending.push(heap.error_data(&error).unwrap()),
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
                })
                .unwrap();
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
            let result = registered.native().unwrap().call(NativeCall {
                args: &args,
                heap: &mut heap,
                counter: &mut counter,
            });
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
                let NativeError::Raised(actual) = result.as_ref().unwrap_err() else {
                    panic!("{name}: expected raised error");
                };
                let expected: lash_kernel_doc::ErrorDatum =
                    serde_json::from_value(error.clone()).unwrap();
                assert_eq!(
                    testsupport::datum(&heap, &Value::Error(actual.clone())),
                    Datum::Error(Box::new(expected)),
                    "{name}"
                );
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
            let warm = registered.native().unwrap().call(NativeCall {
                args: &args,
                heap: &mut heap,
                counter: &mut warm_counter,
            });
            match (&warm, &result) {
                (Ok(a), Ok(b)) => assert_eq!(
                    testsupport::datum(&heap, a),
                    testsupport::datum(&heap, b),
                    "{name} cold/warm"
                ),
                (Err(NativeError::Raised(a)), Err(NativeError::Raised(b))) => assert_eq!(
                    testsupport::datum(&heap, &Value::Error(a.clone())),
                    testsupport::datum(&heap, &Value::Error(b.clone())),
                    "{name} cold/warm"
                ),
                (Err(NativeError::Guard(a)), Err(NativeError::Guard(b))) => {
                    assert_eq!(a, b, "{name} cold/warm")
                }
                (Err(NativeError::Memory), Err(NativeError::Memory)) => (),
                (Err(NativeError::Heap(a)), Err(NativeError::Heap(b))) => {
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
    let object = heap.allocate(Object::List(vec![])).unwrap();
    for _ in 0..2 {
        let mut counter = WorkCounter::new(None);
        let value = reference
            .native()
            .unwrap()
            .call(NativeCall {
                args: &[Value::List(object)],
                heap: &mut heap,
                counter: &mut counter,
            })
            .unwrap();
        assert!(
            matches!(value, Value::Ref(lash_kernel_doc::Identity::Object(id)) if id == object),
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
