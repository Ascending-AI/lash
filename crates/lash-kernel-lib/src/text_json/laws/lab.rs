//! A native call over the real paged heap, observed as main's laws observed
//! it: what the call reserves (a value at [`VALUE`] bytes), whether a room
//! refuses it, and how many objects it allocates. The heap decides every
//! operation; the spy only measures and, below its room, refuses first.
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Datum, Float, FunctionRegistry, Integer, NativeCall, WorkCounter};
use lash_kernel_state::paged::PagedHeap;

use crate::testsupport;

/// The bytes a reserved value counts for, as the paged heap prices it.
pub(super) const VALUE: u64 = 16;

pub(super) struct Lab {
    pub(super) heap: PagedHeap,
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
    pub(super) fn value(&mut self, datum: Datum) -> Value {
        testsupport::decode(&mut self.heap, datum)
    }
    pub(super) fn values(&mut self, data: &[Datum]) -> Vec<Value> {
        data.iter().map(|datum| self.value(datum.clone())).collect()
    }
    pub(super) fn datum(&self, value: &Value) -> Datum {
        testsupport::datum(&self.heap, value)
    }
    /// Runs `f` against the heap through the spy.
    pub(super) fn spy<R>(&mut self, f: impl FnOnce(&mut dyn NativeHeap) -> R) -> R {
        let mut spy = Spy {
            heap: &mut self.heap,
            limit: self.limit,
            reserved: &mut self.reserved,
            allocated: &mut self.allocated,
        };
        f(&mut spy)
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
        self.spy(|heap| {
            native.call(NativeCall {
                args,
                heap,
                counter,
            })
        })
    }
    /// A list holding exactly `items`, sharing whatever they hold.
    pub(super) fn list_of(&mut self, items: Vec<Value>) -> Value {
        Value::List(self.heap.allocate(Object::List(items)).unwrap())
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
        match result {
            Err(NativeError::Raised(error)) => testsupport::text(&self.heap, &error.kind),
            other => panic!("expected a raised error: {other:?}"),
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

struct Spy<'a> {
    heap: &'a mut PagedHeap,
    limit: Option<u64>,
    reserved: &'a mut u64,
    allocated: &'a mut usize,
}

impl NativeHeap for Spy<'_> {
    fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, HeapFault> {
        self.heap.normal_key(value)
    }
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, HeapFault> {
        self.heap.equal(a, b)
    }
    fn data_depth_limit(&self) -> usize {
        self.heap.data_depth_limit()
    }
    fn value_depth_limit(&self) -> usize {
        self.heap.value_depth_limit()
    }
    fn len(&self, object: EntityId) -> Result<u64, HeapFault> {
        self.heap.len(object)
    }
    fn list_get(&self, list: EntityId, index: u64) -> Result<Option<Value>, HeapFault> {
        self.heap.list_get(list, index)
    }
    fn map_get(&self, map: EntityId, key: &Value) -> Result<Option<Value>, HeapFault> {
        self.heap.map_get(map, key)
    }
    fn set_contains(&self, set: EntityId, member: &Value) -> Result<bool, HeapFault> {
        self.heap.set_contains(set, member)
    }
    fn record_get(
        &self,
        record: EntityId,
        field: FieldName<'_>,
    ) -> Result<Option<Value>, HeapFault> {
        self.heap.record_get(record, field)
    }
    fn visit(
        &self,
        object: EntityId,
        visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        self.heap.visit(object, visitor)
    }
    fn error_data(&self, error: &ErrorRef) -> Result<Value, HeapFault> {
        self.heap.error_data(error)
    }
    fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<Value>, HeapFault> {
        self.heap.tuple_get(tuple, index)
    }
    fn tuple_visit(
        &self,
        tuple: &TupleRef,
        visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        self.heap.tuple_visit(tuple, visitor)
    }
    fn payload_visit(
        &self,
        value: &Value,
        range: Range<u64>,
        visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        self.heap.payload_visit(value, range, visitor)
    }
    fn text_offset(&self, text: &TextRef, index: u64) -> Result<u64, HeapFault> {
        self.heap.text_offset(text, index)
    }
    fn materialize_text(&mut self, text: &TextRef) -> Result<Arc<str>, NativeError> {
        self.heap.materialize_text(text)
    }
    fn materialize_bytes(&mut self, bytes: &BytesRef) -> Result<Arc<[u8]>, NativeError> {
        self.heap.materialize_bytes(bytes)
    }
    fn materialize_int(&mut self, int: &IntRef) -> Result<Integer, NativeError> {
        self.heap.materialize_int(int)
    }
    fn text(&mut self, text: &str) -> Result<TextRef, NativeError> {
        self.heap.text(text)
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<BytesRef, NativeError> {
        self.heap.bytes(bytes)
    }
    fn int(&mut self, int: Integer) -> Result<IntRef, NativeError> {
        self.heap.int(int)
    }
    fn tuple(&mut self, members: Vec<Value>) -> Result<TupleRef, NativeError> {
        self.heap.tuple(members)
    }
    fn concat(&mut self, parts: &[Value]) -> Result<Value, NativeError> {
        self.heap.concat(parts)
    }
    fn slice(&mut self, value: &Value, range: Range<u64>) -> Result<Value, NativeError> {
        self.heap.slice(value, range)
    }
    fn allocate(&mut self, object: Object) -> Result<EntityId, NativeError> {
        let id = self.heap.allocate(object)?;
        *self.allocated += 1;
        Ok(id)
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let reserved = self
            .reserved
            .saturating_add(values.saturating_mul(VALUE))
            .saturating_add(bytes);
        if self.limit.is_some_and(|limit| reserved > limit) {
            return Err(NativeError::Memory);
        }
        self.heap.reserve(values, bytes)?;
        *self.reserved = reserved;
        Ok(())
    }
}
