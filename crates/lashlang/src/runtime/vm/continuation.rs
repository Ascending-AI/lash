use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::super::{ExecutionBound, HEAP_SIZE_SCHEDULE_VERSION, HeapEntry, HeapObject};
use super::*;

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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmContinuation {
    pub instruction_pointer: usize,
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

    pub fn materialize(&self, value: &Value) -> Result<Value, ContinuationError> {
        self.heap
            .export(value)
            .map_err(|_| ContinuationError::UnserializableValue {
                location: "continuation heap".to_string(),
                variant: "invalid heap reference",
            })
    }
}

impl Default for VmHeapContinuation {
    fn default() -> Self {
        Self::new(Heap::default())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmIteratorContinuation {
    pub cursor: VmIteratorCursor,
    pub binding_slot: usize,
    #[serde(
        serialize_with = "continuation_serde::serialize_optional_value",
        deserialize_with = "continuation_serde::deserialize_optional_value"
    )]
    pub restore_value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VmIteratorCursor {
    List {
        #[serde(
            serialize_with = "continuation_serde::serialize_values",
            deserialize_with = "continuation_serde::deserialize_values"
        )]
        values: Vec<Value>,
        next_index: usize,
    },
    Range {
        next: i64,
        end: i64,
        step: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmProfileContinuation {
    pub instruction_counts: Vec<u64>,
    pub instruction_times: Vec<u128>,
    pub builtin_counts: Vec<u64>,
    pub builtin_times: Vec<u128>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ContinuationError {
    #[error("cannot capture VM continuation: `{variant}` value at {location} is not serializable")]
    UnserializableValue {
        location: String,
        variant: &'static str,
    },
    #[error(
        "continuation instruction pointer {instruction_pointer} exceeds program length {program_length}"
    )]
    InvalidInstructionPointer {
        instruction_pointer: usize,
        program_length: usize,
    },
    #[error("continuation has {actual} slots but program requires {expected}")]
    SlotCountMismatch { expected: usize, actual: usize },
    #[error(
        "continuation iterator {iterator} binds slot {binding_slot}, but only {slot_count} slots exist"
    )]
    IteratorBindingOutOfBounds {
        iterator: usize,
        binding_slot: usize,
        slot_count: usize,
    },
    #[error("continuation iterator {iterator} has a zero range step")]
    ZeroRangeStep { iterator: usize },
    #[error("continuation profile shape is incompatible with this VM")]
    ProfileShapeMismatch,
    #[error("lashlang instruction budget of {limit} instructions was already exceeded")]
    InstructionBudgetExceeded { limit: u64 },
    #[error("lashlang active-execution deadline of {limit_ms}ms was already exceeded")]
    ExecutionDeadlineExceeded { limit_ms: u128 },
    #[error(
        "lashlang logical memory limit of {limit} bytes was already exceeded by {live} live bytes"
    )]
    MemoryLimitExceeded { limit: u64, live: u64 },
}

impl ContinuationError {
    pub fn is_execution_bound_exhausted(&self) -> bool {
        matches!(
            self,
            Self::InstructionBudgetExceeded { .. }
                | Self::ExecutionDeadlineExceeded { .. }
                | Self::MemoryLimitExceeded { .. }
        )
    }
}

