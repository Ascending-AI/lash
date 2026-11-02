//! Detached storage preparation for an authenticated member boundary.
use super::*;
use lash_kernel_doc::heap::{HeapFault, HostResource};

/// An affine storage reservation. It changes no guest member until the
/// matching ledger publication invokes `publish` under its registration.
pub struct PreparedListWrite {
    version: ContentVersion,
    handle: ArenaHandle,
    index: u64,
    value: Value,
    old: Option<Value>,
    storage: Storage,
}
enum Storage {
    Replace { chunk: usize, at: usize },
    Inline,
    Vector(usize),
    Spill(Vec<Value>),
    Chunk(Vec<Value>),
}
fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<(), HeapFault> {
    values
        .try_reserve(additional)
        .map_err(|_| HeapFault::Exhausted {
            resource: HostResource::StagingBytes,
        })
}
impl Arena {
    /// Cold or ineligible positions decline before any semantic edit. Only
    /// physical capacity is reserved here; the ledger owns logical admission.
    pub fn prepare_list_write(
        &mut self,
        handle: ArenaHandle,
        index: u64,
        value: Value,
    ) -> Result<Option<PreparedListWrite>, HeapFault> {
        let Some(version) = self.content_version(handle) else {
            return Ok(None);
        };
        if !self.facts(handle).is_ok_and(|facts| !facts.cold) {
            return Ok(None);
        }
        let Some(Body::List(sequence)) = self.objects.get_mut(handle) else {
            return Ok(None);
        };
        let len = sequence.len;
        if index > len {
            return Ok(None);
        }
        let (old, storage) = if index < len {
            let (chunk, at) = sequence.locate(index);
            let Some(values) = sequence.chunks.resident(chunk) else {
                return Ok(None);
            };
            let Some(old) = values.get(at).copied() else {
                return Ok(None);
            };
            (Some(old), Storage::Replace { chunk, at })
        } else {
            if len == u64::MAX {
                return Ok(None);
            }
            let storage = match &mut sequence.chunks {
                Chunks::Inline { len, .. } if usize::from(*len) < Chunks::INLINE => Storage::Inline,
                Chunks::Inline { len, values } => {
                    let mut staged = Vec::new();
                    reserve(&mut staged, usize::from(*len) + 1)?;
                    staged.extend_from_slice(&values[..usize::from(*len)]);
                    Storage::Spill(staged)
                }
                Chunks::Whole(values) => {
                    reserve(values, 1)?;
                    Storage::Vector(0)
                }
                Chunks::Split(chunks)
                    if chunks.is_empty()
                        || (chunks.len() > 1
                            && len == chunks.len() as u64 * Sequence::CHUNK as u64) =>
                {
                    reserve(chunks, 1)?;
                    let mut staged = Vec::new();
                    reserve(&mut staged, 1)?;
                    Storage::Chunk(staged)
                }
                Chunks::Split(chunks) => {
                    let last = chunks.len() - 1;
                    let Chunk::Resident(values) = &mut chunks[last] else {
                        return Ok(None);
                    };
                    reserve(values, 1)?;
                    Storage::Vector(last)
                }
            };
            (None, storage)
        };
        // Publication's bookkeeping must not allocate after the member edit.
        reserve(&mut self.zero, 1)?;
        reserve(&mut self.dirty, 1)?;
        reserve(&mut self.facts[handle.index() as usize].cards, 1)?;
        Ok(Some(PreparedListWrite {
            version,
            handle,
            index,
            value,
            old,
            storage,
        }))
    }
}
impl PreparedListWrite {
    /// The private value of this one list position, including an unpublished
    /// append. Reads through certified aliases use the same shadow.
    pub fn value(&self) -> Value {
        self.value
    }

    /// Fold another private write to this position without touching the list.
    /// Source authorization and observer exclusion belong to the certificate.
    pub fn stage(&mut self, value: Value) {
        self.value = value;
    }

