//! FIG-7525: declarations cannot omit an occurrence, fresh identity or member.
//! The observer reads installed output data and nursery allocations, separately
//! from the input-only declaration evaluator and its native-view cost adapter.

thread_local! {
    static POWER_SCRATCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn observe_power_state(
    result: &num_bigint::BigInt,
    base: &num_bigint::BigInt,
    exponent: &num_bigint::BigInt,
) {
    let bytes = 3 * Cost::REFERENCE
        + result.bits().div_ceil(8)
        + base.bits().div_ceil(8)
        + exponent.bits().div_ceil(8);
    POWER_SCRATCH.with(|peak| peak.set(peak.get().max(bytes)));
}

pub(crate) fn observe_power_scratch(
    result: &num_bigint::BigInt,
    base: &num_bigint::BigInt,
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
    exponent: &num_bigint::BigInt,
) {
    let product = a * b;
    let bytes = 4 * Cost::REFERENCE
        + result.bits().div_ceil(8)
        + base.bits().div_ceil(8)
        + product.bits().div_ceil(8)
        + exponent.bits().div_ceil(8);
    POWER_SCRATCH.with(|peak| peak.set(peak.get().max(bytes)));
}
use lash_kernel_doc::{
    Datum, FunctionRegistry, Integer, LogicalHeldV1 as Cost, NativeResourceContract as Contract,
    NativeResourceEntity as Entity, NativeResourceEvent as Event, NativeResourceExit as Exit,
    NativeResourceOrigin as Origin,
};

#[path = "native_contract_observer.rs"]
mod observer;

fn check_case(registry: &FunctionRegistry, name: &str, input: Vec<Datum>) -> Option<Contract> {
    let native = registry
        .iter()
        .map(|(_, entry)| entry)
        .find(|entry| entry.name() == name)
        .unwrap();
    observer::check_case(
        &**native.native().unwrap(),
        name,
        input,
        || POWER_SCRATCH.with(|peak| peak.set(0)),
        || POWER_SCRATCH.with(std::cell::Cell::get),
    )
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
    for (base, exponent) in [
        (1, Integer::new(num_bigint::BigInt::from(1) << 1024)),
        (2, Integer::from(0)),
    ] {
        assert!(check_case(&registry, "int.pow", vec![int(base), Datum::Int(exponent)]).is_some());
        checked += 1;
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
