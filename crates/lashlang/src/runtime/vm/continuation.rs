use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::super::{ExecutionBound, HeapObject, HeapRestoreWire, PersistedRoots};
use super::*;

mod types;
pub use types::{
    ContinuationError, VmFinallyCompletionContinuation, VmFinallyContinuation,
    VmHandlerContinuation, VmIteratorContinuation, VmIteratorCursor, VmProfileContinuation,
};

pub(crate) const VM_CONTINUATION_FORMAT_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum VmRunOutcome {
    EffectCompleted,
    Complete(ExecutionOutcome),
}

#[cfg(test)]
#[derive(Default)]
pub(super) enum TestSuspension {
    #[default]
    Disabled,
    AfterInstructions(usize),
    AfterEffects(usize),
}

#[cfg(test)]
impl TestSuspension {
    pub(super) fn should_suspend(&mut self, completed_effect: bool) -> bool {
        let remaining = match self {
            Self::Disabled => return false,
            Self::AfterInstructions(remaining) => remaining,
            Self::AfterEffects(remaining) if completed_effect => remaining,
            Self::AfterEffects(_) => return false,
        };
        *remaining = remaining.saturating_sub(1);
        *remaining == 0
    }
}

/// A complete, code-independent snapshot of a suspended bytecode VM.
///
/// The compiled program is intentionally not embedded: callers must supply the
/// same content-addressed program to [`Vm::resume_from`]. Derived validation
/// plans are rebuilt lazily after restore.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VmContinuation {
    pub format_version: u32,
    pub instruction_pointer: usize,
    pub active_function: Option<u32>,
    #[serde(
        serialize_with = "continuation_serde::serialize_values",
        deserialize_with = "continuation_serde::deserialize_values"
    )]
    pub operand_stack: Vec<Value>,
    #[serde(
        serialize_with = "continuation_serde::serialize_optional_value",
        deserialize_with = "continuation_serde::deserialize_optional_value"
    )]
    pub last_value: Option<Value>,
    #[serde(
        serialize_with = "continuation_serde::serialize_slots",
        deserialize_with = "continuation_serde::deserialize_slots"
    )]
    pub slots: Vec<Option<Value>>,
    pub projected_slots: Vec<bool>,
    #[serde(
        serialize_with = "continuation_serde::serialize_record",
        deserialize_with = "continuation_serde::deserialize_record"
    )]
    pub globals: Record,
    pub iterator_stack: Vec<VmIteratorContinuation>,
    pub(crate) frame_stack: Vec<VmFrameContinuation>,
    pub handler_stack: Vec<VmHandlerContinuation>,
    pub finally_stack: Vec<VmFinallyContinuation>,
    pub occurrence_counters: std::collections::BTreeMap<String, u64>,
    pub mode: ExecutionMode,
    pub profile: Option<VmProfileContinuation>,
    pub pending_error_span: Option<Span>,
    pub instructions_executed: u64,
    pub active_execution_elapsed: std::time::Duration,
    #[serde(
        serialize_with = "continuation_serde::serialize_heap",
        deserialize_with = "continuation_serde::deserialize_heap"
    )]
    pub heap: VmHeapContinuation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct VmFrameContinuation {
    pub return_instruction_pointer: usize,
    pub function: Option<u32>,
    pub operand_stack_base: usize,
    #[serde(
        serialize_with = "continuation_serde::serialize_slots",
        deserialize_with = "continuation_serde::deserialize_slots"
    )]
    pub slots: Vec<Option<Value>>,
    pub projected_slots: Vec<bool>,
    #[serde(
        serialize_with = "continuation_serde::serialize_record",
        deserialize_with = "continuation_serde::deserialize_record"
    )]
    pub globals: Record,
    pub iterator_stack: Vec<VmIteratorContinuation>,
    pub return_target: VmFrameReturnContinuation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum VmFrameReturnContinuation {
    Direct,
    Map {
        #[serde(
            serialize_with = "continuation_serde::serialize_value",
            deserialize_with = "continuation_serde::deserialize_value"
        )]
        function: Value,
        #[serde(
            serialize_with = "continuation_serde::serialize_values",
            deserialize_with = "continuation_serde::deserialize_values"
        )]
        items: Vec<Value>,
        next_index: usize,
        #[serde(
            serialize_with = "continuation_serde::serialize_values",
            deserialize_with = "continuation_serde::deserialize_values"
        )]
        results: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct VmHeapContinuation {
    heap: Heap,
}

impl VmHeapContinuation {
    fn new(heap: Heap) -> Self {
        Self { heap }
    }

    fn into_heap(self) -> Heap {
        self.heap
    }

    pub fn allocation_counter(&self) -> u64 {
        self.heap.allocations()
    }

    pub fn live_logical_bytes(&self) -> u64 {
        self.heap.live_logical_bytes()
    }

    pub fn size_schedule_version(&self) -> u32 {
        self.heap.schedule_version()
    }

    #[cfg(test)]
    pub(crate) fn storage_slot_count(&self) -> usize {
        self.heap.slots.len()
    }

    #[cfg(test)]
    pub(crate) fn vacant_slot_count(&self) -> usize {
        self.heap.slots.iter().filter(|slot| slot.is_none()).count()
    }

    pub fn materialize(&self, value: &Value) -> Result<Value, ContinuationError> {
        self.heap
            .export(value)
            .map_err(|_| ContinuationError::UnserializableValue {
                location: "continuation heap".to_string(),
                variant: "invalid heap reference",
            })
    }
}

impl VmContinuation {
    /// Number of parked caller frames in this suspended VM.
    pub fn frame_depth(&self) -> usize {
        self.frame_stack.len()
    }
}

impl Default for VmHeapContinuation {
    fn default() -> Self {
        Self::new(Heap::default())
    }
}

mod continuation_serde {
    use super::*;
    use crate::HeapId;