    pub fn edit(&self) -> crate::paged::logical::MemberEdit {
        crate::paged::logical::MemberEdit {
            handle: self.handle,
            member: crate::paged::logical::Member::List(self.index),
            next: Some(self.value),
        }
    }

    /// The accepted journal checks this same content dependency before invoking
    /// the closure. Registration suppresses the legacy member-event mirror.
    pub fn publish(self, arena: &mut Arena, edit: crate::paged::logical::MemberEdit) {
        assert!(arena.registration_active() && arena.content_is(&self.version));
        assert!(
            edit.handle == self.handle
                && matches!(edit.member, crate::paged::logical::Member::List(index) if index == self.index)
                && edit.next.is_some_and(|value| value.same(&self.value))
        );
        let Some(Body::List(sequence)) = arena.objects.get_mut(self.handle) else {
            unreachable!("a checked list publication changed body");
        };
        match self.storage {
            Storage::Replace { chunk, at } => {
                let Some(Chunk::Resident(values)) = sequence.chunks.get_mut(chunk) else {
                    unreachable!("a checked list publication lost its resident chunk");
                };
                values[at] = self.value;
            }
            Storage::Inline => assert!(sequence.chunks.push_inline(self.value)),
            Storage::Vector(chunk) => {
                let Some(Chunk::Resident(values)) = sequence.chunks.vec_mut(chunk) else {
                    unreachable!("a checked list publication lost its reserved vector");
                };
                values.push(self.value);
            }
            Storage::Spill(mut values) => {
                values.push(self.value);
                sequence.chunks = Chunks::Whole(values);
            }
            Storage::Chunk(mut values) => {
                values.push(self.value);
                let Chunks::Split(chunks) = &mut sequence.chunks else {
                    unreachable!("a checked list publication changed chunk representation");
                };
                chunks.push(Chunk::Resident(values));
            }
        }
        if self.old.is_none() {
            sequence.len += 1;
            arena.resident = arena.resident.saturating_add(1);
        }
        arena.update_flat(self.handle, self.old.as_ref(), Some(&self.value));
        arena.hold(&self.value);
        if let Some(old) = self.old {
            arena.unhold(&old);
        }
        Arena::mark_resident(
            &mut arena.facts[self.handle.index() as usize],
            &mut arena.dirty,
            self.handle,
            Some(self.index / CARD),
        );
    }
}

/// Prepared replacement of a resident shared variable's member. The cell's
/// identity and every binding or borrowed capture remain unchanged.
pub struct PreparedCellWrite {
    version: ContentVersion,
    handle: ArenaHandle,
    value: Value,
    old: Value,
}
impl Arena {
    /// Reserve mutation bookkeeping without changing the cell or its edges.
    pub fn prepare_cell_write(
        &mut self,
        handle: ArenaHandle,
        value: Value,
    ) -> Result<Option<PreparedCellWrite>, HeapFault> {
        let Some(version) = self.content_version(handle) else {
            return Ok(None);
        };
        if !self.facts(handle).is_ok_and(|facts| !facts.cold) {
            return Ok(None);
        }
        let Some(Body::Cell(old)) = self.objects.get(handle) else {
            return Ok(None);
        };
        let old = *old;
        reserve(&mut self.zero, 1)?;
        reserve(&mut self.dirty, 1)?;
        Ok(Some(PreparedCellWrite {
            version,
            handle,
            value,
            old,
        }))
    }
}
impl PreparedCellWrite {
    /// Read the private shadow through every certified alias of this cell.
    /// The arena continues to expose its original member until publication.
    pub fn value(&self) -> Value {
        self.value
    }

    /// Replace only the detached shadow. The certified kernel owns observer
    /// exclusion and source-order peak/charge facts; this reserves no logical
    /// authority and changes neither the cell nor its alias/capture owners.
    pub fn stage(&mut self, value: Value) {
        self.value = value;
    }

