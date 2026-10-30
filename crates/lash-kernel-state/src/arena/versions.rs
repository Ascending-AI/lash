//! Process-local fact versions and rollback of unpublished identity and slot grants.
use super::*;

/// A borrowed content fact belongs to one arena, handle generation and value
/// revision. It is not a table-position proof and cannot cross a hard handoff.
#[derive(Clone, Debug)]
pub struct ContentVersion {
    arena: std::rc::Rc<()>,
    handle: ArenaHandle,
    token: u64,
    revision: u64,
}
impl PartialEq for ContentVersion {
    fn eq(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.arena, &other.arena)
            && self.handle == other.handle
            && self.token == other.token
            && self.revision == other.revision
    }
}
impl Eq for ContentVersion {}

impl Arena {
    /// The object generation and semantic content used by ledger facts. A
    /// saturated counter declines proof; table layout versions are insufficient
    /// because replacing an existing value preserves its position.
    pub fn content_version(&self, handle: ArenaHandle) -> Option<ContentVersion> {
        let facts = self.facts(handle).ok()?;
        (facts.content_revision != u64::MAX).then(|| ContentVersion {
            arena: std::rc::Rc::clone(&self.identity),
            handle,
            token: facts.token,
            revision: facts.content_revision,
        })
    }
    pub fn same_generation(&self, version: &ContentVersion) -> bool {
        std::rc::Rc::ptr_eq(&self.identity, &version.arena)
            && self
                .facts(version.handle)
                .is_ok_and(|facts| facts.token == version.token)
    }
    pub fn content_is(&self, version: &ContentVersion) -> bool {
        std::rc::Rc::ptr_eq(&self.identity, &version.arena)
            && self.facts(version.handle).is_ok_and(|facts| {
                facts.token == version.token
                    && facts.content_revision != u64::MAX
                    && facts.content_revision == version.revision
            })
    }

    pub(crate) fn registration_active(&self) -> bool {
        self.registering
    }
    pub fn registration(&mut self) -> Result<Registration<'_>, NativeError> {
        if self.registering {
            return Err(malformed("nested identity registration"));
        }
        self.registration_epoch = self
            .registration_epoch
            .checked_add(1)
            .ok_or_else(|| malformed("identity registrations overflow"))?;
        self.registering = true;
        let next = self.next_ordinal;
        let dirty = self.dirty.len();
        Ok(Registration {
            arena: self,
            next,
            dirty,
            committed: false,
            batch: Vec::new(),
        })
    }
}

/// The mutation owner stages bodies separately. This guard owns the identities
/// and uninstalled slots used by a ledger journal. Abandoning it restores the
/// cursor and grants; immutable edge caches are suppressed while active.
/// Its exclusive borrow cannot survive a callback or another arena writer.
pub struct Registration<'a> {
    arena: &'a mut Arena,
    next: u64,
    dirty: usize,
    committed: bool,
    batch: Vec<(ArenaHandle, NewObject)>,
}
impl Registration<'_> {
    pub(super) fn reserve_batch(
        &mut self,
        new: Vec<(ArenaHandle, NewObject)>,
    ) -> Result<(), NativeError> {
        assert!(self.batch.is_empty());
        self.batch = new;
        let result = self
            .arena
            .objects
            .prepare_install(self.batch.iter().map(|(handle, _)| *handle));
        if matches!(result, Err(lash_kernel_doc::heap::HeapFault::Malformed(_))) {
            // Forged slots confer no authority to return a live or unreserved slot.
            self.batch.clear();
        }
        result.map_err(NativeError::from)
    }
    pub(super) fn batch_len(&self) -> usize {
        self.batch.len()
    }
    pub(super) fn take_batch_object(&mut self, at: usize) -> (ArenaHandle, NewObject) {
        let (handle, object) = &mut self.batch[at];
        (
            *handle,
            std::mem::replace(object, NewObject::List(Vec::new())),
        )
    }

    pub fn reserve_entity(&mut self) -> Result<EntityId, NativeError> {
        let entity = EntityId {
            family: lash_kernel_doc::heap::FamilyId::PRIMARY,
            ordinal: self.arena.next_ordinal,
        };
        self.arena.next_ordinal = self
            .arena
            .next_ordinal
            .checked_add(1)
            .ok_or_else(|| malformed("identity ordinals overflow"))?;
        Ok(entity)
    }
    pub fn arena(&mut self) -> &mut Arena {
        self.arena
    }
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.arena
                .objects
                .discard_install(self.batch.iter().map(|(handle, _)| *handle));
            let mut kept = self.dirty;
            for at in self.dirty..self.arena.dirty.len() {
                let handle = self.arena.dirty[at];
                let facts = &mut self.arena.facts[handle.index() as usize];
                match facts.entity {
                    Some(entity)
                        if entity.family == lash_kernel_doc::heap::FamilyId::PRIMARY
                            && entity.ordinal >= self.next
                            && !facts.based =>
                    {
                        self.arena.entities.remove(&entity);
                        facts.entity = None;
                        facts.listed = false;
                    }
                    _ => {
                        self.arena.dirty[kept] = handle;
                        kept += 1;
                    }
                }
            }
            self.arena.dirty.truncate(kept);
            self.arena.next_ordinal = self.next;
        }
        self.arena.registering = false;
    }
}
