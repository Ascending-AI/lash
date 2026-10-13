//! A native implementation's fast path (`K-LIB-006`, `K-CHG-003`): the
//! machine charges the call it answers by the formula of the function's
//! definition, through a form of that formula derived when the library is
//! prepared.

use std::sync::Arc;

use lash_kernel_doc::{
    self as doc, ErrorValue, Float, Identity, Integer, ObjectId, TaskId, Timestamp, Value,
    parse_definition,
};
use num_bigint::BigInt;

use crate::compile::{Charge, Fast, Plan, Source};
use crate::data::{deep_size, magnitude, nested_size, size};
use crate::heap::{Heap, Obj, Table};

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

/// `K-CHG-003`: sums with a minimum pair compute the declared formula,
/// including mixed sizes, zero, saturation, missing arguments and repeated
/// measurements. Both a whole charge and its shallow form use this path.
#[test]
fn minimum_sum_charges_equal_the_declared_formula() {
    let heap = Heap::new(u64::MAX);
    let params: Vec<doc::Param> = ["x", "y"]
        .into_iter()
        .map(|name| doc::Param {
            name: doc::Name::new(name),
            ty: doc::Type::Any,
            optional: false,
        })
        .collect();
    let values = [
        Value::Absent,
        Value::Float(Float::new(f64::NAN)),
        Value::Int(Integer::from(0)),
        Value::Int(Integer::from(1)),
        Value::Int(Integer::from(-1)),
        Value::Int(Integer::new(BigInt::from(u64::MAX))),
        Value::Int(Integer::new(-BigInt::from(u64::MAX))),
        Value::Int(Integer::new(BigInt::from(1u8) << 64)),
        Value::Tuple(Arc::from([Value::Null, Value::text("xy")])),
        Value::Tuple(Arc::from([Value::Tuple(Arc::from([Value::Absent]))])),
    ];
    let mut draw = Draw(0x5917);

    let x = doc::Operand::Param(doc::Name::new("x"));
    let y = doc::Operand::Param(doc::Name::new("y"));
    let pair = doc::Formula::Min(vec![
        doc::Formula::Size(x.clone()),
        doc::Formula::Size(y.clone()),
    ]);
    let mut formulas: Vec<_> = (0..600).map(|_| draw.formula(3)).collect();
    for constant in [0, 1, u64::MAX] {
        let tail = doc::Formula::DeepSize(doc::Operand::Result);
        formulas.extend([
            doc::Formula::Sum(vec![
                pair.clone(),
                tail.clone(),
                doc::Formula::Constant(constant),
            ]),
            doc::Formula::Sum(vec![
                tail.clone(),
                pair.clone(),
                doc::Formula::Constant(constant),
            ]),
            doc::Formula::Sum(vec![
                pair.clone(),
                doc::Formula::NestedSize(x.clone()),
                doc::Formula::NestedSize(y.clone()),
                tail.clone(),
                doc::Formula::Constant(constant),
            ]),
            doc::Formula::Sum(vec![pair.clone(), pair.clone(), tail]),
        ]);
    }
    for (case, formula) in formulas.into_iter().enumerate() {
        let charge = Plan::new(&formula, &params);
        let fast = Fast::new(&formula, &params, &charge);
        for x in &values {
            for y in &values {
                for result in &values {
                    // A missing argument measures 0, not absent's size 1.
                    for args in [&[x.clone(), y.clone()][..], &[][..]] {
                        let expected = formula.evaluate(&mut |operand, measure| {
                            let value = match operand {
                                doc::Operand::Result => Some(result),
                                doc::Operand::Param(name) => params
                                    .iter()
                                    .position(|param| param.name == *name)
                                    .and_then(|index| args.get(index)),
                            };
                            match (value, measure) {
                                (None, _) => 0,
                                (Some(value), doc::Measure::Size) => size(&heap, value),
                                (Some(value), doc::Measure::DeepSize) => deep_size(&heap, value),
                                (Some(value), doc::Measure::NestedSize) => {
                                    nested_size(&heap, value)
                                }
                                (Some(value), doc::Measure::Magnitude) => magnitude(value),
                            }
                        });
                        let actual = match fast.charge(&charge, args, result) {
                            Charge::Units(units) => units,
                            Charge::Plan(plan) => amount(&heap, plan, args, result),
                        };
                        assert_eq!(
                            actual, expected,
                            "case {case}: {formula:?}, {args:?}, {result:?}"
                        );
                    }
                }
            }
        }
    }
}

/// K-CHG-003/004: a small integer measures one for zero and two otherwise.
/// Preparation closes over every combination, without evaluating a plan at call time.
#[test]
fn small_integer_charges_are_closed_and_invariant() {
    let heap = Heap::new(u64::MAX);
    let definition = parse_definition("function probe.add(x: Int, y: Int) -> Int\nkernel 1\ncharge sum(1, product(sum(deep(x), deep(y)), sum(deep(x), deep(y))), deep(result))\nnative\n").unwrap();
    let charge = Plan::new(&definition.charge, &definition.signature.params);
    let fast = Fast::new(&definition.charge, &definition.signature.params, &charge);
    for a in [0, 1, -1, i64::MIN, i64::MAX] {
        for b in [0, 1, -1, i64::MIN, i64::MAX] {
            let args = [Value::Int(Integer::from(a)), Value::Int(Integer::from(b))];
            for r in [0, 1, -1, i64::MIN, i64::MAX] {
                let result = Value::Int(Integer::from(r));
                let Charge::Units(units) = fast.charge(&charge, &args, &result) else {
                    panic!("small integers must have a closed charge");
                };
                assert_eq!(units, amount(&heap, &charge, &args, &result));
            }
        }
    }
}