    const NUMBER_WIRE_VERSION: u32 = 1;
    const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
    enum OptionalValueWire {
        Unset,
        Set(ValueWire),
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
    enum ValueWire {
        Null,
        Bool(bool),
        Number(NumberWire),
        String(String),
        Image(super::ImageValue),
        Resource(super::ResourceHandle),
        Ref(HeapId),
        Tuple(Vec<ValueWire>),
        List(Vec<ValueWire>),
        Record(Vec<(String, ValueWire)>),
    }

    #[derive(Serialize, Deserialize)]
    struct NumberWire {
        version: u32,
        bits: u64,
    }

    #[derive(Serialize, Deserialize)]
    struct HeapWire {
        next_id: u64,
        allocation_counter: u64,
        live_logical_bytes: u64,
        size_schedule_version: u32,
        objects: Vec<HeapEntryWire>,
    }

    #[derive(Serialize, Deserialize)]
    struct HeapEntryWire {
        id: HeapId,
        object: HeapObjectWire,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum HeapObjectWire {
        Tuple {
            items: Vec<ValueWire>,
        },
        List {
            items: Vec<ValueWire>,
        },
        Record {
            fields: Vec<(String, ValueWire)>,
        },
        Closure {
            function: u32,
            captures: Vec<ValueWire>,
        },
    }

    fn value_to_wire(value: &Value) -> Result<ValueWire, &'static str> {
        Ok(match value {
            Value::Null => ValueWire::Null,
            Value::Bool(value) => ValueWire::Bool(*value),
            Value::Number(value) => ValueWire::Number(NumberWire {
                version: NUMBER_WIRE_VERSION,
                bits: if value.is_nan() {
                    CANONICAL_NAN_BITS
                } else {
                    value.to_bits()
                },
            }),
            Value::String(value) => ValueWire::String(value.to_string()),
            Value::Image(value) => ValueWire::Image((**value).clone()),
            Value::Resource(value) => ValueWire::Resource(value.clone()),
            Value::Ref(value) => ValueWire::Ref(*value),
            Value::Tuple(values) => {
                ValueWire::Tuple(values.iter().map(value_to_wire).collect::<Result<_, _>>()?)
            }
            Value::List(values) => {
                ValueWire::List(values.iter().map(value_to_wire).collect::<Result<_, _>>()?)
            }
            Value::Record(record) => ValueWire::Record(record_to_wire(record)?),
            Value::Projected(_) => return Err("projected value"),
        })
    }

    fn value_from_wire(value: ValueWire) -> Result<Value, &'static str> {
        Ok(match value {
            ValueWire::Null => Value::Null,
            ValueWire::Bool(value) => Value::Bool(value),
            ValueWire::Number(value) => {
                if value.version != NUMBER_WIRE_VERSION {
                    return Err("unsupported continuation number wire version");
                }
                let number = f64::from_bits(value.bits);
                Value::Number(if number.is_nan() {
                    f64::from_bits(CANONICAL_NAN_BITS)
                } else {
                    number
                })
            }
            ValueWire::String(value) => Value::String(value.into()),
            ValueWire::Image(value) => Value::Image(Box::new(value)),
            ValueWire::Resource(value) => Value::Resource(value),
            ValueWire::Ref(value) => Value::Ref(value),
            ValueWire::Tuple(values) => Value::Tuple(
                values
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            ValueWire::List(values) => Value::List(
                values
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            ValueWire::Record(entries) => Value::Record(Arc::new(record_from_wire(entries)?)),
        })
    }

    fn record_to_wire(record: &Record) -> Result<Vec<(String, ValueWire)>, &'static str> {
        record
            .iter()
            .map(|(key, value)| Ok((key.to_string(), value_to_wire(value)?)))
            .collect()
    }

    fn record_from_wire(entries: Vec<(String, ValueWire)>) -> Result<Record, &'static str> {
        let mut record = record_with_capacity(entries.len());
        let mut names = std::collections::BTreeSet::new();
        for (key, value) in entries {
            if !names.insert(key.clone()) {
                return Err("continuation record keys must be unique");
            }
            record.insert(key, value_from_wire(value)?);
        }
        Ok(record)
    }

