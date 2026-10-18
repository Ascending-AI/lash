//! S7 laws: the census counts its entities and sweeps what nothing reaches.
use crate::paged::census::*;
use crate::paged::census_impl::Census;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, Identity, PageValue};

fn certificate(entities: u64) -> AccountingCertificate {
    AccountingCertificate {
        entities,
        ..AccountingCertificate::FRESH
    }
}
fn empty() -> TransientRoots<'static> {
    TransientRoots {
        borrowed: &[],
        registers: &[],
        pinned: &[],
        native_args: &[],
        fresh: &[],
    }
}
fn entity() -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: 0,
    }
}
struct Graph {
    roots: Vec<PageValue>,
    present: bool,
}
impl CensusGraph for Graph {
    fn value_edges(
        &mut self,
        v: &PageValue,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        if let PageValue::List(id) = v {
            out(Identity::Object(*id))?;
        }
        Ok(())
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.roots.iter().try_for_each(visit)
    }
    fn entity_facts(&mut self, _: EntityId) -> Result<Option<EntityFacts>, HeapFault> {
        Ok(self.present.then_some(EntityFacts { has_edges: false }))
    }
    fn entity_edges(
        &mut self,
        _: EntityId,
        _: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        Ok(())
    }
    fn task_result(
        &mut self,
        _: TaskId,
        _: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        _: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        Ok(if self.present && after.is_none() {
            vec![entity()]
        } else {
            vec![]
        })
    }
    fn ended_tasks_after(&mut self, _: Option<TaskId>, _: usize) -> Result<Vec<TaskId>, HeapFault> {
        Ok(vec![])
    }
    fn sweep(&mut self, set: &SweepSet) -> Result<(), HeapFault> {
        if set.entities.contains(&entity()) {
            self.present = false;
        }
        Ok(())
    }
}

/// RESULT-TRANSFER: an existing allocation remains reachable during
/// handoff: a sweep keeps what a transfer root reaches, and removes it once
/// nothing does.
#[test]
fn transfer_roots_keep_referents_through_a_sweep() {
    let mut graph = Graph {
        roots: vec![],
        present: true,
    };
    let mut census = Census::restore(certificate(1)).unwrap();
    let value = [PageValue::List(entity())];
    let roots = TransientRoots {
        borrowed: &value,
        ..empty()
    };
    census.refine(&mut graph, &roots).unwrap();
    assert_eq!(census.certificate().entities, 1);
    assert!(graph.present);
    census.refine(&mut graph, &empty()).unwrap();
    assert_eq!(census.certificate().entities, 0);
    assert_eq!(census.certificate().refinements, 2);
    assert!(!graph.present);
}
