//! A sweep over what a durable run holds in memory (seam I, FIG-6748):
//! the census traces the run's current holdings without the run projecting
//! or publishing anything. The machine walks what it holds in its arena
//! ([`ResidentRoots`]): its frames, session bindings, registers and pins,
//! and each object without an id they reach. The heap traces the rest
//! through its pages, reading a changed object back from the machine.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, PageValue};

use super::census::{CensusGraph, EntityFacts, SweepSet};
use super::census_graph::binding_values;
pub use super::census_graph::{frame_values, state_values};
use super::heap_impl::PagedHeap;

/// What the machine's walk of its holdings gives a sweep.
#[derive(Debug, Default)]
pub struct Walked {
    /// The keys of the session bindings written since the last projection
    /// point: the machine's walk gives what they hold.
    pub session: Vec<Vec<u8>>,
}

/// A durable run's holdings in memory, as a resident sweep reads them in
/// place of installed roots (FIG-6748).
pub trait ResidentRoots {
    /// Walks what the run holds through its arena, from its frames,
    /// session bindings, registers and pins (`Arena::reach`). Called once,
    /// first.
    ///
    /// # Errors
    ///
    /// A fault reading a task's or a frame's row.
    fn walk(&mut self) -> Result<Walked, HeapFault>;
    /// Visits what the run holds in page form (rows of tasks and cold
    /// frames, task states, list joins' members), then what the walk of
    /// its arena gave the census.
    ///
    /// # Errors
    ///
    /// `visit`'s errors.
    fn page_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault>;
    /// Whether the run holds `entity` changed since its projection: what
    /// the heap holds of it is stale.
    fn changed(&mut self, entity: EntityId) -> bool;
    /// Walks what the changed `entity` holds: the identities it names to
    /// `out`, the page forms of immutables on their pages to `pages`.
    ///
    /// # Errors
    ///
    /// `out`'s errors.
    fn changed_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
        pages: &mut Vec<PageValue>,
    ) -> Result<(), HeapFault>;
    /// The result `task` retains, if it ended and retains one.
    ///
    /// # Errors
    ///
    /// A fault reading its row.
    fn ended_result(&mut self, task: TaskId) -> Result<Option<PageValue>, HeapFault>;
    /// The tasks that retain an ended result, ascending, after `after`.
    ///
    /// # Errors
    ///
    /// A fault reading the rows.
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault>;
    /// The entities the run changed since it projected them, or gave ids
    /// and never projected, ascending.
    fn changed_entities(&mut self) -> Vec<EntityId>;
    /// Forgets what the run changed of `entities`, which nothing reaches
    /// and the heap no longer holds: no projection writes them back.
    fn swept(&mut self, entities: &[EntityId]);
    /// Retires the results of `tasks`, which nothing reaches.
    ///
    /// # Errors
    ///
    /// A fault writing a row.
    fn retire(&mut self, tasks: &[TaskId]) -> Result<(), HeapFault>;
    /// Ends the walk.
    fn finish(&mut self);
}

/// The heap as a resident sweep traces it: the machine's holdings over the
/// heap's pages.
pub(super) struct ResidentGraph<'a> {
    pub(super) heap: &'a mut PagedHeap,
    pub(super) resident: &'a mut dyn ResidentRoots,
    pub(super) walked: &'a Walked,
    pub(super) retires: bool,
}

impl CensusGraph for ResidentGraph<'_> {
    fn value_edges(
        &mut self,
        value: &PageValue,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.heap.stream_edges(value, out)
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.heap
            .visit_session_bindings_over(&self.walked.session, &mut |binding| {
                binding_values(binding, visit)
            })?;
        self.resident.page_roots(visit)
    }
    fn entity_facts(&mut self, entity: EntityId) -> Result<Option<EntityFacts>, HeapFault> {
        if self.resident.changed(entity) {
            return Ok(Some(EntityFacts { has_edges: true }));
        }
        self.heap.entity_facts(entity)
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        if !self.resident.changed(entity) {
            return self.heap.entity_edges(entity, out);
        }
        let mut pages = Vec::new();
        self.resident.changed_edges(entity, out, &mut pages)?;
        for page in &pages {
            self.heap.stream_edges(page, out)?;
        }
        Ok(())
    }
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let Some(value) = self.resident.ended_result(task)? else {
            return Ok(());
        };
        self.heap.stream_edges(&value, out)
    }
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault> {
        if !unreachable.entities.is_empty() {
            self.heap.sweep(&SweepSet {
                entities: unreachable.entities.clone(),
                ended_task_results: Vec::new(),
            })?;
            self.resident.swept(&unreachable.entities);
        }
        if !unreachable.ended_task_results.is_empty() {
            self.resident.retire(&unreachable.ended_task_results)?;
        }
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        self.heap.census_after(after, limit)
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        self.resident.ended_tasks_after(after, limit)
    }
    fn retires_results(&self) -> bool {
        self.retires
    }
    fn unprojected(&mut self) -> Vec<EntityId> {
        self.resident.changed_entities()
    }
    fn forget(&mut self, entities: &[EntityId]) {
        self.resident.swept(entities);
    }
}
