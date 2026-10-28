//! FIG-7525: declarations cannot omit an occurrence, fresh identity or member.
//! The observer reads installed output data and nursery allocations, separately
//! from the input-only declaration evaluator and its native-view cost adapter.
use crate::testsupport::{Bench, decode, heap};

thread_local! {
    static OBSERVED_NAMES: std::cell::RefCell<std::collections::BTreeSet<String>> = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
    static SORTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LIVE_SCRATCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static POWER_SCRATCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Test-only leases observe live semantic buffers at their actual construction
/// sites. They never read a declaration or Nursery reservation.
pub(crate) struct SortingObservation(bool);
impl Drop for SortingObservation {
    fn drop(&mut self) {
        SORTING.with(|flag| flag.set(self.0));
    }
}
pub(crate) fn sorting() -> SortingObservation {
    SortingObservation(SORTING.with(|flag| flag.replace(true)))
}
pub(crate) fn ordering_scratch(bytes: u64) -> ScratchLease {
    scratch(if SORTING.with(std::cell::Cell::get) {
        bytes
    } else {
        0
    })
}
pub(crate) fn scratch_indices(indices: &[usize]) -> ScratchLease {
    scratch(
        indices
            .iter()
            .map(|n| 16 + u64::from(usize::BITS - n.leading_zeros()).div_ceil(8))
            .sum(),
    )
}
pub(crate) struct CachedKey {
    value: i64,
    _holding: ScratchLease,
}
impl CachedKey {
    pub(crate) fn new(value: i64, index: usize) -> Self {
        // The cached pair owns a key occurrence and an index occurrence.
        let key = 16 + u64::from(u64::BITS - value.unsigned_abs().leading_zeros()).div_ceil(8);
        let index = 16 + u64::from(usize::BITS - index.leading_zeros()).div_ceil(8);
        Self {
            value,
            _holding: scratch(16 + key + index),
        }
    }
}
impl PartialEq for CachedKey {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl Eq for CachedKey {}
impl PartialOrd for CachedKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CachedKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}
pub(crate) struct ScratchLease(u64);
impl ScratchLease {
    pub(crate) fn grow(&mut self, bytes: u64) {
        self.resize(self.0.checked_add(bytes).unwrap());
    }
    pub(crate) fn resize(&mut self, bytes: u64) {
        LIVE_SCRATCH.with(|live| {
            let n = live.get() - self.0 + bytes;
            live.set(n);
            POWER_SCRATCH.with(|peak| peak.set(peak.get().max(n)));
        });
        self.0 = bytes;
    }
}
impl Drop for ScratchLease {
    fn drop(&mut self) {
        LIVE_SCRATCH.with(|live| live.set(live.get() - self.0));
    }
}
pub(crate) fn scratch(bytes: u64) -> ScratchLease {
    LIVE_SCRATCH.with(|live| {
        let bytes = live.get().checked_add(bytes).unwrap();
        live.set(bytes);
        POWER_SCRATCH.with(|peak| peak.set(peak.get().max(bytes)));
    });
    ScratchLease(bytes)
}
fn observed_local(args: lash_kernel_doc::Args<'_>, value: &Value) -> u64 {
    Cost::REFERENCE
        + match value {
            Value::Int(n) => n
                .unsigned_abs()
                .checked_ilog2()
                .map_or(0, |b| u64::from(b + 1).div_ceil(8)),
            Value::BigInt(_) => args.integer(value).unwrap().unwrap().bits().div_ceil(8),
            Value::Timestamp(n) => n
                .unsigned_abs()
                .checked_ilog2()
                .map_or(0, |b| u64::from(b + 1).div_ceil(8)),
            Value::BigTimestamp(_) => args.timestamp(value).unwrap().unwrap().bits().div_ceil(8),
            Value::Text(_) | Value::LongText(_) => args.text(value).unwrap().unwrap().len(),
            Value::Bytes(_) => args.bytes(value).unwrap().unwrap().len(),
            Value::Tuple(_) => args
                .sequence(value)
                .unwrap()
                .unwrap()
                .chunks()
                .map(|c| {
                    c.unwrap()
                        .iter()
                        .map(|v| observed_local(args, v))
                        .sum::<u64>()
                })
                .sum(),
            Value::Error(_) => {
                let e = args.error(value).unwrap().unwrap();
                args.text(&e.kind).unwrap().unwrap().len()
                    + args.text(&e.message).unwrap().unwrap().len()
                    + observed_local(args, &e.data)
            }
            Value::Function(_) => 64,
            Value::Handle(_) => {
                let h = args.host_handle(value).unwrap().unwrap();
                (h.kind.len() + h.id.len()) as u64
            }
            _ => 0,
        }
}

pub(crate) fn scratch_values(args: lash_kernel_doc::Args<'_>, values: &[Value]) -> ScratchLease {
    scratch(values.iter().map(|value| observed_local(args, value)).sum())
}
pub(crate) fn new_values_bytes(
    args: lash_kernel_doc::Args<'_>,
    new: &lash_kernel_doc::Nursery<'_>,
    values: &[Value],
) -> u64 {
    fn one(
        args: lash_kernel_doc::Args<'_>,
        new: &lash_kernel_doc::Nursery<'_>,
        value: &Value,
    ) -> u64 {
        if matches!(
            value,
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) | Value::Closure(_)
        ) {
            return 16;
        }
        let built = value
            .handle()
            .and_then(|handle| new.built().iter().find(|(id, _)| *id == handle));
        let Some((_, object)) = built else {
            return observed_local(args, value);
        };
        match object {
            NewObject::Tuple(values) => 16 + new_values_bytes(args, new, values),
            NewObject::Error {
                kind,
                message,
                data,
            } => new_values_bytes(args, new, &[*kind, *message, *data]) - 16,
            NewObject::LongText(text) => 16 + text.len() as u64,
            NewObject::Bytes(bytes) => 16 + bytes.len() as u64,
            NewObject::BigInt(n) | NewObject::Timestamp(n) => 16 + n.bits().div_ceil(8),
            NewObject::Function(_) => 16 + 64,
            _ => 16,
        }
    }
    values.iter().map(|value| one(args, new, value)).sum()
}

