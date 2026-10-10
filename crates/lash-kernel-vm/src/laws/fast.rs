//! A native implementation's fast path (`K-LIB-006`, `K-CHG-003`): the
//! machine charges the call it answers by the formula of the function's
//! definition, through a form of that formula derived when the library is
//! prepared.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_kernel_doc::{
    self as doc, ErrorValue, Float, FunctionRegistry, Identity, Integer, NativeCall, NativeError,
    NativeFunction, NativeHeap, ObjectId, TaskId, Timestamp, Value, parse_definition,
    parse_document,
};
use num_bigint::BigInt;

use super::embedder::{ROOMY, World};
use crate::compile::{Charge, Fast, Plan, Source};
use crate::data::{deep_size, magnitude, nested_size, size};
use crate::heap::{Heap, Obj, Table};
use crate::{End, KernelMachine, Layout, Machine, PreparedLibrary, Program, Start, Step, Target};

/// What `plan` computes for a call's values, measured as the machine
/// measures them.
fn amount(heap: &Heap, plan: &Plan, args: &[Value], result: &Value) -> u64 {
    plan.evaluate(|source, measure| {
        let value = match source {
            Source::Arg(index) => args.get(index),
            Source::Result => Some(result),
            Source::Nothing => None,
        };
        match (value, measure) {
            (None, _) => 0,
            (Some(value), doc::Measure::Size) => size(heap, value),
            (Some(value), doc::Measure::DeepSize) => deep_size(heap, value),
            (Some(value), doc::Measure::NestedSize) => nested_size(heap, value),
            (Some(value), doc::Measure::Magnitude) => magnitude(value),
        }
    })
}

/// A small deterministic generator, so that a failure names its case.
struct Draw(u64);

impl Draw {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % bound as u64) as usize
    }

    fn formula(&mut self, depth: u32) -> doc::Formula {
        let operand = |draw: &mut Self| match draw.below(4) {
            0 => doc::Operand::Param(doc::Name::new("x")),
            1 => doc::Operand::Param(doc::Name::new("y")),
            2 => doc::Operand::Result,
            // A parameter the function does not have.
            _ => doc::Operand::Param(doc::Name::new("z")),
        };
        let pick = if depth == 0 {
            self.below(5)
        } else {
            self.below(9)
        };
        match pick {
            0 => doc::Formula::Constant([0, 1, 2, 7, u64::MAX][self.below(5)]),
            1 => doc::Formula::Size(operand(self)),
            2 => doc::Formula::DeepSize(operand(self)),
            3 => doc::Formula::NestedSize(operand(self)),
            4 => doc::Formula::Magnitude(operand(self)),
            combine => {
                let terms = (0..self.below(4))
                    .map(|_| self.formula(depth - 1))
                    .collect();
                match combine {
                    5 => doc::Formula::Sum(terms),
                    6 => doc::Formula::Product(terms),
                    7 => doc::Formula::Max(terms),
                    _ => doc::Formula::Min(terms),
                }
            }
        }
    }
}

/// Values of every kind, with the edges a measure turns on: -0, NaN,
/// empty and large texts and integers, flat and deeply nested tuples and
/// errors, and empty and nested heap objects.
fn values(heap: &mut Heap) -> Vec<Value> {
    let mut values = vec![
        Value::Null,
        Value::Absent,
        Value::Bool(true),
        Value::Int(Integer::from(0)),
        Value::Int(Integer::from(5)),
        Value::Int(Integer::from(-3)),
        Value::Int(Integer::new(BigInt::from(1u8) << 130)),
        Value::Float(Float::new(0.0)),
        Value::Float(Float::new(-0.0)),
        Value::Float(Float::new(f64::NAN)),
        Value::Float(Float::new(f64::INFINITY)),
        Value::Float(Float::new(2.5)),
        Value::Float(Float::new(1e300)),
        Value::text(""),
        Value::text("abc"),
        Value::Bytes(doc::Bytes::new(vec![1u8, 2, 3])),
        Value::Timestamp(Timestamp {
            nanoseconds: Integer::from(7),
        }),
        Value::Task(TaskId(0)),
        Value::Function(doc::Name::new("f")),
        Value::Ref(Identity::Object(ObjectId(0))),
        Value::Tuple(Arc::from([])),
        Value::Tuple(Arc::from([Value::Int(Integer::from(1)), Value::text("ab")])),
        Value::Error(Arc::new(ErrorValue {
            kind: "kind".to_owned(),
            message: "message".to_owned(),
            data: Value::Tuple(Arc::from([Value::Null, Value::Bool(false)])),
        })),
    ];
    let mut deep = Value::Int(Integer::from(1));
    for _ in 0..100 {
        deep = Value::Tuple(Arc::from([deep, Value::text("x")]));
    }
    values.push(deep);
    let empty = heap.insert(Obj::List(Vec::new()));
    values.push(Value::List(empty));
    let inner = heap.insert(Obj::List(vec![Value::Int(Integer::from(2))]));
    let outer = heap.insert(Obj::List(vec![
        Value::Int(Integer::from(1)),
        Value::List(inner),
        Value::Tuple(Arc::from([Value::List(inner)])),
    ]));
    values.push(Value::List(outer));
    let record = heap.insert(Obj::Record(vec![("a".to_owned(), Value::List(outer))]));
    values.push(Value::Record(record));
    let mut table = Table::default();
    table.insert(
        crate::heap::Key::Int(Integer::from(1)),
        Value::Int(Integer::from(1)),
        Value::text("one"),
    );
    values.push(Value::Map(heap.insert(Obj::Map(table))));
    values.push(Value::Set(heap.insert(Obj::Set(Table::default()))));
    values
}

