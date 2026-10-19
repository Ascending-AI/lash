//! A sweep's walk of the arena (seam I, FIG-6748): what a sweep reads of
//! the objects a durable run holds in memory, in place of their
//! projection. The walk goes into each object without an id, and gives
//! every object with an id, and every task, to the census's trace, which
//! reads a changed one back from here ([`Arena::holds_changed`],
//! [`Arena::reach_changed`]). It assigns no id and writes nothing back.
//!
//! It also gives the capacity what a region's statement would install
//! before it installs it ([`Arena::install_bound`],
//! [`Arena::construction_floor`]), in the meter's bytes (FIG-6791).

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue};
use lash_kernel_doc::{NewObject, Value, ValueKind};

use super::{Arena, VALUE_BYTES, each_member, has_cold, is_mutable};

/// What a sweep's walk gives the census's trace.
pub enum Reached<'a> {
    /// An object with an id, or a task.
    Identity(Identity),
    /// The page form of an immutable on its pages: the identities it names.
    Page(&'a PageValue),
}

/// One sweep's walk: the objects it marked.
#[derive(Debug, Default)]
pub struct Reach {
    marked: Vec<ArenaHandle>,
}

fn stale() -> HeapFault {
    HeapFault::Malformed("a stale handle reached a sweep".into())
}

/// The bytes the capacity meter counts for a body of `values` values.
fn body_bytes(values: u64) -> u64 {
    values.saturating_add(1).saturating_mul(VALUE_BYTES)
}

impl Arena {
    /// The bytes [`Arena::install`] adds to what the arena holds for
    /// `new`: what a region admits before it installs what a statement
    /// built, so that their capacity is counted before they exist
    /// (`docs/design/kernel-regions.md` §3.5).
    #[inline]
    pub fn install_bound(&self, new: &[(ArenaHandle, NewObject)]) -> u64 {
        new.iter()
            .map(|(_, object)| self.object_bound(object))
            .fold(0, u64::saturating_add)
    }

    /// What [`Self::install_bound`] gives for `object` alone.
    #[inline]
    pub fn object_bound(&self, object: &NewObject) -> u64 {
        body_bytes(match object {
            NewObject::List(values) | NewObject::Tuple(values) => values.len() as u64,
            NewObject::Map(entries) | NewObject::Record(entries) => 2 * entries.len() as u64,
            NewObject::Set(members) => 2 * members.len() as u64,
            NewObject::LongText(text) | NewObject::LongFunction(text) => text.len() as u64 / 16,
            NewObject::Bytes(bytes) => bytes.len() as u64 / 16,
            NewObject::BigInt(_) | NewObject::Timestamp(_) | NewObject::Error { .. } => 0,
        })
    }

    /// What [`Self::install_bound`] gives for a new long text of `bytes`
    /// UTF-8 bytes.
    #[inline]
    pub fn text_install_bound(&self, bytes: u64) -> u64 {
        body_bytes(bytes / 16)
    }

    /// No more than [`Self::install_bound`] gives for a new list, set, map
    /// or record of `members` members (a map's or a record's entries), nor
    /// than a new tuple of `members` members: what a region admits before
    /// it collects a construction's members, so that one its room cannot
    /// hold leaves its statement before they are allocated.
    #[inline]
    pub fn construction_floor(&self, kind: ValueKind, members: usize) -> u64 {
        let members = members as u64;
        match kind {
            ValueKind::List | ValueKind::Tuple => body_bytes(members),
            ValueKind::Set | ValueKind::Map | ValueKind::Record => {
                body_bytes(members.saturating_mul(2))
            }
            _ => 0,
        }
    }

    /// Walks what `value` reaches in memory: each object without an id
    /// once, walked into; an immutable walked into, or given to `out` by
    /// its page form when it is on its pages; each object with an id, and
    /// each task, given to `out` once and not walked into.
    ///
    /// # Errors
    ///
    /// A stale handle, and `out`'s errors.
    pub fn reach(
        &mut self,
        reach: &mut Reach,
        value: &Value,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match value {
            Value::Task(task) | Value::TaskRef(task) => {
                out(Reached::Identity(Identity::Task(*task)))
            }
            _ => match value.handle() {
                Some(handle) => self.reach_from(reach, vec![handle], out),
                None => Ok(()),
            },
        }
    }