pub(crate) fn decoded_values_bytes(new: &lash_kernel_doc::Nursery<'_>, values: &[Value]) -> u64 {
    fn one(new: &lash_kernel_doc::Nursery<'_>, value: &Value) -> u64 {
        match value {
            Value::Null
            | Value::Bool(_)
            | Value::Float(_)
            | Value::List(_)
            | Value::Map(_)
            | Value::Record(_) => 16,
            Value::Int(n) => {
                16 + u64::from(u64::BITS - n.unsigned_abs().leading_zeros()).div_ceil(8)
            }
            Value::Text(text) => 16 + text.as_bytes().len() as u64,
            _ => match &new
                .built()
                .iter()
                .find(|(id, _)| Some(*id) == value.handle())
                .unwrap()
                .1
            {
                NewObject::LongText(text) => 16 + text.len() as u64,
                NewObject::BigInt(integer) => 16 + integer.bits().div_ceil(8),
                _ => panic!("JSON constructor produced an unexpected immutable body"),
            },
        }
    }
    values.iter().map(|value| one(new, value)).sum()
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
    Datum, FunctionRegistry, Integer, LogicalHeldV1 as Cost,
    NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceOrigin as Origin,
    NewObject, Value, WorkCounter,
};

#[path = "native_contract_observer.rs"]
mod observer;

