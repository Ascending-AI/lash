//! Laws of the natives' fast paths (`K-LIB-006`, `K-CHG-003`). A native's
//! fast path answers a call only with the value its general call returns,
//! and a run that takes it is the run that takes the general call: the same
//! result or error, charge and memory. The machine's odd layouts take every
//! native's general call, so each law runs one document in both.

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionId, FunctionRegistry, Handle, Integer, NativeCall, NativeHeap,
    Object, TaskId, Timestamp, Value, WorkCounter, parse_document,
};
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
            .find(|(_, function)| function.definition.name.as_str() == name)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("no function `{name}`"))
    }

    /// The natives whose names `pick` takes, with their arities.
    fn natives(&self, pick: impl Fn(&str) -> bool) -> Vec<(String, usize)> {
        self.registry
            .iter()
            .filter(|(_, function)| function.native.is_some())
            .map(|(_, function)| function.definition.as_ref())
            .filter(|definition| pick(definition.name.as_str()))
            .map(|definition| {
                (
                    definition.name.as_str().to_owned(),
                    definition.signature.params.len(),
                )
            })
            .collect()
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

/// A tuple nested `depth` deep, in kernel text.
fn nested(depth: usize) -> String {
    let mut text = String::from("1");
    for _ in 0..depth {
        text = format!("({text}, -0.0)");
    }
    text
}

/// Operands of every kind, with the edges a primitive or a measure turns
/// on: -0, NaN, the infinities, the 64-bit word's edge, empty texts and
/// collections, and deeply nested tuples and errors.
fn operands() -> Vec<String> {
    [
        "null",
        "absent",
        "true",
        "false",
        "0",
        "7",
        "-3",
        "9223372036854775807",
        "-99999999999999999999999",
        "0.0",
        "-0.0",
        "2.5",
        "nan",
        "inf",
        "-inf",
        "\"\"",
        "\"abc\"",
        "(1, \"a\")",
        "(nan, (-0.0, (1, (2, 3))))",
        "[]",
        "[1, [2, 3], (4, 5)]",
        "{a: 1}",
        "map{1: \"x\"}",
        "set{1}",
        "fn() { return 1 }",
        "error.new(\"k\", \"m\", (1, (2, 3)))",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([nested(60)])
    .collect()
}

/// `K-LIB-006`: `name`'s fast path answers every call on `corpus` that its
/// general call answers, or leaves it to the general call, and answers at
/// least one. The heap holds a list, a record, a map and a set.
fn answers_as_the_general_call(libraries: &Libraries, name: &str, corpus: &[Value]) {
    let registered = libraries.registry.get(&libraries.id(name)).unwrap();
    let native = registered.native.as_ref().unwrap();
    let arity = registered.definition.signature.params.len();
    let mut answered = 0;
    let mut tuple = vec![0; arity];
    loop {
        let args: Vec<Value> = tuple.iter().map(|index| corpus[*index].clone()).collect();
        let mut heap = heap();
        if let Some(value) = native.fast(&args, &heap) {
            answered += 1;
            let general = native.call(NativeCall {
                args: &args,
                heap: &mut heap,
                counter: &mut WorkCounter::new(None),
            });
            match general {
                Ok(general) => assert!(
                    same(&value, &general),
                    "{name}{args:?}: {value:?} and {general:?}"
                ),
                Err(error) => panic!("{name}{args:?}: {value:?}, but the call raised {error:?}"),
            }
        }
        let Some(position) = tuple.iter().rposition(|index| index + 1 < corpus.len()) else {
            break;
        };
        tuple[position] += 1;
        tuple[position + 1..].fill(0);
    }
    // A guarded native counts its work, and `int.to_text` reserves its
    // digits, which only the general call does: neither has a fast answer.
    assert!(
        answered > 0 || registered.definition.guard.is_some() || name == "int.to_text",
        "{name} answered nothing"
    );
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

/// The spike's six primitives, and the conversions and comparisons the
/// gate programs call most, by name.
fn law(name: &str) {
    let libraries = Libraries::new();
    let operands = operands();
    let corpus: Vec<&str> = operands.iter().map(String::as_str).collect();
    let (_, arity) = libraries
        .natives(|candidate| candidate == name)
        .pop()
        .unwrap();
    libraries.agree_on_all(name, arity, &corpus);
    answers_as_the_general_call(&libraries, name, &values());
}

#[test]
fn kind_takes_its_fast_path_as_its_general_call() {
    law("kind");
}

#[test]
fn same_takes_its_fast_path_as_its_general_call() {
    law("same");
}

#[test]
fn num_lt_takes_its_fast_path_as_its_general_call() {
    law("num.lt");
}

#[test]
fn num_le_takes_its_fast_path_as_its_general_call() {
    law("num.le");
}

#[test]
fn bool_not_takes_its_fast_path_as_its_general_call() {
    law("bool.not");
}

#[test]
fn list_len_takes_its_fast_path_as_its_general_call() {
    law("list.len");
}

#[test]
fn eq_takes_its_fast_path_as_its_general_call() {
    law("eq");
}

#[test]
fn num_add_takes_its_fast_path_as_its_general_call() {
    law("num.add");
}

/// Every other numeric native: its fast path answers as its general call
/// on numbers at their edges and on what is not a number, and a run that
/// takes it is the run that takes the general call.
#[test]
fn every_numeric_fast_path_is_its_general_call() {
    let libraries = Libraries::new();
    let corpus = [
        "0",
        "-3",
        "10",
        "99999999999999999999",
        "-0.0",
        "2.5",
        "nan",
        "\"12\"",
        "(1, 2)",
    ];
    let numeric: Vec<String> = crate::numbers()
        .into_iter()
        .map(|(definition, _)| definition.name.as_str().to_owned())
        .filter(|name| {
            !["kind", "same", "num.lt", "num.le", "eq", "num.add"].contains(&name.as_str())
        })
        .collect();
    let values = values();
    for (name, arity) in libraries.natives(|name| numeric.iter().any(|known| known == name)) {
        // `math.fma` takes three numbers: its edges are the pairs'.
        let corpus = if arity > 2 { &corpus[..5] } else { &corpus[..] };
        libraries.agree_on_all(&name, arity, corpus);
        answers_as_the_general_call(&libraries, &name, &values);
    }
}

/// Every collection native that only reads, and the ones that are
/// functions of their arguments alone: on each kind of collection, empty
/// and nested, at indexes and keys on and past their edges.
#[test]
fn every_reading_collection_fast_path_is_its_general_call() {
    let libraries = Libraries::new();
    let collections = [
        "[]",
        "[1, (2, 3), [4]]",
        "(1, 2)",
        "map{1: \"x\", \"k\": (1, 2)}",
        "set{1, \"a\"}",
        "{a: 1, b: (2, 3)}",
        "\"text\"",
    ];
    let keys = [
        "0", "1", "-1", "1.0", "0.5", "nan", "\"a\"", "\"k\"", "(2, 3)", "null",
    ];
    let reads = [
        ".len",
        ".check",
        ".get",
        ".at",
        ".insert_index",
        ".contains",
        ".index_of",
    ];
    let values = values();
    let natives = libraries.natives(|name| {
        ["list.", "tuple.", "map.", "set.", "record."]
            .iter()
            .any(|kind| name.starts_with(kind))
            && reads.iter().any(|read| name.ends_with(read))
    });
    assert_eq!(natives.len(), 23);
    for (name, arity) in natives {
        match arity {
            1 => collections
                .iter()
                .for_each(|xs| libraries.agree(&name, &[xs])),
            _ => {
                for xs in collections {
                    keys.iter()
                        .for_each(|key| libraries.agree(&name, &[xs, key]));
                }
            }
        }
        answers_as_the_general_call(&libraries, &name, &values);
    }
    let corpus = ["true", "1", "\"k\"", "fn() { return 1 }", "(1, 2)", "null"];
    for name in [
        "bool.not",
        "error.new",
        "collection.function_check",
        "collection.int_check",
    ] {
        let (_, arity) = libraries
            .natives(|candidate| candidate == name)
            .pop()
            .unwrap();
        libraries.agree_on_all(name, arity, &corpus);
        answers_as_the_general_call(&libraries, name, &values);
    }
}
