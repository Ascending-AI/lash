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
    anonymous: u64,
}
impl CensusGraph for Graph {
    fn root_and_control_bytes(&mut self) -> Result<u64, HeapFault> {
        self.anonymous = 0;
        // One 64-byte frame and one local occurrence for each root.
        Ok(64
            + self
                .roots
                .iter()
                .map(lash_kernel_doc::LogicalHeldV1::local)
                .sum::<u64>())
    }
    fn anonymous_entity_bytes(&self) -> u64 {
        self.anonymous
    }
    fn task_result_bytes(&mut self, _: TaskId) -> Result<u64, HeapFault> {
        Ok(0)
    }
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
        Ok(self.present.then_some(EntityFacts {
            has_edges: true,
            local_bytes: 64,
        }))
    }
    fn entity_edges(
        &mut self,
        _: EntityId,
        _: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        // The durable list holds two aliases of an anonymous list with one
        // integer. Its resident walk discovers that child's body here, after
        // the initial root walk has completed.
        self.anonymous = 49;
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
        anonymous: 0,
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
    // LogicalHeldV1: one 64-byte frame, one pinned 16-byte list occurrence,
    // a 64-byte outer list, and a 49-byte anonymous inner list. The borrowed
    // mirror roots the same graph but owns no additional occurrence.
    let owned = TransientRoots {
        pinned: &value,
        borrowed: &value,
        ..empty()
    };
    census.refine(&mut graph, &owned).unwrap();
    assert_eq!(
        census.normalized_holdings(),
        NormalizedHoldings {
            entity_local_bytes: 113,
            root_and_control_bytes: 80,
        }
    );
    let before = census.normalized_holdings();
    census
        .set_trace_limits(crate::paged::census_impl::TraceLimits {
            work: 0,
            ..Default::default()
        })
        .unwrap();
    assert!(matches!(
        census.refine(&mut graph, &owned),
        Err(CensusFault::Heap(HeapFault::Exhausted { .. }))
    ));
    assert_eq!(
        census.normalized_holdings(),
        before,
        "a host failure installs no partial balance"
    );
    census.set_trace_limits(Default::default()).unwrap();
    census.refine(&mut graph, &empty()).unwrap();
    assert_eq!(census.certificate().entities, 0);
    assert_eq!(
        census.normalized_holdings(),
        NormalizedHoldings {
            entity_local_bytes: 0,
            root_and_control_bytes: 64,
        }
    );
    assert_eq!(census.certificate().refinements, 3);
    assert!(!graph.present);
}
