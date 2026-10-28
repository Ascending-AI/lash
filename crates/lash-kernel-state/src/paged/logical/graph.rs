use super::*;
use crate::paged::census::{CensusGraph, EntityFacts, SweepSet, TransientRoots};
use crate::paged::census_impl::{TraceLimits, normalized_trace};
use lash_kernel_doc::heap::PageValue;

struct Graph<'a, 'port> {
    edit: Edit<'a, 'port>,
}
impl CensusGraph for Graph<'_, '_> {
    fn root_and_control_bytes(&mut self) -> Result<u64, HeapFault> {
        Ok(self.edit.state.owners.cost())
    }
    fn task_result_bytes(&mut self, task: TaskId) -> Result<u64, HeapFault> {
        Ok(self
            .edit
            .state
            .ended
            .get(&owner_key(Owner::Ended(task)), self.edit.access)?
            .map_or(0, |r| r.value.bytes))
    }
    fn value_edges(
        &mut self,
        value: &PageValue,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        if let PageValue::Ref(id) = value {
            out(*id)?;
        }
        Ok(())
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        visit_tree(&self.edit.state.owners, self.edit.access, &mut |r| {
            r.value.each_edge(self.edit.access, &mut |edge| {
                visit(&PageValue::Ref(edge.target))
            })
        })
    }
    fn entity_facts(&mut self, entity: EntityId) -> Result<Option<EntityFacts>, HeapFault> {
        Ok(self
            .edit
            .state
            .entities
            .get(&entity_key(entity), self.edit.access)?
            .map(|r| EntityFacts {
                has_edges: !r.body.links.is_empty(),
                local_bytes: r.body.bytes,
            }))
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let r = self
            .edit
            .state
            .entities
            .get(&entity_key(entity), self.edit.access)?
            .ok_or_else(|| invalid("traced entity is absent"))?;
        r.body
            .each_edge(self.edit.access, &mut |edge| out(edge.target))
    }
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        if let Some(r) = self
            .edit
            .state
            .ended
            .get(&owner_key(Owner::Ended(task)), self.edit.access)?
        {
            r.value
                .each_edge(self.edit.access, &mut |edge| out(edge.target))?;
        }
        Ok(())
    }
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault> {
        let mut removed = Vec::new();
        for entity in &unreachable.entities {
            if let Some(r) = self
                .edit
                .state
                .entities
                .remove(&entity_key(*entity), self.edit.access)?
            {
                removed.push(r.body);
            }
        }
        for task in &unreachable.ended_task_results {
            if let Some(r) = self
                .edit
                .state
                .ended
                .remove(&owner_key(Owner::Ended(*task)), self.edit.access)?
            {
                removed.push(r.value);
            }
        }
        for body in removed {
            self.edit.release(&body, true)?;
        }
        self.edit.release_zero(true)
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        let mut key = after.map(entity_key);
        let mut entities = Vec::new();
        while entities.len() < limit {
            let Some((next, _)) = self
                .edit
                .state
                .entities
                .after(key.as_deref(), self.edit.access)?
            else {
                break;
            };
            if next.len() != 16 {
                return Err(invalid("logical entity key is not 16 bytes"));
            }
            entities.push(EntityId {
                family: lash_kernel_doc::heap::FamilyId(u64::from_be_bytes(
                    next[..8]
                        .try_into()
                        .map_err(|_| invalid("logical family key"))?,
                )),
                ordinal: u64::from_be_bytes(
                    next[8..]
                        .try_into()
                        .map_err(|_| invalid("logical ordinal key"))?,
                ),
            });
            key = Some(next.into());
        }
        Ok(entities)
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        let mut key = after.map(|t| owner_key(Owner::Ended(t)));
        let mut tasks = Vec::new();
        while tasks.len() < limit {
            let Some((next, r)) = self
                .edit
                .state
                .ended
                .after(key.as_deref(), self.edit.access)?
            else {
                break;
            };
            let Owner::Ended(task) = r.owner else {
                return Err(invalid("ended owner tree contains another owner"));
            };
            if next != owner_key(r.owner).as_slice() {
                return Err(invalid("ended owner key mismatch"));
            }
            tasks.push(task);
            key = Some(next.into());
        }
        Ok(tasks)
    }
}

impl Ledger {
    /// Normalize cloned roots with the existing bounded exact trace. Failures
    /// discard the entire draft, including any bounded sweep batches. Ended
    /// results are roots only while a source holding reaches their task.
    pub fn normalize(&mut self, access: &Access<'_>, limits: TraceLimits) -> Result<(), HeapFault> {
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("logical revision overflow"))?;
        let refinements = self
            .state
            .refinements
            .checked_add(1)
            .ok_or_else(|| invalid("logical refinement overflow"))?;
        let mut graph = Graph {
            edit: Edit {
                state: self.state.clone(),
                access,
                zero: VecDeque::new(),
                fresh: Vec::new(),
            },
        };
        let empty = TransientRoots {
            borrowed: &[],
            registers: &[],
            pinned: &[],
            native_args: &[],
            fresh: &[],
        };
        let (entities, costs, _) = normalized_trace(&mut graph, &empty, limits)
            .map_err(super::super::heap_impl::census_fault)?;
        let balance = graph.edit.state.balance()?;
        if balance.entities != entities
            || balance.entity_local_bytes != costs.entity_local_bytes
            || balance.root_and_control_bytes != costs.root_and_control_bytes
        {
            return Err(invalid("normalized membership and trace costs disagree"));
        }
        graph.edit.state.refinements = refinements;
        self.state = graph.edit.state;
        self.revision = revision;
        Ok(())
    }
}