fn check_case(registry: &FunctionRegistry, name: &str, input: Vec<Datum>) -> Option<Contract> {
    let mut bench = heap();
    let values = input.into_iter().map(|value| decode(&mut bench, value)).collect();
    check_values(registry, name, bench, values)
}
fn check_values(registry: &FunctionRegistry, name: &str, bench: Bench, values: Vec<Value>) -> Option<Contract> {
    let native = registry.iter().map(|(_, entry)| entry).find(|entry| entry.name() == name).unwrap();
    let contract = observer::check_values(
        &**native.native().unwrap(), name, bench, values,
        || {
            POWER_SCRATCH.with(|peak| peak.set(0));
            LIVE_SCRATCH.with(|live| assert_eq!(live.get(), 0));
        },
        || POWER_SCRATCH.with(std::cell::Cell::get),
    );
    if contract.is_some() {
        OBSERVED_NAMES.with(|names| { names.borrow_mut().insert(name.to_owned()); });
    }
    contract
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
                    "len" | "check" | "copy" | "copy_deep" | "keys" | "values" | "entries" => {
                        vec![xs.clone()]
                    }
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
    for (_, name, args, _) in crate::text_json::laws::corpus::cases() {
        assert!(
            check_case(&registry, &name, args).is_some(),
            "{name} missing contract"
        );
        checked += 1;
    }
    checked += more_shapes(&registry);
    for (definition, _) in crate::text_json::text_json() {
        let name = definition.name.as_str();
        for args in [vec![], vec![Datum::Null; definition.signature.params.len()]] {
            assert!(
                check_case(&registry, name, args).is_some(),
                "{name} missing refusal contract"
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
    OBSERVED_NAMES.with(|names| {
        eprintln!(
            "observed {} native entries: {:?}",
            names.borrow().len(),
            names.borrow()
        )
    });
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

fn more_shapes(registry: &FunctionRegistry) -> usize {
    use lash_kernel_doc::{Bytes, Float};
    let mut checked = 0;
    let mut cases = vec![];
    for n in [0, 1, 2, 17, 65, 257] {
        let payload = " e\u{301}😀ß ".repeat(n);
        let text_value = text(&payload);
        let xs = Datum::List(
            (0..n)
                .map(|at| Datum::List(vec![int(at as i64), text_value.clone()]))
                .collect(),
        );
        for name in [
            "list.copy_deep",
            "list.sort_by_keys",
            "list.sort_by_utf16_keys",
            "collection.holds_none",
            "list.index_of_range",
            "key.identity",
        ] {
            let args = match name {
                "list.copy_deep" => vec![xs.clone()],
                "list.sort_by_keys" | "list.sort_by_utf16_keys" => vec![
                    xs.clone(),
                    Datum::List(
                        (0..n)
                            .map(|at| {
                                if name.ends_with("utf16_keys") {
                                    text(&(n - at).to_string())
                                } else {
                                    int((n - at) as i64)
                                }
                            })
                            .collect(),
                    ),
                    Datum::Bool(false),
                ],
                "collection.holds_none" => vec![xs.clone(), Datum::Set(vec![])],
                "list.index_of_range" => vec![
                    xs.clone(),
                    Datum::Null,
                    int(0),
                    int(n as i64),
                    Datum::Bool(true),
                    Datum::Bool(false),
                ],
                _ => vec![
                    Datum::Tuple(vec![text_value.clone(), Datum::Absent, xs.clone()]),
                    int(7),
                    text("error"),
                ],
            };
            cases.push((name.to_owned(), args));
        }
        for entry in registry.iter().map(|(_, entry)| entry) {
            let name = entry.definition().name.as_str();
            if name.starts_with("text.") {
                let op = name
                    .strip_prefix("text.")
                    .unwrap()
                    .split("_u")
                    .next()
                    .unwrap();
                let args = match op {
                    "get" | "utf16_get" => vec![text_value.clone(), int(-1)],
                    "slice" | "utf16_slice" => vec![text_value.clone(), int(-4), int(99999)],
                    "find" | "utf16_find" => vec![text_value.clone(), text("😀"), int(-4)],
                    "compare" | "utf16_compare" | "starts_with" | "ends_with" => {
                        vec![text_value.clone(), text("😀")]
                    }
                    "split" => vec![text_value.clone(), text(""), int(17)],
                    "replace" => vec![text_value.clone(), text(""), text("λ")],
                    "repeat" => vec![text_value.clone(), int(n as i64)],
                    "pad_start" | "pad_end" => {
                        vec![text_value.clone(), int((n + 8) as i64), text("λ😀")]
                    }
                    "trim_set" => vec![
                        text_value.clone(),
                        text(" e"),
                        Datum::Bool(true),
                        Datum::Bool(true),
                    ],
                    "normalize" => vec![text_value.clone(), text("NFKD")],
                    "trim" | "trim_start" | "trim_end" | "lower" | "upper" | "to_code_points"
                    | "to_utf16_units" => vec![text_value.clone()],
                    "from_code_points" => vec![Datum::List(vec![int(0x1f600); n])],
                    "from_utf16_units" => vec![Datum::List(
                        (0..n).flat_map(|_| [int(0xd83d), int(0xde00)]).collect(),
                    )],
                    _ => continue,
                };
                cases.push((name.to_owned(), args));
            }
        }
        for (name, args) in [
            ("bytes.from_octets", vec![Datum::List(vec![int(255); n])]),
            (
                "bytes.concat",
                vec![
                    Datum::Bytes(Bytes::new(vec![255; n])),
                    Datum::Bytes(Bytes::new(vec![0; n])),
                ],
            ),
            ("format.fixed", vec![int(123), int(n as i64)]),
            (
                "format.scientific",
                vec![Datum::Float(Float::new(-0.125)), int(n as i64)],
            ),
            (
                "format.radix",
                vec![
                    Datum::Int(Integer::new(num_bigint::BigInt::from(1) << n)),
                    int(16),
                ],
            ),
            (
                "json.parse",
                vec![
                    text(&format!("[{},1.5]", vec!["[]"; n + 1].join(","))),
                    text("int"),
                    text("by_spelling"),
                ],
            ),
            (
                "json.parse_binary64",
                vec![text(&format!("[{}]", vec![r#"{"a":"😀"}"#; n].join(",")))],
            ),
            ("json.stringify", vec![xs.clone()]),
        ] {
            cases.push((name.to_owned(), args));
        }
    }
    for (name, args) in [
        (
            "list.sort_by_keys",
            vec![
                Datum::List(vec![int(0), int(1), int(2)]),
                Datum::List(vec![
                    Datum::List(vec![int(2)]),
                    Datum::List(vec![int(1)]),
                    Datum::List(vec![int(1), int(0)]),
                ]),
                Datum::Bool(true),
            ],
        ),
        (
            "list.sort_by_keys",
            vec![
                Datum::List(vec![int(0), int(1), int(2)]),
                Datum::List(vec![int(0), int(1), Datum::Null]),
                Datum::Bool(false),
            ],
        ),
        (
            "list.sort_by_keys",
            vec![
                Datum::List(vec![int(0), int(1)]),
                Datum::List(vec![
                    Datum::Float(Float::new(f64::NAN)),
                    Datum::Float(Float::new(1.0)),
                ]),
                Datum::Bool(false),
            ],
        ),
        (
            "list.sort_by_keys",
            vec![
                Datum::List(vec![int(0)]),
                Datum::List(vec![]),
                Datum::Bool(false),
            ],
        ),
        (
            "list.sort_by_utf16_keys",
            vec![
                Datum::List(vec![int(0), int(1)]),
                Datum::List(vec![text("😀"), text("\u{e000}")]),
                Datum::Bool(true),
            ],
        ),
        (
            "list.sort_by_utf16_keys",
            vec![
                Datum::List(vec![int(0), int(1)]),
                Datum::List(vec![text("a"), Datum::Null]),
                Datum::Bool(false),
            ],
        ),
        (
            "collection.holds_none",
            vec![
                Datum::List(vec![int(0), text("no")]),
                Datum::Set(vec![text("text")]),
            ],
        ),
        (
            "collection.holds_none",
            vec![Datum::Null, Datum::Set(vec![text("invalid")])],
        ),
        (
            "list.index_of_range",
            vec![
                Datum::List(vec![Datum::Float(Float::new(f64::NAN))]),
                Datum::Float(Float::new(f64::NAN)),
                int(0),
                int(1),
                Datum::Bool(false),
                Datum::Bool(true),
            ],
        ),
        (
            "list.index_of_range",
            vec![
                Datum::List(vec![]),
                Datum::Null,
                int(-1),
                int(0),
                Datum::Bool(false),
                Datum::Bool(false),
            ],
        ),
        (
            "key.identity",
            vec![
                Datum::Tuple(vec![
                    Datum::Tuple(vec![int(1)]),
                    Datum::Error(Box::new(ErrorDatum {
                        kind: "kind".into(),
                        message: "message".into(),
                        data: Datum::Null,
                    })),
                ]),
                int(0),
                text("error"),
            ],
        ),
    ] {
        cases.push((name.to_owned(), args));
    }
    for name in ["format.fixed", "format.scientific"] {
        cases.push((
            name.to_owned(),
            vec![Datum::Float(Float::new(f64::from_bits(1))), int(10000)],
        ));
    }
    cases.push((
        "key.identity".to_owned(),
        vec![
            Datum::Tuple(vec![
                Datum::Tuple(vec![Datum::Absent]),
                Datum::Error(Box::new(ErrorDatum {
                    kind: "kind".into(),
                    message: "message".into(),
                    data: Datum::Null,
                })),
            ]),
            text(&"x".repeat(8192)),
            text("error"),
        ],
    ));
    cases.push((
        "json.parse".to_owned(),
        vec![
            text(&format!("[{},1.5]", vec!["1e1000"; 32].join(","))),
            text("int"),
            text("by_spelling"),
        ],
    ));
    for exponent in [100, 1000, 10000] {
        cases.push((
            "json.parse".to_owned(),
            vec![
                text(&format!("1e{exponent}")),
                text("int"),
                text("by_spelling"),
            ],
        ));
    }
    for (name, args) in [
        (
            "text.from_code_points",
            vec![Datum::List(vec![int(0x1f600), int(0x1f600), int(0xd800)])],
        ),
        (
            "text.from_utf16_units",
            vec![Datum::List(vec![int(65), int(66), int(0xd800)])],
        ),
        (
            "bytes.from_octets",
            vec![Datum::List(vec![int(65), int(66), int(256)])],
        ),
        (
            "json.parse",
            vec![
                text(r#"{"a":[],"b":1.5}"#),
                text("int"),
                text("by_spelling"),
            ],
        ),
        (
            "json.parse",
            vec![
                text(r#"["\u0041","\uD800"]"#),
                text("int"),
                text("by_spelling"),
            ],
        ),
    ] {
        cases.push((name.to_owned(), args));
    }
    for (name, args) in cases {
        assert!(
            check_case(registry, &name, args).is_some(),
            "{name} missing contract"
        );
        checked += 1;
    }
    // A copied shared child has one fresh identity; a later cycle retires the
    // already completed child on the raised exit.
    for cycle in [false, true] {
        let mut bench = heap();
        let child = decode(&mut bench, Datum::List(vec![text("child")]));
        let root = bench
            .run(&[child], u64::MAX, |_, new| {
                new.allocate(NewObject::List(vec![child, child]))
            })
            .unwrap()
            .0;
        if cycle {
            bench
                .arena_mut()
                .list_set(root.handle().unwrap(), 1, root)
                .unwrap();
        }
        assert!(check_values(registry, "list.copy_deep", bench, vec![root]).is_some());
        checked += 1;
    }
    {
        let mut bench = heap();
        let mut chain = Value::Null;
        for _ in 0..130 {
            chain = bench
                .run(&[chain], u64::MAX, |_, new| {
                    new.allocate(NewObject::List(vec![chain]))
                })
                .unwrap()
                .0;
        }
        let child = decode(&mut bench, Datum::List(vec![int(1)]));
        let root = bench
            .run(&[child, chain], u64::MAX, |_, new| {
                new.allocate(NewObject::List(vec![child, chain]))
            })
            .unwrap()
            .0;
        assert!(check_values(registry, "list.copy_deep", bench, vec![root]).is_some());
        checked += 1;
    }
    checked
}
