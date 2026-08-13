use std::collections::{BTreeMap, BTreeSet};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

mod reference_assignment;
mod validation;

pub(crate) use validation::PersistedRoots;

use super::{
    CompiledAssignPath, CompiledAssignPathStep, CompiledFunction, Name, Record, RuntimeError,
    Value, add_values, coerce_string, record_with_capacity, resolve_existing_list_assignment_index,
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
    Closure { function: u32, captures: Vec<Value> },
}

impl HeapObject {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Tuple(_) => "tuple",
            Self::List(_) => "list",
            Self::Record(_) => "record",
            Self::Closure { .. } => "function",
        }
    }
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
            Self::Closure { captures, .. } => 4_u64.saturating_add(
                captures
                    .iter()
                    .map(value_logical_bytes)
                    .fold(0_u64, u64::saturating_add),
            ),
        };
        OBJECT_HEADER_BYTES.saturating_add(payload)
    }

    /// The single source of truth for child discovery.
    ///
    /// Every consumer — allocation bookkeeping, reverse parent edges, mark and
    /// sweep, wire validation, and root traversal — resolves children through
    /// this one recursive enumerator, so no caller can accidentally see a
    /// shallower answer than another. Members are normally scalars or
    /// references (`Heap::from_wire` rejects anything else, and every in-process
    /// insertion path imports compounds into their own objects), but the
    /// enumerator still descends into inline compounds so a future member shape
    /// cannot silently hide a reference.
    pub(crate) fn child_refs(&self) -> Vec<HeapId> {
        let mut refs = Vec::new();
        for value in self.values() {
            collect_value_refs(value, &mut refs);
        }
        refs
    }

    fn values(&self) -> Box<dyn Iterator<Item = &Value> + '_> {
        match self {
            Self::Tuple(values) | Self::List(values) => Box::new(values.iter()),
            Self::Record(record) => Box::new(record.values()),
            Self::Closure { captures, .. } => Box::new(captures.iter()),
        }
    }
}

fn value_logical_bytes(value: &Value) -> u64 {
    VALUE_SLOT_BYTES.saturating_add(match value {
        Value::Null | Value::Undefined => 1,
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
    // Boundary identity is indexed both ways: exported tree identity to object
    // for import lookups, and object to identity so `forget` is a constant-time
    // removal instead of a scan.
    boundary_refs: FxHashMap<(u8, usize), HeapId>,
    boundary_identities: FxHashMap<HeapId, (u8, usize)>,
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
            boundary_refs: FxHashMap::default(),
            boundary_identities: FxHashMap::default(),
            materialized: FxHashMap::default(),
            logical_byte_limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
        }
    }
}

impl Heap {
    pub(crate) fn validate_closures(
        &self,
        functions: &[CompiledFunction],
    ) -> Result<(), RuntimeError> {
        for (_, object) in self.objects_in_id_order() {
            let HeapObject::Closure { function, captures } = object else {
                continue;
            };
            let compiled = functions
                .get(*function as usize)
                .ok_or(RuntimeError::UnknownFunction { index: *function })?;
            if captures.len() != compiled.capture_count {
                return Err(RuntimeError::ClosureCaptureCountMismatch {
                    index: *function,
                    expected: compiled.capture_count,
                    actual: captures.len(),
                });
            }
        }
        Ok(())
    }

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

        let mut heap = Self {
            next_id: wire.next_id,
            allocations: wire.allocation_counter,
            schedule_version: wire.size_schedule_version,
            ..Self::default()
        };
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
            for value in object.values() {
                validate_object_member(value)?;
            }
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

