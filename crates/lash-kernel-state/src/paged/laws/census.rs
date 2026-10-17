//! S7 laws: certified U and checked arithmetic.
use crate::paged::census::*;
use crate::paged::census_impl::Census;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, Identity, LogicalBytes, PageValue};

fn certificate(upper: u64, entities: u64) -> AccountingCertificate {
    AccountingCertificate {
        metric: AccountingMetric::LocalCostV1,
        upper: LogicalBytes(upper),
        entities,
        refinements: 0,
    }
}
fn empty() -> TransientRoots<'static> {
    TransientRoots {
        borrowed: &[],
        registers: &[],
        pinned: &[],
        native_args: &[],
        fresh: &[],
        reserved: LogicalBytes(0),
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
    cost: u64,
    present: bool,
}
impl CensusGraph for Graph {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(self.roots.len() as u64 * 16))
    }
    fn value_cost(&mut self, _: &PageValue) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(16))
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
    fn entity_cost(&mut self, _: EntityId) -> Result<Option<EntityCost>, HeapFault> {
        Ok(self.present.then_some(EntityCost {
            local: LogicalBytes(self.cost),
            has_edges: false,
        }))
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
    ) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(0))
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

/// RESULT-TRANSFER: an existing allocation remains reachable during handoff,
/// without pricing a second allocation for the transfer root itself.
#[test]
fn transfer_roots_keep_referents_without_another_allocation() {
    let mut graph = Graph {
        roots: vec![],
        cost: 32,
        present: true,
    };
    let mut census = Census::restore(certificate(32, 1), LogicalBytes(32)).unwrap();
    let value = [PageValue::List(entity())];
    let roots = TransientRoots {
        borrowed: &value,
        ..empty()
    };
    let pending = PendingReservation {
        bytes: LogicalBytes(0),
        overflowed: false,
    };
    assert!(matches!(
        census.refine(&mut graph, &roots, pending).unwrap(),
        RefinedDecision::Granted(_)
    ));
    assert_eq!(census.certificate().upper, LogicalBytes(32));
    assert!(graph.present);
    census.refine(&mut graph, &empty(), pending).unwrap();
    assert_eq!(census.certificate().upper, LogicalBytes(0));
    assert!(!graph.present);
}
