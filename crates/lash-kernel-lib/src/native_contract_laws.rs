//! FIG-7525: declarations cannot omit an occurrence, fresh identity or member.
//! The observer reads installed output data and nursery allocations, separately
//! from the input-only declaration evaluator and its native-view cost adapter.
use crate::testsupport::{Bench, decode, heap};

thread_local! {
    static POWER_SCRATCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn observe_power_scratch(
    result: &num_bigint::BigInt,
    base: &num_bigint::BigInt,
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
) {
    let product = a * b;
    let bytes = 3 * Cost::REFERENCE
        + result.bits().div_ceil(8)
        + base.bits().div_ceil(8)
        + product.bits().div_ceil(8);
    POWER_SCRATCH.with(|peak| peak.set(peak.get().max(bytes)));
}
use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionRegistry, Integer, LogicalHeldV1 as Cost, NativeCall, NativeError,
    NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceOrigin as Origin,
    NewObject, Value, WorkCounter,
};

fn rechunk(bench: &mut Bench, roots: &[Value], split: bool) {
    use lash_kernel_doc::arena::{Body, Chunk, Chunks, Sequence};
    let mut pending = roots.to_vec();
    let mut seen = Vec::new();
    while let Some(value) = pending.pop() {
        let Some(handle) = value.handle() else {
            continue;
        };
        if seen.contains(&handle) {
            continue;
        }
        seen.push(handle);
        let (body, _) = bench.arena_mut().parts_mut(handle).unwrap();
        match body {
            Body::List(sequence) | Body::Tuple(sequence) => {
                let values: Vec<_> = sequence
                    .chunks
                    .iter()
                    .flat_map(|chunk| match chunk {
                        Chunk::Resident(values) => values,
                        _ => panic!("new input is resident"),
                    })
                    .copied()
                    .collect();
                pending.extend_from_slice(&values);
                sequence.chunks = if values.is_empty() || !split {
                    Chunks::whole(values)
                } else {
                    Chunks::Split(
                        values
                            .chunks(Sequence::CHUNK)
                            .map(|chunk| Chunk::Resident(chunk.to_vec()))
                            .collect(),
                    )
                };
            }
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                for chunk in table.chunks.iter() {
                    match chunk {
                        Chunk::Resident(values) => pending.extend_from_slice(values),
                        _ => panic!("new input is resident"),
                    }
                }
            }
            Body::LongText(text) => {
                let whole: String = text
                    .chunks
                    .iter()
                    .map(|chunk| match chunk {
                        Chunk::Resident(text) => text.as_str(),
                        _ => panic!("new input is resident"),
                    })
                    .collect();
                let chars: Vec<_> = whole.chars().collect();
                text.chunks = if split {
                    chars
                        .chunks(64)
                        .map(|chunk| Chunk::Resident(chunk.iter().collect()))
                        .collect()
                } else {
                    vec![Chunk::Resident(whole)].into_boxed_slice()
                };
            }
            Body::Error(error) => pending.extend([error.kind, error.message, error.data]),
            _ => {}
        }
    }
}

fn local(value: &Datum) -> u64 {
    Cost::REFERENCE
        + match value {
            Datum::Int(value) => value.bits().div_ceil(8),
            Datum::Text(value) => value.len() as u64,
            Datum::Bytes(value) => value.as_slice().len() as u64,
            Datum::Timestamp(value) => value.nanoseconds.bits().div_ceil(8),
            Datum::Tuple(values) => values.iter().map(local).sum(),
            Datum::Error(error) => {
                (error.kind.len() + error.message.len()) as u64 + local(&error.data)
            }
            Datum::Handle(handle) => (handle.kind.len() + handle.id.len()) as u64,
            _ => 0,
        }
}

fn value_cost(bench: &Bench, value: &Value) -> u64 {
    match value {
        Value::Function(_) => Cost::REFERENCE + 64,
        Value::ObjectRef(_) | Value::TaskRef(_) | Value::Closure(_) => Cost::REFERENCE,
        _ => local(&bench.datum(value).unwrap()),
    }
}

