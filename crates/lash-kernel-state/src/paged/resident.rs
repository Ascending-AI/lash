//! An exact refinement over what a durable run holds in memory (seam I,
//! FIG-6748): the census traces the run's current holdings without the
//! run projecting or publishing anything. The machine prices what it holds
//! in its arena ([`ResidentRoots`]): its frames, session bindings,
//! registers and pins, and each object without an id they reach. The heap
//! traces the rest through its pages, reading a changed object back from
//! the machine. Cold objects are priced by their persisted summaries and
//! resident ones by their bodies, and both give the same exact live cost,
//! so the verdict does not depend on what is resident.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity, LogicalBytes, PageValue};

use super::census::{CensusGraph, EntityCost, SweepSet};
use super::census_graph::binding_values;
pub use super::census_graph::{frame_values, state_values};
use super::directory::value_cost;
use super::heap_impl::PagedHeap;

/// What the machine prices of its holdings before a resident refinement
/// traces them.
#[derive(Debug, Default)]
pub struct Priced {
    /// The holding cost of its control state as a projection would write
    /// it now: every task's row with its frames and what they bind and
    /// hold, and the list joins.
    pub control: u64,
    /// The cost of the ended tasks' retained results `control` counts: a
    /// trace prices them where a handle reaches them.
    pub retained: u64,
    /// The holding cost of the running statement's registers and pins.
    pub statement: u64,
    /// The session bindings written since the last projection point, by
    /// key, with the holding cost of each one's value.
    pub session: Vec<(Vec<u8>, u64)>,
    /// The cost of the objects without an id its holdings reach.
    pub unidentified: u64,
}

/// What the machine finds once the trace is done.
#[derive(Debug, Default)]
pub struct Traced {
    /// The cost of the objects without an id the trace reached through a
    /// changed object only.
    pub unidentified: u64,
    /// The cost of the objects the running statement made, without an id,
    /// that nothing reached: live, though no root holds them yet.
    pub unreached: u64,
}

/// A durable run's holdings in memory, as a resident refinement reads them
/// in place of installed roots (FIG-6748).
pub trait ResidentRoots {
    /// Prices what the run holds, walking its arena from its frames,
    /// session bindings, registers and pins (`Arena::price`). Called once,
    /// first.
    ///
    /// # Errors
    ///
    /// A fault reading a task's or a frame's row.
    fn price(&mut self) -> Result<Priced, HeapFault>;
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
    /// The current cost of `entity` when the run holds it changed since
    /// its projection: what the heap holds of it is stale.
    fn changed_cost(&mut self, entity: EntityId) -> Option<u64>;
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
    fn finish(&mut self) -> Traced;
}

/// The heap as a resident refinement traces it: the machine's holdings
/// over the heap's pages.
pub(super) struct ResidentGraph<'a> {
    pub(super) heap: &'a mut PagedHeap,
    pub(super) resident: &'a mut dyn ResidentRoots,
    pub(super) priced: &'a Priced,
}

impl CensusGraph for ResidentGraph<'_> {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        let session = self.heap.session_cost_over(&self.priced.session)?;
        let control = self
            .priced
            .control
            .checked_sub(self.priced.retained)
            .ok_or_else(|| {
                HeapFault::Malformed("retained task results exceed the control cost".into())
            })?;
        session
            .checked_add(control)
            .and_then(|cost| cost.checked_add(self.priced.statement))
            .map(LogicalBytes)
            .ok_or_else(|| HeapFault::Malformed("resident root cost overflow".into()))
    }
    fn value_cost(&mut self, value: &PageValue) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(value_cost(value)))
    }
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
            .visit_session_bindings_over(&self.priced.session, &mut |binding| {
                binding_values(binding, visit)
            })?;
        self.resident.page_roots(visit)
    }
    fn entity_cost(&mut self, entity: EntityId) -> Result<Option<EntityCost>, HeapFault> {
        match self.resident.changed_cost(entity) {
            Some(local) => Ok(Some(EntityCost {
                local: LogicalBytes(local),
                has_edges: true,
            })),
            None => self.heap.entity_cost(entity),
        }
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        if self.resident.changed_cost(entity).is_none() {
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
    ) -> Result<LogicalBytes, HeapFault> {
        let Some(value) = self.resident.ended_result(task)? else {
            return Ok(LogicalBytes(0));
        };
        self.heap.stream_edges(&value, out)?;
        Ok(LogicalBytes(value_cost(&value)))
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
}