    /// Checks that every reference a value holds names a live object.
    ///
    /// Reference discovery goes through the one value enumerator, and object
    /// members through `HeapObject::child_refs`, so no validator spells its own
    /// traversal.
    pub(crate) fn validate_resolvable_refs(&self, value: &Value) -> Result<(), String> {
        for id in value_refs(value) {
            self.get(id)
                .map_err(|_| format!("dangling heap reference {}", id.get()))?;
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

    /// Allocates one object directly.
    ///
    /// Production allocation goes through the staged paths — `import_values`
    /// and `isolate_value` — which charge a whole batch before committing any of
    /// it. This single-object form is only used to build heaps in tests.
    #[cfg(test)]
    pub(crate) fn allocate(&mut self, object: HeapObject) -> Result<Value, RuntimeError> {
        self.allocate_object(object)
    }

    pub(crate) fn allocate_closure(
        &mut self,
        function: usize,
        captures: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let function = u32::try_from(function).map_err(|_| RuntimeError::FunctionIndexOverflow)?;
        self.allocate_object(HeapObject::Closure { function, captures })
    }

    pub(crate) fn allocate_list(&mut self, values: Vec<Value>) -> Result<Value, RuntimeError> {
        self.allocate_object(HeapObject::List(values))
    }

    pub(crate) fn allocate_record(&mut self, record: Record) -> Result<Value, RuntimeError> {
        self.allocate_object(HeapObject::Record(Box::new(record)))
    }

    fn allocate_object(&mut self, object: HeapObject) -> Result<Value, RuntimeError> {
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
        let children = object.child_refs();
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

    /// Imports inline compounds, allocating one object per compound.
    ///
    /// This is the only import path: the whole batch is staged and charged
    /// before any of it is committed, so a batch that would cross the memory
    /// bound leaves the heap byte-identical instead of stranding the objects it
    /// already charged for.
    pub(crate) fn import_values(
        &mut self,
        values: Vec<Value>,
        durable_count: usize,
    ) -> Result<Vec<Value>, RuntimeError> {
        let mut next_id = self.next_id;
        let mut staged = Vec::new();
        let mut imported = Vec::with_capacity(values.len());
        // One tree can be held in two places at once — a store leaves the value
        // in its slot and in the last-value register — and importing it twice
        // used to allocate the object twice, so every literal store cost two
        // objects and left one for the collector. A transient holder may point
        // at the object a durable one owns, so the second import of the same
        // tree reuses the first. Durable values never reuse: two of them naming
        // one object is the sharing this heap refuses.
        // A batch is a handful of values, so the lookup is a scan over a vector
        // that stays empty unless the batch actually has a transient holder to
        // satisfy. A map here allocated on every instruction and cost more than
        // the duplicate imports it saved.
        let has_transient = values.len() > durable_count;
        let mut durable_imports = Vec::<((u8, usize), Value)>::new();
        for (index, value) in values.into_iter().enumerate() {
            let identity = compound_identity(&value);
            if index >= durable_count
                && let Some(identity) = identity
                && let Some((_, existing)) = durable_imports
                    .iter()
                    .find(|(candidate, _)| *candidate == identity)
            {
                imported.push(existing.clone());
                continue;
            }
            let staged_value = self.stage_import(value, &mut next_id, &mut staged)?;
            if has_transient
                && index < durable_count
                && let Some(identity) = identity
            {
                durable_imports.push((identity, staged_value.clone()));
            }
            imported.push(staged_value);
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
            pending.extend(self.get(id)?.child_refs());
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
            HeapObject::Closure { .. } => {
                return Err(RuntimeError::FunctionValueAtHostBoundary);
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
        self.boundary_refs.get(&identity).copied()
    }

    fn cache_boundary(&mut self, identity: (u8, usize), id: HeapId) {
        if let Some(previous) = self.boundary_refs.insert(identity, id)
            && previous != id
        {
            self.boundary_identities.remove(&previous);
        }
        if let Some(previous_identity) = self.boundary_identities.insert(id, identity)
            && previous_identity != identity
        {
            self.boundary_refs.remove(&previous_identity);
        }
    }

    fn cache_materialized(&mut self, id: HeapId, value: Value) {
        self.materialized.insert(id, value);
    }

    fn forget(&mut self, id: HeapId) {
        self.materialized.remove(&id);
        if let Some(identity) = self.boundary_identities.remove(&id) {
            self.boundary_refs.remove(&identity);
        }
        self.debug_assert_boundary_cache_invariant();
    }

    /// Moves `parent`'s outgoing edges from `old_children` to `new_children`.
    ///
    /// The reverse-edge map is exact rather than an over-approximation: a member
    /// overwrite drops the replaced child's edge in the same step that adds the
    /// new one, so nothing waits for the next sweep.
    fn retarget_parent_edges(
        &mut self,
        parent: HeapId,
        old_children: &[HeapId],
        new_children: &[HeapId],
    ) {
        for child in old_children {
            if new_children.contains(child) {
                continue;
            }
            if let Some(parents) = self.parents.get_mut(child) {
                parents.retain(|candidate| *candidate != parent);
                if parents.is_empty() {
                    self.parents.remove(child);
                }
            }
        }
        for child in new_children {
            if old_children.contains(child) {
                continue;
            }
            let parents = self.parents.entry(*child).or_default();
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
    }

    fn invalidate_materialized_reaching(&mut self, mutated: HeapId) {
        // Nothing is materialized, so no ancestor can hold a stale export and
        // the reverse-edge walk has nothing to find. This is the common case
        // right after `export_for_mutation`, which drops the whole reachable
        // cache before mutating.
        if self.materialized.is_empty() {
            return;
        }
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
            .flat_map(|(parent, object)| {
                object
                    .child_refs()
                    .into_iter()
                    .map(move |child| (parent, child))
            })
            .collect::<Vec<_>>();
        for (parent, child) in edges {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
    }

    /// The live byte meter must equal the sum of what each object was charged.
    ///
    /// The meter is maintained incrementally — allocation adds, sweep
    /// subtracts, an in-place mutation adjusts by a member delta — so it is one
    /// arithmetic slip away from disagreeing with the objects it claims to
    /// count, and that meter is what the memory bound enforces.
    fn debug_assert_byte_accounting(&self) {
        debug_assert_eq!(
            self.live_logical_bytes,
            self.slots.iter().flatten().fold(0_u64, |total, entry| total
                .saturating_add(entry.logical_bytes)),
            "live logical bytes must equal the sum of the charged object sizes"
        );
    }

    fn debug_assert_boundary_cache_invariant(&self) {
        debug_assert!(self.boundary_refs.iter().all(|(identity, id)| {
            self.materialized.contains_key(id)
                && self.id_to_slot.contains_key(id)
                && self.boundary_identities.get(id) == Some(identity)
        }));
        debug_assert_eq!(self.boundary_refs.len(), self.boundary_identities.len());
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
            HeapObject::Closure { .. } => {
                return Err(RuntimeError::FunctionValueAtHostBoundary);
            }
        };
        active.remove(id);
        Ok(exported)
    }

    /// Copies `value` into a freshly allocated, exclusively owned object graph.
    ///
    /// This is the one isolation operation every durable store uses. It is
    /// recursive by construction: the whole graph reachable from `value` is
    /// reallocated under fresh IDs, so the result can never share an object with
    /// any other root or container member. It deliberately ignores the boundary
    /// materialization cache — that cache exists to make an export/import round
    /// trip identity-preserving, which is exactly the sharing an isolation must
    /// not reintroduce.
    ///
    /// Staging keeps the operation atomic: IDs are reserved and objects built
    /// before anything is charged or committed, so a rejected copy leaves the
    /// heap byte-identical.
    pub(crate) fn isolate_value(&mut self, value: &Value) -> Result<Value, RuntimeError> {
        let mut staging = IsolationStaging {
            base: self.next_id,
            objects: Vec::new(),
            mapping: FxHashMap::default(),
        };
        let root = self.stage_isolation(value, &mut staging)?;
        if staging.objects.is_empty() {
            return Ok(root);
        }
        let objects = staging
            .objects
            .into_iter()
            .map(|object| object.expect("every reserved isolation ID is filled"))
            .collect::<Vec<_>>();
        let staged_bytes = objects.iter().fold(0_u64, |total, object| {
            total.saturating_add(object.logical_bytes())
        });
        let attempted = self.live_logical_bytes.saturating_add(staged_bytes);
        if attempted > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted,
            });
        }
        for (offset, object) in objects.into_iter().enumerate() {
            let logical_bytes = object.logical_bytes();
            let committed = self.commit_precharged_object(object, logical_bytes);
            debug_assert_eq!(
                committed,
                Value::Ref(HeapId::from_counter(staging.base + offset as u64))
            );
        }
        Ok(root)
    }

    fn stage_isolation(
        &self,
        value: &Value,
        staging: &mut IsolationStaging,
    ) -> Result<Value, RuntimeError> {
        match value {
            Value::Ref(id) => {
                if let Some(copy) = staging.mapping.get(id) {
                    return Ok(Value::Ref(*copy));
                }
                // Reserve before recursing so a cyclic graph terminates and both
                // ends of the cycle name the same copy.
                let copy = staging.reserve()?;
                staging.mapping.insert(*id, copy);
                let object = self.stage_isolated_object(self.get(*id)?, staging)?;
                staging.fill(copy, object);
                Ok(Value::Ref(copy))
            }
            Value::Tuple(_) | Value::List(_) | Value::Record(_) => {
                let copy = staging.reserve()?;
                let object = self.stage_isolated_inline(value, staging)?;
                staging.fill(copy, object);
                Ok(Value::Ref(copy))
            }
            value => Ok(value.clone()),
        }
    }

    fn stage_isolated_object(
        &self,
        object: &HeapObject,
        staging: &mut IsolationStaging,
    ) -> Result<HeapObject, RuntimeError> {
        Ok(match object {
            HeapObject::Tuple(values) => HeapObject::Tuple(
                values
                    .iter()
                    .map(|value| self.stage_isolation(value, staging))
                    .collect::<Result<_, _>>()?,
            ),
            HeapObject::List(values) => HeapObject::List(
                values
                    .iter()
                    .map(|value| self.stage_isolation(value, staging))
                    .collect::<Result<_, _>>()?,
            ),
            HeapObject::Record(record) => {
                let mut copied = record_with_capacity(record.len());
                for entry in record.entries.iter() {
                    copied.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.stage_isolation(&entry.value, staging)?,
                    );
                }
                HeapObject::Record(Box::new(copied))
            }
            HeapObject::Closure { function, captures } => HeapObject::Closure {
                function: *function,
                captures: captures
                    .iter()
                    .map(|value| self.stage_isolation(value, staging))
                    .collect::<Result<_, _>>()?,
            },
        })
    }

    fn stage_isolated_inline(
        &self,
        value: &Value,
        staging: &mut IsolationStaging,
    ) -> Result<HeapObject, RuntimeError> {
        Ok(match value {
            Value::Tuple(values) => HeapObject::Tuple(
                values
                    .iter()
                    .map(|value| self.stage_isolation(value, staging))
                    .collect::<Result<_, _>>()?,
            ),
            Value::List(values) => HeapObject::List(
                values
                    .iter()
                    .map(|value| self.stage_isolation(value, staging))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Record(record) => {
                let mut copied = record_with_capacity(record.len());
                for entry in record.entries.iter() {
                    copied.insert_symbolized(
                        entry.symbol,
                        entry.name.clone(),
                        self.stage_isolation(&entry.value, staging)?,
                    );
                }
                HeapObject::Record(Box::new(copied))
            }
            _ => unreachable!("inline isolation only reaches compound values"),
        })
    }

    pub(crate) fn push_list(&mut self, target: &Value, item: Value) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = target else {
            return Err(RuntimeError::PushUnsupported);
        };
        // Insertions hold an exclusively owned copy like every other durable
        // store. A reference has already been isolated by the lowering that
        // produced it; an inline compound is isolated here so it can never enter
        // a container while another root still holds the same object.
        let item = if let Value::Ref(id) = item {
            self.get(id)?;
            item
        } else {
            self.isolate_value(&item)?
        };
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
        self.debug_assert_byte_accounting();
        Ok(Value::Ref(*id))
    }

    /// Appends copies of `source`'s members to the list `target` names.
    ///
    /// `acc = acc + other` builds a new list in the language, but under
    /// exclusive ownership nothing else can observe `acc`'s object, so the new
    /// list can be the old one extended. That turns a per-iteration cost
    /// proportional to the accumulator into one proportional to what is being
    /// appended. The members are copied, not moved: `other` is a binding of its
    /// own and keeps what it holds.
    pub(crate) fn extend_list(
        &mut self,
        target: &Value,
        source: &Value,
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = target else {
            return Err(RuntimeError::PushUnsupported);
        };
        let members = match source {
            Value::Ref(source_id) => {
                let HeapObject::List(values) = self.get(*source_id)? else {
                    return Err(RuntimeError::PushUnsupported);
                };
                values.clone()
            }
            Value::List(values) => values.iter().cloned().collect::<Vec<_>>(),
            _ => return Err(RuntimeError::PushUnsupported),
        };
        self.get(*id)?;

        // The whole extension is staged before any of it is committed. Copying
        // and appending one member at a time meant a bound trip partway through
        // left the accumulator holding some of the appended elements — a
        // half-applied concatenation, durably, since the state that survives the
        // failure is the one that gets persisted.
        let mut staging = IsolationStaging {
            base: self.next_id,
            objects: Vec::new(),
            mapping: FxHashMap::default(),
        };
        let mut copies = Vec::with_capacity(members.len());
        for member in &members {
            copies.push(self.stage_isolation(member, &mut staging)?);
        }
        let objects = staging
            .objects
            .into_iter()
            .map(|object| object.expect("every reserved isolation ID is filled"))
            .collect::<Vec<_>>();
        let object_bytes = objects.iter().fold(0_u64, |total, object| {
            total.saturating_add(object.logical_bytes())
        });
        let member_bytes = copies.iter().fold(0_u64, |total, value| {
            total.saturating_add(value_logical_bytes(value))
        });
        let attempted = self
            .live_logical_bytes
            .saturating_add(object_bytes)
            .saturating_add(member_bytes);
        if attempted > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted,
            });
        }

