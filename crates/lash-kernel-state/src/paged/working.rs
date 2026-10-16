//! Working object heads (FIG-6465, S1 amendment): the activation's bounded
//! directory of the object heads it reads and edits. A write edits its
//! working head in place, as a frame edits its working records (FIG-6435):
//! it neither locates nor re-encodes the enclosing directory row. The
//! persistent directory receives a head when it is flushed: at seal, at
//! eviction, before the census enumerates entities, and when the bound
//! pushes the head out.
use super::ObjectKind;
use super::directory::{ObjectHead, entity_bytes, entity_key};
use super::heap_impl::PagedHeap;
use super::tree::Cell;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault};
use std::cell::Cell as Counter;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

/// The most heads an activation keeps working. Beyond it, the least
/// recently used half leaves: clean heads are dropped, edited ones are
/// written to the directory first.
pub(super) const WORKING_HEADS: usize = 256;

/// Hashes an identity's two words; the heads' order never derives from it.
#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }
    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

#[derive(Default)]
pub(super) struct WorkingHeads {
    heads: HashMap<EntityId, Working, BuildHasherDefault<IdentityHasher>>,
    tick: Counter<u64>,
    /// How many heads hold an edit the directory has not received.
    dirty: usize,
}

struct Working {
    head: Rc<ObjectHead>,
    /// The directory holds a row for the entity, possibly an older version.
    stored: bool,
    /// The directory's row, or its absence, is older than `head`.
    dirty: bool,
    used: Counter<u64>,
}

/// What an edit replaces: the current head's cost and, when the entity owns
/// a directory row (written or pending), that row's populations.
pub(super) struct Taken {
    pub cost: u64,
    pub owned: Option<(bool, u64)>,
}

impl WorkingHeads {
    fn touch(&self, working: &Working) {
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        working.used.set(tick);
    }
    fn get(&self, entity: EntityId) -> Option<Rc<ObjectHead>> {
        let working = self.heads.get(&entity)?;
        self.touch(working);
        Some(Rc::clone(&working.head))
    }
    fn insert(&mut self, entity: EntityId, head: Rc<ObjectHead>, stored: bool, dirty: bool) {
        let working = Working {
            head,
            stored,
            dirty,
            used: Counter::new(0),
        };
        self.touch(&working);
        self.dirty += usize::from(dirty);
        self.heads.insert(entity, working);
    }
    /// The least recently used half of the heads `keep` does not keep, in
    /// identity order: the bound's victims.
    fn victims(&self, keep: impl Fn(&Working) -> bool) -> Vec<EntityId> {
        let mut candidates: Vec<_> = self
            .heads
            .iter()
            .filter(|(_, working)| !keep(working))
            .map(|(entity, working)| (working.used.get(), *entity))
            .collect();
        candidates.sort_unstable();
        candidates.truncate(WORKING_HEADS / 2);
        let mut victims: Vec<_> = candidates.into_iter().map(|(_, entity)| entity).collect();
        victims.sort_unstable();
        victims
    }
    fn remove(&mut self, entity: EntityId) -> Option<Working> {
        let working = self.heads.remove(&entity)?;
        self.dirty -= usize::from(working.dirty);
        Some(working)
    }
}

