//! Native calls publish execution records only when their heap needs them.
//! Reads and immutable payload construction do not collect. Reservations
//! and allocations retry their refused heap operation after publication;
//! the heap itself refines with that operation's proposed values in scope.
use super::tasks::Tasks;
use lash_kernel_doc::heap::{
    BytesRef, Chunk, ErrorRef, FieldName, HeapFault, IntRef, NormalKey, TextRef, TupleRef,
};
use lash_kernel_doc::{Element, EntityId, Integer, NativeError, NativeHeap, Object, Value};
use lash_kernel_state::paged::tree::Tree;
use lash_kernel_state::paged::{HeapWriteError, ListJoinRecord, PagedHeap};
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

/// The machine's authoritative roots while its heap is borrowed by a native.
pub(super) struct Roots<'a> {
    pub(super) tasks: &'a mut Tasks,
    pub(super) joins: &'a Tree<ListJoinRecord>,
    pub(super) registers: &'a [Value],
    pub(super) pinned: &'a [Value],
    pub(super) fresh: &'a [EntityId],
    pub(super) refinements: &'a mut u64,
}

impl Roots<'_> {
    fn reconcile(&mut self, heap: &PagedHeap) -> Result<(), HeapFault> {
        let refinements = heap.certificate().refinements;
        if refinements != *self.refinements {
            self.tasks.after_refinement(heap.task_root().clone())?;
            *self.refinements = refinements;
        }
        Ok(())
    }

    pub(super) fn publish(
        &mut self,
        heap: &mut PagedHeap,
        coordinates: bool,
    ) -> Result<(), HeapWriteError> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::publish();
        self.reconcile(heap)?;
        let tasks = if coordinates {
            self.tasks.roots()?
        } else {
            self.tasks.holdings()?
        };
        heap.set_execution_roots(
            tasks,
            self.joins.clone(),
            self.registers,
            self.pinned,
            self.fresh,
        )?;
        self.reconcile(heap)?;
        Ok(())
    }
}

pub(super) struct CallHeap<'a> {
    pub(super) heap: &'a mut PagedHeap,
    pub(super) roots: Roots<'a>,
}

fn native_error(error: HeapWriteError) -> NativeError {
    match error {
        HeapWriteError::Memory => NativeError::Memory,
        HeapWriteError::Fault(error) => NativeError::Heap(error),
        HeapWriteError::Raised { kind, message } => {
            NativeError::Raised(Arc::new(ErrorRef::new(kind, &message)))
        }
        HeapWriteError::NeedsPrecision { bytes } => NativeError::Heap(HeapFault::Malformed(
            format!("a native refinement still needs precision for {bytes} bytes"),
        )),
    }
}

impl CallHeap<'_> {
    /// As in the machine's write wrapper, a refusal is before any edit.
    /// Retry that heap operation alone, never the native that called it.
    fn write<T>(
        &mut self,
        mut write: impl FnMut(&mut PagedHeap) -> Result<T, NativeError>,
    ) -> Result<T, NativeError> {
        self.heap.take_refusal();
        match write(self.heap) {
            Err(error) => match self.heap.take_refusal() {
                Some(_) => {
                    self.roots.publish(self.heap, false).map_err(native_error)?;
                    // The retry refines with its proposed graph in scope.
                    // Refining first could sweep a child the native built
                    // earlier and holds only in that proposed object.
                    write(self.heap)
                }
                None => Err(error),
            },
            result => result,
        }
    }
}

macro_rules! reads {
    ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) -> $out:ty;)*) => {
        $(fn $name(&self $(, $arg: $ty)*) -> $out {
            NativeHeap::$name(&*self.heap $(, $arg)*)
        })*
    };
}

