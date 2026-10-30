//! Storage preparation for an already journaled list member publication.
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