        for (offset, object) in objects.into_iter().enumerate() {
            let logical_bytes = object.logical_bytes();
            let committed = self.commit_precharged_object(object, logical_bytes);
            debug_assert_eq!(
                committed,
                Value::Ref(HeapId::from_counter(staging.base + offset as u64))
            );
        }

        let children = copies.iter().flat_map(value_refs).collect::<Vec<_>>();
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
        values.extend(copies);
        entry.logical_bytes = entry.logical_bytes.saturating_add(member_bytes);
        self.live_logical_bytes = self.live_logical_bytes.saturating_add(member_bytes);
        for child in children {
            let parents = self.parents.entry(child).or_default();
            if !parents.contains(id) {
                parents.push(*id);
            }
        }
        self.invalidate_materialized_reaching(*id);
        self.debug_assert_byte_accounting();
        Ok(Value::Ref(*id))
    }

    /// Whether this value is a list, whether it is held in the heap or still a
    /// tree.
    pub(crate) fn is_list(&self, value: &Value) -> bool {
        match value {
            Value::List(_) => true,
            Value::Ref(id) => matches!(self.get(*id), Ok(HeapObject::List(_))),
            _ => false,
        }
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
            HeapObject::Closure { .. } => {
                return Err(RuntimeError::CannotAssignIndex {
                    actual: "function".to_string(),
                });
            }
        };
        let current_member = current.clone();
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
        let mut replaced_children = Vec::new();
        collect_value_refs(&current_member, &mut replaced_children);
        let mut added_children = Vec::new();
        collect_value_refs(&value, &mut added_children);
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
        self.retarget_parent_edges(*id, &replaced_children, &added_children);
        let entry = self.slots[slot].as_mut().expect("heap slot exists");
        entry.logical_bytes = entry_bytes;
        self.live_logical_bytes = next_live;
        self.invalidate_materialized_reaching(*id);
        self.debug_assert_byte_accounting();
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
            (
                HeapObject::Closure {
                    function: left_function,
                    captures: left,
                },
                HeapObject::Closure {
                    function: right_function,
                    captures: right,
                },
            ) => {
                if left_function != right_function || left.len() != right.len() {
                    return Ok(false);
                }
                for (left, right) in left.iter().zip(right) {
                    if !self.structural_eq_inner(left, right, visited)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
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
        let old_children = entry.object.child_refs();
        let new_children = object.child_refs();
        entry.object = object;
        entry.logical_bytes = new_bytes;
        self.live_logical_bytes = next_live;
        self.retarget_parent_edges(id, &old_children, &new_children);
        self.invalidate_materialized_reaching(id);
        Ok(())
    }

    pub(crate) fn collect<'a>(&mut self, roots: impl IntoIterator<Item = &'a Value>) {
        let mut marked = BTreeSet::new();
        let mut pending = Vec::new();
        for root in roots {
            collect_value_refs(root, &mut pending);
        }
        while let Some(id) = pending.pop() {
            if !marked.insert(id) {
                continue;
            }
            if let Ok(object) = self.get(id) {
                pending.extend(object.child_refs());
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
            self.retarget_parent_edges(entry.id, &entry.object.child_refs(), &[]);
            self.parents.remove(&entry.id);
            self.id_to_slot.remove(&entry.id);
            self.forget(entry.id);
            self.live_logical_bytes = self.live_logical_bytes.saturating_sub(entry.logical_bytes);
            self.free_slots.push(slot);
        }
        self.debug_assert_byte_accounting();
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

/// Reserved IDs and their objects for one in-flight isolation.
struct IsolationStaging {
    base: u64,
    objects: Vec<Option<HeapObject>>,
    mapping: FxHashMap<HeapId, HeapId>,
}

impl IsolationStaging {
    fn reserve(&mut self) -> Result<HeapId, RuntimeError> {
        let id = self
            .base
            .checked_add(self.objects.len() as u64)
            .ok_or(RuntimeError::HeapIdExhausted)?;
        id.checked_add(1).ok_or(RuntimeError::HeapIdExhausted)?;
        self.objects.push(None);
        Ok(HeapId::from_counter(id))
    }

    fn fill(&mut self, id: HeapId, object: HeapObject) {
        let offset = (id.get() - self.base) as usize;
        self.objects[offset] = Some(object);
    }
}

fn validate_object_member(value: &Value) -> Result<(), String> {
    match value {
        Value::Tuple(_) | Value::List(_) | Value::Record(_) => Err(
            "heap object members must be scalars or heap references, not inline compounds"
                .to_string(),
        ),
        _ => Ok(()),
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
            boundary_refs: FxHashMap::default(),
            boundary_identities: FxHashMap::default(),
            materialized: FxHashMap::default(),
            logical_byte_limit: self.logical_byte_limit,
        }
    }
}

/// Two heaps are equal when they hold the same live objects under the same IDs
/// and the same meters.
///
/// Storage layout — which slot an object occupies, which slots are vacant, and
/// the free list — is a private allocation detail that a decode/encode round
/// trip legitimately compacts, so it is deliberately excluded. Including it made
/// `decode(encode(state)) == state` fail for any program that ever allocated a
/// temporary.
impl PartialEq for Heap {
    fn eq(&self, other: &Self) -> bool {
        self.next_id == other.next_id
            && self.allocations == other.allocations
            && self.live_logical_bytes == other.live_logical_bytes
            && self.schedule_version == other.schedule_version
            && self.id_to_slot.len() == other.id_to_slot.len()
            && self.objects_in_id_order().eq(other.objects_in_id_order())
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

#[cfg(test)]
mod tests;