    fn object_to_wire(object: &HeapObject) -> Result<HeapObjectWire, &'static str> {
        Ok(match object {
            HeapObject::Tuple(values) => HeapObjectWire::Tuple {
                items: values.iter().map(value_to_wire).collect::<Result<_, _>>()?,
            },
            HeapObject::List(values) => HeapObjectWire::List {
                items: values.iter().map(value_to_wire).collect::<Result<_, _>>()?,
            },
            HeapObject::Record(record) => HeapObjectWire::Record {
                fields: record_to_wire(record)?,
            },
            HeapObject::Closure { function, captures } => HeapObjectWire::Closure {
                function: *function,
                captures: captures
                    .iter()
                    .map(value_to_wire)
                    .collect::<Result<_, _>>()?,
            },
        })
    }

    fn object_from_wire(object: HeapObjectWire) -> Result<HeapObject, &'static str> {
        Ok(match object {
            HeapObjectWire::Tuple { items } => HeapObject::Tuple(
                items
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
            ),
            HeapObjectWire::List { items } => HeapObject::List(
                items
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
            ),
            HeapObjectWire::Record { fields } => {
                HeapObject::Record(Box::new(record_from_wire(fields)?))
            }
            HeapObjectWire::Closure { function, captures } => HeapObject::Closure {
                function,
                captures: captures
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
            },
        })
    }

    pub(super) fn serialize_value<S>(value: &Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value_to_wire(value)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_value<'de, D>(deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        ValueWire::deserialize(deserializer)
            .and_then(|value| value_from_wire(value).map_err(serde::de::Error::custom))
    }

    pub(super) fn serialize_heap<S>(
        continuation: &VmHeapContinuation,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let heap = &continuation.heap;
        let objects = heap
            .objects_in_id_order()
            .map(|(id, object)| {
                Ok(HeapEntryWire {
                    id,
                    object: object_to_wire(object)?,
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()
            .map_err(serde::ser::Error::custom)?;
        HeapWire {
            next_id: heap.next_id,
            allocation_counter: heap.allocations(),
            live_logical_bytes: heap.live_logical_bytes(),
            size_schedule_version: heap.schedule_version(),
            objects,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize_heap<'de, D>(deserializer: D) -> Result<VmHeapContinuation, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = HeapWire::deserialize(deserializer)?;
        let objects = wire
            .objects
            .into_iter()
            .map(|entry| object_from_wire(entry.object).map(|object| (entry.id, object)))
            .collect::<Result<_, _>>()
            .map_err(serde::de::Error::custom)?;
        let heap = Heap::from_wire(
            HeapRestoreWire {
                next_id: wire.next_id,
                allocation_counter: wire.allocation_counter,
                live_logical_bytes: wire.live_logical_bytes,
                size_schedule_version: wire.size_schedule_version,
                objects,
            },
            &[],
        )
        .map_err(serde::de::Error::custom)?;
        Ok(VmHeapContinuation::new(heap))
    }

    fn optional_to_wire(value: &Option<Value>) -> Result<OptionalValueWire, &'static str> {
        match value {
            Some(value) => value_to_wire(value).map(OptionalValueWire::Set),
            None => Ok(OptionalValueWire::Unset),
        }
    }

    fn optional_from_wire(value: OptionalValueWire) -> Result<Option<Value>, &'static str> {
        Ok(match value {
            OptionalValueWire::Unset => None,
            OptionalValueWire::Set(value) => Some(value_from_wire(value)?),
        })
    }

    pub(super) fn serialize_values<S>(values: &[Value], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values
            .iter()
            .map(value_to_wire)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_values<'de, D>(deserializer: D) -> Result<Vec<Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<ValueWire>::deserialize(deserializer).and_then(|values| {
            values
                .into_iter()
                .map(value_from_wire)
                .collect::<Result<_, _>>()
                .map_err(serde::de::Error::custom)
        })
    }

    pub(super) fn serialize_optional_value<S>(
        value: &Option<Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        optional_to_wire(value)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_optional_value<'de, D>(
        deserializer: D,
    ) -> Result<Option<Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        OptionalValueWire::deserialize(deserializer)
            .and_then(|value| optional_from_wire(value).map_err(serde::de::Error::custom))
    }

    pub(super) fn serialize_slots<S>(
        slots: &[Option<Value>],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        slots
            .iter()
            .map(optional_to_wire)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_slots<'de, D>(deserializer: D) -> Result<Vec<Option<Value>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<OptionalValueWire>::deserialize(deserializer).and_then(|slots| {
            slots
                .into_iter()
                .map(optional_from_wire)
                .collect::<Result<_, _>>()
                .map_err(serde::de::Error::custom)
        })
    }

    pub(super) fn serialize_record<S>(record: &Record, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value_to_wire(&Value::Record(Arc::new(record.clone())))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_record<'de, D>(deserializer: D) -> Result<Record, D::Error>
    where
        D: Deserializer<'de>,
    {
        ValueWire::deserialize(deserializer).and_then(|value| match value_from_wire(value) {
            Err(error) => Err(serde::de::Error::custom(error)),
            Ok(Value::Record(record)) => Ok((*record).clone()),
            Ok(_) => Err(serde::de::Error::custom("expected continuation record")),
        })
    }
}

/// Splits a continuation's roots into the ones that own what they name and the
/// ones that only borrow it.
///
/// Slots, globals and a parked loop binding all survive the boundary: whatever
/// they name is theirs, and two of them naming one object is the aliasing this
/// round exists to make unrepresentable. The operand stack, the last-value
/// register and an iterator's captured cursor are execution scratch — the VM
/// legitimately holds a value on the stack and in the slot it was just stored
/// into, and a cursor holds the elements it is handing out one at a time — so
/// they borrow without owning.
fn continuation_forest_roots(continuation: &VmContinuation) -> PersistedRoots<'_> {
    let mut roots = PersistedRoots::default();
    visit_vm_roots(continuation, &mut roots);
    roots
}

fn validate_iterator_invariants(
    iterators: &[VmIteratorContinuation],
    slot_count: usize,
    frame: Option<usize>,
) -> Result<(), ContinuationError> {
    for (iterator_index, iterator) in iterators.iter().enumerate() {
        if iterator.binding_slot >= slot_count {
            return match frame {
                Some(frame) => Err(ContinuationError::FrameIteratorBindingOutOfBounds {
                    frame,
                    iterator: iterator_index,
                    binding_slot: iterator.binding_slot,
                    slot_count,
                }),
                None => Err(ContinuationError::IteratorBindingOutOfBounds {
                    iterator: iterator_index,
                    binding_slot: iterator.binding_slot,
                    slot_count,
                }),
            };
        }
        if matches!(iterator.cursor, VmIteratorCursor::Range { step: 0, .. }) {
            return match frame {
                Some(frame) => Err(ContinuationError::FrameZeroRangeStep {
                    frame,
                    iterator: iterator_index,
                }),
                None => Err(ContinuationError::ZeroRangeStep {
                    iterator: iterator_index,
                }),
            };
        }
    }
    Ok(())
}

