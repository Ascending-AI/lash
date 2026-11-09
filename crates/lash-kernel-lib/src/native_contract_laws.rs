//! FIG-7525: declarations cannot omit an occurrence, fresh identity or member.
//! The observer reads installed output data and nursery allocations, separately
//! from the input-only declaration evaluator and its native-view cost adapter.
use crate::testsupport::{Bench, decode, heap};

mod allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    fn record() {
        let _ = COUNT.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
    }

    struct Counter;
    #[global_allocator]
    static ALLOCATOR: Counter = Counter;

    #[expect(
        unsafe_code,
        reason = "test allocation counter forwards the GlobalAlloc contract unchanged to System"
    )]
    unsafe impl GlobalAlloc for Counter {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: The caller's valid layout is passed unchanged to System.
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: The caller's valid layout is passed unchanged to System.
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: System allocated this pointer with the same layout.
            unsafe { System.dealloc(pointer, layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record();
            // SAFETY: The caller's pointer, layout and new size are unchanged.
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    struct Active;
    impl Drop for Active {
        fn drop(&mut self) {
            COUNT.with(|count| count.set(None));
        }
    }

    pub(super) fn during<T>(body: impl FnOnce() -> T) -> (T, usize) {
        COUNT.with(|count| {
            assert!(count.get().is_none());
            count.set(Some(0));
        });
        let active = Active;
        let value = body();
        let count = COUNT.with(|count| count.take().unwrap());
        drop(active);
        (value, count)
    }
}

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
    try_observed_local(args, value).unwrap()
}
fn try_observed_local(args: lash_kernel_doc::Args<'_>, value: &Value) -> Result<u64, NativeError> {
    Ok(Cost::REFERENCE
        + match value {
            Value::Int(n) => n
                .unsigned_abs()
                .checked_ilog2()
                .map_or(0, |b| u64::from(b + 1).div_ceil(8)),
            Value::BigInt(_) => args.integer(value)?.unwrap().bits().div_ceil(8),
            Value::Timestamp(n) => n
                .unsigned_abs()
                .checked_ilog2()
                .map_or(0, |b| u64::from(b + 1).div_ceil(8)),
            Value::BigTimestamp(_) => args.timestamp(value)?.unwrap().bits().div_ceil(8),
            Value::Text(_) | Value::LongText(_) => args.text(value)?.unwrap().len(),
            Value::Bytes(_) => args.bytes(value)?.unwrap().len(),
            Value::Tuple(_) => {
                args.sequence(value)?
                    .unwrap()
                    .chunks()
                    .try_fold(0, |sum, chunk| {
                        chunk?.iter().try_fold(sum, |sum, v| {
                            Ok::<_, NativeError>(sum + try_observed_local(args, v)?)
                        })
                    })?
            }
            Value::Error(_) => {
                let e = args.error(value)?.unwrap();
                args.text(&e.kind)?.unwrap().len()
                    + args.text(&e.message)?.unwrap().len()
                    + try_observed_local(args, &e.data)?
            }
            Value::Function(_) => 64,
            Value::Handle(_) => {
                let h = args.host_handle(value)?.unwrap();
                (h.kind.len() + h.id.len()) as u64
            }
            _ => 0,
        })
}

pub(crate) fn scratch_values(args: lash_kernel_doc::Args<'_>, values: &[Value]) -> ScratchLease {
    scratch(values.iter().map(|value| observed_local(args, value)).sum())
}
pub(crate) fn try_scratch_values(
    args: lash_kernel_doc::Args<'_>,
    values: &[Value],
) -> Result<ScratchLease, NativeError> {
    Ok(scratch(values.iter().try_fold(0, |sum, value| {
        Ok::<_, NativeError>(sum + try_observed_local(args, value)?)
    })?))
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
            } => new_values_bytes(args, new, &[*kind, *message, data.value()]) - 16,
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
    Datum, ErrorDatum, FunctionRegistry, Integer, LogicalHeldV1 as Cost, NativeError,
    NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceOrigin as Origin,
    NewObject, Value,
};

#[path = "native_contract_observer.rs"]
mod observer;

fn reset_scratch() {
    POWER_SCRATCH.with(|peak| peak.set(0));
    LIVE_SCRATCH.with(|live| assert_eq!(live.get(), 0));
}
fn peak_scratch() -> u64 {
    POWER_SCRATCH.with(std::cell::Cell::get)
}
fn check_case(registry: &FunctionRegistry, name: &str, input: Vec<Datum>) -> Option<Contract> {
    let native = registry
        .iter()
        .map(|(_, entry)| entry)
        .find(|entry| entry.name() == name)
        .unwrap();
    let contract = observer::check_case(
        &**native.native().unwrap(),
        name,
        input,
        reset_scratch,
        peak_scratch,
    );
    if contract.is_some() {
        OBSERVED_NAMES.with(|names| {
            names.borrow_mut().insert(name.to_owned());
        });
    }
    contract
}

