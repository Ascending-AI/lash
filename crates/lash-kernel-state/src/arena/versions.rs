//! Process-local fact versions and rollback of unpublished identity grants.
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
        self.registering = true;
        let next = self.next_ordinal;
        let dirty = self.dirty.len();
        Ok(Registration {
            arena: self,
            next,
            dirty,
            committed: false,
        })
    }
}

/// The mutation owner stages bodies separately. This guard only reserves the
/// identities used by a ledger journal. Abandoning it restores the cursor and
/// every new identity grant; immutable edge caches are suppressed while active.
/// Its exclusive borrow cannot survive a callback or another arena writer.
pub struct Registration<'a> {
    arena: &'a mut Arena,
    next: u64,
    dirty: usize,
    committed: bool,
}
impl Registration<'_> {
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
