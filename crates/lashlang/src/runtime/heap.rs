use std::collections::{BTreeMap, BTreeSet};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use super::{
    Record, RuntimeError, Value, add_values, coerce_string, record_with_capacity,
    resolve_existing_list_assignment_index,
};

pub const HEAP_SIZE_SCHEDULE_VERSION: u32 = 1;
pub const HEAP_GC_ALLOCATION_INTERVAL: u64 = 1_024;
pub const DEFAULT_HEAP_LOGICAL_BYTE_LIMIT: u64 = 64 * 1024 * 1024;

const OBJECT_HEADER_BYTES: u64 = 16;
const VALUE_SLOT_BYTES: u64 = 16;
const RECORD_FIELD_BYTES: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HeapId(u64);

impl HeapId {
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn from_counter(counter: u64) -> Self {
        Self(counter)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HeapObject {
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Record(Box<Record>),
}

impl HeapObject {
    pub(crate) fn logical_bytes(&self) -> u64 {
        let payload = match self {
            Self::Tuple(values) | Self::List(values) => values
                .iter()
                .map(value_logical_bytes)
                .fold(0_u64, u64::saturating_add),
            Self::Record(record) => record.iter().fold(0_u64, |total, (name, value)| {
                total
                    .saturating_add(RECORD_FIELD_BYTES)
                    .saturating_add(name.len() as u64)
                    .saturating_add(value_logical_bytes(value))
            }),
        };
        OBJECT_HEADER_BYTES.saturating_add(payload)
    }

    fn children(&self) -> impl Iterator<Item = HeapId> + '_ {
        self.values().filter_map(|value| match value {
            Value::Ref(id) => Some(*id),
            _ => None,
        })
    }

    fn values(&self) -> Box<dyn Iterator<Item = &Value> + '_> {
        match self {
            Self::Tuple(values) | Self::List(values) => Box::new(values.iter()),
            Self::Record(record) => Box::new(record.values()),
        }
    }
}

