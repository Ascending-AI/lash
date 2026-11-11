//! Collection by reachability for the bytecode core's heap (cutover
//! contract §6, cold roots): before any memory refusal, the collector
//! traces the current roots through resident members and, past an object
//! whose members are not all resident, through its row's edge summary.
//! Only an object so reached keeps what it names resident: a cold object
//! no root reaches, and a dirty object only it named, are garbage, so
//! dropping the last root of a cycle half paged out frees its resident
//! half before another seal. No payload is loaded and nothing is sealed.

use std::collections::HashSet;

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::{EntityId, HeapFault};

use super::{Arena, Visit, each_member, has_cold};

impl Arena {
    /// Frees every object `roots` do not reach through resident members
    /// and the edges `cold_edges` gives for each object with an id whose
    /// members are not all resident (a proxy, or a large object with cold
    /// chunks), and for each id so reached that the arena does not hold.
    ///
    /// # Errors
    ///
    /// `cold_edges`'s, before anything is freed.
    pub fn collect_reachable(
        &mut self,
        roots: &mut dyn FnMut(&mut Visit<'_>),
        cold_edges: &mut dyn FnMut(EntityId) -> Result<Vec<EntityId>, HeapFault>,
    ) -> Result<(), HeapFault> {
        let mut pending: Vec<ArenaHandle> = Vec::new();
        roots(&mut |value| pending.extend(value.handle()));
        let mut traced: HashSet<EntityId> = HashSet::new();
        let mut absent: Vec<EntityId> = Vec::new();
        let mut marked = Vec::new();
        let mut failed = None;
        loop {
            while let Some(handle) = pending.pop() {
                let Some(body) = self.objects.get(handle) else {
                    continue;
                };
                let facts = &mut self.facts[handle.index() as usize];
                if facts.mark {
                    continue;
                }
                facts.mark = true;
                marked.push(handle);
                let entity = facts.entity();
                each_member(body, &mut |value| pending.extend(value.handle()));
                if let Body::Closure { captures, .. } = body {
                    pending.extend(captures.iter().copied());
                }
                if let Some(entity) = entity
                    && (has_cold(body) || facts.cold)
                    && traced.insert(entity)
                {
                    absent.push(entity);
                }
            }
            let Some(entity) = absent.pop() else {
                break;
            };
            match cold_edges(entity) {
                Ok(edges) => {
                    for edge in edges {
                        match self.entity_handle(edge) {
                            Some(handle) => pending.push(handle),
                            None => {
                                if traced.insert(edge) {
                                    absent.push(edge);
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failed {
            for handle in marked {
                self.facts[handle.index() as usize].mark = false;
            }
            return Err(error);
        }
        self.sweep_unmarked();
        Ok(())
    }
}
