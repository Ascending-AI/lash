//! A refinement's walk of the arena (seam I, FIG-6748): what an exact
//! refinement reads of the objects a durable run holds in memory, in place
//! of their projection. Refinement prices cold objects by their persisted
//! summaries and resident ones by their bodies, and both give the same
//! exact live cost: so the walk prices each object without an id at its
//! cost and walks into it, and gives every object with an id, and every
//! task, to the census's trace, which reads a changed one back from here
//! ([`Arena::changed_cost`], [`Arena::price_changed`]). It assigns no id
//! and writes nothing back.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue};

use super::{Arena, each_member, has_cold, is_mutable};

/// What a refinement's walk gives the census's trace.
pub enum Reached<'a> {
    /// An object with an id, or a task.
    Identity(Identity),
    /// The page form of an immutable on its pages: the identities it names.
    Page(&'a PageValue),
}

/// One refinement's walk: the objects it marked, and what it priced.
#[derive(Debug, Default)]
pub struct Pricing {
    marked: Vec<ArenaHandle>,
    /// The cost of the objects without an id the walk reached.
    pub unidentified: u64,
}

fn stale() -> HeapFault {
    HeapFault::Malformed("a stale handle reached a refinement".into())
}

impl Arena {
    /// Walks what `value` reaches in memory: each object without an id
    /// once, priced at its cost and walked into; an immutable walked into,
    /// its holder pricing it, or given to `out` by its page form when it
    /// is on its pages; each object with an id, and each task, given to
    /// `out` once and not walked into.
    ///
    /// # Errors
    ///
    /// A stale handle, and `out`'s errors.
    pub fn price(
        &mut self,
        pricing: &mut Pricing,
        value: &Value,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match value {
            Value::Task(task) | Value::TaskRef(task) => {
                out(Reached::Identity(Identity::Task(*task)))
            }
            _ => match value.handle() {
                Some(handle) => self.price_from(pricing, vec![handle], out),
                None => Ok(()),
            },
        }
    }

    /// The cost of `entity` when the arena holds it resident and changed
    /// since its projection: what the heap holds of it is stale.
    pub fn changed_cost(&self, entity: EntityId) -> Option<u64> {
        let handle = *self.entities.get(&entity)?;
        self.objects.get(handle)?;
        let facts = self.facts.get(handle.index() as usize)?;
        (facts.dirty && !facts.cold).then_some(facts.summary.cost)
    }

    /// Walks what the changed object `entity` holds, as [`Self::price`]
    /// walks a value's members.
    ///
    /// # Errors
    ///
    /// As [`Self::price`]'s.
    pub fn price_changed(
        &mut self,
        pricing: &mut Pricing,
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
        self.price_from(pricing, pending, out)
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

    /// Ends the walk: unmarks what it marked, and gives the cost of the
    /// objects the running statement made, without an id, that it did not
    /// reach: live, though no root holds them yet.
    pub fn end_pricing(&mut self, pricing: Pricing) -> u64 {
        let unreached = self
            .statement
            .iter()
            .filter(|handle| self.objects.get(**handle).is_some())
            .filter_map(|handle| self.facts.get(handle.index() as usize))
            .filter(|facts| facts.entity.is_none() && !facts.mark)
            .map(|facts| facts.summary.cost)
            .fold(0, u64::saturating_add);
        for handle in pricing.marked {
            self.facts[handle.index() as usize].mark = false;
        }
        unreached
    }

    fn price_from(
        &mut self,
        pricing: &mut Pricing,
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
            crate::perf_witness::record(|work| work.arena_marks += 1);
            pricing.marked.push(handle);
            if let Some(entity) = facts.entity {
                out(Reached::Identity(Identity::Object(entity)))?;
                continue;
            }
            if is_mutable(body) {
                pricing.unidentified = pricing.unidentified.saturating_add(facts.summary.cost);
            } else if facts.cold || has_cold(body) {
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