fn check_case(registry: &FunctionRegistry, name: &str, input: Vec<Datum>) -> Option<Contract> {
    let registered = registry
        .iter()
        .map(|(_, entry)| entry)
        .find(|entry| entry.definition().name.as_str() == name)
        .unwrap();
    let native = &**registered.native().unwrap();
    let mut bench = heap();
    let values: Vec<_> = input
        .into_iter()
        .map(|value| decode(&mut bench, value))
        .collect();
    let (prediction, _) = bench
        .run(&values, u64::MAX, |args, _| native.resource_contract(args))
        .unwrap();
    let contract = match prediction {
        Ok(Some(contract)) => contract,
        Ok(None) | Err(NativeError::Decline) => return None,
        Err(error) => panic!("{name}: prediction {error:?}"),
    };
    rechunk(&mut bench, &values, true);
    let (chunked, _) = bench
        .run(&values, u64::MAX, |args, _| native.resource_contract(args))
        .unwrap();
    match chunked {
        Ok(chunked) => assert_eq!(chunked, Some(contract.clone()), "{name} chunk geometry"),
        Err(NativeError::Retry(lash_kernel_doc::Retry::Cold { .. })) => {
            // Equality can request a flat read even for resident chunks.
            rechunk(&mut bench, &values, false);
            let (warmed, _) = bench
                .run(&values, u64::MAX, |args, _| native.resource_contract(args))
                .unwrap();
            assert_eq!(
                warmed.unwrap(),
                Some(contract.clone()),
                "{name} after read retry"
            );
        }
        Err(error) => panic!("{name} chunked prediction: {error:?}"),
    }
    POWER_SCRATCH.with(|peak| peak.set(0));
    let ((answer, built), _) = bench
        .run(&values, u64::MAX, |args, new| {
            let answer = native.call(NativeCall {
                args,
                new,
                counter: &mut WorkCounter::new(None),
            });
            (answer, new.built().to_vec())
        })
        .unwrap();
    // The data decoder needs whole views. Normalize only after execution,
    // without changing values or identities, before reading its oracle.
    rechunk(&mut bench, &values, false);
    let (exit, output, output_value) = match &answer {
        Ok(value) => (Exit::Returned, value_cost(&bench, value), Some(value)),
        Err(NativeError::Raised(value)) => (Exit::Raised, value_cost(&bench, value), Some(value)),
        Err(NativeError::Refused { kind, message }) => (
            Exit::Raised,
            local(&Datum::Error(Box::new(ErrorDatum {
                kind: (*kind).into(),
                message: message.clone(),
                data: Datum::Null,
            }))),
            None,
        ),
        Err(error) => panic!("{name}: execution {error:?}"),
    };
    assert_eq!(contract.exit, exit, "{name} exit");
    assert_eq!(contract.output.bytes, output, "{name} output occurrence");
    let mut actual = Vec::new();
    for (handle, object) in &built {
        let (kind, members) = match object {
            NewObject::List(values) => (Entity::List, values.clone()),
            NewObject::Map(pairs) => (
                Entity::Map,
                pairs
                    .iter()
                    .flat_map(|(key, value)| [*key, *value])
                    .collect(),
            ),
            NewObject::Set(values) => (
                Entity::Set,
                values
                    .iter()
                    .flat_map(|value| [*value, Value::Absent])
                    .collect(),
            ),
            NewObject::Record(fields) => (
                Entity::Record {
                    field_name_bytes: fields
                        .iter()
                        .map(|(key, _)| match bench.datum(key).unwrap() {
                            Datum::Text(text) => text.len() as u64,
                            _ => panic!("record key"),
                        })
                        .sum(),
                },
                fields.iter().map(|(_, value)| *value).collect(),
            ),
            _ => continue,
        };
        actual.push((*handle, kind, members));
    }
    let events: Vec<_> = contract
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Entity { kind, members, .. } => Some((kind, members)),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), actual.len(), "{name} fresh identities");
    let mut retained = output;
    for ((expected_kind, expected_members), (_, kind, members)) in events.iter().zip(&actual) {
        assert_eq!(**expected_kind, *kind, "{name} entity kind/names");
        assert_eq!(
            expected_members.len(),
            members.len(),
            "{name} direct members"
        );
        let bytes: u64 = members.iter().map(|value| value_cost(&bench, value)).sum();
        for (expected, value) in expected_members.iter().zip(members) {
            assert_eq!(
                expected.bytes,
                value_cost(&bench, value),
                "{name} member occurrence"
            );
            let mutable = matches!(
                value,
                Value::List(_)
                    | Value::Map(_)
                    | Value::Set(_)
                    | Value::Record(_)
                    | Value::Closure(_)
            );
            let fresh = actual
                .iter()
                .any(|(handle, _, _)| Some(*handle) == value.handle());
            assert_eq!(
                matches!(expected.origin, Origin::Alias),
                mutable && !fresh,
                "{name} member alias"
            );
            assert_eq!(
                matches!(expected.origin, Origin::Fresh(_)),
                mutable && fresh,
                "{name} member identity"
            );
        }
        retained += Cost::object(bytes);
        if let Entity::Record { field_name_bytes } = kind {
            retained += field_name_bytes;
        }
    }
    assert_eq!(
        contract.retained_bytes(),
        Some(retained),
        "{name} retained holdings"
    );
    if let Some(value) = output_value {
        let fresh = actual
            .iter()
            .any(|(handle, _, _)| Some(*handle) == value.handle());
        let mutable = matches!(
            value,
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) | Value::Closure(_)
        );
        assert_eq!(
            matches!(contract.output.origin, Origin::Fresh(_)),
            fresh,
            "{name} output identity"
        );
        assert_eq!(
            matches!(contract.output.origin, Origin::Alias),
            mutable && !fresh,
            "{name} output alias"
        );
    }
    let actual_scratch = POWER_SCRATCH.with(std::cell::Cell::get);
    assert!(
        actual_scratch <= contract.peak_scratch_bytes().unwrap(),
        "{name}: actual scratch {actual_scratch} exceeds {contract:?}"
    );
    Some(contract)
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}
fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

