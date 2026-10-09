//! One witness per numbered rule/declared library edge, plus the strict-domain
//! law that checks every operand of every catalogue entry (`K-FN-007`).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::sync::Arc;

use lash_kernel_doc::{
    Bytes, Element, ErrorValue, Float, FunctionRegistry, Handle, Identity, Integer, Name,
    NativeCall, NativeError, NativeHeap, Object, ObjectId, TaskId, Timestamp, Type, Value,
    WorkCounter,
};
use num_bigint::BigInt;
use num_traits::One;

use crate::{
    Key, compare, equal, float_to_integer, integer_to_float, numbers, register_numbers, same,
};

#[derive(Default)]
pub(crate) struct Heap(BTreeMap<ObjectId, Object>, pub(crate) Room);

/// A reservation spy: it adds up what a call reserves, a value at
/// [`Room::VALUE`] bytes, and refuses the reservation that passes its limit.
#[derive(Default)]
pub(crate) struct Room {
    pub(crate) limit: Option<u64>,
    pub(crate) reserved: u64,
}

impl Room {
    pub(crate) const VALUE: u64 = 16;

    pub(crate) fn of(limit: u64) -> Self {
        Self {
            limit: Some(limit),
            reserved: 0,
        }
    }

    pub(crate) fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let reserved = self
            .reserved
            .saturating_add(values.saturating_mul(Self::VALUE))
            .saturating_add(bytes);
        if self.limit.is_some_and(|limit| reserved > limit) {
            return Err(NativeError::Memory);
        }
        self.reserved = reserved;
        Ok(())
    }
}