fn check_values(
    registry: &FunctionRegistry,
    name: &str,
    bench: Bench,
    values: Vec<Value>,
) -> Option<Contract> {
    let native = registry
        .iter()
        .map(|(_, entry)| entry)
        .find(|entry| entry.name() == name)
        .unwrap();
    let contract = observer::check_values(
        &**native.native().unwrap(),
        name,
        bench,
        values,
        reset_scratch,
        peak_scratch,
    );
    if contract.is_some() {
        OBSERVED_NAMES.with(|names| {
            names.borrow_mut().insert(name.to_owned());
        });
    }
    contract
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}
fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

/// R8: a kind tag and shallow identity need no traversal workspace. In
/// particular, `same` must not allocate a pending-pair Vec before inspecting
/// two inline texts, which the Python call wrapper compares on every call.
#[test]
fn call_wrapper_leaf_natives_answer_without_allocating() {
    use lash_kernel_doc::{Float, NativeCall, WorkCounter};
    let mut bench = heap();
    let list = decode(&mut bench, Datum::List(vec![]));
    let other = decode(&mut bench, Datum::List(vec![]));
    let pair = decode(&mut bench, Datum::Tuple(vec![int(7), int(8)]));
    let map = decode(&mut bench, Datum::Map(vec![(text("x"), int(7))]));
    let short = |value| Value::Text(lash_kernel_doc::arena::ShortText::new(value).unwrap());
    let cases: &[(&str, &[Value], Value)] = &[
        ("kind", &[list], short("list")),
        (
            "same",
            &[short("py.function"), short("py.function")],
            Value::Bool(true),
        ),
        (
            "same",
            &[short("py.function"), short("py.builtin")],
            Value::Bool(false),
        ),
        ("same", &[Value::Int(3), Value::Int(3)], Value::Bool(true)),
        (
            "same",
            &[
                Value::Float(Float::new(-0.0)),
                Value::Float(Float::new(0.0)),
            ],
            Value::Bool(false),
        ),
        (
            "same",
            &[
                Value::Float(Float::new(f64::NAN)),
                Value::Float(Float::new(f64::NAN)),
            ],
            Value::Bool(true),
        ),
        ("same", &[list, list], Value::Bool(true)),
        ("same", &[list, other], Value::Bool(false)),
        ("same", &[list, Value::Absent], Value::Bool(false)),
        ("bool.not", &[Value::Bool(true)], Value::Bool(false)),
        ("bool.not", &[Value::Bool(false)], Value::Bool(true)),
        ("list.len", &[list], Value::Int(0)),
        ("tuple.len", &[pair], Value::Int(2)),
        ("map.len", &[map], Value::Int(1)),
        ("map.contains", &[map, short("x")], Value::Bool(true)),
        ("map.contains", &[map, short("y")], Value::Bool(false)),
        ("map.contains", &[map, Value::Int(7)], Value::Bool(false)),
        ("num.to_text", &[Value::Int(-1234)], short("-1234")),
        ("num.to_text", &[Value::Int(0)], short("0")),
        (
            "int.to_text",
            &[Value::Int(255), Value::Int(16)],
            short("ff"),
        ),
        (
            "int.to_text",
            &[Value::Int(-5), Value::Int(2)],
            short("-101"),
        ),
        ("int.to_text", &[Value::Int(35), Value::Int(36)], short("z")),
    ];
    for &(name, values, expected) in cases {
        let native = crate::native(name).unwrap();
        let mut counter = WorkCounter::new(Some(0));
        let ((answer, count), usage) = bench
            .run(values, 0, |args, new| {
                allocations::during(|| {
                    native.call(NativeCall {
                        args,
                        new,
                        counter: &mut counter,
                    })
                })
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "{name} {values:?}: a shallow answer needs no workspace"
        );
        assert!(answer.unwrap().same(&expected));
        assert_eq!(counter.spent(), 0);
        assert_eq!(usage.reserved(), 0);
        assert_eq!(usage.grown, 0);
        let inline = native.specialization().inline.unwrap();
        let ((answer, count), _) = bench
            .run(values, 0, |args, _| {
                allocations::during(|| inline.answer_read(1, args))
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(answer.unwrap().unwrap().same(&expected));
        assert!(inline.for_version(2).is_none());
    }
    let tuple = decode(&mut bench, Datum::Tuple(vec![int(3)]));
    let same = crate::native("same")
        .unwrap()
        .specialization()
        .inline
        .unwrap();
    assert!(same.scalar_answer(1, &[tuple, tuple]).is_none());

    // Force the same registered implementations through the ordinary call,
    // including unary completion billing and refusals at the last fee.
    struct Ordinary(std::sync::Arc<dyn lash_kernel_doc::NativeFunction>);
    impl lash_kernel_doc::NativeFunction for Ordinary {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            self.0.call(call)
        }
        fn resource_contract(
            &self,
            args: lash_kernel_doc::Args<'_>,
        ) -> Result<Option<Contract>, NativeError> {
            self.0.resource_contract(args)
        }
    }
    let mut catalog = FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut catalog).unwrap();
    crate::register_numbers(&mut catalog).unwrap();
    crate::register_collections(&mut catalog).unwrap();
    let run = |inline: bool, limit: u64| {
        use lash_kernel_vm::{Bounds, Charge, CoreMachine, Machine, PreparedLibrary};
        let mut registry = FunctionRegistry::new();
        for (_, registered) in catalog.iter().filter(|(_, registered)| {
            matches!(
                registered.name(),
                "kind"
                    | "same"
                    | "bool.not"
                    | "list.len"
                    | "tuple.len"
                    | "map.len"
                    | "map.contains"
                    | "num.to_text"
                    | "int.to_text"
                    | "num.add"
            )
        }) {
            let definition = (**registered.definition()).clone();
            let native = std::sync::Arc::clone(registered.native().unwrap());
            let native: std::sync::Arc<dyn lash_kernel_doc::NativeFunction> = if inline {
                native
            } else {
                std::sync::Arc::new(Ordinary(native))
            };
            registry.register(definition, Some(native)).unwrap();
        }
        let document = std::sync::Arc::new(crate::testsupport::document(
            "kernel 1\nnumbers by_spelling\nmain { let xs = [] let pair = (7, 8) let kw = map{\"x\": 7} return (kind(xs), same(\"py.function\", \"py.function\"), same(-0.0, 0.0), same(xs, xs), list.len(xs), tuple.len(pair), map.len(kw), bool.not(false), map.contains(kw, \"x\"), num.to_text(-1234), int.to_text(255, 16), num.add(-3, 1), same(\"a\", \"b\")) }",
            &registry,
        ));
        let program = crate::testsupport::program(
            document,
            PreparedLibrary::new(std::sync::Arc::new(registry)),
        );
        let mut machine = CoreMachine::start(
            program,
            Bounds {
                memory_meter: lash_kernel_vm::MemoryMeter::On,
                charge: Charge::Exact,
                charge_limit: limit,
                memory: 64 << 20,
                call_depth: 1,
                live_tasks: 100,
                requests_per_park: 100,
                join_members: 100,
            },
            lash_kernel_vm::CellStart {
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                target: lash_kernel_vm::Target::Main,
                args: vec![],
                session: None,
                modes: Default::default(),
                host: vec![],
            },
            Box::<crate::testsupport::Source>::default(),
        )
        .unwrap();
        let step = machine
            .run(&mut crate::testsupport::World::default(), u64::MAX)
            .unwrap();
        (step, machine.meters())
    };
    let ordinary = run(false, u64::MAX);
    assert!(matches!(
        ordinary.0,
        lash_kernel_vm::Step::Ended(lash_kernel_vm::End::Finished(_))
    ));
    let total = ordinary.1.charged.unwrap();
    for limit in [0, 1, total - 1, total, u64::MAX] {
        assert_eq!(run(true, limit), run(false, limit), "charge limit {limit}");
    }
}

/// R8/K-LIB-006: the declared word/binary64 kernel is the registered
/// native's answer, including NaN, signed zero and word overflow fallback.
#[test]
fn known_answers_use_their_kernel_and_reject_disagreement() {
    let float = |value| Value::Float(lash_kernel_doc::Float::new(value));
    for (definition, native) in crate::numbers() {
        let Some(inline) = native.specialization().inline else {
            continue;
        };
        let version = definition.native_version;
        for args in [
            [Value::Int(3), Value::Int(4)],
            [Value::Int(i64::MAX), Value::Int(1)],
            [Value::Int(i64::MIN), Value::Int(-1)],
            [float(1.0), float(2.0)],
            [float(-0.0), float(0.0)],
            [float(f64::INFINITY), float(f64::NEG_INFINITY)],
            [float(f64::NAN), float(2.0)],
        ] {
            let Some(answer) = inline.scalar_answer(version, &args) else {
                continue;
            };
            let mut bench = heap();
            let mut counter = lash_kernel_doc::WorkCounter::new(Some(0));
            let called = bench.call(&*native, &args, u64::MAX, &mut counter);
            let ordinary = called.answer.unwrap();
            assert!(answer.same(&ordinary), "{} {args:?}", definition.name);
            assert_eq!(counter.spent(), 0);
            assert_eq!(called.usage.objects, 0);
            assert!(inline.scalar_answer(version + 1, &args).is_none());
        }
    }
    let add = crate::native("int.add").unwrap();
    let inline = add.specialization().inline.unwrap();
    assert!(matches!(
        inline.scalar_answer(1, &[Value::Int(3), Value::Int(4)]),
        Some(Value::Int(7))
    ));
    let args = [Value::Int(i64::MAX), Value::Int(1)];
    assert!(inline.scalar_answer(1, &args).is_none());
    let mut bench = heap();
    let mut counter = lash_kernel_doc::WorkCounter::new(None);
    let overflow = bench
        .call(&*add, &args, u64::MAX, &mut counter)
        .answer
        .unwrap();
    assert_eq!(
        crate::testsupport::datum(&bench, &overflow),
        Datum::Int(Integer::parse("9223372036854775808").unwrap())
    );
    let wrong_kind = [float(1.0), float(2.0)];
    assert!(inline.scalar_answer(1, &wrong_kind).is_none());
    assert!(matches!(
        bench
            .call(&*add, &wrong_kind, u64::MAX, &mut counter)
            .answer,
        Err(NativeError::Refused {
            kind: "type_error",
            ..
        })
    ));
}

/// R8: neither a replacement's name nor a different native version carries
/// another implementation's authorization. A native may still Decline.
#[test]
fn inline_selection_preserves_a_replacement_natives_decline() {
    struct Declines;
    impl lash_kernel_doc::NativeFunction for Declines {
        fn call(&self, _: lash_kernel_doc::NativeCall<'_, '_>) -> Result<Value, NativeError> {
            Err(NativeError::Decline)
        }
    }
    let mut registry = FunctionRegistry::new();
    let definition = lash_kernel_doc::parse_definition(
        "function int.add(a: Int, b: Int) -> Int\nkernel 1\ncharge 5\nnative\nbody { return 7 }",
    )
    .unwrap();
    let function = registry
        .register(definition, Some(std::sync::Arc::new(Declines)))
        .unwrap();
    let registered = registry.get(&function).unwrap();
    let native = registered.native().unwrap();
    let args = [Value::Int(3), Value::Int(4)];
    assert!(
        native
            .specialization()
            .inline
            .and_then(|inline| {
                inline.scalar_answer(registered.definition().native_version, &args)
            })
            .is_none()
    );
    let mut bench = heap();
    let mut counter = lash_kernel_doc::WorkCounter::new(None);
    assert!(matches!(
        bench.call(&**native, &args, u64::MAX, &mut counter).answer,
        Err(NativeError::Decline)
    ));
    let declared = crate::native("int.add")
        .unwrap()
        .specialization()
        .inline
        .unwrap();
    assert!(declared.for_version(2).is_none());
}

/// R8/K-BND-003: scalar eligibility never certifies graph-bearing operands
/// or removes the allocation admission needed by overflow and long text.
#[test]
fn prepared_scalar_answers_supply_only_graph_free_contracts() {
    let native = crate::native("int.add").unwrap();
    let specialization = native.specialization();
    let inline = specialization.inline.unwrap();
    let mut bench = heap();
    let args = [Value::Int(3), Value::Int(4)];
    let scalar = inline.scalar_answer(1, &args).unwrap();
    let contract = bench
        .run(&args, 0, |args, _| native.resource_contract(args))
        .unwrap()
        .0
        .unwrap()
        .unwrap();
    assert_eq!(
        contract,
        Contract::value(Exit::Returned, Cost::REFERENCE + 1, Origin::Immutable)
    );
    assert!(scalar.same(&Value::Int(7)));
    for graph in [
        Datum::List(vec![int(3)]),
        Datum::Tuple(vec![Datum::List(vec![int(3)])]),
        Datum::Int(Integer::parse("9223372036854775808").unwrap()),
    ] {
        let value = decode(&mut bench, graph);
        assert!(!inline.operation.accepts(&[value, Value::Int(4)]));
        assert!(inline.scalar_answer(1, &[value, Value::Int(4)]).is_none());
    }
    let mut counter = lash_kernel_doc::WorkCounter::new(None);
    let overflow = [Value::Int(i64::MAX), Value::Int(1)];
    assert!(inline.scalar_answer(1, &overflow).is_none());
    assert!(crate::testsupport::exhausted(
        &bench.call(&*native, &overflow, 0, &mut counter).answer
    ));
    let format = crate::native("num.to_text").unwrap();
    let digits = format.specialization().inline.unwrap();
    assert!(digits.operation.accepts(&[Value::Int(i64::MAX)]));
    assert!((format.prepare().unwrap().entry)(&[Value::Int(i64::MAX)]).is_none());
    assert!(digits.scalar_answer(1, &[Value::Int(i64::MAX)]).is_none());
    assert!(crate::testsupport::exhausted(
        &bench
            .call(&*format, &[Value::Int(i64::MAX)], 0, &mut counter)
            .answer
    ));
    let concat = crate::native("text.concat").unwrap();
    let long = decode(&mut bench, text("longer than a short text"));
    let args = [
        long,
        Value::Text(lash_kernel_doc::arena::ShortText::new("x").unwrap()),
    ];
    assert!(
        concat
            .specialization()
            .inline
            .unwrap()
            .operation
            .accepts(&args)
    );
    assert!((concat.prepare().unwrap().entry)(&args).is_none());
    assert!(crate::testsupport::exhausted(
        &bench.call(&*concat, &args, 0, &mut counter).answer
    ));
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
    check_case(
        &registry,
        "text.utf16_slice",
        vec![text("a😀z"), int(3), int(2)],
    )
    .unwrap();
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
                    Datum::Error(Box::new(ErrorDatum::new(
                        "kind".into(),
                        "message".into(),
                        Datum::Null,
                    ))),
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
                Datum::Error(Box::new(ErrorDatum::new(
                    "kind".into(),
                    "message".into(),
                    Datum::Null,
                ))),
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
    // Exponent signs/widths, fractional suffixes and UTF-16 escapes are
    // separate byte obligations in the independent JSON counting model.
    for (from, until) in [(-5, 21), (1, 1), (0, 0)] {
        for (ascii, plus, negative_zero, width, fractional) in [
            (false, false, false, 0, false),
            (true, true, true, 4, true),
            (false, true, false, 2, false),
        ] {
            let layout = Datum::Tuple(vec![
                int(from),
                int(until),
                Datum::Bool(plus),
                Datum::Bool(negative_zero),
            ]);
            let value = Datum::List(vec![
                Datum::Float(Float::new(-0.0)),
                Datum::Float(Float::new(1e-12)),
                Datum::Float(Float::new(1e24)),
                Datum::Float(Float::new(0.1)),
                Datum::Float(Float::new(f64::from_bits(1))),
                Datum::Float(Float::new(f64::MAX)),
                int(0),
                int(100),
                Datum::Int(Integer::parse("123456789012345678900000").unwrap()),
                text("λ😀\0\"\\\n"),
            ]);
            let style = Datum::Tuple(vec![
                Datum::Bool(ascii),
                text(", "),
                text(": "),
                int(width),
                Datum::Bool(fractional),
                Datum::Tuple(vec![text("reserved"), text("other"), text("private")]),
                Datum::Absent,
                Datum::Bool(false),
                Datum::Bool(false),
            ]);
            cases.push((
                "json.encode".to_owned(),
                vec![value.clone(), layout.clone(), style],
            ));
            cases.push((
                "json.render".to_owned(),
                vec![
                    value,
                    Datum::Set(if ascii { vec![text("list")] } else { vec![] }),
                    Datum::Absent,
                    layout,
                ],
            ));
        }
    }
    for (name, args) in [
        (
            "json.parse",
            vec![text("[1,2]"), text("invalid"), text("by_spelling")],
        ),
        (
            "json.parse",
            vec![text("[1,2]"), text("int"), text("invalid")],
        ),
        (
            "json.parse",
            vec![
                text(r#"{"a":1,"b":[],"a":["last"]}"#),
                text("number"),
                text("by_spelling"),
            ],
        ),
        (
            "json.stringify",
            vec![Datum::List(vec![
                text("prefix"),
                Datum::Bytes(Bytes::new(vec![1])),
            ])],
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
        let child = decode(
            &mut bench,
            Datum::Record(vec![("child".into(), text("λ😀"))]),
        );
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
        assert!(check_values(registry, "json.stringify", bench, vec![root]).is_some());
        checked += 1;
    }
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
