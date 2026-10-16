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
impl<T: Cell> Stage for DirtyNode<T> {
    fn can_spill(&self) -> bool {
        self.try_borrow_mut().is_ok()
    }
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault> {
        let node = self.borrow().clone();
        if matches!(node, Node::Staged(..)) {
            return Ok(());
        }
        // Preparing and sealing may change storage links, never guest content.
        // All working versions sharing this node see the same immutable stage.
        let mut branch = dirty(node, access, sequence)?;
        let mut tree = Tree {
            root: Some(branch.clone()),
            sequence,
        };
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        tree.seal(access, &mut work)?;
        branch = tree
            .root
            .ok_or_else(|| HeapFault::Malformed("nonempty staged tree lost its root".into()))?;
        let Link::Sealed(page) = branch.link else {
            unreachable!()
        };
        *self.borrow_mut() = Node::Staged(
            page,
            branch
                .encoded_bytes
                .ok_or_else(|| HeapFault::Malformed("staged tree has no byte summary".into()))?,
            branch
                .is_leaf
                .ok_or_else(|| HeapFault::Malformed("staged tree has no node kind".into()))?,
        );
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

#[cfg(test)]
mod laws {
    use super::*;

    struct UnusedSource;
    impl PageSource for UnusedSource {
        fn read(&mut self, _: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
            unreachable!("this law never reads")
        }
        fn seal(&mut self, _: PageBody) -> Result<PageRef, HeapFault> {
            unreachable!("this law never stages")
        }
    }

    /// D-CAP / FIG-6568: a bounded live set has bounded bookkeeping, even
    /// when private working versions are discarded before staging pressure.
    #[test]
    fn short_lived_nodes_reuse_one_residency_slot() {
        let mut source = UnusedSource;
        let access = Access::borrowed(&mut source);
        for _ in 0..128 {
            let page = Rc::new(DirtyNode::new(Node::<lash_kernel_doc::TaskId>::Leaf(
                Vec::new(),
            )));
            for _ in 0..16 {
                access.record(&page, 1, false, None);
            }
            drop(page);
        }
        let dirty = access.dirty.borrow();
        assert_eq!(
            dirty.entry_allocations, 1,
            "reuse bookkeeping storage for a bounded live set"
        );
        assert_eq!(
            dirty.bytes,
            dirty.slots.capacity() * std::mem::size_of::<Slot>(),
            "only the reusable slot storage remains resident"
        );
    }
    fn page() -> Rc<DirtyNode<lash_kernel_doc::TaskId>> {
        Rc::new(DirtyNode::new(Node::Leaf(Vec::new())))
    }

    fn identity(page: &DirtyNode<lash_kernel_doc::TaskId>) -> Identity {
        let registration = page.registration.borrow();
        let Some(registration) = registration.as_ref() else {
            panic!("page is registered")
        };
        registration.id
    }

    /// D-CAP: a nested publication edit keeps borrowed ancestors charged;
    /// a later spill failure restores its FIFO position behind those pages.
    #[test]
    fn borrowed_publication_pages_keep_their_weight_and_fifo_on_spill_failure() {
        struct RefusingSource;
        impl PageSource for RefusingSource {
            fn read(&mut self, _: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
                unreachable!("no read")
            }
            fn seal(&mut self, _: PageBody) -> Result<PageRef, HeapFault> {
                Err(HeapFault::Exhausted {
                    resource: lash_kernel_doc::heap::HostResource::StagingBytes,
                })
            }
        }
        let mut source = RefusingSource;
        let access = Access::borrowed(&mut source);
        let (first, next) = (page(), page());
        access.record(&first, DIRTY_LIMIT / 2 + 1, false, None);
        access.record(&next, DIRTY_LIMIT / 2 + 1, false, None);
        let first_id = identity(&first);
        let next_id = identity(&next);
        let bytes = access.dirty.borrow().bytes;
        let _editing = first.borrow_mut();
        assert!(matches!(
            access.stage_pressure(),
            Err(HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })
        ));
        let dirty = access.dirty.borrow();
        assert_eq!(dirty.bytes, bytes);
        assert_eq!(dirty.head, Some(first_id.slot));
        assert_eq!(dirty.slots[first_id.slot].next, Some(next_id.slot));
        assert_eq!(dirty.tail, Some(next_id.slot));
    }

    /// D-CAP / D-SEALTIME: reuse cannot alias a staged generation or
    /// another Access, reorder surviving entries, or lose a failed spill.
    #[test]
    fn recycled_generations_keep_fifo_and_failed_spill_identity() {
        let mut source = UnusedSource;
        let access = Access::borrowed(&mut source);
        let (first, middle, last) = (page(), page(), page());
        for page in [&first, &middle, &last] {
            access.record(page, 1, false, None);
        }
        let first_id = identity(&first);
        let middle_id = identity(&middle);
        let last_id = identity(&last);
        // A warm update keeps its FIFO position and allocates no new slot.
        access.record(&first, 7, false, None);
        drop(middle);
        let replacement = page();
        access.record(&replacement, 3, false, None);
        let replacement_id = identity(&replacement);
        assert_eq!(replacement_id.slot, middle_id.slot);
        assert_ne!(replacement_id.generation, middle_id.generation);
        {
            let dirty = access.dirty.borrow();
            assert_eq!(dirty.head, Some(first_id.slot));
            assert_eq!(dirty.slots[first_id.slot].next, Some(last_id.slot));
            assert_eq!(dirty.slots[last_id.slot].next, Some(replacement_id.slot));
            assert_eq!(dirty.entry_allocations, 3);
        }
        // Stage the first generation while it remains shared, then reuse
        // its slot. Its later destructor must not unregister the new owner.
        {
            let mut dirty = access.dirty.borrow_mut();
            dirty.take(first_id.slot);
            dirty.recycle(first_id);
        }
        let reused = page();
        access.record(&reused, 5, false, None);
        drop(first);
        assert!(
            access
                .dirty
                .borrow()
                .entry(&reused, &access.dirty)
                .is_some()
        );
        // A failed spill reserves the removed slot while nested work tracks
        // another page, and restores the original weight at the FIFO front.
        let bytes = access.dirty.borrow().bytes;
        let capacity = access.dirty.borrow().slots.capacity();
        let entry = access.dirty.borrow_mut().take(last_id.slot);
        let nested = page();
        access.record(&nested, 11, false, None);
        assert_ne!(identity(&nested).slot, last_id.slot);
        drop(nested);
        access.dirty.borrow_mut().restore_front(last_id, entry);
        assert_eq!(access.dirty.borrow().head, Some(last_id.slot));
        let retained_growth =
            std::mem::size_of::<Slot>() * (access.dirty.borrow().slots.capacity() - capacity);
        assert_eq!(access.dirty.borrow().bytes, bytes + retained_growth);
        // A page edited through a different Access leaves the first registry
        // without letting that registry's recycled identity affect the second.
        let mut other_source = UnusedSource;
        let other = Access::borrowed(&mut other_source);
        other.record(&last, 13, true, None);
        assert!(access.dirty.borrow().entry(&last, &access.dirty).is_none());
        assert_eq!(
            other
                .dirty
                .borrow()
                .entry(&last, &other.dirty)
                .map(|entry| entry.bytes),
            Some(13 + ENTRY_OVERHEAD)
        );
        drop(last);
        assert!(other.dirty.borrow().head.is_none());
    }
}