    /// The exact final member installed by this storage reservation. Its old
    /// provenance remains the original cell and content version captured at
    /// preparation, regardless of how many private writes were folded.
    pub fn edit(&self) -> crate::paged::logical::MemberEdit {
        crate::paged::logical::MemberEdit {
            handle: self.handle,
            member: crate::paged::logical::Member::Variable,
            next: Some(self.value),
        }
    }

    /// Install exactly the checked member edit under its accepted registration.
    pub fn publish(self, arena: &mut Arena, edit: crate::paged::logical::MemberEdit) {
        assert!(arena.registration_active() && arena.content_is(&self.version));
        assert!(
            edit.handle == self.handle
                && matches!(edit.member, crate::paged::logical::Member::Variable)
                && edit.next.is_some_and(|value| value.same(&self.value))
        );
        let Some(Body::Cell(held)) = arena.objects.get_mut(self.handle) else {
            unreachable!("a checked variable publication changed body");
        };
        *held = self.value;
        arena.hold(&self.value);
        arena.unhold(&self.old);
        Arena::mark_resident(
            &mut arena.facts[self.handle.index() as usize],
            &mut arena.dirty,
            self.handle,
            None,
        );
    }
}

/// A resident list removal prepared without changing the list or its edges.
pub struct PreparedListRemove {
    version: ContentVersion,
    handle: ArenaHandle,
    index: u64,
    len: u64,
    removed: Value,
    values: Vec<Value>,
}
impl Arena {
    /// Reserve resident storage and mutation bookkeeping without editing a member.
    pub fn prepare_list_remove(
        &mut self,
        handle: ArenaHandle,
        index: u64,
    ) -> Result<Option<PreparedListRemove>, HeapFault> {
        let Some(version) = self.content_version(handle) else {
            return Ok(None);
        };
        let Ok(Body::List(sequence)) = self.body(handle) else {
            return Ok(None);
        };
        if !self.facts(handle).is_ok_and(|facts| !facts.cold) || index >= sequence.len {
            return Ok(None);
        }
        let len = sequence.len;
        let Ok(held) = self.sequence(handle) else {
            return Ok(None);
        };
        let Some(removed) = held.get(index as usize).copied() else {
            return Ok(None);
        };
        let mut values = Vec::new();
        reserve(&mut values, held.len() - 1)?;
        values.extend_from_slice(&held[..index as usize]);
        values.extend_from_slice(&held[index as usize + 1..]);
        reserve(&mut self.zero, 1)?;
        reserve(&mut self.dirty, 1)?;
        let cards = ((len - 1) / CARD - index / CARD + 1) as usize;
        reserve(&mut self.facts[handle.index() as usize].cards, cards)?;
        Ok(Some(PreparedListRemove {
            version,
            handle,
            index,
            len,
            removed,
            values,
        }))
    }
}
impl PreparedListRemove {
    /// Install only the checked removal under its accepted registration.
    pub fn publish(self, arena: &mut Arena, edit: crate::paged::logical::MemberEdit) {
        assert!(arena.registration_active() && arena.content_is(&self.version));
        assert!(
            edit.handle == self.handle
                && matches!(edit.member, crate::paged::logical::Member::List(index) if index == self.index)
                && edit.next.is_none()
        );
        let Some(Body::List(sequence)) = arena.objects.get_mut(self.handle) else {
            unreachable!("a checked list removal changed body");
        };
        *sequence = Sequence::whole(self.values);
        arena.resident = arena.resident.saturating_sub(1);
        arena.update_flat(self.handle, Some(&self.removed), None);
        arena.unhold(&self.removed);
        for card in self.index / CARD..=(self.len - 1) / CARD {
            Arena::mark_resident(
                &mut arena.facts[self.handle.index() as usize],
                &mut arena.dirty,
                self.handle,
                Some(card),
            );
        }
    }
}
