//! S7 laws: certified U and checked arithmetic.
use crate::paged::census::*;
use crate::paged::census_impl::{Census, TraceLimits};
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, Identity, LogicalBytes, Value};

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
    roots: Vec<Value>,
    cost: u64,
    present: bool,
}
impl CensusGraph for Graph {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(self.roots.len() as u64 * 16))
    }
    fn value_cost(&mut self, _: &Value) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(16))
    }
    fn value_edges(
        &mut self,
        v: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        if let Value::List(id) = v {
            out(Identity::Object(*id))?;
        }
        Ok(())
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
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

#[test]
fn overflow_refines_and_never_grants() {
    let mut census = Census::restore(certificate(u64::MAX, 1), LogicalBytes(64)).unwrap();
    let ReservationDecision::NeedsPrecision(pending) = census.reserve(LogicalBytes(32)) else {
        panic!("overflow granted")
    };
    assert!(pending.overflowed);
    let mut graph = Graph {
        roots: vec![],
        cost: 32,
        present: true,
    };
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending).unwrap(),
        RefinedDecision::Granted(_)
    ));
    assert!(!graph.present);
    assert_eq!(census.certificate().upper, LogicalBytes(0));
    // Checked mutations fail atomically instead of wrapping the certificate.
    let mut census = Census::restore(certificate(u64::MAX, 1), LogicalBytes(u64::MAX)).unwrap();
    let before = census.certificate();
    assert!(census.added(LogicalBytes(32), 1).is_err());
    assert!(census.added(LogicalBytes(u64::MAX), u64::MAX).is_err());
    assert_eq!(census.certificate(), before);
}

#[test]
fn an_undercounted_certificate_is_refused_before_it_grants() {
    assert!(Census::restore(certificate(31, 1), LogicalBytes(64)).is_err());
    let mut graph = Graph {
        roots: vec![Value::List(entity())],
        cost: 128,
        present: true,
    };
    // A plausible entity minimum still cannot certify a foreign aggregate.
    assert!(
        Census::validate_population(certificate(48, 1), &mut graph, TraceLimits::default())
            .is_err()
    );
    Census::validate_population(certificate(144, 1), &mut graph, TraceLimits::default()).unwrap();
    assert!(graph.present);
}

#[test]
fn last_and_nonlast_alias_removal_keep_u_an_upper_bound() {
    let mut graph = Graph {
        roots: vec![Value::List(entity()), Value::List(entity())],
        cost: 64,
        present: true,
    };
    let mut census = Census::restore(certificate(96, 1), LogicalBytes(96)).unwrap();
    graph.roots.pop();
    census.changed(LogicalBytes(16), LogicalBytes(0)).unwrap();
    assert_eq!(census.certificate().upper, LogicalBytes(80));
    let ReservationDecision::NeedsPrecision(pending) = census.reserve(LogicalBytes(32)) else {
        panic!("expected pressure")
    };
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending).unwrap(),
        RefinedDecision::Refused {
            reachable: LogicalBytes(80)
        }
    ));
    assert!(graph.present);
    graph.roots.clear();
    census.changed(LogicalBytes(16), LogicalBytes(0)).unwrap();
    assert_eq!(census.certificate().upper, LogicalBytes(64));
    let ReservationDecision::NeedsPrecision(pending) = census.reserve(LogicalBytes(64)) else {
        panic!("expected pressure")
    };
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending).unwrap(),
        RefinedDecision::Granted(_)
    ));
    assert!(!graph.present);
}
