//! A cell's touched names are slots over its cold session root. Tree edits
//! happen only at projection; the census visits the overlay as working roots.
use super::heap_impl::PagedHeap;
use super::tree::Cell;
use super::{BindingRecord, BoundRecord, HeapWriteError, SemanticHeap};
use lash_kernel_doc::heap::{HeapFault, LogicalBytes, Value};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Default)]
pub(super) struct SessionSlots {
    names: HashMap<Rc<[u8]>, usize>,
    slots: Vec<Slot>,
    dirty: Vec<usize>,
    removed: u64,
    added: u64,
    metadata: u64,
}
struct Slot {
    key: Rc<[u8]>,
    record: Option<BindingRecord>,
    dirty: bool,
}

impl SessionSlots {
    pub(super) fn cost(&self, base: u64) -> u64 {
        base - self.removed + self.added + self.metadata
    }
    fn get(&self, key: &[u8]) -> Option<&Option<BindingRecord>> {
        self.names.get(key).map(|slot| &self.slots[*slot].record)
    }
}

impl PagedHeap {
    /// Prices the effective binding roots and the overlay's reserved storage.
    pub(super) fn session_cost(&self) -> u64 {
        self.session_slots.cost(self.session.cost())
    }

    /// Loads a touched name once. Inspectors can read without installing a
    /// working slot; execution uses this admission before growing the overlay.
    pub fn load_session_binding_at(&mut self, key: &[u8]) -> Result<Option<Value>, HeapWriteError> {
        self.session_slot(key)?;
        self.session_binding_at(key).map_err(Into::into)
    }

    fn session_slot(&mut self, key: &[u8]) -> Result<usize, HeapWriteError> {
        if let Some(slot) = self.session_slots.names.get(key) {
            return Ok(*slot);
        }
        let record = self.session.get(key, &self.access)?;
        let cost = record.as_ref().map_or(0, Cell::cost);
        let slots = &self.session_slots;
        let capacity = (slots.slots.len() + 1)
            .checked_next_power_of_two()
            .ok_or(HeapWriteError::Memory)?;
        // Grow geometrically, but reserve an explicit capacity so the first
        // name does not allocate Vec's four-element minimum. Admission pays
        // for both vectors before growth. Four hash buckets per name bound
        // the hash table (including its smallest allocation and control
        // bytes); Rc counters and allocator headers count too.
        let vector_bytes = (capacity - slots.slots.capacity()) * std::mem::size_of::<Slot>()
            + (capacity - slots.dirty.capacity()) * std::mem::size_of::<usize>();
        let metadata = (vector_bytes + 4 * (std::mem::size_of::<(Rc<[u8]>, usize)>() + 1) + 64)
            as u64
            + key.len() as u64;
        let slot = self.session_slots.slots.len();
        self.replace_holding(0, metadata, 0, 0, |heap| {
            let slots = &mut heap.session_slots;
            let key: Rc<[u8]> = Rc::from(key);
            slots.names.insert(Rc::clone(&key), slot);
            slots.slots.reserve_exact(capacity - slots.slots.len());
            slots.dirty.reserve_exact(capacity - slots.dirty.len());
            slots.slots.push(Slot {
                key,
                record,
                dirty: false,
            });
            slots.removed += cost;
            slots.added += cost;
            slots.metadata += metadata;
        })?;
        Ok(slot)
    }

    /// The value the session binds `name` to, if it binds it.
    pub fn session_binding(
        &self,
        name: &lash_kernel_doc::Name,
    ) -> Result<Option<Value>, HeapFault> {
        self.session_binding_at(&super::metadata::metadata_key(name))
    }

    /// Reads the working slot, falling back to the untouched cold root.
    pub fn session_binding_at(&self, key: &[u8]) -> Result<Option<Value>, HeapFault> {
        let binding = match self.session_slots.get(key) {
            Some(record) => record.as_ref().map(|record| record.value.clone()),
            None => self
                .session
                .get_with(key, &self.access, |record| record.map(|r| r.value.clone()))?,
        };
        binding
            .map(|binding| match binding {
                BoundRecord::Value(value) => Ok(value),
                BoundRecord::Shared(id) => self.variable_get(id),
            })
            .transpose()
    }

    /// Prepares and admits a slot replacement without editing a heap tree.
    /// `None` leaves a missing assignment untouched.
    pub fn write_session_binding(
        &mut self,
        key: &[u8],
        write: impl FnOnce(
            &mut Self,
            Option<&BindingRecord>,
        ) -> Result<Option<BindingRecord>, HeapWriteError>,
    ) -> Result<bool, HeapWriteError> {
        let slot = self.session_slot(key)?;
        let previous = self.session_slots.slots[slot].record.clone();
        let Some(record) = write(self, previous.as_ref())? else {
            return Ok(false);
        };
        let before = previous.as_ref().map_or(0, Cell::cost);
        let after = record.cost();
        let old_tasks = previous.as_ref().map_or(0, Cell::task_edges);
        let new_tasks = record.task_edges();
        self.replace_holding(before, after, old_tasks, new_tasks, |heap| {
            let slots = &mut heap.session_slots;
            slots.added = slots.added - before + after;
            slots.slots[slot].record = Some(record);
            // Updating an already dirty slot allocates nothing.
            if !slots.slots[slot].dirty {
                slots.slots[slot].dirty = true;
                slots.dirty.push(slot);
            }
        })?;
        Ok(true)
    }

    /// Projects dirty slots in canonical key order. Clean slots never edit
    /// the tree. Dropping the lookup cache releases its admitted metadata;
    /// warm and cold continuations therefore start from the same census.
    pub(super) fn project_session(&mut self) -> Result<(), HeapFault> {
        if self.session_slots.slots.is_empty() {
            return Ok(());
        }
        let mut root = self.session.clone();
        self.session_slots.dirty.sort_unstable_by(|a, b| {
            self.session_slots.slots[*a]
                .key
                .cmp(&self.session_slots.slots[*b].key)
        });
        for slot in &self.session_slots.dirty {
            let slot = &self.session_slots.slots[*slot];
            if let Some(record) = &slot.record {
                root.write(slot.key.to_vec(), record.clone(), &self.access)?;
            }
        }
        let metadata = self.session_slots.metadata;
        if let Some(census) = &mut self.census {
            census
                .changed(LogicalBytes(metadata), LogicalBytes(0))
                .map_err(super::heap_impl::census_fault)?;
            self.accounting = census.certificate();
            self.header = None;
        }
        self.session = root;
        self.session_slots = SessionSlots::default();
        Ok(())
    }

    pub(super) fn visit_session_bindings(
        &self,
        visit: &mut dyn FnMut(&BindingRecord) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let mut result = Ok(());
        self.session.visit(&self.access, &mut |key, record| {
            if !self.session_slots.names.contains_key(key) {
                result = visit(record);
            }
            result.is_ok()
        })?;
        result?;
        for slot in &self.session_slots.slots {
            if let Some(record) = &slot.record {
                visit(record)?;
            }
        }
        Ok(())
    }
}
