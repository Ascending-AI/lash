//! Independent recorded native outcomes and charges.

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::{Datum, FunctionRegistry, Measure, NativeError, Operand, Value, WorkCounter};

use lash_kernel_value::FunctionId;
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
                && let Some(prepared) = prepared.answer(&args, true)
            {
                assert_eq!(
                    testsupport::datum(&heap, &prepared.value),
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
                definition
                    .charge()
                    .expect("formula-metered library function")
                    .evaluate(&mut |operand, measure| {
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
                crate::testsupport::program(
                    std::sync::Arc::new(document),
                    lash_kernel_vm::PreparedLibrary::new(std::sync::Arc::new(registry.clone())),
                ),
                lash_kernel_vm::Bounds {
                    memory_meter: lash_kernel_vm::MemoryMeter::On,
                    charge: lash_kernel_vm::Charge::Exact,
                    charge_limit: 10_000_000,
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

/// K-CHG-003/004: a guard's zero/nonzero mask selects exactly the declared
/// completion formula over the recorded operator corpus. Errors keep the
/// generic raise path; no result bit is invented for them.
#[test]
fn prepared_scalar_completion_matches_operator_corpus() {
    let mut registry = FunctionRegistry::new();
    register_numbers(&mut registry).unwrap();
    let mut specialized = 0;
    let mut masks = std::collections::BTreeSet::new();
    for shard in NATIVE_SHARDS {
        let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
        for case in shard["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let id: FunctionId = serde_json::from_value(case["function"].clone()).unwrap();
            let registered = registry.get(&id).unwrap();
            let Some(prepared) = registered.native().unwrap().prepare() else {
                continue;
            };
            let definition = registered.definition();
            let mut heap = testsupport::heap();
            let args: Vec<Value> = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| decode(serde_json::from_value(arg.clone()).unwrap(), &mut heap))
                .collect();
            let Some(answer) = prepared.answer(&args, true) else {
                continue;
            };
            let uncharged = (prepared.entry)(&args).expect("the value-only guard agrees");
            assert_eq!(
                testsupport::datum(&heap, &answer.value),
                testsupport::datum(&heap, &uncharged),
                "{name}: charged/value-only entry"
            );
            let Some(mask) = answer.scalar_sizes else {
                continue;
            };
            let Some(units) =
                lash_kernel_vm::kernel_internals::prepared_scalar_charge(definition, mask)
            else {
                continue;
            };
            let generic = definition
                .charge()
                .expect("formula-metered library function")
                .evaluate(&mut |operand, measure| {
                    let value = match operand {
                        Operand::Result => Some(&answer.value),
                        Operand::Param(name) => definition
                            .signature
                            .params
                            .iter()
                            .position(|param| param.name == *name)
                            .and_then(|index| args.get(index)),
                    };
                    value.map_or(0, |value| match measure {
                        Measure::Size => size(value, &heap),
                        Measure::DeepSize => deep_size(value, &heap),
                        Measure::Magnitude => magnitude(value, &heap),
                        Measure::NestedSize => nested_size(value, &heap),
                    })
                });
            assert_eq!(units, generic, "{name}: guard mask {mask:04b}");
            assert_eq!(
                units,
                case["expected"]["charged"].as_u64().unwrap(),
                "{name}: recorded charge"
            );
            specialized += 1;
            masks.insert(mask);
        }
    }
    assert!(specialized > 0);
    assert!(masks.contains(&0) && masks.iter().any(|mask| mask & 8 != 0));
}

/// K-CHG-003: what a region charges a call from its values alone, where
/// each carries its measurements (an immediate, or a construction of no
/// members), is the declared formula's amount, for every numeric and
/// collection function of at most three parameters over every combination
/// of such operands, `same`'s sum with a minimum included. `same` of two
/// immediates, or of one and an empty construction, takes it every time.
#[test]
fn immediate_charges_are_the_declared_formula() {
    let mut registry = FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry).unwrap();
    register_numbers(&mut registry).unwrap();
    crate::register_collections(&mut registry).unwrap();
    crate::register_text_json(&mut registry).unwrap();
    let mut heap = testsupport::heap();
    let long = "fourteen bytes";
    let mut operands: Vec<(Value, bool)> = [
        Datum::Null,
        Datum::Absent,
        Datum::Bool(true),
        Datum::Int(0.into()),
        Datum::Int(7.into()),
        Datum::Int((-3).into()),
        Datum::Float(lash_kernel_doc::Float::new(2.5)),
        Datum::Text(String::new()),
        Datum::Text("record".into()),
        Datum::Text(long.into()),
    ]
    .into_iter()
    .map(|datum| (decode(datum, &mut heap), false))
    .collect();
    for empty in [
        Datum::Tuple(Vec::new()),
        Datum::List(Vec::new()),
        Datum::Record(Vec::new()),
    ] {
        operands.push((decode(empty, &mut heap), true));
    }
    let results: Vec<Value> = [
        Datum::Bool(false),
        Datum::Int(0.into()),
        Datum::Int(9.into()),
        Datum::Text("text".into()),
    ]
    .into_iter()
    .map(|datum| decode(datum, &mut heap))
    .collect();
    let mut answered = 0u64;
    for (_, registered) in registry.iter() {
        let definition = registered.definition();
        let params = &definition.signature.params;
        if params.len() > 3 {
            continue;
        }
        let same = definition.name.as_str() == "same";
        let combinations = operands.len().pow(params.len() as u32);
        for combination in 0..combinations {
            let mut rest = combination;
            let mut args = Vec::new();
            let mut fresh = 0u16;
            for at in 0..params.len() {
                let (value, empty) = operands[rest % operands.len()];
                rest /= operands.len();
                args.push(value);
                if empty {
                    fresh |= 1 << at;
                }
            }
            for result in &results {
                let fast = lash_kernel_vm::kernel_internals::immediate_charge(
                    definition, &args, fresh, result,
                );
                if same {
                    assert!(fast.is_some(), "same{args:?} takes the immediate charge");
                }
                let Some(fast) = fast else {
                    continue;
                };
                let general = definition
                    .charge()
                    .expect("formula-metered library function")
                    .evaluate(&mut |operand, measure| {
                        let value = match operand {
                            Operand::Result => Some(result),
                            Operand::Param(name) => params
                                .iter()
                                .position(|param| param.name == *name)
                                .and_then(|index| args.get(index)),
                        };
                        value.map_or(0, |value| match measure {
                            Measure::Size => size(value, &heap),
                            Measure::DeepSize => deep_size(value, &heap),
                            Measure::Magnitude => magnitude(value, &heap),
                            Measure::NestedSize => nested_size(value, &heap),
                        })
                    });
                assert_eq!(fast, general, "{}{args:?} -> {result:?}", definition.name);
                answered += 1;
            }
        }
    }
    assert!(answered > 0);
}

/// FIG-6941, K-NUM and K-CHG-003: every published typed kernel is the
/// general native's value and completion charge, including signed zeros,
/// NaNs, infinities, word overflow and remainder's exceptional divisors.
#[test]
fn typed_scalar_kernels_agree_with_general_entries_on_edge_pairs() {
    use lash_kernel_doc::{Float, scalar};

    let floats = [
        f64::NAN,
        -0.0,
        0.0,
        -1.0,
        1.0,
        -2.5,
        2.5,
        f64::NEG_INFINITY,
        f64::INFINITY,
        i64::MIN as f64,
        i64::MAX as f64,
        f64::from_bits((i64::MAX as f64).to_bits() - 1),
        f64::from_bits((i64::MIN as f64).to_bits() + 1),
    ];
    let ints = [
        i64::MIN,
        i64::MIN + 1,
        -2,
        -1,
        0,
        1,
        2,
        i64::MAX - 1,
        i64::MAX,
    ];
    let mut seen = Vec::new();
    for (definition, native) in crate::numbers() {
        let Some(prepared) = native.prepare() else {
            continue;
        };
        let Some(pair) = prepared.pair else { continue };
        for (op, values) in [
            (
                pair.floats,
                floats
                    .iter()
                    .map(|n| Value::Float(Float::new(*n)))
                    .collect::<Vec<_>>(),
            ),
            (
                pair.ints,
                ints.iter().copied().map(Value::Int).collect::<Vec<_>>(),
            ),
        ] {
            let Some(op) = op else { continue };
            if !seen.contains(&op) {
                seen.push(op);
            }
            for a in &values {
                for b in &values {
                    let args = [*a, *b];
                    let typed = match (*a, *b) {
                        (Value::Float(a), Value::Float(b)) => scalar::floats(op, a, b),
                        (Value::Int(a), Value::Int(b)) => scalar::ints(op, a, b),
                        _ => unreachable!(),
                    };
                    let mut heap = testsupport::heap();
                    let general = testsupport::call(
                        &mut heap,
                        native.as_ref(),
                        &args,
                        64 << 20,
                        &mut WorkCounter::new(None),
                    );
                    let context = format!("{} {args:?}", definition.name);
                    let prepared = prepared.answer(&args, true);
                    if let Some(value) = typed {
                        let general = general
                            .as_ref()
                            .unwrap_or_else(|e| panic!("{context}: {e}"));
                        assert_eq!(
                            testsupport::datum(&heap, &value),
                            testsupport::datum(&heap, general),
                            "{context}"
                        );
                        let answer = prepared.as_ref().expect("a typed answer is prepared");
                        assert_eq!(
                            testsupport::datum(&heap, &value),
                            testsupport::datum(&heap, &answer.value),
                            "{context}"
                        );
                        let nonzero = |v| u8::from(matches!(v, Value::Int(n) if n != 0));
                        let mask = nonzero(*a) | nonzero(*b) << 1 | nonzero(value) << 3;
                        assert_eq!(answer.scalar_sizes, Some(mask), "{context}");
                    }
                    // A kernel decline leaves the prepared/general fallback
                    // intact (overflow, zero divisors and non-integral rem).
                    if let Some(answer) = prepared {
                        let general = general.unwrap_or_else(|e| panic!("{context}: {e}"));
                        assert_eq!(
                            testsupport::datum(&heap, &answer.value),
                            testsupport::datum(&heap, &general),
                            "{context}"
                        );
                        let units = lash_kernel_vm::kernel_internals::prepared_scalar_charge(
                            &definition,
                            answer.scalar_sizes.unwrap(),
                        )
                        .unwrap();
                        let generic = definition
                            .charge()
                            .expect("formula-metered library function")
                            .evaluate(&mut |operand, measure| {
                                let value = match operand {
                                    Operand::Result => &general,
                                    Operand::Param(name) => {
                                        &args[definition
                                            .signature
                                            .params
                                            .iter()
                                            .position(|p| p.name == *name)
                                            .unwrap()]
                                    }
                                };
                                match measure {
                                    Measure::Size => size(value, &heap),
                                    Measure::DeepSize => deep_size(value, &heap),
                                    Measure::Magnitude => magnitude(value, &heap),
                                    Measure::NestedSize => nested_size(value, &heap),
                                }
                            });
                        assert_eq!(units, generic, "{context}");
                    }
                }
            }
        }
    }
    assert_eq!(seen.len(), 12, "every scalar operator has a library entry");
}