/// Omitted immutable payloads, fresh bodies and record-key payloads must fail
/// even when physical reservations overestimate them or inputs are chunked.
#[test]
fn native_contracts_match_logical_holdings_and_reject_understatements() {
    let mut registry = FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry).unwrap();
    crate::register_numbers(&mut registry).unwrap();
    crate::register_collections(&mut registry).unwrap();
    crate::register_text_json(&mut registry).unwrap();
    crate::register_brands(&mut registry).unwrap();
    let mut checked = 0;
    let mut uncertified = std::collections::BTreeSet::new();
    for shard in crate::corpus_tests::NATIVE_SHARDS {
        let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
        for case in shard["cases"].as_array().unwrap() {
            if case["expected"]["outcome"].get("guard").is_some() {
                continue;
            }
            let function = serde_json::from_value(case["function"].clone()).unwrap();
            let name = registry
                .get(&function)
                .unwrap()
                .definition()
                .name
                .to_string();
            let input = serde_json::from_value(case["args"].clone()).unwrap();
            if check_case(&registry, &name, input).is_some() {
                checked += 1;
            } else {
                uncertified.insert(name);
            }
        }
    }
    for size in [0, 1, 2, 17, 65, 257] {
        let payload = "a😀z".repeat(size);
        let member = Datum::Tuple(vec![text(&payload), Datum::List(vec![int(7)])]);
        let items: Vec<_> = (0..size).map(|_| member.clone()).collect();
        for (prefix, xs, key) in [
            ("list", Datum::List(items.clone()), member.clone()),
            ("tuple", Datum::Tuple(items.clone()), member.clone()),
            (
                "map",
                Datum::Map(
                    (0..size)
                        .map(|at| (text(&at.to_string()), member.clone()))
                        .collect(),
                ),
                text("0"),
            ),
            (
                "set",
                Datum::Set((0..size).map(|at| text(&at.to_string())).collect()),
                text("0"),
            ),
            (
                "record",
                Datum::Record(
                    (0..size)
                        .map(|at| (at.to_string(), member.clone()))
                        .collect(),
                ),
                text("0"),
            ),
        ] {
            let names: Vec<_> = registry
                .iter()
                .map(|(_, entry)| entry.definition().name.to_string())
                .filter(|name| name.starts_with(&format!("{prefix}.")))
                .collect();
            for name in names {
                let op = name.split_once('.').unwrap().1;
                let input = match op {
                    "len" | "check" | "copy" | "keys" | "values" | "entries" => vec![xs.clone()],
                    "slice" => vec![xs.clone(), int(0), int(size as i64)],
                    "concat" => vec![xs.clone(), xs.clone()],
                    "get" | "at" | "get_or" | "insert_index" => vec![
                        xs.clone(),
                        if prefix == "map" || prefix == "record" {
                            text("0")
                        } else {
                            int(0)
                        },
                    ],
                    "contains" | "index_of" => vec![xs.clone(), key.clone()],
                    "with" => vec![xs.clone(), text("field"), member.clone()],
                    "without" => vec![xs.clone(), text("0")],
                    _ => continue,
                };
                assert!(
                    check_case(&registry, &name, input).is_some(),
                    "{name} missing contract"
                );
                checked += 1;
                assert!(check_case(&registry, &name, vec![Datum::Null]).is_some());
                checked += 1;
            }
        }
        for (name, args) in [
            (
                "text.join",
                vec![Datum::List(vec![text(&payload); size]), text("λ")],
            ),
            (
                "text.join_or_index",
                vec![Datum::List(vec![text(&payload), Datum::Null]), int(0)],
            ),
            ("text.join", vec![Datum::List(vec![Datum::Null]), text(",")]),
            ("text.concat", vec![text(&payload), text("λ")]),
            ("text.len", vec![text(&payload)]),
            ("text.utf16_len", vec![text(&payload)]),
            (
                "error.new",
                vec![text("kind"), text(&payload), member.clone()],
            ),
            ("error.new", vec![int(0)]),
            (
                "brand.of",
                vec![Datum::Record(vec![(
                    "__lash_brand".into(),
                    Datum::Tuple(vec![text(&payload), text("name")]),
                )])],
            ),
        ] {
            assert!(
                check_case(&registry, name, args).is_some(),
                "{name} missing contract"
            );
            checked += 1;
        }
    }
    eprintln!(
        "checked {checked} native input/exit contracts; uncertified corpus leaves: {uncertified:?}"
    );
    // These leases do not overlap: release happens before acquisition at site 1.
    use lash_kernel_doc::{NativeResourceOwner as Owner, NativeResourceRelease as Release};
    let mut leases = Contract::value(Exit::Returned, Cost::REFERENCE, Origin::Immutable);
    leases.events = vec![
        Event::Scratch {
            owner: Owner(0),
            bytes: 17,
            release: Release::Before(Owner(1)),
        },
        Event::Scratch {
            owner: Owner(1),
            bytes: 19,
            release: Release::Exit,
        },
    ];
    assert_eq!(leases.peak_scratch_bytes(), Some(19));
    assert_eq!(leases.retained_bytes(), Some(Cost::REFERENCE));
    leases.events[0] = Event::Scratch {
        owner: Owner(0),
        bytes: 17,
        release: Release::Exit,
    };
    assert_eq!(leases.peak_scratch_bytes(), Some(36));
    leases.events[0] = Event::Scratch {
        owner: Owner(0),
        bytes: 17,
        release: Release::Before(Owner(9)),
    };
    assert_eq!(leases.peak_scratch_bytes(), None);
    leases.events.clear();
    leases.output.origin = Origin::Fresh(Owner(9));
    assert_eq!(leases.retained_bytes(), None);
    leases.output.origin = Origin::Immutable;
    leases.events = vec![
        Event::Entity {
            owner: Owner(0),
            kind: Entity::List,
            release: Some(Release::Before(Owner(1))),
            members: vec![],
        },
        Event::Scratch {
            owner: Owner(1),
            bytes: 19,
            release: Release::Exit,
        },
    ];
    assert_eq!(leases.retained_bytes(), Some(Cost::REFERENCE));
    assert_eq!(
        leases.peak_extra_bytes(),
        Some(Cost::REFERENCE + Cost::OBJECT)
    );
    leases.output.origin = Origin::Fresh(Owner(0));
    assert_eq!(leases.retained_bytes(), None);
}