/// `K-CHG-003`: for any formula and any arguments and result, the charge
/// the fast path derives from the formula is what the formula computes.
/// Both of its forms are reached: an amount fixed when the library was
/// prepared, and the formula as it computes for values that hold nothing.
#[test]
fn a_fast_paths_charge_is_what_its_formula_computes() {
    let mut heap = Heap::new(u64::MAX);
    let values = values(&mut heap);
    let params: Vec<doc::Param> = ["x", "y"]
        .into_iter()
        .map(|name| doc::Param {
            name: doc::Name::new(name),
            ty: doc::Type::Any,
            optional: false,
        })
        .collect();
    let mut draw = Draw(0x5eed);
    let (mut fixed, mut shallow, mut whole) = (0, 0, 0);
    for case in 0..600 {
        let formula = draw.formula(3);
        let charge = Plan::new(&formula, &params);
        let fast = Fast::new(&formula, &params, &charge);
        for _ in 0..300 {
            let args = [
                values[draw.below(values.len())].clone(),
                values[draw.below(values.len())].clone(),
            ];
            let result = &values[draw.below(values.len())];
            let expected = amount(&heap, &charge, &args, result);
            let derived = match fast.charge(&charge, &args, result) {
                Charge::Units(units) => {
                    fixed += 1;
                    units
                }
                Charge::Plan(plan) => {
                    if std::ptr::eq(plan, &charge) {
                        whole += 1;
                    } else {
                        shallow += 1;
                    }
                    amount(&heap, plan, &args, result)
                }
            };
            assert_eq!(
                derived, expected,
                "case {case}: {formula:?} of {args:?} returning {result:?}"
            );
        }
    }
    assert!(fixed > 0 && shallow > 0 && whole > 0);
}

/// How many calls `probe.echo` answered through its general call.
static GENERAL_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Returns its argument, through either path.
struct Echo;

impl NativeFunction for Echo {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        GENERAL_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(call.args[0].clone())
    }

    fn fast(&self, args: &[Value], _heap: &dyn NativeHeap) -> Option<Value> {
        Some(args[0].clone())
    }
}

/// What a run of `main` that calls `probe.echo`, registered with `charge`,
/// on `argument` is charged, in the natural layout and in one that takes
/// every native's general call, with the general calls each made.
fn echo_charges(charge: &str, argument: &str) -> [(u64, usize); 2] {
    let mut registry = FunctionRegistry::new();
    let definition = parse_definition(&format!(
        "function probe.echo(x: Any) -> Any\nkernel 1\ncharge {charge}\nnative\n"
    ))
    .unwrap();
    let echo = registry.register(definition, Some(Arc::new(Echo))).unwrap();
    let registry = Arc::new(registry);
    let document = Arc::new(
        parse_document(&format!(
            "kernel 1\nnumbers by_spelling\nuse probe.echo = @{echo}\n\
             main {{ let x = {argument} return probe.echo(x) }}"
        ))
        .unwrap(),
    );
    [Layout(0), Layout(1)].map(|layout| {
        let program = Program {
            document: Arc::clone(&document),
            library: PreparedLibrary::with_layout(Arc::clone(&registry), layout),
        };
        let before = GENERAL_CALLS.load(Ordering::SeqCst);
        let start = Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: Default::default(),
        };
        let mut machine = KernelMachine::start_with_layout(program, ROOMY, start, layout).unwrap();
        let step = machine.run(&mut World::default(), u64::MAX).unwrap();
        assert!(matches!(step, Step::Ended(End::Finished(_))), "{step:?}");
        (
            machine.meters().charged,
            GENERAL_CALLS.load(Ordering::SeqCst) - before,
        )
    })
}

/// `K-CHG-003`: a fast path is charged by the formula of the definition
/// its library was prepared from. One implementation registered under two
/// formulas is charged each one's amount, on a scalar and on a nested
/// argument, and the general call is charged the same.
#[test]
fn a_fast_path_is_charged_by_the_formula_it_was_prepared_from() {
    let few = "sum(1, deep(x))";
    let more = "product(3, sum(deep(x), deep(result)))";
    // 5 is of size 2, and the tuple of deep size 3 + 2 + 3 + 2 + 2 = 12.
    for (argument, few_units, more_units) in [("5", 3, 12), ("(1, (2, 3))", 13, 72)] {
        let [(few_fast, few_general_calls), (few_general, one)] = echo_charges(few, argument);
        let [(more_fast, no_general_calls), (more_general, _)] = echo_charges(more, argument);
        assert_eq!(
            (few_general_calls, no_general_calls, one),
            (0, 0, 1),
            "the natural layout takes the fast path"
        );
        assert_eq!(few_fast, few_general);
        assert_eq!(more_fast, more_general);
        assert_eq!(more_fast - few_fast, more_units - few_units, "{argument}");
    }
}
