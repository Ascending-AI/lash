//! Dirty pages share a bounded staging registry across all activation trees.
use super::decoded::{RowSize, RowSums};
use super::*;
use std::rc::Weak;

pub(super) const DIRTY_LIMIT: usize = 32 << 20;
const DIRTY_TARGET: usize = 24 << 20;
trait Stage {
    fn can_spill(&self) -> bool;
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault>;
}
/// Only leaves stage: a staged leaf keeps its slot in its working tree,
/// whose seal reads it back and places its rows canonically. Index nodes
/// are small beside the leaves they name.
impl<T: Cell> Stage for DirtyNode<T> {
    fn can_spill(&self) -> bool {
        self.try_borrow_mut()
            .is_ok_and(|node| matches!(&*node, Node::Leaf(_)))
    }
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault> {
        let Node::Leaf(mut rows) = self.borrow().clone() else {
            return Ok(());
        };
        // Preparing and sealing may change storage links, never guest content.
        // All working versions sharing this node see the same immutable stage.
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        let mut children = Vec::new();
        for (_, cell) in &mut rows {
            if !cell.prepare(access, &mut work)? {
                return Err(HeapFault::Malformed(
                    "unbounded stage ran out of budget".into(),
                ));
            }
            cell.children(&mut children);
        }
        let tokens = encode_body(&rows);
        let bytes = tokens.len() as u64;
        let kind = if sequence {
            PageKind::SEQUENCE_LEAF
        } else {
            PageKind::KEYED_LEAF
        };
        let page = access.seal(kind, tokens, children)?;
        *self.borrow_mut() = Node::Staged(page, bytes, true);
        Ok(())
    }
}
struct Entry {
    node: Weak<dyn Stage>,
    bytes: usize,
    sequence: bool,
    /// A leaf's row sums as of its last tracking; None once its rows may
    /// have changed untracked.
    rows: Option<RowSums>,
}
/// A working page carries its registry identity, so a warm update needs no
/// address-keyed lookup. The weak registry link also pins its allocation:
/// another Access cannot acquire the same identity by address reuse.
pub(super) struct DirtyNode<T: Cell> {
    node: RefCell<Node<T>>,
    pub(super) revision: std::cell::Cell<u64>,
    registration: RefCell<Option<Registration>>,
}
impl<T: Cell> DirtyNode<T> {
    pub(super) fn new(node: Node<T>) -> Self {
        Self {
            node: RefCell::new(node),
            revision: std::cell::Cell::new(0),
            registration: RefCell::new(None),
        }
    }
}
impl<T: Cell> std::ops::Deref for DirtyNode<T> {
    type Target = RefCell<Node<T>>;
    fn deref(&self) -> &Self::Target {
        &self.node
    }
}
impl<T: Cell> Drop for DirtyNode<T> {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.get_mut().take() {
            registration.release();
        }
    }
}
#[derive(Clone, Copy)]
struct Identity {
    slot: usize,
    generation: u64,
}
struct Registration {
    registry: Weak<RefCell<DirtyResidency>>,
    id: Identity,
}
impl Registration {
    fn release(self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut dirty = registry.borrow_mut();
            if dirty.contains(self.id) {
                dirty.take(self.id.slot);
                dirty.recycle(self.id);
            }
        }
    }
}
struct Slot {
    generation: u64,
    entry: Option<Entry>,
    prev: Option<usize>,
    // Also the free-list link when entry is None and the slot is recycled.
    next: Option<usize>,
}
// The slot vector's full capacity is charged separately, including free
// slots. Each live page also pays for its registration and allocator overhead.
const ENTRY_OVERHEAD: usize = std::mem::size_of::<RefCell<Option<Registration>>>() + 64;
#[derive(Default)]
pub(super) struct DirtyResidency {
    slots: Vec<Slot>,
    free: Option<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    bytes: usize,
    staging: bool,
    #[cfg(test)]
    entry_allocations: usize,
}
impl DirtyResidency {
    fn contains(&self, id: Identity) -> bool {
        self.slots
            .get(id.slot)
            .is_some_and(|slot| slot.generation == id.generation && slot.entry.is_some())
    }
    fn entry<T: Cell>(&self, page: &DirtyNode<T>, registry: &Rc<RefCell<Self>>) -> Option<&Entry> {
        let registration = page.registration.borrow();
        let registration = registration.as_ref()?;
        if registration.registry.as_ptr() != Rc::as_ptr(registry) {
            return None;
        }
        if self.contains(registration.id) {
            self.slots[registration.id.slot].entry.as_ref()
        } else {
            None
        }
    }
    fn append(&mut self, entry: Entry) -> Identity {
        let bytes = entry.bytes;
        let index = if let Some(index) = self.free {
            self.free = self.slots[index].next;
            index
        } else {
            #[cfg(test)]
            {
                self.entry_allocations += 1;
            }
            let capacity = self.slots.capacity();
            self.slots.push(Slot {
                generation: 0,
                entry: None,
                prev: None,
                next: None,
            });
            self.bytes += (self.slots.capacity() - capacity) * std::mem::size_of::<Slot>();
            self.slots.len() - 1
        };
        let slot = &mut self.slots[index];
        slot.entry = Some(entry);
        slot.prev = self.tail;
        slot.next = None;
        if let Some(tail) = self.tail {
            self.slots[tail].next = Some(index);
        } else {
            self.head = Some(index);
        }
        self.tail = Some(index);
        self.bytes += bytes;
        Identity {
            slot: index,
            generation: self.slots[index].generation,
        }
    }
    /// Detach but reserve this generation while a spill may fail. Nested
    /// tracking cannot reuse the slot before it is restored or recycled.
    fn take(&mut self, index: usize) -> Entry {
        let slot = &mut self.slots[index];
        let Some(entry) = slot.entry.take() else {
            unreachable!("occupied residency slot")
        };
        let prev = slot.prev.take();
        let next = slot.next.take();
        if let Some(prev) = prev {
            self.slots[prev].next = next;
        } else {
            self.head = next;
        }
        if let Some(next) = next {
            self.slots[next].prev = prev;
        } else {
            self.tail = prev;
        }
        self.bytes -= entry.bytes;
        entry
    }
    fn recycle(&mut self, id: Identity) {
        let slot = &mut self.slots[id.slot];
        debug_assert_eq!(slot.generation, id.generation);
        debug_assert!(slot.entry.is_none());
        // Exhausted generations are retired rather than aliasing an old id.
        if let Some(generation) = slot.generation.checked_add(1) {
            slot.generation = generation;
            slot.next = self.free;
            self.free = Some(id.slot);
        }
    }
    fn restore_after(&mut self, id: Identity, entry: Entry, previous: Option<Identity>) {
        let Some(previous) = previous.filter(|id| self.contains(*id)) else {
            self.restore_front(id, entry);
            return;
        };
        let next = self.slots[previous.slot].next;
        self.bytes += entry.bytes;
        let slot = &mut self.slots[id.slot];
        slot.entry = Some(entry);
        slot.prev = Some(previous.slot);
        slot.next = next;
        self.slots[previous.slot].next = Some(id.slot);
        if let Some(next) = next {
            self.slots[next].prev = Some(id.slot);
        } else {
            self.tail = Some(id.slot);
        }
    }
    fn restore_front(&mut self, id: Identity, entry: Entry) {
        let slot = &mut self.slots[id.slot];
        debug_assert_eq!(slot.generation, id.generation);
        debug_assert!(slot.entry.is_none());
        self.bytes += entry.bytes;
        slot.entry = Some(entry);
        slot.prev = None;
        slot.next = self.head;
        if let Some(head) = self.head {
            self.slots[head].prev = Some(id.slot);
        } else {
            self.tail = Some(id.slot);
        }
        self.head = Some(id.slot);
    }
}
impl Access<'_> {
    pub(super) fn track<T: Cell>(&self, page: &Rc<DirtyNode<T>>, node: &Node<T>, sequence: bool) {
        if matches!(node, Node::Staged(..)) {
            return;
        }
        let rows = match node {
            Node::Leaf(rows) => Some(super::decoded::row_sums(rows)),
            _ => None,
        };
        let weight = match (node, rows) {
            (Node::Leaf(leaf), Some(sums)) => super::decoded::leaf_weight(leaf, sums),
            _ => super::decoded::node_weight(node),
        };
        self.record(page, weight, sequence, rows);
    }
    /// Tracks `page` unless it is tracked already with what it holds now.
    pub(super) fn ensure_tracked<T: Cell>(&self, page: &Rc<DirtyNode<T>>, sequence: bool) {
        let known = self
            .dirty
            .borrow()
            .entry(page, &self.dirty)
            .is_some_and(|entry| entry.rows.is_some() || !matches!(&*page.borrow(), Node::Leaf(_)));
        if !known {
            self.track(page, &page.borrow(), sequence);
        }
    }
    /// Tracks `copy`, a private copy-on-write duplicate of `page`, as
    /// `page` is tracked: the same rows weigh the same, so a write after
    /// a publication does not measure the leaf again (FIG-6318).
    pub(super) fn track_copy<T: Cell>(
        &self,
        page: &Rc<DirtyNode<T>>,
        copy: &Rc<DirtyNode<T>>,
        sequence: bool,
    ) {
        let known = self
            .dirty
            .borrow()
            .entry(page, &self.dirty)
            .map(|entry| (entry.bytes, entry.rows));
        match known {
            Some((bytes, rows)) => self.record(copy, bytes - ENTRY_OVERHEAD, sequence, rows),
            None => self.track(copy, &copy.borrow(), sequence),
        }
    }
    /// Re-tracks a leaf after one of its rows changed from `before` (None
    /// for an inserted row) to `after` (None for a removed row), and gives
    /// the leaf's encoded body size. The other rows are not encoded again.
    pub(super) fn track_row<T: Cell>(
        &self,
        page: &Rc<DirtyNode<T>>,
        rows: &Vec<(Key, T)>,
        sequence: bool,
        before: Option<RowSize>,
        after: Option<RowSize>,
    ) -> usize {
        let known = self
            .dirty
            .borrow()
            .entry(page, &self.dirty)
            .and_then(|entry| entry.rows);
        let sums = known
            .and_then(|sums| match before {
                Some(row) => sums.without(row),
                None => Some(sums),
            })
            .map(|sums| after.map_or(sums, |row| sums.add(row)))
            .unwrap_or_else(|| super::decoded::row_sums(rows));
        let weight = super::decoded::leaf_weight(rows, sums);
        self.record(page, weight, sequence, Some(sums));
        sums.encoded
    }
    /// Seal preparation may relink a leaf's cells in place: its row sums
    /// are measured again when it is next tracked.
    pub(super) fn forget_rows<T: Cell>(&self, page: &Rc<DirtyNode<T>>) {
        let registration = page.registration.borrow();
        if let Some(registration) = registration.as_ref()
            && registration.registry.as_ptr() == Rc::as_ptr(&self.dirty)
        {
            let mut dirty = self.dirty.borrow_mut();
            if dirty.contains(registration.id) {
                let Some(entry) = dirty.slots[registration.id.slot].entry.as_mut() else {
                    unreachable!("registered residency slot")
                };
                entry.rows = None;
            }
        }
    }
    fn record<T: Cell>(
        &self,
        page: &Rc<DirtyNode<T>>,
        weight: usize,
        sequence: bool,
        rows: Option<RowSums>,
    ) {
        let bytes = weight + ENTRY_OVERHEAD;
        let mut registration = page.registration.borrow_mut();
        {
            let mut dirty = self.dirty.borrow_mut();
            if let Some(registered) = registration.as_ref()
                && registered.registry.as_ptr() == Rc::as_ptr(&self.dirty)
                && dirty.contains(registered.id)
            {
                let Some(entry) = dirty.slots[registered.id.slot].entry.as_mut() else {
                    unreachable!("registered residency slot")
                };
                let old_bytes = entry.bytes;
                entry.bytes = bytes;
                entry.sequence = sequence;
                entry.rows = rows;
                dirty.bytes = dirty.bytes - old_bytes + bytes;
                return;
            }
        }
        // A tree can be edited through another Access. Release the old
        // registry before borrowing this one, including stale generations.
        if let Some(old) = registration.take() {
            old.release();
        }
        // Weak references do not change copy-on-write's ownership test.
        let erased: Rc<dyn Stage> = page.clone();
        let entry = Entry {
            node: Rc::downgrade(&erased),
            bytes,
            sequence,
            rows,
        };
        let id = self.dirty.borrow_mut().append(entry);
        *registration = Some(Registration {
            registry: Rc::downgrade(&self.dirty),
            id,
        });
    }

    pub(super) fn stage_pressure(&self) -> Result<(), HeapFault> {
        {
            let mut dirty = self.dirty.borrow_mut();
            if dirty.staging || dirty.bytes <= DIRTY_LIMIT {
                return Ok(());
            }
            dirty.staging = true;
        }
        let result = (|| {
            loop {
                let entry = {
                    let mut dirty = self.dirty.borrow_mut();
                    if dirty.bytes <= DIRTY_TARGET {
                        break;
                    }
                    let mut candidate = dirty.head;
                    while let Some(index) = candidate {
                        if dirty.slots[index]
                            .entry
                            .as_ref()
                            .and_then(|entry| entry.node.upgrade())
                            .is_none_or(|node| node.can_spill())
                        {
                            break;
                        }
                        candidate = dirty.slots[index].next;
                    }
                    let Some(index) = candidate else {
                        if dirty.bytes > DIRTY_LIMIT {
                            return Err(HeapFault::Exhausted {
                                resource: lash_kernel_doc::heap::HostResource::DirtyResidency,
                            });
                        }
                        break;
                    };
                    let id = Identity {
                        slot: index,
                        generation: dirty.slots[index].generation,
                    };
                    let previous = dirty.slots[index].prev.map(|slot| Identity {
                        slot,
                        generation: dirty.slots[slot].generation,
                    });
                    (id, dirty.take(index), previous)
                };
                if let Some(node) = entry.1.node.upgrade()
                    && let Err(fault) = node.spill(self, entry.1.sequence)
                {
                    self.dirty
                        .borrow_mut()
                        .restore_after(entry.0, entry.1, entry.2);
                    return Err(fault);
                }
                self.dirty.borrow_mut().recycle(entry.0);
            }
            Ok(())
        })();
        self.dirty.borrow_mut().staging = false;
        result
    }
    #[cfg(test)]
    pub(crate) fn dirty_bytes(&self) -> usize {
        self.dirty.borrow().bytes
    }
}
