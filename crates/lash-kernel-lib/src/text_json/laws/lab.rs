//! A native call on an arena bench, observed as main's laws observed it:
//! what the call reserves (a value at [`VALUE`] bytes), whether a room
//! refuses it, and how many objects it allocates. The bench calls the
//! native as the machine does; the room is what is left of the lab's.
use lash_kernel_doc::{
    Args, Datum, Float, FunctionRegistry, Integer, NativeError, NewObject, Nursery, Value,
    WorkCounter,
};

use crate::testsupport::{self, Bench};

/// The bytes a reserved value counts for, as the paged heap prices it.
pub(super) const VALUE: u64 = 16;

pub(super) struct Lab {
    pub(super) heap: Bench,
    /// The room the next calls reserve against; `None` is unbounded.
    pub(super) limit: Option<u64>,
    /// What calls reserved since the room was last set.
    pub(super) reserved: u64,
    /// The objects calls allocated since the lab was made.
    pub(super) allocated: usize,
}

impl Lab {
    pub(super) fn new() -> Self {
        Self {
            heap: testsupport::heap(),
            limit: None,
            reserved: 0,
            allocated: 0,
        }
    }
    /// Sets the room and forgets what was reserved, as `heap.1 = Room::..`.
    pub(super) fn room(&mut self, limit: Option<u64>) {
        self.limit = limit;
        self.reserved = 0;
    }
    fn left(&self) -> u64 {
        self.limit
            .map_or(u64::MAX, |limit| limit.saturating_sub(self.reserved))
    }
    pub(super) fn value(&mut self, datum: Datum) -> Value {
        testsupport::decode(&mut self.heap, datum)
    }
    pub(super) fn values(&mut self, data: &[Datum]) -> Vec<Value> {
        data.iter().map(|datum| self.value(datum.clone())).collect()
    }
    pub(super) fn datum(&self, value: &Value) -> Datum {
        testsupport::datum(&self.heap, value)
    }
    /// Runs `f` as a call's body over `args`, against the lab's room.
    pub(super) fn spy<R>(
        &mut self,
        args: &[Value],
        f: impl for<'c> FnOnce(Args<'c>, &mut Nursery<'c>) -> R,
    ) -> R {
        let (answer, usage) = self.heap.run(args, self.left(), f).unwrap();
        self.reserved = self.reserved.saturating_add(usage.reserved());
        self.allocated += usage.objects;
        answer
    }
    pub(super) fn call(&mut self, name: &str, args: &[Value]) -> Result<Value, NativeError> {
        self.counted(name, args, &mut WorkCounter::new(None))
    }
    /// `call`, spending from `counter`.
    pub(super) fn counted(
        &mut self,
        name: &str,
        args: &[Value],
        counter: &mut WorkCounter,
    ) -> Result<Value, NativeError> {
        let mut registry = FunctionRegistry::new();
        super::super::register_text_json(&mut registry).unwrap();
        let (_, registered) = registry
            .iter()
            .find(|(_, function)| function.definition().name.as_str() == name)
            .unwrap_or_else(|| panic!("no native {name}"));
        let native = registered.native().unwrap();
        let left = self.left();
        let called = self.heap.call(&**native, args, left, counter);
        self.reserved = self.reserved.saturating_add(called.usage.reserved());
        self.allocated += called.usage.objects;
        called.answer
    }
    /// A list holding exactly `items`, sharing whatever they hold.
    pub(super) fn list_of(&mut self, items: Vec<Value>) -> Value {
        self.heap
            .run(&[], u64::MAX, |_, new| new.allocate(NewObject::List(items)))
            .unwrap()
            .0
    }
    /// A list whose one item is itself.
    pub(super) fn cycle(&mut self) -> Value {
        let list = self.list_of(Vec::new());
        let Value::List(handle) = list else {
            unreachable!("a list")
        };
        self.heap.arena_mut().list_set(handle, 0, list).unwrap();
        list
    }
    /// The datum `name` returns for `args`.
    pub(super) fn run(&mut self, name: &str, args: &[Datum]) -> Datum {
        let args = self.values(args);
        let value = self
            .call(name, &args)
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        self.datum(&value)
    }
    /// The kind of the error `name` raises for `args`.
    pub(super) fn raised(&mut self, name: &str, args: &[Datum]) -> String {
        let args = self.values(args);
        let result = self.call(name, &args);
        self.kind(result)
    }
    pub(super) fn kind<T: std::fmt::Debug>(&self, result: Result<T, NativeError>) -> String {
        match result
            .as_ref()
            .err()
            .and_then(|error| testsupport::error_datum(&self.heap, error))
        {
            Some(Datum::Error(error)) => error.kind,
            _ => panic!("expected a raised error: {result:?}"),
        }
    }
}

/// `name` on a fresh lab.
pub(super) fn run(name: &str, args: &[Datum]) -> Datum {
    Lab::new().run(name, args)
}
/// The kind `name` raises on a fresh lab.
pub(super) fn raised(name: &str, args: &[Datum]) -> String {
    Lab::new().raised(name, args)
}
pub(super) fn t(text: &str) -> Datum {
    Datum::Text(text.to_owned())
}
pub(super) fn i(value: impl Into<num_bigint::BigInt>) -> Datum {
    Datum::Int(Integer::new(value))
}
pub(super) fn f(value: f64) -> Datum {
    Datum::Float(Float::new(value))
}
pub(super) fn b(value: bool) -> Datum {
    Datum::Bool(value)
}
pub(super) fn bytes(value: &[u8]) -> Datum {
    Datum::Bytes(lash_kernel_doc::Bytes::new(value.to_vec()))
}
pub(super) fn list(items: Vec<Datum>) -> Datum {
    Datum::List(items)
}
pub(super) fn unicode(name: &str) -> String {
    let (a, b, c) = char::UNICODE_VERSION;
    format!("text.{name}_u{a}_{b}_{c}")
}
/// The deep size the charge formulas measure a datum by.
pub(super) fn deep(datum: &Datum) -> u64 {
    match datum {
        Datum::Int(integer) => 1 + integer.bits().div_ceil(64),
        Datum::Text(text) => 1 + text.len() as u64,
        Datum::Bytes(bytes) => 1 + bytes.as_slice().len() as u64,
        Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
            1 + items.len() as u64 + items.iter().map(deep).sum::<u64>()
        }
        Datum::Record(fields) => {
            1 + fields.len() as u64 + fields.iter().map(|(_, value)| deep(value)).sum::<u64>()
        }
        _ => 1,
    }
}