impl PagedHeap {
    /// The entity's current head: its working head, or the head the
    /// directory or its frozen family holds, which becomes working. A read
    /// never writes the directory, so when every working head holds an
    /// edit the head it loads is not kept.
    pub(super) fn current_head(
        &self,
        entity: EntityId,
    ) -> Result<Option<Rc<ObjectHead>>, HeapFault> {
        if let Some(head) = self.working.borrow().get(entity) {
            return Ok(Some(head));
        }
        let Some((head, stored)) = self.load_head(entity)? else {
            return Ok(None);
        };
        let head = Rc::new(head);
        let mut working = self.working.borrow_mut();
        if working.heads.len() >= WORKING_HEADS {
            for victim in working.victims(|working| working.dirty) {
                working.remove(victim);
            }
        }
        if working.heads.len() < WORKING_HEADS {
            working.insert(entity, Rc::clone(&head), stored, false);
        }
        Ok(Some(head))
    }
    /// The head the persistent directory or the entity's frozen family
    /// holds, and whether the directory holds it.
    fn load_head(&self, entity: EntityId) -> Result<Option<(ObjectHead, bool)>, HeapFault> {
        let family = if entity.family == FamilyId::PRIMARY {
            None
        } else {
            let Some(family) = self
                .families
                .get(&entity.family.0.to_be_bytes(), &self.access)?
            else {
                return Ok(None);
            };
            if entity.ordinal >= family.extent {
                return Err(HeapFault::Corrupt {
                    page: family.base.root.id,
                    reason: "relative occurrence is outside its family extent".into(),
                });
            }
            if !self.family_member(&family, entity.ordinal)? {
                return Ok(None);
            }
            Some(family)
        };
        if let Some(head) = self.directory.get(&entity_bytes(entity), &self.access)? {
            return Ok(Some((head, true)));
        }
        let Some(family) = family else {
            return Ok(None);
        };
        let mut base = self.family_base.borrow_mut();
        let base = base
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("lazy family reader is not installed".into()))?;
        let head = base
            .base_object(entity, &family.base, &self.access)?
            .ok_or_else(|| {
                HeapFault::Malformed("a live family entity has no frozen body".into())
            })?;
        if head.family != entity.family {
            return Err(HeapFault::Malformed(
                "frozen head has another family context".into(),
            ));
        }
        Ok(Some((head, false)))
    }
    /// Makes the entity's head working for an edit. `None` for an entity
    /// that has none, such as one being allocated.
    pub(super) fn take_working(&mut self, entity: EntityId) -> Result<Option<Taken>, HeapFault> {
        if !self.working.get_mut().heads.contains_key(&entity) {
            if entity.family == FamilyId::PRIMARY && entity.ordinal >= self.next_ordinal() {
                return Ok(None);
            }
            let Some((head, stored)) = self.load_head(entity)? else {
                return Ok(None);
            };
            self.make_room()?;
            self.working
                .get_mut()
                .insert(entity, Rc::new(head), stored, false);
        }
        let working = self.working.get_mut();
        let entry = &working.heads[&entity];
        working.touch(entry);
        Ok(Some(Taken {
            cost: entry.head.cost(),
            owned: (entry.stored || entry.dirty).then(|| {
                (
                    entry.head.kind() == ObjectKind::Closure,
                    entry.head.task_edges(),
                )
            }),
        }))
    }
    /// Edits a head [`Self::take_working`] made working, in place. A
    /// caller holding a copy of the head keeps its version: the edit copies
    /// what it shares.
    pub(super) fn edit_working<R>(
        &mut self,
        entity: EntityId,
        edit: impl FnOnce(&mut ObjectHead, &super::tree::Access<'static>) -> Result<R, HeapFault>,
    ) -> Result<R, HeapFault> {
        let working = self.working.get_mut();
        let entry = working
            .heads
            .get_mut(&entity)
            .ok_or_else(|| HeapFault::Malformed("an edited head is not working".into()))?;
        if !entry.dirty {
            entry.dirty = true;
            working.dirty += 1;
        }
        edit(Rc::make_mut(&mut entry.head), &self.access)
    }
    /// A newly allocated entity's head, which the directory receives when
    /// it is flushed.
    pub(super) fn insert_working(
        &mut self,
        entity: EntityId,
        head: ObjectHead,
    ) -> Result<(), HeapFault> {
        self.make_room()?;
        self.working
            .get_mut()
            .insert(entity, Rc::new(head), false, true);
        Ok(())
    }
    /// Keeps the bound: at it, the least recently used half leaves.
    fn make_room(&mut self) -> Result<(), HeapFault> {
        if self.working.get_mut().heads.len() < WORKING_HEADS {
            return Ok(());
        }
        let victims = self.working.get_mut().victims(|_| false);
        self.write_back(victims)
    }
    /// Writes the named heads' edits to the directory and drops them.
    fn write_back(&mut self, entities: Vec<EntityId>) -> Result<(), HeapFault> {
        for entity in entities {
            if let Some(working) = self.working.get_mut().remove(entity)
                && working.dirty
            {
                let head = Rc::unwrap_or_clone(working.head);
                self.directory
                    .write(entity_key(entity), head, &self.access)?;
            }
        }
        Ok(())
    }
    /// Writes every edited head to the directory, in identity order, and
    /// drops it; clean heads stay working.
    pub(super) fn flush_working(&mut self) -> Result<(), HeapFault> {
        let working = self.working.get_mut();
        if working.dirty == 0 {
            return Ok(());
        }
        let mut dirty: Vec<_> = working
            .heads
            .iter()
            .filter(|(_, working)| working.dirty)
            .map(|(entity, _)| *entity)
            .collect();
        dirty.sort_unstable();
        self.write_back(dirty)
    }
    /// Flushes every edit and drops every working head: what a seal
    /// publishes and an eviction releases.
    pub(super) fn clear_working(&mut self) -> Result<(), HeapFault> {
        self.flush_working()?;
        *self.working.get_mut() = WorkingHeads::default();
        Ok(())
    }
    /// Whether a head holds an edit the directory has not received.
    pub(super) fn working_dirty(&self) -> bool {
        self.working.borrow().dirty != 0
    }
    /// Drops a swept entity's working head: its current head, and whether
    /// the entity owned a directory row, when it was working.
    pub(super) fn sweep_working(&mut self, entity: EntityId) -> Option<(Rc<ObjectHead>, bool)> {
        self.working
            .get_mut()
            .remove(entity)
            .map(|working| (working.head, working.stored || working.dirty))
    }
}
