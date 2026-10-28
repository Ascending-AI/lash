//! A sweep's walk of the arena (seam I, FIG-6748): what a sweep reads of
//! the objects a durable run holds in memory, in place of their
//! projection. The walk goes into each object without an id, and gives
//! every object with an id, and every task, to the census's trace, which
//! reads a resident one back from here ([`Arena::holds_resident`],
//! [`Arena::reach_entity`]). It assigns no id and writes nothing back.
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
    anonymous_local_bytes: u64,
}

impl Reach {
    pub fn anonymous_local_bytes(&self) -> u64 {
        self.anonymous_local_bytes
    }
}

fn stale() -> HeapFault {
    HeapFault::Malformed("a stale handle reached a sweep".into())
}

/// The bytes counted for a body, its values and its bounded size metadata.
fn body_bytes(values: u64) -> u64 {
    values
        .saturating_add(super::BODY_SLOTS)
        .saturating_mul(VALUE_BYTES)
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
        match object {
            NewObject::List(values) | NewObject::Tuple(values) => self.sequence_bound(values.len()),
            NewObject::Map(entries) | NewObject::Record(entries) => self.table_bound(entries.len()),
            NewObject::Set(members) => self.table_bound(members.len()),
            NewObject::LongText(text) => body_bytes(text.len() as u64 / 16),
            NewObject::Bytes(bytes) => body_bytes(bytes.len() as u64 / 16),
            NewObject::Function(_) => body_bytes(4),
            NewObject::BigInt(_) | NewObject::Timestamp(_) | NewObject::Error { .. } => {
                body_bytes(0)
            }
        }
    }

    /// What [`Self::install_bound`] gives for a new list or tuple of
    /// `values` values.
    #[inline]
    pub fn sequence_bound(&self, values: usize) -> u64 {
        body_bytes(values as u64)
    }

    /// What [`Self::install_bound`] gives for a new map, set or record of
    /// `entries` entries: a key and a value each.
    #[inline]
    pub fn table_bound(&self, entries: usize) -> u64 {
        body_bytes((entries as u64).saturating_mul(2))
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
        match kind {
            ValueKind::List | ValueKind::Tuple => self.sequence_bound(members),
            ValueKind::Set | ValueKind::Map | ValueKind::Record => self.table_bound(members),
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

    /// Whether the arena holds every member of `entity` in memory, so a
    /// trace can read its current edges without decoding its stored head.
    pub fn holds_resident(&self, entity: EntityId) -> bool {
        let Some(handle) = self.entities.get(&entity).copied() else {
            return false;
        };
        self.objects.get(handle).is_some_and(|body| !has_cold(body))
            && self
                .facts
                .get(handle.index() as usize)
                .is_some_and(|facts| !facts.cold)
    }

    /// Walks what the resident object `entity` holds, as [`Self::reach`]
    /// walks a value's members.
    ///
    /// # Errors
    ///
    /// As [`Self::reach`]'s.
    pub fn reach_entity(
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
            reach.marked.push(handle);
            if facts.entity.is_none() && is_mutable(body) {
                reach.anonymous_local_bytes = reach
                    .anonymous_local_bytes
                    .checked_add(self.logical_body_bytes(handle)?)
                    .ok_or_else(|| {
                        HeapFault::Malformed("anonymous holding cost overflow".into())
                    })?;
            }
            let facts = &self.facts[handle.index() as usize];
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| work.arena_marks += 1);
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

impl Arena {
    /// The occurrence price without projecting an immutable or loading its
    /// payload. An admitted cold descriptor already owns the complete summary.
    pub fn logical_local(&self, value: &Value) -> Result<u64, HeapFault> {
        use lash_kernel_doc::LogicalHeldV1 as Cost;
        let overflow = || HeapFault::Malformed("immutable holding cost overflow".into());
        if let Some(handle) = value.handle() {
            let body = self.objects.get(handle).ok_or_else(stale)?;
            let facts = &self.facts[handle.index() as usize];
            if !is_mutable(body)
                && let Some(page) = facts.page.get()
            {
                return Ok(Cost::local(page));
            }
            if matches!(value, Value::Tuple(_) | Value::Error(_)) {
                if let Some(cost) = facts.logical_immutable_cost.get() {
                    return Ok(*cost);
                }
                let cost = match (value, body) {
                    (Value::Tuple(_), Body::Tuple(sequence)) => {
                        let mut bytes = Cost::REFERENCE;
                        for index in 0..sequence.chunks.count() {
                            let values = sequence.chunks.resident(index).ok_or_else(|| {
                                HeapFault::Malformed("cold tuple has no admitted summary".into())
                            })?;
                            for member in values {
                                bytes = bytes
                                    .checked_add(self.logical_local(member)?)
                                    .ok_or_else(overflow)?;
                            }
                        }
                        bytes
                    }
                    (Value::Error(_), Body::Error(error)) => {
                        if !matches!(error.kind, Value::Text(_) | Value::LongText(_))
                            || !matches!(error.message, Value::Text(_) | Value::LongText(_))
                        {
                            return Err(HeapFault::Malformed(
                                "error text holding is malformed".into(),
                            ));
                        }
                        let kind = self
                            .logical_local(&error.kind)?
                            .checked_sub(Cost::REFERENCE)
                            .ok_or_else(overflow)?;
                        let message = self
                            .logical_local(&error.message)?
                            .checked_sub(Cost::REFERENCE)
                            .ok_or_else(overflow)?;
                        let data = self.logical_local(&error.data)?;
                        Cost::REFERENCE
                            .checked_add(kind)
                            .and_then(|n| n.checked_add(message))
                            .and_then(|n| n.checked_add(data))
                            .ok_or_else(overflow)?
                    }
                    _ => {
                        return Err(HeapFault::Malformed(
                            "immutable holding has another body kind".into(),
                        ));
                    }
                };
                let _ = facts.logical_immutable_cost.set(cost);
                return Ok(cost);
            }
        }
        let semantics = self.semantics();
        Cost::native_local(
            lash_kernel_doc::Args::new(&[], self.objects(), &semantics),
            value,
        )
        .map_err(|problem| HeapFault::Malformed(format!("logical occurrence cost: {problem}")))
    }

    /// Full direct body cost. A partly cold mutable uses its current paged
    /// head instead; calling this method must never price resident chunks only.
    pub fn logical_body_bytes(&self, handle: ArenaHandle) -> Result<u64, HeapFault> {
        use lash_kernel_doc::LogicalHeldV1 as Cost;
        let body = self.objects.get(handle).ok_or_else(stale)?;
        let members = |chunks: &lash_kernel_doc::arena::Chunks| -> Result<u64, HeapFault> {
            let mut bytes = 0_u64;
            for index in 0..chunks.count() {
                let values = chunks
                    .resident(index)
                    .ok_or_else(|| HeapFault::Malformed("logical body has cold members".into()))?;
                for value in values {
                    bytes = bytes
                        .checked_add(self.logical_local(value)?)
                        .ok_or_else(|| {
                            HeapFault::Malformed("member holding cost overflow".into())
                        })?;
                }
            }
            Ok(bytes)
        };
        let bytes = match body {
            Body::List(seq) => members(&seq.chunks)?,
            Body::Map(table) | Body::Set(table) => members(&table.chunks)?,
            Body::Record(table) => members(&table.chunks)?
                .checked_sub(
                    table
                        .len
                        .checked_mul(Cost::REFERENCE)
                        .ok_or_else(|| HeapFault::Malformed("record cost overflow".into()))?,
                )
                .ok_or_else(|| HeapFault::Malformed("record cost underflow".into()))?,
            Body::Closure { captures, .. } => (captures.len() as u64)
                .checked_mul(Cost::ENTITY_EDGE)
                .ok_or_else(|| HeapFault::Malformed("capture cost overflow".into()))?,
            Body::Cell(value) => self.logical_local(value)?,
            _ => {
                return Err(HeapFault::Malformed(
                    "immutable has no entity body price".into(),
                ));
            }
        };
        Cost::OBJECT
            .checked_add(bytes)
            .ok_or_else(|| HeapFault::Malformed("body holding cost overflow".into()))
    }

    pub fn logical_entity_bytes(&self, entity: EntityId) -> Result<u64, HeapFault> {
        let handle = self.entities.get(&entity).copied().ok_or_else(stale)?;
        self.logical_body_bytes(handle)
    }
}