fn value_logical_bytes(value: &Value) -> u64 {
    VALUE_SLOT_BYTES.saturating_add(match value {
        Value::Null => 1,
        Value::Bool(_) => 1,
        Value::Number(_) => 8,
        Value::String(value) => value.len() as u64,
        Value::Image(value) => 24_u64
            .saturating_add(value.id.len() as u64)
            .saturating_add(value.label.len() as u64),
        Value::Resource(value) => 8_u64
            .saturating_add(value.resource_type.len() as u64)
            .saturating_add(value.alias.len() as u64),
        Value::Ref(_) => 8,
        Value::Tuple(values) | Value::List(values) => values
            .iter()
            .map(value_logical_bytes)
            .fold(OBJECT_HEADER_BYTES, u64::saturating_add),
        Value::Record(record) => record
            .iter()
            .fold(OBJECT_HEADER_BYTES, |total, (key, value)| {
                total
                    .saturating_add(RECORD_FIELD_BYTES)
                    .saturating_add(key.len() as u64)
                    .saturating_add(value_logical_bytes(value))
            }),
        Value::Projected(_) => VALUE_SLOT_BYTES,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HeapEntry {
    pub(crate) id: HeapId,
    pub(crate) object: HeapObject,
    pub(crate) logical_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Heap {
    pub(crate) slots: Vec<Option<HeapEntry>>,
    pub(crate) id_to_slot: BTreeMap<HeapId, usize>,
    parents: FxHashMap<HeapId, Vec<HeapId>>,
    pub(crate) free_slots: Vec<usize>,
    pub(crate) next_id: u64,
    pub(crate) allocations: u64,
    pub(crate) live_logical_bytes: u64,
    pub(crate) schedule_version: u32,
    next_collection_at: u64,
    collect_every_allocation: bool,
    stress_pins: Vec<Value>,
    boundary_refs: Vec<((u8, usize), HeapId)>,
    materialized: FxHashMap<HeapId, Value>,
    logical_byte_limit: u64,
}

pub(crate) struct HeapRestoreWire {
    pub(crate) next_id: u64,
    pub(crate) allocation_counter: u64,
    pub(crate) live_logical_bytes: u64,
    pub(crate) size_schedule_version: u32,
    pub(crate) objects: Vec<(HeapId, HeapObject)>,
}

impl Default for Heap {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            id_to_slot: BTreeMap::new(),
            parents: FxHashMap::default(),
            free_slots: Vec::new(),
            next_id: 1,
            allocations: 0,
            live_logical_bytes: 0,
            schedule_version: HEAP_SIZE_SCHEDULE_VERSION,
            next_collection_at: HEAP_GC_ALLOCATION_INTERVAL,
            collect_every_allocation: false,
            stress_pins: Vec::new(),
            boundary_refs: Vec::new(),
            materialized: FxHashMap::default(),
            logical_byte_limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
        }
    }
}

impl Heap {
    pub(crate) fn with_limit(logical_byte_limit: u64) -> Self {
        Self {
            logical_byte_limit,
            ..Self::default()
        }
    }

    pub(crate) fn from_wire(wire: HeapRestoreWire, roots: &[Value]) -> Result<Self, String> {
        if wire.size_schedule_version != HEAP_SIZE_SCHEDULE_VERSION {
            return Err(format!(
                "unsupported heap size schedule version {}",
                wire.size_schedule_version
            ));
        }
        let expected_next_id = wire
            .allocation_counter
            .checked_add(1)
            .ok_or_else(|| "heap allocation counter cannot advance to a next ID".to_string())?;
        if wire.next_id != expected_next_id {
            return Err("heap next ID must equal the allocation counter plus one".to_string());
        }

        let mut heap = Self::default();
        heap.next_id = wire.next_id;
        heap.allocations = wire.allocation_counter;
        heap.schedule_version = wire.size_schedule_version;
        let mut prior_id = None;
        for (id, object) in wire.objects {
            if prior_id.is_some_and(|prior| id <= prior) {
                return Err("heap objects must be strictly ordered by ID".to_string());
            }
            if id.get() == 0 || id.get() >= heap.next_id {
                return Err(
                    "heap object ID must be nonzero and below the next allocation ID".to_string(),
                );
            }
            prior_id = Some(id);
            let logical_bytes = object.logical_bytes();
            heap.live_logical_bytes = heap
                .live_logical_bytes
                .checked_add(logical_bytes)
                .ok_or_else(|| "heap live logical byte counter overflowed".to_string())?;
            let slot = heap.slots.len();
            heap.slots.push(Some(HeapEntry {
                id,
                object,
                logical_bytes,
            }));
            heap.id_to_slot.insert(id, slot);
        }
        if heap.live_logical_bytes != wire.live_logical_bytes {
            return Err("heap live logical byte counter does not match its objects".to_string());
        }
        for root in roots {
            heap.validate_resolvable_refs(root)?;
        }
        for entry in heap.slots.iter().flatten() {
            for value in entry.object.values() {
                heap.validate_resolvable_refs(value)?;
            }
        }
        heap.rebuild_parents();
        heap.restore_collection_schedule();
        Ok(heap)
    }

    pub(crate) fn validate_wire_graph(
        &self,
        roots: &[Value],
        require_acyclic: bool,
    ) -> Result<(), String> {
        #[derive(Clone, Copy)]
        enum Visit {
            Enter(HeapId),
            Leave(HeapId),
        }

        let mut state = BTreeMap::<HeapId, u8>::new();
        let mut stack = Vec::new();
        for root in roots {
            collect_value_refs(root, &mut stack);
        }
        let mut visits = stack.into_iter().map(Visit::Enter).collect::<Vec<_>>();
        while let Some(visit) = visits.pop() {
            match visit {
                Visit::Enter(id) => match state.get(&id) {
                    Some(1) if require_acyclic => {
                        return Err("heap object graph must be acyclic".to_string());
                    }
                    Some(1) => continue,
                    Some(2) => continue,
                    _ => {
                        state.insert(id, 1);
                        visits.push(Visit::Leave(id));
                        let object = self
                            .get(id)
                            .map_err(|_| format!("dangling heap reference {}", id.get()))?;
                        let mut children = Vec::new();
                        for value in object.values() {
                            collect_value_refs(value, &mut children);
                        }
                        visits.extend(children.into_iter().rev().map(Visit::Enter));
                    }
                },
                Visit::Leave(id) => {
                    state.insert(id, 2);
                }
            }
        }
        if state.len() != self.id_to_slot.len() {
            return Err("heap wire must not contain unreachable objects".to_string());
        }
        Ok(())
    }

    pub(crate) fn validate_isolated_roots<'a>(
        &self,
        roots: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Result<(), String> {
        let mut owner = BTreeMap::<HeapId, &str>::new();
        for (name, root) in roots {
            let mut pending = Vec::new();
            collect_value_refs(root, &mut pending);
            let mut seen = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if let Some(previous) = owner.insert(id, name)
                    && previous != name
                {
                    return Err(format!(
                        "heap roots `{previous}` and `{name}` must not share object {}",
                        id.get()
                    ));
                }
                let object = self
                    .get(id)
                    .map_err(|_| format!("dangling heap reference {}", id.get()))?;
                for value in object.values() {
                    collect_value_refs(value, &mut pending);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn validate_resolvable_refs(&self, value: &Value) -> Result<(), String> {
        match value {
            Value::Ref(id) => {
                self.get(*id)
                    .map_err(|_| format!("dangling heap reference {}", id.get()))?;
            }
            Value::Tuple(values) | Value::List(values) => {
                for value in values.iter() {
                    self.validate_resolvable_refs(value)?;
                }
            }
            Value::Record(record) => {
                for value in record.values() {
                    self.validate_resolvable_refs(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn set_collect_every_allocation(&mut self, enabled: bool) {
        self.collect_every_allocation = enabled;
    }

    pub(crate) fn allocation_scope_needs_roots(&self) -> bool {
        self.collect_every_allocation
    }

    pub(crate) fn begin_allocation_scope(&mut self, roots: Vec<Value>) {
        if self.collect_every_allocation {
            let mut pins = Vec::new();
            for root in &roots {
                self.collect_boundary_root_refs(root, &mut pins);
            }
            self.stress_pins = pins;
            let pins = self.stress_pins.clone();
            self.collect(pins.iter());
        }
    }

    fn collect_boundary_root_refs(&self, value: &Value, pins: &mut Vec<Value>) {
        if let Value::Ref(id) = value {
            pins.push(Value::Ref(*id));
            return;
        }
        if let Some(identity) = compound_identity(value)
            && let Some(id) = self.boundary_id(identity)
        {
            pins.push(Value::Ref(id));
            return;
        }
        match value {
            Value::Tuple(values) | Value::List(values) => {
                for value in values.iter() {
                    self.collect_boundary_root_refs(value, pins);
                }
            }
            Value::Record(record) => {
                for value in record.values() {
                    self.collect_boundary_root_refs(value, pins);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn end_allocation_scope(&mut self) {
        self.stress_pins.clear();
    }

    pub(crate) fn set_limit(&mut self, logical_byte_limit: u64) {
        self.logical_byte_limit = logical_byte_limit;
    }

    pub(crate) fn allocations(&self) -> u64 {
        self.allocations
    }

    pub(crate) fn has_runtime_state(&self) -> bool {
        self.allocations != 0 || self.live_logical_bytes != 0
    }

    pub(crate) fn live_logical_bytes(&self) -> u64 {
        self.live_logical_bytes
    }

    pub(crate) fn schedule_version(&self) -> u32 {
        self.schedule_version
    }

    pub(crate) fn needs_collection(&self) -> bool {
        self.collect_every_allocation || self.allocations >= self.next_collection_at
    }

    pub(crate) fn get(&self, id: HeapId) -> Result<&HeapObject, RuntimeError> {
        let slot = self
            .id_to_slot
            .get(&id)
            .copied()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        self.slots[slot]
            .as_ref()
            .map(|entry| &entry.object)
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })
    }

    pub(crate) fn allocate(&mut self, object: HeapObject) -> Result<Value, RuntimeError> {
        let logical_bytes = object.logical_bytes();
        let next_live = self.live_logical_bytes.saturating_add(logical_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        self.next_id
            .checked_add(1)
            .ok_or(RuntimeError::HeapIdExhausted)?;
        Ok(self.commit_precharged_object(object, logical_bytes))
    }

    fn commit_precharged_object(&mut self, object: HeapObject, logical_bytes: u64) -> Value {
        let id = HeapId::from_counter(self.next_id);
        // IDs name exactly one object for their entire lifetime. Reusing a
        // vacant storage slot therefore never reuses or rewinds the ID.
        self.next_id += 1;
        self.allocations = self.allocations.saturating_add(1);
        self.live_logical_bytes = self.live_logical_bytes.saturating_add(logical_bytes);
        let children = object.children().collect::<Vec<_>>();
        let entry = HeapEntry {
            id,
            object,
            logical_bytes,
        };
        let slot = if let Some(slot) = self.free_slots.pop() {
            self.slots[slot] = Some(entry);
            slot
        } else {
            self.slots.push(Some(entry));
            self.slots.len() - 1
        };
        self.id_to_slot.insert(id, slot);
        for child in children {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(&id) {
                parents.push(id);
            }
        }
        if self.collect_every_allocation {
            let allocated = Value::Ref(id);
            self.stress_pins.push(allocated.clone());
            let pins = self.stress_pins.clone();
            self.collect(pins.iter());
        }
        Value::Ref(id)
    }

    pub(crate) fn import(&mut self, value: Value) -> Result<Value, RuntimeError> {
        if let Some(identity) = compound_identity(&value)
            && let Some(id) = self.boundary_id(identity)
        {
            return Ok(Value::Ref(id));
        }
        match value {
            Value::Tuple(values) => {
                let values = values
                    .into_vec()
                    .into_iter()
                    .map(|value| self.import(value))
                    .collect::<Result<_, _>>()?;
                self.allocate(HeapObject::Tuple(values))
            }
            Value::List(values) => {
                let values = values
                    .into_vec()
                    .into_iter()
                    .map(|value| self.import(value))
                    .collect::<Result<_, _>>()?;
                self.allocate(HeapObject::List(values))
            }
            Value::Record(record) => {
                let mut imported = record_with_capacity(record.len());
                for entry in record.entries.iter() {
                    imported.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.import(entry.value.clone())?,
                    );
                }
                self.allocate(HeapObject::Record(Box::new(imported)))
            }
            Value::Ref(id) => {
                self.get(id)?;
                Ok(Value::Ref(id))
            }
            value => Ok(value),
        }
    }

    pub(crate) fn import_values(&mut self, values: Vec<Value>) -> Result<Vec<Value>, RuntimeError> {
        let mut next_id = self.next_id;
        let mut staged = Vec::new();
        let mut imported = Vec::with_capacity(values.len());
        for value in values {
            imported.push(self.stage_import(value, &mut next_id, &mut staged)?);
        }
        let staged_bytes = staged.iter().fold(0_u64, |total, (_, object)| {
            total.saturating_add(object.logical_bytes())
        });
        let attempted = self.live_logical_bytes.saturating_add(staged_bytes);
        if attempted > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted,
            });
        }
        for (expected_id, object) in staged {
            let logical_bytes = object.logical_bytes();
            let committed = self.commit_precharged_object(object, logical_bytes);
            debug_assert_eq!(committed, Value::Ref(expected_id));
        }
        Ok(imported)
    }

    fn stage_import(
        &self,
        value: Value,
        next_id: &mut u64,
        staged: &mut Vec<(HeapId, HeapObject)>,
    ) -> Result<Value, RuntimeError> {
        if let Some(identity) = compound_identity(&value)
            && let Some(id) = self.boundary_id(identity)
        {
            return Ok(Value::Ref(id));
        }
        let object = match value {
            Value::Tuple(values) => {
                let values = values
                    .into_vec()
                    .into_iter()
                    .map(|value| self.stage_import(value, next_id, staged))
                    .collect::<Result<_, _>>()?;
                HeapObject::Tuple(values)
            }
            Value::List(values) => {
                let values = values
                    .into_vec()
                    .into_iter()
                    .map(|value| self.stage_import(value, next_id, staged))
                    .collect::<Result<_, _>>()?;
                HeapObject::List(values)
            }
            Value::Record(record) => {
                let mut imported = record_with_capacity(record.len());
                for entry in record.entries.iter() {
                    imported.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.stage_import(entry.value.clone(), next_id, staged)?,
                    );
                }
                HeapObject::Record(Box::new(imported))
            }
            Value::Ref(id) => {
                self.get(id)?;
                return Ok(Value::Ref(id));
            }
            value => return Ok(value),
        };
        let id = HeapId::from_counter(*next_id);
        *next_id = next_id
            .checked_add(1)
            .ok_or(RuntimeError::HeapIdExhausted)?;
        staged.push((id, object));
        Ok(Value::Ref(id))
    }

    pub(crate) fn export(&self, value: &Value) -> Result<Value, RuntimeError> {
        self.export_inner(value, &mut BTreeSet::new())
    }

    pub(crate) fn export_for_instruction(&mut self, value: &Value) -> Result<Value, RuntimeError> {
        self.export_instruction_inner(value, &mut BTreeSet::new())
    }

    pub(crate) fn export_for_mutation(&mut self, value: &Value) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = value else {
            return Ok(value.clone());
        };
        self.uncache_reachable(*id)?;
        self.export(value)
    }

    fn uncache_reachable(&mut self, root: HeapId) -> Result<(), RuntimeError> {
        let mut pending = vec![root];
        let mut visited = FxHashSet::default();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            pending.extend(self.get(id)?.children());
            self.forget(id);
        }
        Ok(())
    }

    fn export_instruction_inner(
        &mut self,
        value: &Value,
        active: &mut BTreeSet<HeapId>,
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = value else {
            return Ok(value.clone());
        };
        if let Some(exported) = self.cached_materialized(*id) {
            return Ok(exported.clone());
        }
        if !active.insert(*id) {
            return Err(RuntimeError::CyclicHostValue { id: id.get() });
        }
        let object = self.get(*id)?.clone();
        let exported = match object {
            HeapObject::Tuple(values) => Value::Tuple(
                values
                    .iter()
                    .map(|value| self.export_instruction_inner(value, active))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            HeapObject::List(values) => Value::List(
                values
                    .iter()
                    .map(|value| self.export_instruction_inner(value, active))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            HeapObject::Record(record) => {
                let mut output = record_with_capacity(record.len());
                for entry in &record.entries {
                    output.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.export_instruction_inner(&entry.value, active)?,
                    );
                }
                Value::Record(std::sync::Arc::new(output))
            }
        };
        active.remove(id);
        if let Some(identity) = compound_identity(&exported) {
            self.cache_boundary(identity, *id);
        }
        self.cache_materialized(*id, exported.clone());
        self.debug_assert_boundary_cache_invariant();
        Ok(exported)
    }

    fn cached_materialized(&self, id: HeapId) -> Option<&Value> {
        self.materialized.get(&id)
    }

    fn boundary_id(&self, identity: (u8, usize)) -> Option<HeapId> {
        self.boundary_refs
            .iter()
            .find_map(|(candidate, id)| (*candidate == identity).then_some(*id))
    }

    fn cache_boundary(&mut self, identity: (u8, usize), id: HeapId) {
        if let Some((_, cached_id)) = self
            .boundary_refs
            .iter_mut()
            .find(|(candidate, _)| *candidate == identity)
        {
            *cached_id = id;
        } else {
            self.boundary_refs.push((identity, id));
        }
    }

    fn cache_materialized(&mut self, id: HeapId, value: Value) {
        self.materialized.insert(id, value);
    }

    fn forget(&mut self, id: HeapId) {
        self.materialized.remove(&id);
        self.boundary_refs.retain(|(_, cached_id)| *cached_id != id);
        self.debug_assert_boundary_cache_invariant();
    }

    fn invalidate_materialized_reaching(&mut self, mutated: HeapId) {
        let mut pending = vec![mutated];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if visited.insert(id) {
                pending.extend(self.parents.get(&id).into_iter().flatten().copied());
            }
        }
        for id in visited {
            self.forget(id);
        }
        self.debug_assert_boundary_cache_invariant();
    }

    fn rebuild_parents(&mut self) {
        self.parents.clear();
        let edges = self
            .objects_in_id_order()
            .flat_map(|(parent, object)| object.children().map(move |child| (parent, child)))
            .collect::<Vec<_>>();
        for (parent, child) in edges {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
    }

    fn debug_assert_boundary_cache_invariant(&self) {
        debug_assert!(self.boundary_refs.iter().all(|(_, id)| {
            self.materialized.contains_key(id) && self.id_to_slot.contains_key(id)
        }));
    }

    fn export_inner(
        &self,
        value: &Value,
        active: &mut BTreeSet<HeapId>,
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = value else {
            return Ok(value.clone());
        };
        if !active.insert(*id) {
            return Err(RuntimeError::CyclicHostValue { id: id.get() });
        }
        let exported = match self.get(*id)? {
            HeapObject::Tuple(values) => Value::Tuple(
                values
                    .iter()
                    .map(|value| self.export_inner(value, active))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            HeapObject::List(values) => Value::List(
                values
                    .iter()
                    .map(|value| self.export_inner(value, active))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            HeapObject::Record(record) => {
                let mut output = record_with_capacity(record.len());
                for entry in record.entries.iter() {
                    output.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.export_inner(&entry.value, active)?,
                    );
                }
                Value::Record(std::sync::Arc::new(output))
            }
        };
        active.remove(id);
        Ok(exported)
    }

    #[cfg(test)]
    pub(crate) fn deep_copy(&mut self, value: &Value) -> Result<Value, RuntimeError> {
        let mut copied = FxHashMap::default();
        self.deep_copy_inner(value, &mut copied)
    }

    pub(crate) fn isolate_value(&mut self, value: &Value) -> Result<Value, RuntimeError> {
        match value {
            Value::Ref(id) => self.allocate(self.get(*id)?.clone()),
            Value::Tuple(_) | Value::List(_) | Value::Record(_) => self.import(value.clone()),
            value => Ok(value.clone()),
        }
    }

    pub(crate) fn push_list(&mut self, target: &Value, item: Value) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = target else {
            return Err(RuntimeError::PushUnsupported);
        };
        let item = self.import(item)?;
        let children = value_refs(&item);
        let added_bytes = value_logical_bytes(&item);
        let next_live = self.live_logical_bytes.saturating_add(added_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        let slot = self
            .id_to_slot
            .get(id)
            .copied()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let entry = self.slots[slot]
            .as_mut()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let HeapObject::List(values) = &mut entry.object else {
            return Err(RuntimeError::PushUnsupported);
        };
        values.push(item);
        for child in children {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(id) {
                parents.push(*id);
            }
        }
        entry.logical_bytes = entry.logical_bytes.saturating_add(added_bytes);
        self.live_logical_bytes = next_live;
        self.invalidate_materialized_reaching(*id);
        Ok(Value::Ref(*id))
    }

    pub(crate) fn add_assign_index_number(
        &mut self,
        target: &Value,
        index: &Value,
        right: f64,
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = target else {
            return Err(RuntimeError::CannotAssignIndex {
                actual: super::value_type_name(target).to_string(),
            });
        };
        let slot = self
            .id_to_slot
            .get(id)
            .copied()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        enum Target {
            List {
                index: usize,
                old_member_bytes: u64,
            },
            Record {
                key: compact_str::CompactString,
                old_member_bytes: u64,
            },
        }
        let (target_kind, current) = match &self.slots[slot]
            .as_ref()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?
            .object
        {
            HeapObject::List(values) => {
                let index = resolve_existing_list_assignment_index(index, values.len())?;
                let current = values[index].clone();
                (
                    Target::List {
                        index,
                        old_member_bytes: value_logical_bytes(&current),
                    },
                    current,
                )
            }
            HeapObject::Record(record) => {
                let key = match index {
                    Value::String(key) => key.clone(),
                    _ => compact_str::CompactString::from(coerce_string(index)?.as_ref()),
                };
                let stored = record.get(key.as_str()).cloned();
                let old_member_bytes = stored.as_ref().map_or(0, |value| {
                    RECORD_FIELD_BYTES
                        .saturating_add(key.len() as u64)
                        .saturating_add(value_logical_bytes(value))
                });
                (
                    Target::Record {
                        key,
                        old_member_bytes,
                    },
                    stored.unwrap_or(Value::Number(0.0)),
                )
            }
            HeapObject::Tuple(_) => return Err(RuntimeError::ImmutableTupleIndexes),
        };
        let value = match current {
            Value::Number(left) => Value::Number(left + right),
            left => add_values(left, Value::Number(right))?,
        };
        let (old_member_bytes, new_member_bytes) = match &target_kind {
            Target::List {
                old_member_bytes, ..
            } => (*old_member_bytes, value_logical_bytes(&value)),
            Target::Record {
                key,
                old_member_bytes,
            } => (
                *old_member_bytes,
                RECORD_FIELD_BYTES
                    .saturating_add(key.len() as u64)
                    .saturating_add(value_logical_bytes(&value)),
            ),
        };
        let entry_bytes = self.slots[slot]
            .as_ref()
            .expect("heap slot exists")
            .logical_bytes
            .saturating_sub(old_member_bytes)
            .saturating_add(new_member_bytes);
        let next_live = self
            .live_logical_bytes
            .saturating_sub(old_member_bytes)
            .saturating_add(new_member_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        match (
            &mut self.slots[slot].as_mut().expect("heap slot exists").object,
            target_kind,
        ) {
            (HeapObject::List(values), Target::List { index, .. }) => {
                values[index] = value.clone();
            }
            (HeapObject::Record(record), Target::Record { key, .. }) => {
                record.insert_str(key.as_str(), value.clone());
            }
            _ => unreachable!("object kind was checked"),
        }
        let entry = self.slots[slot].as_mut().expect("heap slot exists");
        entry.logical_bytes = entry_bytes;
        self.live_logical_bytes = next_live;
        self.invalidate_materialized_reaching(*id);
        Ok(value)
    }

    pub(crate) fn structural_eq(&self, left: &Value, right: &Value) -> Result<bool, RuntimeError> {
        self.structural_eq_inner(left, right, &mut BTreeSet::new())
    }

    fn structural_eq_inner(
        &self,
        left: &Value,
        right: &Value,
        visited: &mut BTreeSet<(HeapId, HeapId)>,
    ) -> Result<bool, RuntimeError> {
        let (Value::Ref(left_id), Value::Ref(right_id)) = (left, right) else {
            return Ok(left == right);
        };
        if !visited.insert((*left_id, *right_id)) {
            return Ok(true);
        }
        match (self.get(*left_id)?, self.get(*right_id)?) {
            (HeapObject::Tuple(left), HeapObject::Tuple(right))
            | (HeapObject::List(left), HeapObject::List(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for (left, right) in left.iter().zip(right) {
                    if !self.structural_eq_inner(left, right, visited)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (HeapObject::Record(left), HeapObject::Record(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for entry in &left.entries {
                    let Some(right_value) = right.get_symbol(entry.symbol) else {
                        return Ok(false);
                    };
                    if !self.structural_eq_inner(&entry.value, right_value, visited)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    #[cfg(test)]
    fn deep_copy_inner(
        &mut self,
        value: &Value,
        copied: &mut FxHashMap<HeapId, HeapId>,
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = value else {
            return self.import(value.clone());
        };
        if let Some(copy) = copied.get(id) {
            return Ok(Value::Ref(*copy));
        }
        let object = self.get(*id)?.clone();
        let placeholder = match &object {
            HeapObject::Tuple(_) => HeapObject::Tuple(Vec::new()),
            HeapObject::List(_) => HeapObject::List(Vec::new()),
            HeapObject::Record(_) => HeapObject::Record(Box::default()),
        };
        let Value::Ref(copy_id) = self.allocate(placeholder)? else {
            unreachable!("heap allocation always returns a reference")
        };
        copied.insert(*id, copy_id);
        let copied_object = match object {
            HeapObject::Tuple(values) => HeapObject::Tuple(
                values
                    .iter()
                    .map(|value| self.deep_copy_inner(value, copied))
                    .collect::<Result<_, _>>()?,
            ),
            HeapObject::List(values) => HeapObject::List(
                values
                    .iter()
                    .map(|value| self.deep_copy_inner(value, copied))
                    .collect::<Result<_, _>>()?,
            ),
            HeapObject::Record(record) => {
                let mut output = record_with_capacity(record.len());
                for entry in record.entries.iter() {
                    output.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.deep_copy_inner(&entry.value, copied)?,
                    );
                }
                HeapObject::Record(Box::new(output))
            }
        };
        self.replace_object(copy_id, copied_object)?;
        Ok(Value::Ref(copy_id))
    }

    #[cfg(test)]
    pub(crate) fn replace_object(
        &mut self,
        id: HeapId,
        object: HeapObject,
    ) -> Result<(), RuntimeError> {
        let slot = self
            .id_to_slot
            .get(&id)
            .copied()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let old_bytes = self
            .slots
            .get(slot)
            .and_then(Option::as_ref)
            .map(|entry| entry.logical_bytes)
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let new_bytes = object.logical_bytes();
        let next_live = self
            .live_logical_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        let entry = self.slots[slot]
            .as_mut()
            .ok_or(RuntimeError::DanglingHeapReference { id: id.get() })?;
        let old_children = entry.object.children().collect::<Vec<_>>();
        let new_children = object.children().collect::<Vec<_>>();
        entry.object = object;
        entry.logical_bytes = new_bytes;
        self.live_logical_bytes = next_live;
        for child in old_children {
            if let Some(parents) = self.parents.get_mut(&child) {
                parents.retain(|parent| *parent != id);
            }
            if self.parents.get(&child).is_some_and(Vec::is_empty) {
                self.parents.remove(&child);
            }
        }
        for child in new_children {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(&id) {
                parents.push(id);
            }
        }
        self.invalidate_materialized_reaching(id);
        Ok(())
    }

    pub(crate) fn collect<'a>(&mut self, roots: impl IntoIterator<Item = &'a Value>) {
        let mut marked = BTreeSet::new();
        let mut pending = Vec::new();
        for root in roots {
            append_root_heap_ids(root, &mut pending);
        }
        while let Some(id) = pending.pop() {
            if !marked.insert(id) {
                continue;
            }
            if let Ok(object) = self.get(id) {
                pending.extend(object.children());
            }
        }
        for slot in 0..self.slots.len() {
            let Some(entry) = self.slots[slot].as_ref() else {
                continue;
            };
            if marked.contains(&entry.id) {
                continue;
            }
            let entry = self.slots[slot].take().expect("live heap slot was checked");
            for child in entry.object.children() {
                if let Some(parents) = self.parents.get_mut(&child) {
                    parents.retain(|parent| *parent != entry.id);
                }
                if self.parents.get(&child).is_some_and(Vec::is_empty) {
                    self.parents.remove(&child);
                }
            }
            self.parents.remove(&entry.id);
            self.id_to_slot.remove(&entry.id);
            self.forget(entry.id);
            self.live_logical_bytes = self.live_logical_bytes.saturating_sub(entry.logical_bytes);
            self.free_slots.push(slot);
        }
        if self.allocations >= self.next_collection_at {
            self.next_collection_at = self
                .allocations
                .checked_div(HEAP_GC_ALLOCATION_INTERVAL)
                .and_then(|period| period.checked_add(1))
                .and_then(|period| period.checked_mul(HEAP_GC_ALLOCATION_INTERVAL))
                .unwrap_or(u64::MAX);
        }
    }

    pub(crate) fn objects_in_id_order(&self) -> impl Iterator<Item = (HeapId, &HeapObject)> {
        self.id_to_slot
            .iter()
            .filter_map(|(id, slot)| self.slots[*slot].as_ref().map(|entry| (*id, &entry.object)))
    }

    pub(crate) fn restore_collection_schedule(&mut self) {
        self.next_collection_at = self
            .allocations
            .checked_div(HEAP_GC_ALLOCATION_INTERVAL)
            .and_then(|period| period.checked_add(1))
            .and_then(|period| period.checked_mul(HEAP_GC_ALLOCATION_INTERVAL))
            .unwrap_or(u64::MAX);
    }
}

fn collect_value_refs(value: &Value, refs: &mut Vec<HeapId>) {
    match value {
        Value::Ref(id) => refs.push(*id),
        Value::Tuple(values) | Value::List(values) => {
            for value in values.iter() {
                collect_value_refs(value, refs);
            }
        }
        Value::Record(record) => {
            for value in record.values() {
                collect_value_refs(value, refs);
            }
        }
        _ => {}
    }
}

fn value_refs(value: &Value) -> Vec<HeapId> {
    let mut refs = Vec::new();
    collect_value_refs(value, &mut refs);
    refs
}

impl Clone for Heap {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            id_to_slot: self.id_to_slot.clone(),
            parents: self.parents.clone(),
            free_slots: self.free_slots.clone(),
            next_id: self.next_id,
            allocations: self.allocations,
            live_logical_bytes: self.live_logical_bytes,
            schedule_version: self.schedule_version,
            next_collection_at: self.next_collection_at,
            collect_every_allocation: self.collect_every_allocation,
            stress_pins: Vec::new(),
            boundary_refs: Vec::new(),
            materialized: FxHashMap::default(),
            logical_byte_limit: self.logical_byte_limit,
        }
    }
}

impl PartialEq for Heap {
    fn eq(&self, other: &Self) -> bool {
        self.slots == other.slots
            && self.id_to_slot == other.id_to_slot
            && self.free_slots == other.free_slots
            && self.next_id == other.next_id
            && self.allocations == other.allocations
            && self.live_logical_bytes == other.live_logical_bytes
            && self.schedule_version == other.schedule_version
    }
}

fn compound_identity(value: &Value) -> Option<(u8, usize)> {
    match value {
        Value::Tuple(values) => Some((0, values.identity())),
        Value::List(values) => Some((1, values.identity())),
        Value::Record(record) => Some((2, std::sync::Arc::as_ptr(record) as usize)),
        _ => None,
    }
}

fn append_root_heap_ids(value: &Value, pending: &mut Vec<HeapId>) {
    match value {
        Value::Ref(id) => pending.push(*id),
        Value::Tuple(values) | Value::List(values) => {
            for value in values.iter() {
                append_root_heap_ids(value, pending);
            }
        }
        Value::Record(record) => {
            for value in record.values() {
                append_root_heap_ids(value, pending);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swept_storage_gets_a_fresh_monotonic_identity() {
        let mut heap = Heap::default();
        let Value::Ref(first) = heap
            .allocate(HeapObject::List(Vec::new()))
            .expect("first allocation")
        else {
            unreachable!()
        };
        heap.collect(std::iter::empty());
        let Value::Ref(second) = heap
            .allocate(HeapObject::List(Vec::new()))
            .expect("second allocation")
        else {
            unreachable!()
        };
        assert!(second > first);
        assert!(matches!(
            heap.get(first),
            Err(RuntimeError::DanglingHeapReference { .. })
        ));
    }

    #[test]
    fn deep_copy_preserves_cycles_with_fresh_ids() {
        let mut heap = Heap::default();
        let Value::Ref(original) = heap
            .allocate(HeapObject::List(Vec::new()))
            .expect("original")
        else {
            unreachable!()
        };
        heap.replace_object(original, HeapObject::List(vec![Value::Ref(original)]))
            .expect("cycle");
        let Value::Ref(copy) = heap.deep_copy(&Value::Ref(original)).expect("deep copy") else {
            unreachable!()
        };
        assert_ne!(copy, original);
        assert_eq!(
            heap.get(copy),
            Ok(&HeapObject::List(vec![Value::Ref(copy)]))
        );
        assert!(
            heap.structural_eq(&Value::Ref(original), &Value::Ref(copy))
                .expect("compare cycles")
        );
    }

    #[test]
    fn sparse_object_bookkeeping_stays_bounded_by_live_objects() {
        let mut heap = Heap::default();
        for _ in 0..5_000 {
            heap.allocate(HeapObject::List(Vec::new()))
                .expect("allocate transient object");
            heap.collect(std::iter::empty());
        }

        assert_eq!(heap.slots.len(), 1, "vacant storage slot should be reused");
        assert!(heap.id_to_slot.is_empty(), "no dead ID bookkeeping remains");
        assert!(heap.materialized.is_empty());
        assert!(heap.boundary_refs.is_empty());
    }

    #[test]
    fn indexed_add_charges_before_record_growth_and_updates_incrementally() {
        let key = Value::String("a-long-new-key".into());
        let added_bytes = RECORD_FIELD_BYTES
            + "a-long-new-key".len() as u64
            + value_logical_bytes(&Value::Number(1.0));
        let base_bytes = HeapObject::Record(Box::default()).logical_bytes();

        let mut exact = Heap::with_limit(base_bytes + added_bytes);
        let target = exact
            .allocate(HeapObject::Record(Box::default()))
            .expect("allocate exact-limit record");
        exact
            .add_assign_index_number(&target, &key, 1.0)
            .expect("exact limit must succeed");
        assert_eq!(exact.live_logical_bytes(), base_bytes + added_bytes);

        let mut over = Heap::with_limit(base_bytes + added_bytes - 1);
        let target = over
            .allocate(HeapObject::Record(Box::default()))
            .expect("allocate one-byte-over record");
        assert!(matches!(
            over.add_assign_index_number(&target, &key, 1.0),
            Err(RuntimeError::MemoryLimitExceeded { .. })
        ));
        assert_eq!(over.live_logical_bytes(), base_bytes);
        assert_eq!(
            over.export(&target)
                .expect("post-error record remains valid"),
            Value::Record(std::sync::Arc::new(Record::new()))
        );
    }

    #[test]
    fn child_mutation_invalidates_materialized_ancestor_cache() {
        let mut heap = Heap::default();
        let child = heap
            .allocate(HeapObject::List(vec![Value::Number(1.0)]))
            .expect("allocate child");
        let parent = heap
            .allocate(HeapObject::List(vec![child.clone()]))
            .expect("allocate parent");

        assert_eq!(
            heap.export(&parent).expect("materialize parent"),
            Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
        );
        heap.push_list(&child, Value::Number(2.0))
            .expect("mutate child");
        assert_eq!(
            heap.export(&parent).expect("rematerialize parent"),
            Value::List(
                vec![Value::List(
                    vec![Value::Number(1.0), Value::Number(2.0)].into()
                )]
                .into()
            )
        );
    }
}