mod continuation_serde {
    use super::*;
    use crate::HeapId;

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
        Number(f64),
        String(String),
        Image(super::ImageValue),
        Resource(super::ResourceHandle),
        Ref(HeapId),
        Tuple(Vec<ValueWire>),
        List(Vec<ValueWire>),
        Record(Vec<(String, ValueWire)>),
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
        Tuple { items: Vec<ValueWire> },
        List { items: Vec<ValueWire> },
        Record { fields: Vec<(String, ValueWire)> },
    }

    fn value_to_wire(value: &Value) -> Result<ValueWire, &'static str> {
        Ok(match value {
            Value::Null => ValueWire::Null,
            Value::Bool(value) => ValueWire::Bool(*value),
            Value::Number(value) => ValueWire::Number(*value),
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
            Value::Record(record) => ValueWire::Record(
                record
                    .iter()
                    .map(|(key, value)| Ok((key.to_string(), value_to_wire(value)?)))
                    .collect::<Result<_, &'static str>>()?,
            ),
            Value::Projected(_) => return Err("projected value"),
        })
    }

    fn value_from_wire(value: ValueWire) -> Value {
        match value {
            ValueWire::Null => Value::Null,
            ValueWire::Bool(value) => Value::Bool(value),
            ValueWire::Number(value) => Value::Number(value),
            ValueWire::String(value) => Value::String(value.into()),
            ValueWire::Image(value) => Value::Image(Box::new(value)),
            ValueWire::Resource(value) => Value::Resource(value),
            ValueWire::Ref(value) => Value::Ref(value),
            ValueWire::Tuple(values) => {
                Value::Tuple(values.into_iter().map(value_from_wire).collect())
            }
            ValueWire::List(values) => {
                Value::List(values.into_iter().map(value_from_wire).collect())
            }
            ValueWire::Record(entries) => {
                let mut record = record_with_capacity(entries.len());
                for (key, value) in entries {
                    record.insert(key, value_from_wire(value));
                }
                Value::Record(Arc::new(record))
            }
        }
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
                fields: record
                    .iter()
                    .map(|(key, value)| Ok((key.to_string(), value_to_wire(value)?)))
                    .collect::<Result<_, &'static str>>()?,
            },
        })
    }

    fn object_from_wire(object: HeapObjectWire) -> HeapObject {
        match object {
            HeapObjectWire::Tuple { items } => {
                HeapObject::Tuple(items.into_iter().map(value_from_wire).collect())
            }
            HeapObjectWire::List { items } => {
                HeapObject::List(items.into_iter().map(value_from_wire).collect())
            }
            HeapObjectWire::Record { fields } => {
                let mut record = record_with_capacity(fields.len());
                for (key, value) in fields {
                    record.insert(key, value_from_wire(value));
                }
                HeapObject::Record(Box::new(record))
            }
        }
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
        if wire.size_schedule_version != HEAP_SIZE_SCHEDULE_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported heap size schedule version {}",
                wire.size_schedule_version
            )));
        }
        let mut heap = Heap::default();
        heap.next_id = wire.next_id;
        heap.allocations = wire.allocation_counter;
        heap.schedule_version = wire.size_schedule_version;
        heap.restore_collection_schedule();
        let mut prior_id = None;
        for entry in wire.objects {
            if prior_id.is_some_and(|prior| entry.id <= prior) {
                return Err(serde::de::Error::custom(
                    "heap objects must be strictly ordered by ID",
                ));
            }
            if entry.id.get() >= heap.next_id {
                return Err(serde::de::Error::custom(
                    "heap object ID must be below the next allocation ID",
                ));
            }
            prior_id = Some(entry.id);
            let object = object_from_wire(entry.object);
            let logical_bytes = object.logical_bytes();
            let slot = heap.slots.len();
            heap.slots.push(Some(HeapEntry {
                id: entry.id,
                object,
                logical_bytes,
            }));
            let id_index = usize::try_from(entry.id.get()).map_err(|_| {
                serde::de::Error::custom("heap object ID exceeds the platform storage index")
            })?;
            if heap.id_to_slot.len() <= id_index {
                heap.id_to_slot.resize(id_index + 1, None);
            }
            heap.id_to_slot[id_index] = Some(slot);
            heap.live_logical_bytes = heap.live_logical_bytes.saturating_add(logical_bytes);
        }
        if heap.live_logical_bytes != wire.live_logical_bytes {
            return Err(serde::de::Error::custom(
                "heap live logical byte counter does not match its objects",
            ));
        }
        if heap.allocations < heap.id_to_slot.iter().flatten().count() as u64 {
            return Err(serde::de::Error::custom(
                "heap allocation counter is smaller than the live object count",
            ));
        }
        Ok(VmHeapContinuation::new(heap))
    }

    fn optional_to_wire(value: &Option<Value>) -> Result<OptionalValueWire, &'static str> {
        match value {
            Some(value) => value_to_wire(value).map(OptionalValueWire::Set),
            None => Ok(OptionalValueWire::Unset),
        }
    }

    fn optional_from_wire(value: OptionalValueWire) -> Option<Value> {
        match value {
            OptionalValueWire::Unset => None,
            OptionalValueWire::Set(value) => Some(value_from_wire(value)),
        }
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
        Vec::<ValueWire>::deserialize(deserializer)
            .map(|values| values.into_iter().map(value_from_wire).collect())
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
        OptionalValueWire::deserialize(deserializer).map(optional_from_wire)
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
        Vec::<OptionalValueWire>::deserialize(deserializer)
            .map(|slots| slots.into_iter().map(optional_from_wire).collect())
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
            Value::Record(record) => Ok((*record).clone()),
            _ => Err(serde::de::Error::custom("expected continuation record")),
        })
    }
}