impl NativeHeap for CallHeap<'_> {
    reads! {
        fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, HeapFault>;
        fn equal(&self, a: &Value, b: &Value) -> Result<bool, HeapFault>;
        fn data_depth_limit(&self) -> usize;
        fn value_depth_limit(&self) -> usize;
        fn len(&self, object: EntityId) -> Result<u64, HeapFault>;
        fn list_get(&self, list: EntityId, index: u64) -> Result<Option<Value>, HeapFault>;
        fn map_get(&self, map: EntityId, key: &Value) -> Result<Option<Value>, HeapFault>;
        fn set_contains(&self, set: EntityId, member: &Value) -> Result<bool, HeapFault>;
        fn record_get(&self, record: EntityId, field: FieldName<'_>) -> Result<Option<Value>, HeapFault>;
        fn visit(&self, object: EntityId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) -> Result<(), HeapFault>;
        fn error_data(&self, error: &ErrorRef) -> Result<Value, HeapFault>;
        fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<Value>, HeapFault>;
        fn tuple_visit(&self, tuple: &TupleRef, visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>) -> Result<(), HeapFault>;
        fn payload_visit(&self, value: &Value, range: Range<u64>, visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>) -> Result<(), HeapFault>;
        fn text_offset(&self, text: &TextRef, index: u64) -> Result<u64, HeapFault>;
    }

    fn materialize_text(&mut self, text: &TextRef) -> Result<Arc<str>, NativeError> {
        self.write(|heap| heap.materialize_text(text))
    }
    fn materialize_bytes(&mut self, bytes: &BytesRef) -> Result<Arc<[u8]>, NativeError> {
        self.write(|heap| heap.materialize_bytes(bytes))
    }
    fn materialize_int(&mut self, int: &IntRef) -> Result<Integer, NativeError> {
        self.write(|heap| heap.materialize_int(int))
    }
    fn text(&mut self, text: &str) -> Result<TextRef, NativeError> {
        self.write(|heap| heap.text(text))
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<BytesRef, NativeError> {
        self.write(|heap| heap.bytes(bytes))
    }
    fn int(&mut self, int: Integer) -> Result<IntRef, NativeError> {
        self.write(|heap| heap.int(int.clone()))
    }
    fn tuple(&mut self, members: Vec<Value>) -> Result<Arc<TupleRef>, NativeError> {
        self.write(|heap| heap.tuple(members.clone()))
    }
    fn concat(&mut self, parts: &[Value]) -> Result<Value, NativeError> {
        self.write(|heap| heap.concat(parts))
    }
    fn slice(&mut self, value: &Value, range: Range<u64>) -> Result<Value, NativeError> {
        self.write(|heap| heap.slice(value, range.clone()))
    }
    fn allocate(&mut self, object: Object) -> Result<EntityId, NativeError> {
        self.write(|heap| heap.allocate(object.clone()))
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        self.write(|heap| heap.reserve(values, bytes))
    }
}

#[cfg(test)]
mod laws {
    use super::*;
    use crate::Bounds;
    use crate::laws::embedder::{Embedder, Setup};

    /// K-BND-003: the operation which requested precision must bring its
    /// proposed edges to the trace. A child held only by a native's local
    /// value survives that allocation; unrelated garbage does not.
    #[test]
    fn allocation_pressure_keeps_the_proposed_objects_native_only_child() {
        let mut embedder = Embedder::with(
            "main { return null }",
            Setup {
                bounds: Bounds {
                    memory: 4096,
                    ..crate::laws::embedder::ROOMY
                },
                ..Setup::default()
            },
        );
        let machine = &mut embedder.machine;
        machine.heap.execution_roots_moved();
        let mut heap = CallHeap {
            heap: &mut machine.heap,
            roots: Roots {
                tasks: &mut machine.tasks,
                joins: &machine.joins,
                registers: &machine.regs,
                pinned: &machine.pins.values,
                fresh: &machine.fresh,
                refinements: &mut machine.census_refinements,
            },
        };
        heap.allocate(Object::List(vec![Value::Null; 150])).unwrap();
        let child = heap.allocate(Object::List(vec![Value::Null; 50])).unwrap();
        let before = heap.heap.certificate().refinements;
        let parent = heap
            .allocate(Object::List(vec![Value::List(child); 50]))
            .unwrap();
        assert!(heap.heap.certificate().refinements > before);
        assert_eq!(heap.len(child).unwrap(), 50);
        assert!(matches!(heap.list_get(parent, 0).unwrap(), Some(Value::List(id)) if id == child));
    }
}