impl NativeHeap for Heap {
    fn len(&self, id: ObjectId) -> usize {
        match self.0.get(&id) {
            Some(Object::List(v) | Object::Set(v)) => v.len(),
            Some(Object::Map(v)) => v.len(),
            Some(Object::Record(v)) => v.len(),
            _ => 0,
        }
    }
    fn list_get(&self, id: ObjectId, index: usize) -> Option<Value> {
        if let Some(Object::List(values)) = self.0.get(&id) {
            values.get(index).cloned()
        } else {
            None
        }
    }
    fn map_get(&self, id: ObjectId, key: &Value) -> Option<Value> {
        let key = Key::new(key.clone()).ok()?;
        if let Some(Object::Map(entries)) = self.0.get(&id) {
            entries
                .iter()
                .find(|(candidate, _)| Key::new(candidate.clone()).ok().as_ref() == Some(&key))
                .map(|(_, value)| value.clone())
        } else {
            None
        }
    }
    fn set_contains(&self, id: ObjectId, member: &Value) -> bool {
        let Ok(key) = Key::new(member.clone()) else {
            return false;
        };
        matches!(self.0.get(&id), Some(Object::Set(values)) if values.iter().any(|value| Key::new(value.clone()).ok().as_ref() == Some(&key)))
    }
    fn record_get(&self, id: ObjectId, field: &str) -> Option<Value> {
        if let Some(Object::Record(fields)) = self.0.get(&id) {
            fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value.clone())
        } else {
            None
        }
    }
    fn visit(&self, id: ObjectId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) {
        match self.0.get(&id) {
            Some(Object::List(values) | Object::Set(values)) => {
                for value in values {
                    if visitor(Element::Item(value)).is_break() {
                        break;
                    }
                }
            }
            Some(Object::Map(entries)) => {
                for (key, value) in entries {
                    if visitor(Element::Entry { key, value }).is_break() {
                        break;
                    }
                }
            }
            Some(Object::Record(fields)) => {
                for (name, value) in fields {
                    if visitor(Element::Field { name, value }).is_break() {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    fn allocate(&mut self, object: Object) -> Result<ObjectId, NativeError> {
        let id = ObjectId(self.0.len() as u64 + 1);
        self.0.insert(id, object);
        Ok(id)
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        self.1.reserve(values, bytes)
    }
}

fn int(value: i64) -> Value {
    Value::Int(Integer::from(value))
}
fn big(value: BigInt) -> Value {
    Value::Int(Integer::new(value))
}
fn float(value: f64) -> Value {
    Value::Float(Float::new(value))
}
fn tuple(values: Vec<Value>) -> Value {
    Value::Tuple(values.into())
}
fn error_kind(error: NativeError) -> String {
    match error {
        NativeError::Raised(error) => error.kind,
        other => panic!("expected raise, got {other:?}"),
    }
}
fn invoke(name: &str, args: &[Value]) -> Result<Value, NativeError> {
    invoke_heap(name, args, &mut Heap::default())
}
fn invoke_heap(
    name: &str,
    args: &[Value],
    heap: &mut dyn NativeHeap,
) -> Result<Value, NativeError> {
    let (definition, native) = numbers()
        .into_iter()
        .find(|(definition, _)| definition.name.as_str() == name)
        .unwrap();
    let mut counter = WorkCounter::new(
        definition
            .guard
            .map(|guard| guard.limit.evaluate(&mut |_, _| 0)),
    );
    native.call(NativeCall {
        args,
        heap,
        counter: &mut counter,
    })
}

#[test]
fn k_num_001_integer_arithmetic_is_arbitrary_precision() {
    let a = BigInt::one() << 2048usize;
    assert_eq!(
        invoke("int.add", &[big(a.clone()), int(1)]).unwrap(),
        big(&a + 1)
    );
    assert_eq!(
        invoke("int.sub", &[big(a.clone()), int(1)]).unwrap(),
        big(&a - 1)
    );
    assert_eq!(
        invoke("int.mul", &[big(a.clone()), big(a.clone())]).unwrap(),
        big(&a * &a)
    );
    assert_eq!(invoke("int.neg", &[big(a.clone())]).unwrap(), big(-a));
}
#[test]
fn k_num_002_binary64_rounds_ties_to_even() {
    assert_eq!(
        invoke("float.add", &[float(9007199254740992.0), float(1.0)]).unwrap(),
        float(9007199254740992.0)
    );
    assert_eq!(
        invoke("float.mul", &[float(f64::MAX), float(2.0)]).unwrap(),
        float(f64::INFINITY)
    );
}
#[test]
fn k_num_003_mixed_arithmetic_converts_once_and_refuses_overflow() {
    assert_eq!(
        invoke(
            "num.sub",
            &[int(9007199254740993), float(9007199254740992.0)]
        )
        .unwrap(),
        float(0.0)
    );
    let threshold = (BigInt::one() << 1024usize) - (BigInt::one() << 970usize);
    assert_eq!(
        integer_to_float(&Integer::new(&threshold - 1))
            .unwrap()
            .get(),
        f64::MAX
    );
    assert_eq!(
        error_kind(integer_to_float(&Integer::new(threshold)).unwrap_err()),
        "number_range"
    );
    assert_eq!(
        error_kind(invoke("num.add", &[big(BigInt::one() << 1100usize), float(0.0)]).unwrap_err()),
        "number_range"
    );
}
#[test]
fn k_num_004_integer_division_rounds_the_exact_quotient_once() {
    let enormous = BigInt::one() << 2000usize;
    assert_eq!(
        invoke("int.div", &[big(enormous.clone()), big(enormous.clone())]).unwrap(),
        float(1.0)
    );
    assert_eq!(
        invoke("div", &[int(9007199254740993), int(3)]).unwrap(),
        float(3002399751580331.0)
    );
    assert_eq!(
        invoke("div", &[int(1), big(BigInt::one() << 1075usize)]).unwrap(),
        float(0.0)
    );
    assert_eq!(
        invoke("div", &[int(3), big(BigInt::one() << 1075usize)]).unwrap(),
        float(f64::from_bits(2))
    );
    assert_eq!(
        error_kind(invoke("div", &[int(1), int(0)]).unwrap_err()),
        "division_by_zero"
    );
    assert_eq!(
        error_kind(invoke("div", &[big(enormous), int(1)]).unwrap_err()),
        "number_range"
    );
    assert_eq!(
        invoke("div", &[float(1.0), int(0)]).unwrap(),
        float(f64::INFINITY)
    );
    assert_eq!(
        invoke("div", &[float(0.0), float(0.0)]).unwrap(),
        float(f64::NAN)
    );
}
#[test]
fn k_num_005_floor_division_remainder_has_divisors_sign() {
    for (a, b, q, r) in [(-7, 3, -3, 2), (7, -3, -3, -2), (-7, -3, 2, -1)] {
        assert_eq!(invoke("div_floor", &[int(a), int(b)]).unwrap(), int(q));
        assert_eq!(invoke("rem_floor", &[int(a), int(b)]).unwrap(), int(r));
        assert_eq!(a, q * b + r);
    }
    for name in ["div_floor", "rem_floor"] {
        assert_eq!(
            error_kind(invoke(name, &[int(1), int(0)]).unwrap_err()),
            "division_by_zero"
        );
    }
}
#[test]
fn k_num_006_truncating_remainder_has_dividends_sign() {
    for (a, b, q, r) in [(-7, 3, -2, -1), (7, -3, -2, 1), (-7, -3, 2, -1)] {
        assert_eq!(invoke("div_trunc", &[int(a), int(b)]).unwrap(), int(q));
        assert_eq!(invoke("rem_trunc", &[int(a), int(b)]).unwrap(), int(r));
        assert_eq!(a, q * b + r);
    }
    for name in ["div_trunc", "rem_trunc"] {
        assert_eq!(
            error_kind(invoke(name, &[int(1), int(0)]).unwrap_err()),
            "division_by_zero"
        );
    }
}
#[test]
fn k_num_007_float_remainders_and_zero_division_are_explicit() {
    assert_eq!(
        invoke("rem_floor", &[float(-7.5), int(3)]).unwrap(),
        float(1.5)
    );
    assert_eq!(
        invoke("div_floor", &[float(-7.5), int(3)]).unwrap(),
        float(-3.0)
    );
    assert_eq!(
        invoke("rem_trunc", &[float(-7.5), int(3)]).unwrap(),
        float(-1.5)
    );
    assert_eq!(
        invoke("div_trunc", &[float(-7.5), int(3)]).unwrap(),
        float(-2.0)
    );
    assert_eq!(
        invoke("rem_floor", &[float(6.0), float(-3.0)]).unwrap(),
        float(-0.0)
    );
    assert_eq!(
        invoke("rem_trunc", &[float(-6.0), float(3.0)]).unwrap(),
        float(-0.0)
    );
    for name in ["div_floor", "div_trunc"] {
        assert_eq!(
            invoke(name, &[float(1.0), float(0.0)]).unwrap(),
            float(f64::INFINITY)
        );
    }
    for name in ["rem_floor", "rem_trunc"] {
        assert_eq!(
            invoke(name, &[float(1.0), float(0.0)]).unwrap(),
            float(f64::NAN)
        );
    }
}
#[test]
fn k_num_008_arithmetic_never_coerces() {
    for value in [Value::Null, Value::Bool(true), Value::text("1")] {
        assert_eq!(
            error_kind(invoke("add", &[value.clone(), int(1)]).unwrap_err()),
            "type_error"
        );
        assert_eq!(
            error_kind(invoke("lt", &[value, int(1)]).unwrap_err()),
            "type_error"
        );
    }
}
#[test]
fn k_val_020_cross_kind_values_are_unequal_without_coercion() {
    for (a, b) in [
        (Value::Null, Value::Absent),
        (Value::Bool(true), int(1)),
        (Value::text("1"), int(1)),
        (tuple(vec![int(1)]), Value::List(ObjectId(1))),
    ] {
        assert!(!equal(&a, &b, &Heap::default()));
    }
}
#[test]
fn k_val_021_numbers_compare_exactly_even_beyond_float_range() {
    let heap = Heap::default();
    assert!(!equal(
        &int(9007199254740993),
        &float(9007199254740992.0),
        &heap
    ));
    assert_eq!(
        compare(&int(9007199254740993), &float(9007199254740992.0), &heap).unwrap(),
        Some(Ordering::Greater)
    );
    assert!(!equal(
        &big(BigInt::one() << 1100usize),
        &float(f64::INFINITY),
        &heap
    ));
    assert_eq!(
        compare(
            &big(BigInt::one() << 1100usize),
            &float(f64::INFINITY),
            &heap
        )
        .unwrap(),
        Some(Ordering::Less)
    );
    assert!(equal(&int(1), &float(1.0), &heap));
    assert!(equal(&float(-0.0), &float(0.0), &heap));
    assert!(!equal(&float(f64::NAN), &float(f64::NAN), &heap));
    assert_eq!(
        compare(&int(-1), &float(-0.5), &heap).unwrap(),
        Some(Ordering::Less)
    );
    assert_eq!(
        compare(&int(0), &float(f64::from_bits(1)), &heap).unwrap(),
        Some(Ordering::Less)
    );
}
#[test]
fn k_val_022_scalar_contents_define_equality() {
    for value in [
        Value::Null,
        Value::Absent,
        Value::Bool(false),
        Value::text("🙂"),
        Value::Bytes(Bytes::new(vec![0, 255])),
        Value::Timestamp(Timestamp {
            nanoseconds: Integer::from(1),
        }),
    ] {
        assert!(equal(&value, &value.clone(), &Heap::default()));
    }
}
#[test]
fn k_val_023_tuples_compare_members_and_nan_stays_unequal() {
    assert!(equal(
        &tuple(vec![int(1), Value::text("a")]),
        &tuple(vec![float(1.0), Value::text("a")]),
        &Heap::default()
    ));
    let nan_tuple = tuple(vec![float(f64::NAN)]);
    assert!(!equal(&nan_tuple, &nan_tuple, &Heap::default()));
    assert!(!equal(
        &tuple(vec![int(1), int(2)]),
        &tuple(vec![int(2), int(1)]),
        &Heap::default()
    ));
}
#[test]
fn k_val_024_container_contents_ignore_map_set_and_record_order() {
    let mut heap = Heap::default();
    let list_a = Value::List(heap.allocate(Object::List(vec![int(1)])).unwrap());
    let list_b = Value::List(heap.allocate(Object::List(vec![float(1.0)])).unwrap());
    assert!(equal(&list_a, &list_b, &heap));
    for (a, b, wrap) in [
        (
            Object::Map(vec![(int(1), int(2)), (float(f64::NAN), list_a.clone())]),
            Object::Map(vec![
                (float(f64::NAN), list_b.clone()),
                (float(1.0), float(2.0)),
            ]),
            Value::Map as fn(ObjectId) -> Value,
        ),
        (
            Object::Set(vec![int(1), int(2), float(f64::NAN)]),
            Object::Set(vec![float(f64::NAN), float(2.0), float(1.0)]),
            Value::Set,
        ),
        (
            Object::Record(vec![("a".into(), int(1)), ("b".into(), int(2))]),
            Object::Record(vec![("b".into(), float(2.0)), ("a".into(), float(1.0))]),
            Value::Record,
        ),
    ] {
        let a = wrap(heap.allocate(a).unwrap());
        let b = wrap(heap.allocate(b).unwrap());
        assert!(equal(&a, &b, &heap));
    }
    let nan_list = Value::List(heap.allocate(Object::List(vec![float(f64::NAN)])).unwrap());
    assert!(!equal(&nan_list, &nan_list, &heap));
}
#[test]
fn k_val_025_errors_functions_handles_closures_tasks_and_refs() {
    let heap = Heap::default();
    let a = Value::Error(Arc::new(ErrorValue {
        kind: "example".into(),
        message: "message".into(),
        data: int(1),
    }));
    let b = Value::Error(Arc::new(ErrorValue {
        kind: "example".into(),
        message: "message".into(),
        data: float(1.0),
    }));
    assert!(equal(&a, &b, &heap));
    for value in [
        Value::Function(Name::new("f")),
        Value::Handle(Arc::new(Handle {
            kind: "file".into(),
            id: "one".into(),
        })),
        Value::Closure(ObjectId(1)),
        Value::Task(TaskId(1)),
        Value::Ref(Identity::Object(ObjectId(1))),
    ] {
        assert!(equal(&value, &value, &heap));
    }
    assert!(!equal(
        &Value::Closure(ObjectId(1)),
        &Value::Closure(ObjectId(2)),
        &heap
    ));
    assert!(!equal(
        &Value::Task(TaskId(1)),
        &Value::Task(TaskId(2)),
        &heap
    ));
}
#[test]
fn k_val_026_cyclic_equality_uses_active_pairs_and_finite_paths() {
    let mut heap = Heap::default();
    heap.0.insert(
        ObjectId(1),
        Object::List(vec![Value::List(ObjectId(1)), int(1)]),
    );
    heap.0.insert(
        ObjectId(2),
        Object::List(vec![Value::List(ObjectId(3)), float(1.0)]),
    );
    heap.0.insert(
        ObjectId(3),
        Object::List(vec![Value::List(ObjectId(2)), int(1)]),
    );
    assert!(equal(
        &Value::List(ObjectId(1)),
        &Value::List(ObjectId(2)),
        &heap
    ));
    heap.0.insert(
        ObjectId(3),
        Object::List(vec![Value::List(ObjectId(2)), int(2)]),
    );
    assert!(!equal(
        &Value::List(ObjectId(1)),
        &Value::List(ObjectId(2)),
        &heap
    ));
}
#[test]
fn k_val_027_same_is_datum_or_object_identity() {
    assert!(!same(&int(1), &float(1.0)));
    assert!(!same(&float(-0.0), &float(0.0)));
    assert!(same(
        &float(f64::NAN),
        &float(f64::from_bits(0xfff0000000000001))
    ));
    assert!(!same(&Value::List(ObjectId(1)), &Value::List(ObjectId(2))));
    assert!(same(
        &tuple(vec![float(f64::NAN), Value::List(ObjectId(1))]),
        &tuple(vec![float(f64::NAN), Value::List(ObjectId(1))])
    ));
}
#[test]
fn k_val_028_equal_numeric_keys_share_hashes() {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let hash = |key: &Key| {
        let mut state = DefaultHasher::new();
        key.hash(&mut state);
        state.finish()
    };
    for (a, b) in [
        (int(1), float(1.0)),
        (int(0), float(-0.0)),
        (
            big(BigInt::one() << 1000usize),
            float(libm::scalbn(1.0, 1000)),
        ),
        (float(f64::NAN), float(f64::from_bits(0x7ff0000000000001))),
        (
            tuple(vec![int(1), float(f64::NAN)]),
            tuple(vec![float(1.0), float(f64::NAN)]),
        ),
    ] {
        let a = Key::new(a).unwrap();
        let b = Key::new(b).unwrap();
        assert_eq!(a, b);
        assert_eq!(hash(&a), hash(&b));
    }
}
#[test]
fn k_val_029_order_is_exact_and_lexicographic_with_prefix_first() {
    let heap = Heap::default();
    for (a, b) in [
        (Value::Bool(false), Value::Bool(true)),
        (
            Value::Bytes(Bytes::new(vec![1])),
            Value::Bytes(Bytes::new(vec![1, 0])),
        ),
        (
            Value::Timestamp(Timestamp {
                nanoseconds: Integer::from(1),
            }),
            Value::Timestamp(Timestamp {
                nanoseconds: Integer::from(2),
            }),
        ),
        (
            tuple(vec![Value::Null, int(1)]),
            tuple(vec![Value::Null, float(2.0)]),
        ),
    ] {
        assert_eq!(compare(&a, &b, &heap).unwrap(), Some(Ordering::Less));
    }
    assert_eq!(
        compare(
            &tuple(vec![int(1)]),
            &tuple(vec![float(1.0), int(0)]),
            &heap
        )
        .unwrap(),
        Some(Ordering::Less)
    );
    assert_eq!(
        error_kind(compare(&Value::Null, &Value::Null, &heap).unwrap_err()),
        "type_error"
    );
}
#[test]
fn k_val_030_text_orders_by_unicode_scalar_value() {
    assert_eq!(
        compare(
            &Value::text("\u{10000}"),
            &Value::text("\u{e000}"),
            &Heap::default()
        )
        .unwrap(),
        Some(Ordering::Greater)
    );
}
#[test]
fn k_val_031_float_text_is_shortest_round_trip_with_pinned_layout() {
    for (number, text) in [
        (0.0, "0.0"),
        (-0.0, "-0.0"),
        (f64::NAN, "nan"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
        (1.0, "1.0"),
        (0.1, "0.1"),
        (0.0001, "0.0001"),
        (0.00009999, "9.999e-5"),
        (1e15, "1000000000000000.0"),
        (1e16, "1e16"),
        (1.5e300, "1.5e300"),
        (f64::from_bits(1), "5e-324"),
    ] {
        assert_eq!(
            invoke("float.to_text", &[float(number)]).unwrap(),
            Value::text(text)
        );
        assert_eq!(
            invoke("float.parse", &[Value::text(text)]).unwrap(),
            float(number)
        );
    }
}
#[test]
fn k_val_032_integer_text_is_canonical_decimal() {
    assert_eq!(
        invoke("num.to_text", &[int(-123)]).unwrap(),
        Value::text("-123")
    );
    assert_eq!(invoke("num.to_text", &[int(0)]).unwrap(), Value::text("0"));
}
#[test]
fn k_val_033_nan_is_unordered_for_all_predicates() {
    for name in ["lt", "le", "gt", "ge"] {
        assert_eq!(
            invoke(name, &[float(f64::NAN), int(1)]).unwrap(),
            Value::Bool(false)
        );
    }
    assert_eq!(
        error_kind(invoke("compare", &[int(1), float(f64::NAN)]).unwrap_err()),
        "unordered"
    );
}
#[test]
fn k_key_001_only_immutable_keys_and_refs_are_admitted() {
    for value in [
        Value::Absent,
        Value::List(ObjectId(1)),
        Value::Map(ObjectId(1)),
        Value::Set(ObjectId(1)),
        Value::Record(ObjectId(1)),
        Value::Closure(ObjectId(1)),
        Value::Task(TaskId(1)),
        Value::Function(Name::new("f")),
        Value::Handle(Arc::new(Handle {
            kind: "h".into(),
            id: "1".into(),
        })),
        tuple(vec![Value::Absent]),
    ] {
        assert_eq!(error_kind(Key::new(value).unwrap_err()), "invalid_key");
    }
}
#[test]
fn k_key_002_equal_numbers_nan_and_mixed_tuples_are_one_key() {
    let mut map = HashMap::new();
    for (a, b) in [
        (int(1), float(1.0)),
        (float(f64::NAN), float(f64::NAN)),
        (
            tuple(vec![int(1), Value::text("x"), float(f64::NAN)]),
            tuple(vec![float(1.0), Value::text("x"), float(f64::NAN)]),
        ),
    ] {
        map.insert(Key::new(a).unwrap(), "first");
        assert_eq!(map.insert(Key::new(b).unwrap(), "last"), Some("first"));
    }
    assert_eq!(map.len(), 3);
    assert_ne!(
        Key::new(Value::Bool(true)).unwrap(),
        Key::new(int(1)).unwrap()
    );
}
#[test]
fn k_key_004_ref_takes_identity_and_rejects_immutable_values() {
    for value in [
        Value::List(ObjectId(1)),
        Value::Map(ObjectId(1)),
        Value::Set(ObjectId(1)),
        Value::Record(ObjectId(1)),
        Value::Closure(ObjectId(1)),
    ] {
        assert_eq!(
            invoke("ref", &[value]).unwrap(),
            Value::Ref(Identity::Object(ObjectId(1)))
        );
    }
    assert_eq!(
        invoke("ref", &[Value::Task(TaskId(1))]).unwrap(),
        Value::Ref(Identity::Task(TaskId(1)))
    );
    assert_eq!(
        error_kind(invoke("ref", &[int(1)]).unwrap_err()),
        "type_error"
    );
}
#[test]
fn n_pow_nonnegative_integer_exponents_are_exact_and_guarded() {
    assert_eq!(
        invoke("int.pow", &[int(2), int(1000)]).unwrap(),
        big(BigInt::one() << 1000usize)
    );
    assert_eq!(invoke("int.pow", &[int(0), int(0)]).unwrap(), int(1));
    assert_eq!(
        error_kind(invoke("int.pow", &[int(2), int(-1)]).unwrap_err()),
        "number_range"
    );
    assert!(matches!(
        invoke("int.pow", &[int(2), int(1 << 30)]),
        Err(NativeError::Guard(_))
    ));
}
/// `K-BND-001`, `K-LIB-007`: an integer power's size follows its exponent.
/// The guard counts its work; the room for each product is reserved before
/// the product is taken.
#[test]
fn n_pow_reserves_each_product_before_it_multiplies() {
    let args = [int(2), int(1 << 20)];
    let mut heap = Heap(BTreeMap::new(), Room::of(16 << 10));
    assert_eq!(
        invoke_heap("int.pow", &args, &mut heap),
        Err(NativeError::Memory)
    );
    let mut heap = Heap::default();
    let Ok(Value::Int(power)) = invoke_heap("int.pow", &args, &mut heap) else {
        panic!("the power fits a roomy heap");
    };
    assert!(heap.1.reserved >= power.as_bigint().bits().div_ceil(8));
}
#[test]
fn n_abs_neg_and_min_max_preserve_pinned_zero_and_nan_edges() {
    assert_eq!(invoke("float.abs", &[float(-0.0)]).unwrap(), float(0.0));
    assert_eq!(invoke("float.neg", &[float(0.0)]).unwrap(), float(-0.0));
    assert_eq!(
        invoke("min", &[int(9007199254740993), float(9007199254740992.0)]).unwrap(),
        float(9007199254740992.0)
    );
    assert_eq!(
        invoke("max", &[int(9007199254740993), float(9007199254740992.0)]).unwrap(),
        int(9007199254740993)
    );
    assert_eq!(
        invoke("min", &[float(0.0), float(-0.0)]).unwrap(),
        float(-0.0)
    );
    assert_eq!(
        invoke("max", &[float(-0.0), float(0.0)]).unwrap(),
        float(0.0)
    );
    assert_eq!(
        invoke("min", &[float(f64::NAN), float(1.0)]).unwrap(),
        float(f64::NAN)
    );
}
#[test]
fn n_round_modes_keep_kinds_nonfinite_values_and_signed_zero() {
    for (name, input, expected) in [
        ("floor", -1.2, -2.0),
        ("ceil", -1.2, -1.0),
        ("trunc", -1.2, -1.0),
        ("round_even", 2.5, 2.0),
        ("round_even", 3.5, 4.0),
        ("round_away", -2.5, -3.0),
        ("round_up", -2.5, -2.0),
        ("round_up", -0.5, -0.0),
        ("round_up", 9007199254740991.0, 9007199254740991.0),
        ("round_up", f64::from_bits(0x3fdfffffffffffff), 0.0),
    ] {
        assert_eq!(invoke(name, &[float(input)]).unwrap(), float(expected));
        assert_eq!(invoke(name, &[int(3)]).unwrap(), int(3));
        assert_eq!(
            invoke(name, &[float(f64::INFINITY)]).unwrap(),
            float(f64::INFINITY)
        );
        assert_eq!(invoke(name, &[float(f64::NAN)]).unwrap(), float(f64::NAN));
    }
    assert_eq!(invoke("sign", &[float(-0.0)]).unwrap(), float(-0.0));
}
#[test]
fn n_conversions_require_finite_integral_values_and_exact_range() {
    assert_eq!(
        float_to_integer(Float::new(libm::scalbn(1.0, 1000))).unwrap(),
        Integer::new(BigInt::one() << 1000usize)
    );
    assert_eq!(
        float_to_integer(Float::new(-0.0)).unwrap(),
        Integer::from(0)
    );
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.5] {
        assert_eq!(
            error_kind(float_to_integer(Float::new(value)).unwrap_err()),
            "number_range"
        );
    }
    assert_eq!(invoke("float.to_int", &[float(1.0)]).unwrap(), int(1));
    assert_eq!(
        invoke("int.to_float", &[int(9007199254740995)]).unwrap(),
        float(9007199254740996.0)
    );
}
#[test]
fn n_parse_and_radix_format_are_explicit_and_noncoercing() {
    assert_eq!(
        invoke("int.to_text", &[int(-255), int(16)]).unwrap(),
        Value::text("-ff")
    );
    assert_eq!(
        invoke("int.parse", &[Value::text("+00FF"), int(16)]).unwrap(),
        int(255)
    );
    assert_eq!(
        invoke("float.parse", &[Value::text("+.5e+1")]).unwrap(),
        float(5.0)
    );
    for value in [
        "", " 1", "1 ", "0x10", "1_000", "1e", "1.2.3", "Infinity", "NaN",
    ] {
        assert_eq!(
            error_kind(invoke("float.parse", &[Value::text(value)]).unwrap_err()),
            "number_parse"
        );
    }
    for value in ["", "0xff", "1_000", " 1", "1.0"] {
        assert_eq!(
            error_kind(invoke("int.parse", &[Value::text(value), int(16)]).unwrap_err()),
            "number_parse"
        );
    }
    assert_eq!(
        error_kind(invoke("float.parse", &[Value::text("1e9999")]).unwrap_err()),
        "number_range"
    );
    assert_eq!(
        invoke("float.parse", &[Value::text("-1e-9999")]).unwrap(),
        float(-0.0)
    );
    assert_eq!(
        error_kind(invoke("int.to_text", &[int(1), int(37)]).unwrap_err()),
        "number_range"
    );
}

macro_rules! math_edge {
    ($test:ident,$name:literal,[$($input:expr),+],$result:expr) => {
        #[test] fn $test() { assert_eq!(invoke(concat!("math.",$name),&[$(float($input)),+]).unwrap(),float($result)); }
    };
}
math_edge!(n_math_acos_outside_domain, "acos", [2.0], f64::NAN);
math_edge!(n_math_acosh_below_one, "acosh", [0.0], f64::NAN);
math_edge!(n_math_asin_outside_domain, "asin", [-2.0], f64::NAN);
math_edge!(
    n_math_asinh_negative_infinity,
    "asinh",
    [f64::NEG_INFINITY],
    f64::NEG_INFINITY
);
math_edge!(n_math_atan_negative_zero, "atan", [-0.0], -0.0);
math_edge!(n_math_atanh_pole, "atanh", [-1.0], f64::NEG_INFINITY);
math_edge!(n_math_cbrt_negative_input, "cbrt", [-8.0], -2.0);
math_edge!(n_math_cos_infinity, "cos", [f64::INFINITY], f64::NAN);
math_edge!(
    n_math_cosh_infinity,
    "cosh",
    [f64::NEG_INFINITY],
    f64::INFINITY
);
math_edge!(
    n_math_erf_negative_infinity,
    "erf",
    [f64::NEG_INFINITY],
    -1.0
);
math_edge!(n_math_erfc_infinity, "erfc", [f64::INFINITY], 0.0);
math_edge!(
    n_math_exp_negative_infinity,
    "exp",
    [f64::NEG_INFINITY],
    0.0
);
math_edge!(n_math_exp2_underflow, "exp2", [-1075.0], 0.0);
math_edge!(n_math_expm1_negative_zero, "expm1", [-0.0], -0.0);
math_edge!(
    n_math_gamma_negative_integer_pole,
    "gamma",
    [-1.0],
    f64::NAN
);
math_edge!(n_math_lgamma_integer_pole, "lgamma", [-1.0], f64::INFINITY);
math_edge!(n_math_log_zero_pole, "log", [0.0], f64::NEG_INFINITY);
math_edge!(n_math_log2_negative_input, "log2", [-1.0], f64::NAN);
math_edge!(
    n_math_log10_negative_zero_pole,
    "log10",
    [-0.0],
    f64::NEG_INFINITY
);
math_edge!(n_math_log1p_negative_zero, "log1p", [-0.0], -0.0);
math_edge!(n_math_sin_infinity, "sin", [f64::INFINITY], f64::NAN);
math_edge!(
    n_math_sinh_negative_infinity,
    "sinh",
    [f64::NEG_INFINITY],
    f64::NEG_INFINITY
);
math_edge!(n_math_sqrt_negative_zero, "sqrt", [-0.0], -0.0);
math_edge!(n_math_tan_infinity, "tan", [f64::INFINITY], f64::NAN);
math_edge!(
    n_math_tanh_negative_infinity,
    "tanh",
    [f64::NEG_INFINITY],
    -1.0
);
math_edge!(
    n_math_atan2_opposite_signed_zeros,
    "atan2",
    [-0.0, 0.0],
    -0.0
);
math_edge!(
    n_math_hypot_infinity_wins_over_nan,
    "hypot",
    [f64::NAN, f64::INFINITY],
    f64::INFINITY
);
math_edge!(n_math_copysign_zero, "copysign", [0.0, -1.0], -0.0);
math_edge!(
    n_math_nextafter_smallest_subnormal,
    "nextafter",
    [0.0, 1.0],
    f64::from_bits(1)
);
math_edge!(n_math_remainder_ties_to_even, "remainder", [7.0, 2.0], -1.0);
math_edge!(n_math_pow_nan_zero_exponent, "pow", [f64::NAN, 0.0], 1.0);
math_edge!(
    n_math_fma_rounds_once,
    "fma",
    [1.0000000000000002, 0.9999999999999998, -1.0],
    -libm::scalbn(1.0, -104)
);

#[test]
fn k_fn_007_every_strict_function_checks_every_operand_kind() {
    let mut registry = FunctionRegistry::new();
    register_numbers(&mut registry).unwrap();
    for (_, registered) in registry.iter() {
        let definition = &registered.definition;
        // Catalogue evidence also supplies exact identities to independent readers.
        println!("DEFINITION {}", definition.to_json().unwrap());
        let mut args: Vec<Value> = definition
            .signature
            .params
            .iter()
            .map(|param| match param.ty {
                Type::Int | Type::Number => int(2),
                Type::Float => float(2.0),
                Type::Text => Value::text("2"),
                _ => Value::Null,
            })
            .collect();
        for (index, param) in definition.signature.params.iter().enumerate() {
            if param.ty == Type::Any {
                continue;
            }
            let previous = args[index].clone();
            args[index] = Value::Null;
            let mut heap = Heap::default();
            let mut counter = WorkCounter::new(None);
            let result = registered.native.as_ref().unwrap().call(NativeCall {
                args: &args,
                heap: &mut heap,
                counter: &mut counter,
            });
            assert_eq!(
                error_kind(result.unwrap_err()),
                "type_error",
                "{} operand {index}",
                definition.name
            );
            args[index] = previous;
        }
    }
}

macro_rules! kind_case {
    ($test:ident,$value:expr,$name:literal) => {
        #[test]
        fn $test() {
            assert_eq!(invoke("kind", &[$value]).unwrap(), Value::text($name));
        }
    };
}
kind_case!(k_val_001_kind_null, Value::Null, "null");
kind_case!(k_val_001_kind_absent, Value::Absent, "absent");
kind_case!(k_val_001_kind_bool, Value::Bool(false), "bool");
kind_case!(k_val_001_kind_integer, int(1), "integer");
kind_case!(k_val_001_kind_float, float(1.0), "float");
kind_case!(k_val_001_kind_text, Value::text("x"), "text");
kind_case!(
    k_val_001_kind_bytes,
    Value::Bytes(Bytes::new(vec![1])),
    "bytes"
);
kind_case!(
    k_val_001_kind_timestamp,
    Value::Timestamp(Timestamp {
        nanoseconds: Integer::from(0)
    }),
    "timestamp"
);
kind_case!(k_val_001_kind_tuple, tuple(vec![int(1)]), "tuple");
kind_case!(k_val_001_kind_list, Value::List(ObjectId(1)), "list");
kind_case!(k_val_001_kind_map, Value::Map(ObjectId(1)), "map");
kind_case!(k_val_001_kind_set, Value::Set(ObjectId(1)), "set");
kind_case!(k_val_001_kind_record, Value::Record(ObjectId(1)), "record");
kind_case!(
    k_val_001_kind_closure,
    Value::Closure(ObjectId(1)),
    "closure"
);
kind_case!(
    k_val_001_kind_error,
    Value::Error(Arc::new(ErrorValue::new("example", "message"))),
    "error"
);
kind_case!(k_val_001_kind_task, Value::Task(TaskId(1)), "task");
kind_case!(
    k_val_001_kind_function,
    Value::Function(Name::new("f")),
    "function"
);
kind_case!(
    k_val_001_kind_handle,
    Value::Handle(Arc::new(Handle {
        kind: "h".into(),
        id: "1".into()
    })),
    "handle"
);
kind_case!(
    k_val_001_kind_ref,
    Value::Ref(Identity::Object(ObjectId(1))),
    "ref"
);
