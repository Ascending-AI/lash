//! Prepared native admission, charging and observation laws (`K-LIB-006`, `K-CHG-003`). A native's
//! prepared entry answers a call only with the value its general call returns,
//! and a run that takes it is the run that takes the general call: the same
//! result or error, charge and memory. The machine's odd layouts take every
//! native's general call, so each law runs one document in both.

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionId, FunctionRegistry, Handle, Integer, NativeCall, NativeHeap,
    Object, TaskId, Timestamp, Value, WorkCounter, parse_document,
};
use lash_kernel_vm::observe::{GuestSymbols, WorkReport};
use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Layout, Machine, Meters, PreparedLibrary, Program, Start,
    Step, Target, register_machine_functions,
};
use num_bigint::BigInt;

use crate::tests::Heap;
use crate::{register_collections, register_numbers, same};

const BOUNDS: Bounds = Bounds {
    charge: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct World;

impl Host for World {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(Datum::Null)
    }
    fn print(&mut self, _: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// The kernel library prepared twice: in the natural layout, which takes
/// each native's fast path, and in one that takes every general call.
struct Libraries {
    registry: Arc<FunctionRegistry>,
    fast: PreparedLibrary,
    general: PreparedLibrary,
}

impl Libraries {
    fn new() -> Self {
        let mut registry = FunctionRegistry::new();
        register_machine_functions(&mut registry).unwrap();
        register_numbers(&mut registry).unwrap();
        register_collections(&mut registry).unwrap();
        let registry = Arc::new(registry);
        Self {
            fast: PreparedLibrary::new(Arc::clone(&registry)),
            general: PreparedLibrary::with_layout(Arc::clone(&registry), Layout(1)),
            registry,
        }
    }

    fn id(&self, name: &str) -> FunctionId {
        self.registry
            .iter()
            .find(|(_, function)| function.definition().name.as_str() == name)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("no function `{name}`"))
    }

    /// Runs `main`, which binds `args` and returns `name` called on them,
    /// in both libraries, and asserts the two runs alike.
    fn agree(&self, name: &str, args: &[&str]) {
        let lets: String = args
            .iter()
            .enumerate()
            .map(|(index, arg)| format!("let a{index} = {arg} "))
            .collect();
        let call: Vec<String> = (0..args.len()).map(|index| format!("a{index}")).collect();
        let mut uses = format!("use {name} = @{}\n", self.id(name));
        if name != "error.new" {
            uses.push_str(&format!("use error.new = @{}\n", self.id("error.new")));
        }
        let text = format!(
            "kernel 1\nnumbers by_spelling\n{uses}main {{ {lets}return {name}({}) }}",
            call.join(", ")
        );
        let document = Arc::new(parse_document(&text).unwrap_or_else(|error| panic!("{error}")));
        let run = |library: &PreparedLibrary, layout: Layout| -> (End, Meters) {
            let program = Program {
                document: Arc::clone(&document),
                library: library.clone(),
            };
            let start = Start {
                target: Target::Main,
                args: Vec::new(),
                bindings: Default::default(),
            };
            let mut machine =
                KernelMachine::start_with_layout(program, BOUNDS, start, layout).unwrap();
            match machine.run(&mut World, u64::MAX).unwrap() {
                Step::Ended(end) => (end, machine.meters()),
                other => panic!("{text}: {other:?}"),
            }
        };
        let fast = run(&self.fast, Layout(0));
        let general = run(&self.general, Layout(1));
        assert_eq!(fast, general, "{text}");
    }

    /// Runs `name` on every tuple of `corpus` of its arity in both.
    fn agree_on_all(&self, name: &str, arity: usize, corpus: &[&str]) {
        let mut tuple = vec![0; arity];
        loop {
            let args: Vec<&str> = tuple.iter().map(|index| corpus[*index]).collect();
            self.agree(name, &args);
            let Some(position) = tuple.iter().rposition(|index| index + 1 < corpus.len()) else {
                return;
            };
            tuple[position] += 1;
            tuple[position + 1..].fill(0);
        }
    }
}

fn heap() -> Heap {
    let mut heap = Heap::default();
    let int = |value: i64| Value::Int(Integer::from(value));
    heap.allocate(Object::List(vec![int(1), Value::text("a")]))
        .unwrap();
    heap.allocate(Object::Record(vec![("a".to_owned(), int(1))]))
        .unwrap();
    heap.allocate(Object::Map(vec![(int(1), Value::text("x"))]))
        .unwrap();
    heap.allocate(Object::Set(vec![int(1)])).unwrap();
    heap
}

/// The values the native laws call a primitive on, and the heap objects of
/// [`heap`]: a list, a record, a map and a set.
fn values() -> Vec<Value> {
    let heap = heap();
    let [list, record, map, set] = [1, 2, 3, 4].map(lash_kernel_doc::ObjectId);
    assert_eq!(heap.len(list), 2);
    let float = |value: f64| Value::Float(lash_kernel_doc::Float::new(value));
    vec![
        Value::Null,
        Value::Absent,
        Value::Bool(true),
        Value::Int(Integer::from(0)),
        Value::Int(Integer::from(1)),
        Value::Int(Integer::from(10)),
        Value::Int(Integer::from(-3)),
        Value::Int(Integer::new(BigInt::from(1u8) << 70)),
        float(0.0),
        float(-0.0),
        float(2.5),
        float(f64::NAN),
        float(f64::INFINITY),
        Value::text(""),
        Value::text("12"),
        Value::text("a"),
        Value::Tuple(Arc::from([Value::Int(Integer::from(1)), float(-0.0)])),
        Value::List(list),
        Value::Record(record),
        Value::Map(map),
        Value::Set(set),
        Value::Task(TaskId(0)),
        Value::Function(lash_kernel_doc::Name::new("f")),
    ]
}

/// K-LIB-006: every prepared operation admits only answers of its general
/// call, including all prefix families, Any-domain comparisons, overflow,
/// mixed numbers, NaN, signed zero, wrong domains and structural fallback.
#[test]
fn prepared_entries_answer_as_the_general_call() {
    let libraries = Libraries::new();
    let corpus = values();
    for (_, registered) in libraries.registry.iter() {
        let Some(native) = registered.native() else {
            continue;
        };
        let Some(prepared) = native.prepare() else {
            continue;
        };
        let arity = registered.definition().signature.params.len();
        let mut tuple = vec![0; arity];
        let mut answered = 0;
        loop {
            let args: Vec<Value> = tuple.iter().map(|index| corpus[*index].clone()).collect();
            if let Some(value) = (prepared.entry)(&args) {
                answered += 1;
                let general = native
                    .call(NativeCall {
                        args: &args,
                        heap: &mut heap(),
                        counter: &mut WorkCounter::new(None),
                    })
                    .unwrap_or_else(|error| panic!("{} {args:?}: {error:?}", prepared.operation));
                assert!(
                    same(&value, &general),
                    "{} {args:?}: {value:?}, {general:?}",
                    prepared.operation
                );
            }
            let Some(position) = tuple.iter().rposition(|index| index + 1 < corpus.len()) else {
                break;
            };
            tuple[position] += 1;
            tuple[position + 1..].fill(0);
        }
        assert!(answered > 0, "{} answered nothing", prepared.operation);
    }
}

/// K-CHG-001/003, K-MACH-008: the prepared and general entry give the same
/// result, error, charge and memory on all numeric identities and domains.
#[test]
fn charge_invariance() {
    let libraries = Libraries::new();
    let corpus = [
        "0",
        "1",
        "-3",
        "9223372036854775807",
        "999999999999999999999",
        "-0.0",
        "2.5",
        "nan",
        "inf",
        "\"12\"",
        "(1, 2)",
        "[1, 2]",
    ];
    for (definition, _) in crate::numbers() {
        let name = definition.name.as_str();
        let arity = definition.signature.params.len();
        // Three-argument math has the same admission as its binary siblings.
        let corpus = if arity > 2 { &corpus[..5] } else { &corpus[..] };
        libraries.agree_on_all(name, arity, corpus);
    }
}

/// Canonicalize only executable coordinates and layout-specific diagnostics.
/// Every attribution, call, operation, allocation, guard and retry remains.
fn canonical(mut work: WorkReport, symbols: &GuestSymbols) -> WorkReport {
    let mut libs: Vec<_> = work
        .natives
        .iter()
        .map(|native| native.symbol.function)
        .collect();
    libs.sort();
    let token = |old| {
        libs.binary_search(&symbols.lib(old).unwrap().function)
            .unwrap() as u32
    };
    for code in &mut work.codes {
        code.symbol.token = 0;
        for (called, _) in &mut code.native_calls {
            *called = token(*called);
        }
        code.native_calls.sort();
    }
    for (called, _) in &mut work.unattributed_native_calls {
        *called = token(*called);
    }
    work.unattributed_native_calls.sort();
    for native in &mut work.natives {
        native.symbol.token = token(native.symbol.token);
        native.counts.fast_calls = 0;
        native.counts.formula_evaluations = 0;
    }
    work.codes.sort_by(|a, b| a.symbol.site.cmp(&b.symbol.site));
    work.natives.sort_by_key(|native| native.symbol.function);
    work
}

/// K-MACH-008: preparing native entries preserves counted work ownership,
/// including a successful scalar, structural fallback and an admitted error.
#[test]
fn prepared_and_general_layouts_attribute_the_same_work() {
    let libraries = Libraries::new();
    let uses: String = [
        "add",
        "num.add",
        "int.mul",
        "float.add",
        "lt",
        "eq",
        "bool.not",
    ]
    .iter()
    .map(|name| format!("use {name} = @{}\n", libraries.id(name)))
    .collect();
    let document = Arc::new(
        parse_document(&format!(
            "kernel 1\nnumbers by_spelling\n{uses}
fn helper(x) {{ return add(x, 1) }}
main {{
    let a = call helper(0)
    let b = num.add(-0.0, 2.5)
    let c = int.mul(3, 7)
    let d = float.add(-0.0, 0.0)
    let ordered = lt(a, c)
    let structural = eq([1, 2], [1, 2])
    let negated = bool.not(ordered)
    try {{ let bad = num.add(\"text\", a) }} catch e {{ print e.kind }}
    return (a, b, c, d, ordered, structural, negated)
}}"
        ))
        .unwrap(),
    );
    let mut reports = Vec::new();
    let mut ends = Vec::new();
    for layout in [Layout(0), Layout(1)] {
        let program = Program {
            document: Arc::clone(&document),
            library: if layout == Layout(0) {
                libraries.fast.clone()
            } else {
                libraries.general.clone()
            },
        };
        let mut machine = KernelMachine::start_with_layout(
            program,
            BOUNDS,
            Start {
                target: Target::Main,
                args: Vec::new(),
                bindings: Default::default(),
            },
            layout,
        )
        .unwrap();
        machine.count_work();
        ends.push(machine.run(&mut World, u64::MAX).unwrap());
        let work = machine.work().unwrap();
        let fast_calls: u64 = work
            .natives
            .iter()
            .map(|native| native.counts.fast_calls)
            .sum();
        assert_eq!(fast_calls, if layout == Layout(0) { 6 } else { 0 });
        assert!(
            work.natives
                .iter()
                .map(|native| native.counts.formula_evaluations)
                .sum::<u64>()
                > 0
        );
        reports.push(canonical(work, &machine.guest_symbols()));
    }
    assert_eq!(ends[0], ends[1]);
    assert!(matches!(ends[0], Step::Ended(End::Finished(_))));
    assert_eq!(reports[0], reports[1]);
}