    /// Whether the arena holds `entity` resident and changed since its
    /// projection: what the heap holds of it is stale.
    pub fn holds_changed(&self, entity: EntityId) -> bool {
        let Some(handle) = self.entities.get(&entity).copied() else {
            return false;
        };
        self.objects.get(handle).is_some()
            && self
                .facts
                .get(handle.index() as usize)
                .is_some_and(|facts| facts.dirty && !facts.cold)
    }

    /// Walks what the changed object `entity` holds, as [`Self::reach`]
    /// walks a value's members.
    ///
    /// # Errors
    ///
    /// As [`Self::reach`]'s.
    pub fn reach_changed(
        &mut self,
        reach: &mut Reach,
        entity: EntityId,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let Some(handle) = self.entities.get(&entity).copied() else {
            return Ok(());
        };
        let mut pending = Vec::new();
        let tasks = self.members(handle, &mut pending)?;
        for task in tasks {
            out(Reached::Identity(Identity::Task(task)))?;
        }
        self.reach_from(reach, pending, out)
    }

    /// The changed objects with an id that are not resident whole: what a
    /// refinement decodes first, so that their bodies give their edges.
    pub fn changed_cold(&self) -> Vec<ArenaHandle> {
        self.dirty
            .iter()
            .copied()
            .filter(|handle| {
                self.objects.get(*handle).is_some_and(|body| {
                    let facts = &self.facts[handle.index() as usize];
                    facts.dirty && facts.entity.is_some() && has_cold(body)
                })
            })
            .collect()
    }

    /// The ids of the objects changed since they were projected, or never
    /// projected, ascending: what a sweep forgets when nothing reaches it.
    pub fn changed_entities(&self) -> Vec<EntityId> {
        let mut entities: Vec<EntityId> = self
            .dirty
            .iter()
            .filter(|handle| self.objects.get(**handle).is_some())
            .filter_map(|handle| {
                let facts = &self.facts[handle.index() as usize];
                facts.dirty.then_some(facts.entity).flatten()
            })
            .collect();
        entities.sort_unstable();
        entities.dedup();
        entities
    }

    /// Forgets the changes to `entities`, which a refinement swept: no
    /// projection writes them back, and nothing keeps them for one.
    pub fn forget_swept(&mut self, entities: &[EntityId]) {
        for entity in entities {
            let Some(handle) = self.entities.get(entity).copied() else {
                continue;
            };
            if self.objects.get(handle).is_none() {
                continue;
            }
            let facts = &mut self.facts[handle.index() as usize];
            facts.dirty = false;
            facts.cards.clear();
        }
    }

    /// Ends the walk: unmarks what it marked.
    pub fn end_reach(&mut self, reach: Reach) {
        for handle in reach.marked {
            self.facts[handle.index() as usize].mark = false;
        }
    }

    fn reach_from(
        &mut self,
        reach: &mut Reach,
        mut pending: Vec<ArenaHandle>,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        while let Some(handle) = pending.pop() {
            let body = self.objects.get(handle).ok_or_else(stale)?;
            let facts = &mut self.facts[handle.index() as usize];
            if facts.mark {
                continue;
            }
            facts.mark = true;
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| work.arena_marks += 1);
            reach.marked.push(handle);
            if let Some(entity) = facts.entity {
                out(Reached::Identity(Identity::Object(entity)))?;
                continue;
            }
            if !is_mutable(body) && (facts.cold || has_cold(body)) {
                if let Some(page) = facts.page.get() {
                    out(Reached::Page(page))?;
                }
                continue;
            }
            for task in self.members(handle, &mut pending)? {
                out(Reached::Identity(Identity::Task(task)))?;
            }
        }
        Ok(())
    }

    /// Pushes the objects `handle` holds, and gives the tasks it names.
    fn members(
        &self,
        handle: ArenaHandle,
        pending: &mut Vec<ArenaHandle>,
    ) -> Result<Vec<lash_kernel_doc::TaskId>, HeapFault> {
        let body = self.objects.get(handle).ok_or_else(stale)?;
        let mut tasks = Vec::new();
        each_member(body, &mut |member| match member {
            Value::Task(task) | Value::TaskRef(task) => tasks.push(*task),
            _ => pending.extend(member.handle()),
        });
        if let Body::Closure { captures, .. } = body {
            pending.extend(captures.iter().copied());
        }
        Ok(tasks)
    }
}
