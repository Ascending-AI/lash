//! Measurement only: the actual heap decides every operation and refusal.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Integer, NativeError, Value};
use lash_kernel_state::paged::PagedHeap;
use std::{
    ops::{ControlFlow, Range},
    sync::Arc,
};
pub(super) struct Reservation {
    pub(super) values: u64,
    pub(super) bytes: u64,
    pub(super) accepted: bool,
}
pub(super) struct Recording<'a> {
    pub(super) heap: &'a mut PagedHeap,
    pub(super) log: Vec<Reservation>,
}
impl NativeHeap for Recording<'_> {
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
        self.heap.allocate(object)
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let result = self.heap.reserve(values, bytes);
        self.log.push(Reservation {
            values,
            bytes,
            accepted: result.is_ok(),
        });
        result
    }
}