fn validate_continuation(continuation: &VmContinuation) -> Result<(), ContinuationError> {
    if continuation.format_version != VM_CONTINUATION_FORMAT_VERSION {
        return Err(ContinuationError::FormatVersionMismatch {
            expected: VM_CONTINUATION_FORMAT_VERSION,
            found: continuation.format_version,
        });
    }
    if continuation.slots.len() != continuation.projected_slots.len() {
        return Err(ContinuationError::SlotCountMismatch {
            expected: continuation.slots.len(),
            actual: continuation.projected_slots.len(),
        });
    }
    if continuation.active_function.is_none() && !continuation.frame_stack.is_empty() {
        return Err(ContinuationError::UnserializableValue {
            location: "frame stack".to_string(),
            variant: "frames without an active function",
        });
    }
    if continuation.active_function.is_some()
        && continuation
            .frame_stack
            .first()
            .is_none_or(|frame| frame.function.is_some())
    {
        return Err(ContinuationError::MissingRootFrame);
    }
    validate_values(&continuation.operand_stack, "operand stack")?;
    validate_heap_references(&continuation.heap.heap, &continuation.operand_stack)?;
    validate_optional_value(continuation.last_value.as_ref(), "last value")?;
    if let Some(value) = continuation.last_value.as_ref() {
        validate_heap_reference(&continuation.heap.heap, value)?;
    }
    for (index, (value, projected)) in continuation
        .slots
        .iter()
        .zip(&continuation.projected_slots)
        .enumerate()
    {
        if *projected {
            return Err(ContinuationError::UnserializableValue {
                location: format!("slot {index}"),
                variant: "Projected",
            });
        }
        validate_optional_value(value.as_ref(), &format!("slot {index}"))?;
        if let Some(value) = value.as_ref() {
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
    }
    for (key, value) in continuation.globals.iter() {
        validate_value(value, &format!("global `{key}`"))?;
        validate_heap_reference(&continuation.heap.heap, value)?;
    }
    for (depth, iterator) in continuation.iterator_stack.iter().enumerate() {
        validate_optional_value(
            iterator.restore_value.as_ref(),
            &format!("iterator {depth} restore value"),
        )?;
        if let Some(value) = iterator.restore_value.as_ref() {
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
        if let VmIteratorCursor::List { values, .. } = &iterator.cursor {
            validate_values(values, &format!("iterator {depth} values"))?;
            validate_heap_references(&continuation.heap.heap, values)?;
        }
    }
    validate_iterator_invariants(&continuation.iterator_stack, continuation.slots.len(), None)?;
    for (id, object) in continuation.heap.heap.objects_in_id_order() {
        match object {
            HeapObject::Tuple(values) | HeapObject::List(values) => {
                validate_values(values, &format!("heap object {}", id.get()))?;
                validate_heap_references(&continuation.heap.heap, values)?;
            }
            HeapObject::Record(record) => {
                for (key, value) in record.iter() {
                    validate_value(value, &format!("heap object {}.{key}", id.get()))?;
                    validate_heap_reference(&continuation.heap.heap, value)?;
                }
            }
            HeapObject::Closure { captures, .. } => {
                validate_values(captures, &format!("heap closure {}", id.get()))?;
                validate_heap_references(&continuation.heap.heap, captures)?;
            }
        }
    }
    for (depth, frame) in continuation.frame_stack.iter().enumerate() {
        if frame.slots.len() != frame.projected_slots.len() {
            return Err(ContinuationError::SlotCountMismatch {
                expected: frame.slots.len(),
                actual: frame.projected_slots.len(),
            });
        }
        if frame.operand_stack_base > continuation.operand_stack.len() {
            return Err(ContinuationError::UnserializableValue {
                location: format!("frame {depth} operand stack base"),
                variant: "out-of-bounds stack base",
            });
        }
        validate_values(
            &frame.slots.iter().flatten().cloned().collect::<Vec<_>>(),
            &format!("frame {depth} slots"),
        )?;
        validate_heap_references(
            &continuation.heap.heap,
            &frame.slots.iter().flatten().cloned().collect::<Vec<_>>(),
        )?;
        for value in frame.globals.values() {
            validate_value(value, &format!("frame {depth} globals"))?;
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
        validate_iterator_invariants(&frame.iterator_stack, frame.slots.len(), Some(depth))?;
        if let VmFrameReturnContinuation::Map {
            function,
            items,
            next_index,
            results,
        } = &frame.return_target
        {
            if *next_index > items.len() || results.len().saturating_add(1) != *next_index {
                return Err(ContinuationError::UnserializableValue {
                    location: format!("frame {depth} map callback"),
                    variant: "invalid callback cursor",
                });
            }
            validate_value(function, &format!("frame {depth} map function"))?;
            validate_heap_reference(&continuation.heap.heap, function)?;
            validate_values(items, &format!("frame {depth} map items"))?;
            validate_heap_references(&continuation.heap.heap, items)?;
            validate_values(results, &format!("frame {depth} map results"))?;
            validate_heap_references(&continuation.heap.heap, results)?;
        }
    }
    for (index, handler) in continuation.handler_stack.iter().enumerate() {
        let Some((owner_function, iterator_count)) =
            continuation_frame_owner(continuation, handler.frame_depth)
        else {
            return Err(ContinuationError::HandlerFrameDepthOutOfBounds {
                handler: index,
                frame_depth: handler.frame_depth,
                frame_count: continuation.frame_stack.len(),
            });
        };
        if owner_function != handler.frame_function {
            return Err(ContinuationError::HandlerFrameIdentityMismatch {
                handler: index,
                frame_depth: handler.frame_depth,
            });
        }
        if handler.operand_stack_depth > continuation.operand_stack.len() {
            return Err(ContinuationError::HandlerStackDepthOutOfBounds {
                handler: index,
                stack_depth: handler.operand_stack_depth,
                stack_size: continuation.operand_stack.len(),
            });
        }
        if handler.iterator_stack_depth > iterator_count {
            return Err(ContinuationError::HandlerIteratorDepthOutOfBounds {
                handler: index,
                iterator_depth: handler.iterator_stack_depth,
                iterator_count,
            });
        }
    }
    for (index, finally) in continuation.finally_stack.iter().enumerate() {
        let Some((owner_function, _)) = continuation_frame_owner(continuation, finally.frame_depth)
        else {
            return Err(ContinuationError::FinallyFrameIdentityMismatch { finally: index });
        };
        if owner_function != finally.frame_function {
            return Err(ContinuationError::FinallyFrameIdentityMismatch { finally: index });
        }
        if finally.handler_stack_depth > continuation.handler_stack.len() {
            return Err(ContinuationError::FinallyHandlerDepthOutOfBounds {
                finally: index,
                handler_depth: finally.handler_stack_depth,
                handler_count: continuation.handler_stack.len(),
            });
        }
        if finally.operand_stack_depth > continuation.operand_stack.len() {
            return Err(ContinuationError::FinallyStackDepthOutOfBounds {
                finally: index,
                stack_depth: finally.operand_stack_depth,
                stack_size: continuation.operand_stack.len(),
            });
        }
        if let VmFinallyCompletionContinuation::Throw { value } = &finally.completion {
            validate_value(value, &format!("finally {index} thrown value"))?;
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
    }
    continuation
        .heap
        .heap
        .validate_persisted_forest(&continuation_forest_roots(continuation))
        .map_err(|reason| ContinuationError::UnserializableValue {
            location: format!("continuation heap: {reason}"),
            variant: "invalid heap object graph",
        })?;
    Ok(())
}

fn continuation_frame_owner(
    continuation: &VmContinuation,
    frame_depth: usize,
) -> Option<(Option<u32>, usize)> {
    if frame_depth == continuation.frame_stack.len() {
        return Some((
            continuation.active_function,
            continuation.iterator_stack.len(),
        ));
    }
    continuation
        .frame_stack
        .get(frame_depth)
        .map(|frame| (frame.function, frame.iterator_stack.len()))
}

fn validate_heap_references(heap: &Heap, values: &[Value]) -> Result<(), ContinuationError> {
    for value in values {
        validate_heap_reference(heap, value)?;
    }
    Ok(())
}

fn validate_heap_reference(heap: &Heap, value: &Value) -> Result<(), ContinuationError> {
    heap.validate_resolvable_refs(value)
        .map_err(|_| ContinuationError::UnserializableValue {
            location: "continuation heap root".to_string(),
            variant: "dangling heap reference",
        })
}

fn validate_values(values: &[Value], location: &str) -> Result<(), ContinuationError> {
    for (index, value) in values.iter().enumerate() {
        validate_value(value, &format!("{location}[{index}]"))?;
    }
    Ok(())
}

fn validate_optional_value(value: Option<&Value>, location: &str) -> Result<(), ContinuationError> {
    if let Some(value) = value {
        validate_value(value, location)?;
    }
    Ok(())
}

fn validate_value(value: &Value, location: &str) -> Result<(), ContinuationError> {
    match value {
        Value::Projected(_) => Err(ContinuationError::UnserializableValue {
            location: location.to_string(),
            variant: "Projected",
        }),
        Value::Tuple(values) | Value::List(values) => {
            for (index, value) in values.iter().enumerate() {
                validate_value(value, &format!("{location}[{index}]"))?;
            }
            Ok(())
        }
        Value::Record(record) => {
            for (key, value) in record.iter() {
                validate_value(value, &format!("{location}.{key}"))?;
            }
            Ok(())
        }
        Value::Null
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Image(_)
        | Value::Resource(_)
        | Value::Ref(_) => Ok(()),
    }
}

fn iterator_to_continuation(
    iterator: &IterState,
    location: &str,
) -> Result<VmIteratorContinuation, ContinuationError> {
    validate_optional_value(
        iterator.restore.previous.as_ref(),
        &format!("{location} restore value"),
    )?;
    let cursor = match &iterator.cursor {
        IterCursor::List { values, index } => {
            validate_values(values, &format!("{location} values"))?;
            VmIteratorCursor::List {
                values: values.iter().cloned().collect(),
                next_index: *index,
            }
        }
        IterCursor::Range { next, end, step } => VmIteratorCursor::Range {
            next: *next,
            end: *end,
            step: *step,
        },
    };
    Ok(VmIteratorContinuation {
        cursor,
        binding_slot: iterator.binding,
        restore_value: iterator.restore.previous.clone(),
    })
}

fn iterator_from_continuation(iterator: VmIteratorContinuation) -> IterState {
    IterState {
        cursor: match iterator.cursor {
            VmIteratorCursor::List { values, next_index } => IterCursor::List {
                values: values.into(),
                index: next_index,
            },
            VmIteratorCursor::Range { next, end, step } => IterCursor::Range { next, end, step },
        },
        binding: iterator.binding_slot,
        restore: LoopRestore {
            previous: iterator.restore_value,
        },
        heapified: false,
    }
}

fn profile_from_continuation(
    profile: VmProfileContinuation,
) -> Result<ProfileAccumulator, ContinuationError> {
    Ok(ProfileAccumulator {
        instruction_counts: profile
            .instruction_counts
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
        instruction_times: profile
            .instruction_times
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
        builtin_counts: profile
            .builtin_counts
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
        builtin_times: profile
            .builtin_times
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
    })
}

mod program_validation;
use program_validation::validate_program_continuation;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    #[cfg(test)]
    pub(crate) fn live_logical_bytes(&self) -> u64 {
        self.heap.live_logical_bytes()
    }

    #[cfg(test)]
    pub(crate) fn instructions_executed(&self) -> u64 {
        self.instructions_executed
    }

    fn new_heap(host: &H) -> Heap {
        let limit = match host.execution_bounds().memory_limit {
            ExecutionBound::Bounded(limit) => limit.get(),
            ExecutionBound::Unbounded => u64::MAX,
        };
        let mut heap = Heap::with_limit(limit);
        heap.set_collect_every_allocation(host.collect_heap_every_allocation());
        heap
    }

    pub(crate) fn install_heap(&mut self, mut heap: Heap) {
        let limit = match self.host.execution_bounds().memory_limit {
            ExecutionBound::Bounded(limit) => limit.get(),
            ExecutionBound::Unbounded => u64::MAX,
        };
        heap.set_limit(limit);
        heap.set_collect_every_allocation(self.host.collect_heap_every_allocation());
        self.heap = heap;
    }

    pub(crate) fn new_with_mode(
        chunk: &'a Chunk,
        slots: SlotState,
        host: &'a H,
        mode: ExecutionMode,
    ) -> Self {
        Self {
            chunk,
            ip: 0,
            stack: Vec::new(),
            last_value: None,
            slots,
            host,
            mode: VmMode::from(mode),
            iter_stack: Vec::new(),
            active_function: None,
            frames: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            lashlang_execution_occurrences: FxHashMap::default(),
            profile: None,
            validation_plans: FxHashMap::default(),
            pending_error_span: None,
            instructions_executed: 0,
            active_execution_elapsed: std::time::Duration::ZERO,
            heap: Self::new_heap(host),
            heap_initialized: false,
            extras_heapified: false,
            assigned_globals: std::collections::BTreeSet::new(),
            #[cfg(test)]
            test_suspension: TestSuspension::Disabled,
        }
    }

    pub(crate) fn new_with_scratch_and_mode(
        chunk: &'a Chunk,
        slots: SlotState,
        host: &'a H,
        scratch: &mut ExecutionScratch,
        mode: ExecutionMode,
    ) -> Self {
        Self {
            chunk,
            ip: 0,
            stack: std::mem::take(&mut scratch.stack),
            last_value: None,
            slots,
            host,
            mode: VmMode::from(mode),
            iter_stack: std::mem::take(&mut scratch.iter_stack),
            active_function: None,
            frames: Vec::new(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            lashlang_execution_occurrences: FxHashMap::default(),
            profile: None,
            validation_plans: FxHashMap::default(),
            pending_error_span: None,
            instructions_executed: 0,
            active_execution_elapsed: std::time::Duration::ZERO,
            heap: Self::new_heap(host),
            heap_initialized: false,
            extras_heapified: false,
            assigned_globals: std::collections::BTreeSet::new(),
            #[cfg(test)]
            test_suspension: TestSuspension::Disabled,
        }
    }

    /// Captures all mutable execution state without consuming the VM.
    ///
    /// Projected host values, even when nested inside another value, cannot be
    /// reconstructed without their host descriptor and decline the boundary.
    pub fn suspend(&mut self) -> Result<VmContinuation, ContinuationError> {
        let roots = self.heap_roots();
        self.heap.collect(roots.iter());
        validate_values(&self.stack, "operand stack")?;
        validate_optional_value(self.last_value.as_ref(), "last value")?;
        for (index, value) in self.slots.values.iter().enumerate() {
            validate_optional_value(value.as_ref(), &format!("slot {index}"))?;
        }
        for (key, value) in self.slots.extras.iter() {
            validate_value(value, &format!("global `{key}`"))?;
        }
        for (index, finally) in self.finally_stack.iter().enumerate() {
            if let FinallyCompletion::Throw { value } = &finally.completion {
                validate_value(value, &format!("finally {index} thrown value"))?;
            }
        }

        let iterator_stack = self
            .iter_stack
            .iter()
            .enumerate()
            .map(|(depth, iterator)| {
                iterator_to_continuation(iterator, &format!("iterator {depth}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut frame_stack = Vec::with_capacity(self.frames.len());
        for frame in &self.frames {
            let iterator_stack = frame
                .iter_stack
                .iter()
                .enumerate()
                .map(|(depth, iterator)| {
                    iterator_to_continuation(iterator, &format!("frame iterator {depth}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let return_target = match &frame.return_target {
                ReturnTarget::Direct => VmFrameReturnContinuation::Direct,
                ReturnTarget::Map(callback) => VmFrameReturnContinuation::Map {
                    function: callback.function.clone(),
                    items: callback.items.clone(),
                    next_index: callback.next_index,
                    results: callback.results.clone(),
                },
            };
            frame_stack.push(VmFrameContinuation {
                return_instruction_pointer: frame.return_ip,
                function: frame
                    .function
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
                operand_stack_base: frame.operand_stack_base,
                slots: frame.slots.values.clone(),
                projected_slots: frame.slots.projected.clone(),
                globals: frame.slots.extras.clone(),
                iterator_stack,
                return_target,
            });
        }
        let handler_stack = self
            .handlers
            .iter()
            .map(|handler| {
                Ok(VmHandlerContinuation {
                    handler_instruction_pointer: handler.handler_ip,
                    finally_instruction_pointer: handler.finally_ip,
                    catches: handler.catches,
                    frame_depth: handler.frame_depth,
                    frame_function: handler
                        .frame_function
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
                    operand_stack_depth: handler.stack_depth,
                    iterator_stack_depth: handler.iterator_depth,
                })
            })
            .collect::<Result<Vec<_>, ContinuationError>>()?;
        let finally_stack = self
            .finally_stack
            .iter()
            .map(|finally| {
                Ok(VmFinallyContinuation {
                    completion: match &finally.completion {
                        FinallyCompletion::Normal { resume_ip } => {
                            VmFinallyCompletionContinuation::Normal {
                                resume_instruction_pointer: *resume_ip,
                            }
                        }
                        FinallyCompletion::Throw { value } => {
                            VmFinallyCompletionContinuation::Throw {
                                value: value.clone(),
                            }
                        }
                    },
                    handler_stack_depth: finally.handler_depth,
                    frame_depth: finally.frame_depth,
                    frame_function: finally
                        .frame_function
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
                    operand_stack_depth: finally.stack_depth,
                })
            })
            .collect::<Result<Vec<_>, ContinuationError>>()?;

        let continuation = VmContinuation {
            format_version: VM_CONTINUATION_FORMAT_VERSION,
            instruction_pointer: self.ip,
            active_function: self
                .active_function
                .map(u32::try_from)
                .transpose()
                .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
            operand_stack: self.stack.clone(),
            last_value: self.last_value.clone(),
            slots: self.slots.values.clone(),
            projected_slots: self.slots.projected.clone(),
            globals: self.slots.extras.clone(),
            iterator_stack,
            frame_stack,
            handler_stack,
            finally_stack,
            occurrence_counters: self
                .lashlang_execution_occurrences
                .iter()
                .map(|(key, value)| (key.clone(), *value))
                .collect(),
            mode: self.mode.into(),
            profile: self.profile.as_ref().map(|profile| VmProfileContinuation {
                instruction_counts: profile.instruction_counts.to_vec(),
                instruction_times: profile.instruction_times.to_vec(),
                builtin_counts: profile.builtin_counts.to_vec(),
                builtin_times: profile.builtin_times.to_vec(),
            }),
            pending_error_span: self.pending_error_span,
            instructions_executed: self.instructions_executed,
            active_execution_elapsed: self.active_execution_elapsed,
            heap: VmHeapContinuation::new(self.heap.clone()),
        };
        validate_continuation(&continuation)?;
        Ok(continuation)
    }

    /// Reconstructs a VM at the saved instruction pointer using caller-supplied
    /// immutable bytecode and host dependencies.
    pub fn resume_from(
        continuation: VmContinuation,
        program: &'a CompiledProgram,
        host: &'a H,
    ) -> Result<Self, ContinuationError> {
        if continuation.format_version != VM_CONTINUATION_FORMAT_VERSION {
            return Err(ContinuationError::FormatVersionMismatch {
                expected: VM_CONTINUATION_FORMAT_VERSION,
                found: continuation.format_version,
            });
        }
        validate_continuation(&continuation)?;
        validate_program_continuation(&continuation, &program.chunk)?;
        let active_function = continuation.active_function.map(|index| index as usize);
        let active_slot_count = match active_function {
            Some(index) => program
                .chunk
                .functions
                .get(index)
                .ok_or(ContinuationError::UnknownFunction {
                    index: index as u32,
                })?
                .slot_names
                .len(),
            None => program.chunk.slot_names.len(),
        };
        if continuation.slots.len() != active_slot_count
            || continuation.projected_slots.len() != active_slot_count
        {
            return Err(ContinuationError::SlotCountMismatch {
                expected: active_slot_count,
                actual: continuation.slots.len(),
            });
        }
        for frame in &continuation.frame_stack {
            let expected = match frame.function {
                Some(index) => program
                    .chunk
                    .functions
                    .get(index as usize)
                    .ok_or(ContinuationError::UnknownFunction { index })?
                    .slot_names
                    .len(),
                None => program.chunk.slot_names.len(),
            };
            if frame.slots.len() != expected || frame.projected_slots.len() != expected {
                return Err(ContinuationError::SlotCountMismatch {
                    expected,
                    actual: frame.slots.len(),
                });
            }
        }
        let bounds = host.execution_bounds();
        if continuation.frame_stack.len() as u64 > bounds.max_frame_depth.get() {
            return Err(ContinuationError::FrameDepthExceeded {
                limit: bounds.max_frame_depth.get(),
            });
        }
        if let ExecutionBound::Bounded(limit) = bounds.instruction_budget
            && continuation.instructions_executed > limit.get()
        {
            return Err(ContinuationError::InstructionBudgetExceeded { limit: limit.get() });
        }
        if let ExecutionBound::Bounded(limit) = bounds.deadline
            && continuation.active_execution_elapsed > limit
        {
            return Err(ContinuationError::ExecutionDeadlineExceeded {
                limit_ms: limit.as_millis(),
            });
        }
        if let ExecutionBound::Bounded(limit) = bounds.memory_limit
            && continuation.heap.live_logical_bytes() > limit.get()
        {
            return Err(ContinuationError::MemoryLimitExceeded {
                limit: limit.get(),
                live: continuation.heap.live_logical_bytes(),
            });
        }
        let profile = continuation
            .profile
            .map(profile_from_continuation)
            .transpose()?;
        let iter_stack = continuation
            .iterator_stack
            .into_iter()
            .map(iterator_from_continuation)
            .collect();
        let handlers = continuation
            .handler_stack
            .into_iter()
            .map(|handler| ExceptionHandler {
                handler_ip: handler.handler_instruction_pointer,
                finally_ip: handler.finally_instruction_pointer,
                catches: handler.catches,
                frame_depth: handler.frame_depth,
                frame_function: handler.frame_function.map(|index| index as usize),
                stack_depth: handler.operand_stack_depth,
                iterator_depth: handler.iterator_stack_depth,
            })
            .collect();
        let finally_stack = continuation
            .finally_stack
            .into_iter()
            .map(|finally| FinallyState {
                completion: match finally.completion {
                    VmFinallyCompletionContinuation::Normal {
                        resume_instruction_pointer,
                    } => FinallyCompletion::Normal {
                        resume_ip: resume_instruction_pointer,
                    },
                    VmFinallyCompletionContinuation::Throw { value } => {
                        FinallyCompletion::Throw { value }
                    }
                },
                handler_depth: finally.handler_stack_depth,
                frame_depth: finally.frame_depth,
                frame_function: finally.frame_function.map(|index| index as usize),
                stack_depth: finally.operand_stack_depth,
            })
            .collect();
        let frames = continuation
            .frame_stack
            .into_iter()
            .map(|frame| CallFrame {
                return_ip: frame.return_instruction_pointer,
                function: frame.function.map(|index| index as usize),
                operand_stack_base: frame.operand_stack_base,
                slots: SlotState {
                    values: frame.slots,
                    projected: frame.projected_slots,
                    extras: frame.globals,
                },
                iter_stack: frame
                    .iterator_stack
                    .into_iter()
                    .map(iterator_from_continuation)
                    .collect(),
                extras_heapified: false,
                return_target: match frame.return_target {
                    VmFrameReturnContinuation::Direct => ReturnTarget::Direct,
                    VmFrameReturnContinuation::Map {
                        function,
                        items,
                        next_index,
                        results,
                    } => ReturnTarget::Map(MapCallback {
                        function,
                        items,
                        next_index,
                        results,
                    }),
                },
            })
            .collect();
        Ok(Self {
            chunk: &program.chunk,
            ip: continuation.instruction_pointer,
            stack: continuation.operand_stack,
            last_value: continuation.last_value,
            slots: SlotState {
                values: continuation.slots,
                projected: continuation.projected_slots,
                extras: continuation.globals,
            },
            host,
            mode: continuation.mode.into(),
            iter_stack,
            active_function,
            frames,
            handlers,
            finally_stack,
            lashlang_execution_occurrences: continuation.occurrence_counters.into_iter().collect(),
            profile,
            validation_plans: FxHashMap::default(),
            pending_error_span: continuation.pending_error_span,
            instructions_executed: continuation.instructions_executed,
            active_execution_elapsed: continuation.active_execution_elapsed,
            heap: {
                let mut heap = continuation.heap.into_heap();
                let limit = match bounds.memory_limit {
                    ExecutionBound::Bounded(limit) => limit.get(),
                    ExecutionBound::Unbounded => u64::MAX,
                };
                heap.set_limit(limit);
                heap.set_collect_every_allocation(host.collect_heap_every_allocation());
                heap
            },
            heap_initialized: true,
            extras_heapified: false,
            // A resumed VM records assignments from here on. Continuations are
            // only used by durable process segments, which run on their own
            // `State` and never recycle into an `ExecutionScratch`, so there are
            // no earlier marks to carry across the handover blob.
            assigned_globals: std::collections::BTreeSet::new(),
            #[cfg(test)]
            test_suspension: TestSuspension::Disabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::HeapId;

    fn empty_continuation(heap: Heap) -> VmContinuation {
        VmContinuation {
            format_version: VM_CONTINUATION_FORMAT_VERSION,
            instruction_pointer: 0,
            active_function: None,
            operand_stack: Vec::new(),
            last_value: None,
            slots: Vec::new(),
            projected_slots: Vec::new(),
            globals: Record::new(),
            iterator_stack: Vec::new(),
            frame_stack: Vec::new(),
            handler_stack: Vec::new(),
            finally_stack: Vec::new(),
            occurrence_counters: Default::default(),
            mode: ExecutionMode::Process,
            profile: None,
            pending_error_span: None,
            instructions_executed: 0,
            active_execution_elapsed: std::time::Duration::ZERO,
            heap: VmHeapContinuation::new(heap),
        }
    }

    mod program_validation;

    #[test]
    fn continuation_heap_round_trip_is_canonical_and_rejects_cycles() {
        // A cyclic object used to validate "by identity" and resume. Under the
        // forest invariant it cannot be expressed: the object holds itself, so
        // it has an owner no root can account for.
        let mut cyclic = Heap::default();
        let Value::Ref(root) = cyclic
            .allocate(HeapObject::List(Vec::new()))
            .expect("allocate cyclic root")
        else {
            unreachable!()
        };
        cyclic
            .replace_object(root, HeapObject::List(vec![Value::Ref(root)]))
            .expect("close cycle");
        let mut continuation = empty_continuation(cyclic);
        continuation.slots = vec![Some(Value::Ref(root))];
        continuation.projected_slots = vec![false];
        let error = validate_continuation(&continuation)
            .expect_err("a cyclic continuation must be rejected");
        assert!(
            error.to_string().contains("must have one owner"),
            "unexpected rejection: {error}"
        );

        // The acyclic case still round-trips byte-for-byte, negative zero and
        // all.
        let mut heap = Heap::default();
        let Value::Ref(id) = heap
            .allocate(HeapObject::List(vec![Value::Number(-0.0)]))
            .expect("allocate root")
        else {
            unreachable!()
        };
        let mut continuation = empty_continuation(heap);
        continuation.slots = vec![Some(Value::Ref(id))];
        continuation.projected_slots = vec![false];
        validate_continuation(&continuation).expect("an owned tree validates");
        let bytes = serde_json::to_vec(&continuation).expect("serialize heap");
        let restored: VmContinuation = serde_json::from_slice(&bytes).expect("restore heap");
        assert_eq!(serde_json::to_vec(&restored).expect("redump heap"), bytes);
        let HeapObject::List(values) = restored.heap.heap.get(id).expect("restored root") else {
            panic!("root should remain a list")
        };
        let Value::Number(number) = values[0] else {
            panic!("first member should be a number")
        };
        assert_eq!(number.to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn continuation_numbers_canonicalize_nan_and_preserve_negative_zero() {
        let mut left = empty_continuation(Heap::default());
        left.operand_stack = vec![
            Value::Number(f64::from_bits(0x7ff0_0000_0000_0001)),
            Value::Number(-0.0),
        ];
        let mut right = empty_continuation(Heap::default());
        right.operand_stack = vec![
            Value::Number(f64::from_bits(0xfff8_0000_0000_0042)),
            Value::Number(-0.0),
        ];

        let left_bytes = serde_json::to_vec(&left).expect("serialize NaN continuation");
        let right_bytes = serde_json::to_vec(&right).expect("serialize NaN continuation");
        assert_eq!(left_bytes, right_bytes, "all NaN payloads canonicalize");
        let restored: VmContinuation =
            serde_json::from_slice(&left_bytes).expect("restore NaN continuation");
        assert_eq!(
            serde_json::to_vec(&restored).expect("redump continuation"),
            left_bytes
        );
        let Value::Number(nan) = restored.operand_stack[0] else {
            panic!("expected NaN")
        };
        let Value::Number(negative_zero) = restored.operand_stack[1] else {
            panic!("expected negative zero")
        };
        assert_eq!(nan.to_bits(), 0x7ff8_0000_0000_0000);
        assert_eq!(negative_zero.to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn continuation_decode_rejects_descending_counters_and_dangling_refs() {
        let mut heap = Heap::default();
        heap.allocate(HeapObject::List(Vec::new())).expect("first");
        heap.allocate(HeapObject::List(Vec::new())).expect("second");
        let continuation = empty_continuation(heap);
        let mut descending = serde_json::to_value(&continuation).expect("wire");
        descending["heap"]["objects"]
            .as_array_mut()
            .expect("heap objects")
            .reverse();
        assert!(
            serde_json::from_value::<VmContinuation>(descending)
                .expect_err("descending IDs must fail")
                .to_string()
                .contains("strictly ordered by ID")
        );

        let mut counter = serde_json::to_value(&continuation).expect("wire");
        counter["heap"]["next_id"] = serde_json::json!(1000);
        assert!(
            serde_json::from_value::<VmContinuation>(counter)
                .expect_err("counter mismatch must fail")
                .to_string()
                .contains("allocation counter plus one")
        );

        let mut dangling = empty_continuation(Heap::default());
        dangling.operand_stack = vec![Value::Ref(HeapId::from_counter(99))];
        let bytes = serde_json::to_vec(&dangling).expect("dangling wire");
        assert!(
            serde_json::from_slice::<VmContinuation>(&bytes)
                .expect_err("dangling continuation root must fail")
                .to_string()
                .contains("dangling heap reference")
        );
    }
}