fn validate_continuation(continuation: &VmContinuation) -> Result<(), ContinuationError> {
    validate_values(&continuation.operand_stack, "operand stack")?;
    validate_optional_value(continuation.last_value.as_ref(), "last value")?;
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
    }
    for (key, value) in continuation.globals.iter() {
        validate_value(value, &format!("global `{key}`"))?;
    }
    for (depth, iterator) in continuation.iterator_stack.iter().enumerate() {
        validate_optional_value(
            iterator.restore_value.as_ref(),
            &format!("iterator {depth} restore value"),
        )?;
        if let VmIteratorCursor::List { values, .. } = &iterator.cursor {
            validate_values(values, &format!("iterator {depth} values"))?;
        }
    }
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
        }
    }
    Ok(())
}

fn validate_heap_references(heap: &Heap, values: &[Value]) -> Result<(), ContinuationError> {
    for value in values {
        validate_heap_reference(heap, value)?;
    }
    Ok(())
}

fn validate_heap_reference(heap: &Heap, value: &Value) -> Result<(), ContinuationError> {
    if let Value::Ref(id) = value
        && heap.get(*id).is_err()
    {
        return Err(ContinuationError::UnserializableValue {
            location: format!("heap reference {}", id.get()),
            variant: "dangling heap reference",
        });
    }
    Ok(())
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
        Value::Number(number) if !number.is_finite() => {
            Err(ContinuationError::UnserializableValue {
                location: location.to_string(),
                variant: "non-finite Number",
            })
        }
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

impl<'a, H: ExecutionHost> Vm<'a, H> {
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
            lashlang_execution_occurrences: FxHashMap::default(),
            profile: None,
            validation_plans: FxHashMap::default(),
            pending_error_span: None,
            instructions_executed: 0,
            active_execution_elapsed: std::time::Duration::ZERO,
            heap: Self::new_heap(host),
            heap_initialized: false,
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
            lashlang_execution_occurrences: FxHashMap::default(),
            profile: None,
            validation_plans: FxHashMap::default(),
            pending_error_span: None,
            instructions_executed: 0,
            active_execution_elapsed: std::time::Duration::ZERO,
            heap: Self::new_heap(host),
            heap_initialized: false,
            assigned_globals: std::collections::BTreeSet::new(),
            #[cfg(test)]
            test_suspension: TestSuspension::Disabled,
        }
    }

    /// Captures all mutable execution state without consuming the VM.
    ///
    /// Projected host values, even when nested inside another value, cannot be
    /// reconstructed without their host descriptor and decline the boundary.
    /// Non-finite numbers are also declined because `Value`'s JSON serde maps
    /// them to `null`.
    pub fn suspend(&self) -> Result<VmContinuation, ContinuationError> {
        validate_values(&self.stack, "operand stack")?;
        validate_optional_value(self.last_value.as_ref(), "last value")?;
        for (index, value) in self.slots.values.iter().enumerate() {
            validate_optional_value(value.as_ref(), &format!("slot {index}"))?;
        }
        for (key, value) in self.slots.extras.iter() {
            validate_value(value, &format!("global `{key}`"))?;
        }

        let mut iterator_stack = Vec::with_capacity(self.iter_stack.len());
        for (depth, iterator) in self.iter_stack.iter().enumerate() {
            validate_optional_value(
                iterator.restore.previous.as_ref(),
                &format!("iterator {depth} restore value"),
            )?;
            let cursor = match &iterator.cursor {
                IterCursor::List { values, index } => {
                    validate_values(values, &format!("iterator {depth} values"))?;
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
            iterator_stack.push(VmIteratorContinuation {
                cursor,
                binding_slot: iterator.binding,
                restore_value: iterator.restore.previous.clone(),
            });
        }

        let mut heap = self.heap.clone();
        let roots = self.heap_roots();
        heap.collect(roots.iter());
        let continuation = VmContinuation {
            instruction_pointer: self.ip,
            operand_stack: self.stack.clone(),
            last_value: self.last_value.clone(),
            slots: self.slots.values.clone(),
            projected_slots: self.slots.projected.clone(),
            globals: self.slots.extras.clone(),
            iterator_stack,
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
            heap: VmHeapContinuation::new(heap),
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
        if continuation.instruction_pointer > program.chunk.code.len() {
            return Err(ContinuationError::InvalidInstructionPointer {
                instruction_pointer: continuation.instruction_pointer,
                program_length: program.chunk.code.len(),
            });
        }
        if continuation.slots.len() != program.chunk.slot_names.len()
            || continuation.projected_slots.len() != program.chunk.slot_names.len()
        {
            return Err(ContinuationError::SlotCountMismatch {
                expected: program.chunk.slot_names.len(),
                actual: continuation.slots.len(),
            });
        }
        for (index, iterator) in continuation.iterator_stack.iter().enumerate() {
            if iterator.binding_slot >= continuation.slots.len() {
                return Err(ContinuationError::IteratorBindingOutOfBounds {
                    iterator: index,
                    binding_slot: iterator.binding_slot,
                    slot_count: continuation.slots.len(),
                });
            }
            if matches!(iterator.cursor, VmIteratorCursor::Range { step: 0, .. }) {
                return Err(ContinuationError::ZeroRangeStep { iterator: index });
            }
        }
        validate_continuation(&continuation)?;
        let bounds = host.execution_bounds();
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
            .map(|iterator| IterState {
                cursor: match iterator.cursor {
                    VmIteratorCursor::List { values, next_index } => IterCursor::List {
                        values: values.into(),
                        index: next_index,
                    },
                    VmIteratorCursor::Range { next, end, step } => {
                        IterCursor::Range { next, end, step }
                    }
                },
                binding: iterator.binding_slot,
                restore: LoopRestore {
                    previous: iterator.restore_value,
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

    #[test]
    fn continuation_heap_round_trip_is_canonical_and_cycle_safe() {
        let mut heap = Heap::default();
        let Value::Ref(root) = heap
            .allocate(HeapObject::List(Vec::new()))
            .expect("allocate cyclic root")
        else {
            unreachable!()
        };
        heap.replace_object(
            root,
            HeapObject::List(vec![Value::Number(-0.0), Value::Ref(root)]),
        )
        .expect("close cycle");
        let continuation = VmContinuation {
            instruction_pointer: 0,
            operand_stack: vec![Value::Ref(root)],
            last_value: None,
            slots: Vec::new(),
            projected_slots: Vec::new(),
            globals: Record::new(),
            iterator_stack: Vec::new(),
            occurrence_counters: Default::default(),
            mode: ExecutionMode::Process,
            profile: None,
            pending_error_span: None,
            instructions_executed: 0,
            active_execution_elapsed: std::time::Duration::ZERO,
            heap: VmHeapContinuation::new(heap),
        };
        validate_continuation(&continuation).expect("cycle should validate by identity");
        let bytes = serde_json::to_vec(&continuation).expect("serialize cyclic heap");
        let restored: VmContinuation = serde_json::from_slice(&bytes).expect("restore cyclic heap");
        assert_eq!(
            serde_json::to_vec(&restored).expect("redump cyclic heap"),
            bytes
        );
        assert!(matches!(
            restored.heap.heap.export(&Value::Ref(root)),
            Err(RuntimeError::CyclicHostValue { .. })
        ));
        let HeapObject::List(values) = restored.heap.heap.get(root).expect("restored root") else {
            panic!("root should remain a list")
        };
        let Value::Number(number) = values[0] else {
            panic!("first cycle member should be a number")
        };
        assert_eq!(number.to_bits(), (-0.0_f64).to_bits());
    }
}
