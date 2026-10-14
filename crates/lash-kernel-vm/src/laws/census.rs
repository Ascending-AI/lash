//! S7 layer laws over summary pages. The real heap/machine composition is
//! proved by hp-integrate; these exercise the independent census policy.
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{
    EntityId, FamilyId, HeapFault, HostResource, Identity, LogicalBytes, PageGrant, PageId,
    PageRef, Payload, TupleRef, TupleSummary, Value,
};
use lash_kernel_state::paged::census_impl::{Census, TraceLimits};
use lash_kernel_state::paged::{
    AccountingCertificate, AccountingMetric, AllocationCensus, CensusFault, CensusGraph,
    EntityCost, PageAnswer, PageBody, PageKind, PageSource, PendingReservation, RefinedDecision,
    ReservationDecision, SweepSet, TransientRoots,
};
use std::collections::BTreeMap;

fn id(n: u64) -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: n,
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
fn certificate(upper: u64, entities: u64) -> AccountingCertificate {
    AccountingCertificate {
        metric: AccountingMetric::LocalCostV1,
        upper: LogicalBytes(upper),
        entities,
        refinements: 0,
    }
}
fn pressured(census: &mut Census, bytes: u64) -> PendingReservation {
    match census.reserve(LogicalBytes(bytes)) {
        ReservationDecision::NeedsPrecision(pending) => pending,
        ReservationDecision::Granted(_) => panic!("fixture must pressure the census"),
    }
}
fn edge(n: u64) -> Identity {
    Identity::Object(id(n))
}

/// The bytes are only cost/identity summaries; there is no payload image.
/// Immutable backing survives eviction and opening a fresh reader.
#[derive(Clone, Default)]
struct Source {
    pages: BTreeMap<PageId, PageAnswer>,
    cache: BTreeMap<PageId, PageAnswer>,
    reads: usize,
    misses: usize,
    fail_after: Option<usize>,
}
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        let mut answers = Vec::new();
        for page in pages {
            if self.fail_after.is_some_and(|n| self.reads >= n) {
                return Err(HeapFault::LeaseLost);
            }
            self.reads += 1;
            if !self.cache.contains_key(&page.id) {
                self.misses += 1;
                let answer = self
                    .pages
                    .get(&page.id)
                    .ok_or(HeapFault::Missing { page: page.id })?
                    .clone();
                assert_eq!(
                    answer.kind,
                    PageKind::KEYED_LEAF,
                    "tracing must not read payloads"
                );
                self.cache.insert(page.id, answer);
            }
            answers.push(self.cache[&page.id].clone());
        }
        Ok(answers)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut digest = [0; 32];
        digest[..8].copy_from_slice(&(self.pages.len() as u64).to_le_bytes());
        let page = PageRef {
            id: PageId(digest),
            grant: PageGrant::NONE,
        };
        self.pages.insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
#[derive(Clone, Default)]
struct Graph {
    source: Source,
    directory: BTreeMap<EntityId, PageRef>,
    results: BTreeMap<TaskId, PageRef>,
    roots: Vec<Value>,
    swept: usize,
}
impl Graph {
    fn summary(&mut self, cost: u64, edges: &[Identity]) -> PageRef {
        let mut tokens = cost.to_le_bytes().to_vec();
        for identity in edges {
            match identity {
                Identity::Object(entity) => {
                    tokens.push(0);
                    tokens.extend(entity.family.0.to_le_bytes());
                    tokens.extend(entity.ordinal.to_le_bytes());
                }
                Identity::Task(task) => {
                    tokens.push(1);
                    tokens.extend(0_u64.to_le_bytes());
                    tokens.extend(task.0.to_le_bytes());
                }
            }
        }
        self.source
            .seal(PageBody {
                kind: PageKind::KEYED_LEAF,
                tokens,
                children: vec![],
            })
            .unwrap()
    }
    fn object(&mut self, n: u64, cost: u64, edges: &[Identity]) {
        let page = self.summary(cost, edges);
        self.directory.insert(id(n), page);
    }
    fn task(&mut self, task: u64, cost: u64, edges: &[Identity]) {
        let page = self.summary(cost, edges);
        self.results.insert(TaskId(task), page);
    }
    fn read_summary(
        &mut self,
        page: PageRef,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<u64, HeapFault> {
        let answer = self.source.read(&[page])?.pop().unwrap();
        let cost = u64::from_le_bytes(answer.tokens[..8].try_into().unwrap());
        for row in answer.tokens[8..].as_chunks::<17>().0 {
            let family = u64::from_le_bytes(row[1..9].try_into().unwrap());
            let ordinal = u64::from_le_bytes(row[9..].try_into().unwrap());
            out(if row[0] == 0 {
                Identity::Object(EntityId {
                    family: FamilyId(family),
                    ordinal,
                })
            } else {
                Identity::Task(TaskId(ordinal))
            })?;
        }
        Ok(cost)
    }
    fn prepare(&mut self, mode: usize) {
        if mode == 0 {
            self.source.cache = self.source.pages.clone();
        } else {
            self.source.cache.clear();
        }
        self.source.reads = 0;
        self.source.misses = 0;
    }
}
impl CensusGraph for Graph {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        let roots = self.roots.clone();
        roots.iter().try_fold(LogicalBytes(0), |total, v| {
            Ok(LogicalBytes(total.0 + self.value_cost(v)?.0))
        })
    }
    fn value_cost(&mut self, value: &Value) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(
            16 + match value {
                Value::Tuple(tuple) => tuple.summary.cost.0,
                _ => 0,
            },
        ))
    }
    fn value_edges(
        &mut self,
        value: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match value {
            Value::List(entity) | Value::Ref(Identity::Object(entity)) => {
                out(Identity::Object(*entity))?;
            }
            Value::Task(task) | Value::Ref(Identity::Task(task)) => out(Identity::Task(*task))?,
            Value::Tuple(tuple) if tuple.summary.has_edges => match &tuple.body {
                Payload::Paged(page) => {
                    self.read_summary(*page, out)?;
                }
                Payload::Resident(values) => {
                    for v in values.iter() {
                        self.value_edges(v, out)?;
                    }
                }
            },
            Value::Error(error) if error.has_edges => self.value_edges(&error.data, out)?,
            _ => {}
        }
        Ok(())
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.roots.iter().try_for_each(visit)
    }
    fn entity_cost(&mut self, entity: EntityId) -> Result<Option<EntityCost>, HeapFault> {
        let Some(page) = self.directory.get(&entity).copied() else {
            return Ok(None);
        };
        let mut has_edges = false;
        let cost = self.read_summary(page, &mut |_| {
            has_edges = true;
            Ok(())
        })?;
        Ok(Some(EntityCost {
            local: LogicalBytes(cost),
            has_edges,
        }))
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.read_summary(self.directory[&entity], out)?;
        Ok(())
    }
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<LogicalBytes, HeapFault> {
        match self.results.get(&task).copied() {
            Some(page) => Ok(LogicalBytes(self.read_summary(page, out)?)),
            None => Ok(LogicalBytes(0)),
        }
    }
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault> {
        for entity in &unreachable.entities {
            assert!(self.directory.remove(entity).is_some());
            self.swept += 1;
        }
        for task in &unreachable.ended_task_results {
            assert!(self.results.remove(task).is_some());
        }
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        Ok(self
            .directory
            .keys()
            .copied()
            .filter(|id| after.is_none_or(|last| *id > last))
            .take(limit)
            .collect())
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        Ok(self
            .results
            .keys()
            .copied()
            .filter(|id| after.is_none_or(|last| *id > last))
            .take(limit)
            .collect())
    }
}

#[test]
fn an_unreachable_cycle_never_fails_an_allocation() {
    let mut graph = Graph::default();
    graph.object(0, 48, &[edge(1)]);
    graph.object(1, 48, &[edge(0)]);
    let mut census = Census::restore(certificate(96, 2), LogicalBytes(112)).unwrap();
    let pending = pressured(&mut census, 112);
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending).unwrap(),
        RefinedDecision::Granted(_)
    ));
    assert_eq!(graph.swept, 2);
    assert_eq!(census.certificate().upper, LogicalBytes(0));
}

#[test]
fn the_verdict_is_the_same_warm_evicted_and_reopened() {
    for scenario in 0..3 {
        let mut base = Graph::default();
        let (count, upper, bound, request) = match scenario {
            0 => {
                base.object(0, 64, &[edge(1)]);
                base.object(1, 64, &[]);
                base.roots = vec![Value::List(id(0)), Value::List(id(1))];
                (2, 160, 160, 32)
            }
            1 => {
                base.object(0, 48, &[edge(1)]);
                base.object(1, 48, &[edge(0)]);
                (2, 96, 112, 112)
            }
            _ => {
                for n in 0..4096 {
                    let next = [edge(n + 1)];
                    base.object(
                        n,
                        if n == 4095 { 32 } else { 48 },
                        if n == 4095 { &[] } else { &next },
                    );
                }
                base.roots = vec![Value::List(id(2048))];
                (4096, 196608, 196608, 98304)
            }
        };
        let saved = certificate(upper, count);
        let mut expected = None;
        for mode in 0..3 {
            let mut graph = base.clone();
            graph.prepare(mode);
            // Opening restores only the admitted header, with a fresh cache.
            let mut census = Census::restore(saved, LogicalBytes(bound)).unwrap();
            assert_eq!(graph.source.reads, 0);
            let pending = pressured(&mut census, request);
            let decision = census.refine(&mut graph, &empty(), pending).unwrap();
            let observed = (
                matches!(decision, RefinedDecision::Granted(_)),
                census.certificate(),
            );
            if let Some(expected) = expected {
                assert_eq!(observed, expected);
            } else {
                expected = Some(observed);
            }
            if mode == 0 {
                assert_eq!(graph.source.misses, 0);
            } else if scenario != 1 {
                assert!(graph.source.misses > 0);
            }
        }
    }
}

#[test]
fn refinement_counts_registers_native_args_and_reservations() {
    let mut graph = Graph::default();
    for n in 0..6 {
        graph.object(n, 32, &[]);
    }
    graph.roots = vec![Value::List(id(3))];
    graph.task(10, 64, &[edge(4)]);
    graph.task(11, 1024, &[edge(5)]); // unreachable result and its payload
    let tuple = graph.summary(32, &[edge(1), Identity::Task(TaskId(10))]);
    let registers = [Value::List(id(0))];
    let native_args = [Value::Tuple(TupleRef {
        summary: TupleSummary {
            len: 2,
            cost: LogicalBytes(32),
            has_edges: true,
            depth: 1,
        },
        body: Payload::Paged(tuple),
    })];
    let pinned = [Value::Ref(Identity::Task(TaskId(10)))];
    let transient = TransientRoots {
        registers: &registers,
        pinned: &pinned,
        native_args: &native_args,
        fresh: &[id(2)],
        reserved: LogicalBytes(40),
    };
    let mut census = Census::restore(certificate(1296, 6), LogicalBytes(1416)).unwrap();
    census.changed(LogicalBytes(0), LogicalBytes(120)).unwrap();
    let pending = pressured(&mut census, 1057);
    let decision = census.refine(&mut graph, &transient, pending).unwrap();
    // Five objects once, one result once, the persistent root and three
    // transient holding locations plus
    // the tuple's local contents and the outstanding reservation.
    assert!(matches!(
        decision,
        RefinedDecision::Refused {
            reachable: LogicalBytes(360)
        }
    ));
    assert_eq!(census.certificate().entities, 5);
    assert!(graph.directory.contains_key(&id(3)));
    assert!(graph.directory.contains_key(&id(4)));
    assert!(!graph.directory.contains_key(&id(5)));
    assert_eq!(graph.results.len(), 1);
}

#[test]
fn a_large_partially_shared_detach_frees_only_the_dead_part() {
    let mut graph = Graph::default();
    for n in 0..8192 {
        let next = [edge(n + 1)];
        graph.object(
            n,
            if n == 8191 { 32 } else { 48 },
            if n == 8191 { &[] } else { &next },
        );
    }
    graph.roots = vec![Value::List(id(0)), Value::List(id(4096))];
    let mut census = Census::restore(certificate(393232, 8192), LogicalBytes(393232)).unwrap();
    graph.roots.remove(0);
    census.changed(LogicalBytes(16), LogicalBytes(0)).unwrap();
    let pending = pressured(&mut census, 196624);
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending).unwrap(),
        RefinedDecision::Granted(_)
    ));
    assert_eq!(graph.swept, 4096);
    assert_eq!(census.certificate().upper, LogicalBytes(196608));
    assert!(graph.directory.keys().all(|entity| entity.ordinal >= 4096));
}

#[test]
fn an_interrupted_refinement_is_infrastructure() {
    let mut scalars = Graph::default();
    let args = [Value::Null];
    let scalar_roots = TransientRoots {
        registers: &[],
        pinned: &[],
        native_args: &args,
        fresh: &[],
        reserved: LogicalBytes(0),
    };
    let mut scalar_census = Census::restore(certificate(0, 0), LogicalBytes(16)).unwrap();
    scalar_census
        .changed(LogicalBytes(0), LogicalBytes(16))
        .unwrap();
    scalar_census
        .set_trace_limits(TraceLimits {
            work: 0,
            ..TraceLimits::default()
        })
        .unwrap();
    let request = pressured(&mut scalar_census, 1);
    assert!(matches!(
        scalar_census.refine(&mut scalars, &scalar_roots, request),
        Err(CensusFault::Heap(HeapFault::Exhausted {
            resource: HostResource::HostWork
        }))
    ));
    assert_eq!(scalar_census.certificate().refinements, 0);
    let mut graph = Graph::default();
    graph.object(0, 32, &[edge(1)]);
    graph.object(1, 32, &[]);
    graph.object(2, 32, &[]);
    graph.roots = vec![Value::List(id(0))];
    graph.source.fail_after = Some(2);
    let before = certificate(112, 3);
    let mut census = Census::restore(before, LogicalBytes(112)).unwrap();
    let pending = pressured(&mut census, 32);
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending),
        Err(CensusFault::Heap(HeapFault::LeaseLost))
    ));
    assert_eq!(census.certificate(), before);
    assert_eq!(graph.swept, 0);
    graph.source.fail_after = None;
    census
        .set_trace_limits(TraceLimits {
            work: 0,
            ..TraceLimits::default()
        })
        .unwrap();
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending),
        Err(CensusFault::Heap(HeapFault::Exhausted {
            resource: HostResource::HostWork
        }))
    ));
    assert_eq!(census.certificate(), before);
    assert_eq!(graph.swept, 0);
    census.set_trace_limits(TraceLimits::default()).unwrap();
    let pending = pressured(&mut census, 32);
    assert!(matches!(
        census.refine(&mut graph, &empty(), pending).unwrap(),
        RefinedDecision::Granted(_)
    ));
}

#[test]
fn parks_never_collect() {
    let saved = certificate(64, 2); // detached cycle, deliberately retained
    let mut census = Census::restore(saved, LogicalBytes(128)).unwrap();
    for _ in 0..100 {
        let parked = census.certificate();
        census = Census::restore(parked, LogicalBytes(128)).unwrap();
    }
    assert_eq!(census.certificate(), saved);
    assert!(matches!(
        census.reserve(LogicalBytes(32)),
        ReservationDecision::Granted(_)
    ));
    assert_eq!(census.certificate().refinements, 0);
}

#[test]
fn census_garbage_stays_within_the_logical_bound() {
    let mut graph = Graph::default();
    let mut census = Census::restore(certificate(0, 0), LogicalBytes(128)).unwrap();
    for n in 0..32 {
        match census.reserve(LogicalBytes(32)) {
            ReservationDecision::Granted(_) => {}
            ReservationDecision::NeedsPrecision(pending) => {
                assert!(matches!(
                    census.refine(&mut graph, &empty(), pending).unwrap(),
                    RefinedDecision::Granted(_)
                ));
            }
        }
        census.added(LogicalBytes(32), 1).unwrap();
        graph.object(n, 32, &[]);
        assert!(census.certificate().upper.0 <= 128);
        assert!(census.certificate().entities <= 4);
    }
}

/// Dense summary generator keeps the measuring fixture independent of
/// retained object payloads. A wide edge summary streams directly into bits.
struct Dense {
    count: u64,
    roots: u64,
}
impl CensusGraph for Dense {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(16 * self.roots))
    }
    fn value_cost(&mut self, _: &Value) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(16))
    }
    fn value_edges(
        &mut self,
        _: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        out(edge(0))?;
        Ok(())
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        for _ in 0..self.roots {
            visit(&Value::List(id(0)))?;
        }
        Ok(())
    }
    fn entity_cost(&mut self, entity: EntityId) -> Result<Option<EntityCost>, HeapFault> {
        Ok((entity.ordinal < self.count).then_some(EntityCost {
            local: LogicalBytes(
                32 + if entity.ordinal == 0 {
                    (self.count - 1) * 16
                } else {
                    0
                },
            ),
            has_edges: entity.ordinal == 0,
        }))
    }
    fn entity_edges(
        &mut self,
        _: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        for n in 1..self.count {
            out(edge(n))?;
        }
        Ok(())
    }
    fn task_result(
        &mut self,
        _: TaskId,
        _: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(0))
    }
    fn sweep(&mut self, set: &SweepSet) -> Result<(), HeapFault> {
        assert!(set.entities.is_empty());
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        let start = after.map_or(0, |id| id.ordinal + 1);
        Ok((start..self.count).take(limit).map(id).collect())
    }
    fn ended_tasks_after(&mut self, _: Option<TaskId>, _: usize) -> Result<Vec<TaskId>, HeapFault> {
        Ok(vec![])
    }
}

#[test]
fn exceeding_the_trace_budget_is_host_only_and_raising_it_preserves_the_verdict() {
    // Both the fetched census batch and the deletion batch coexist.
    // Reserving just one buffer silently exceeds this host quota.
    let mut graph = Graph::default();
    for n in 0..128 {
        graph.object(n, 32, &[]);
    }
    let saved = certificate(4096, 128);
    let mut census = Census::restore(saved, saved.upper).unwrap();
    census
        .set_trace_limits(TraceLimits {
            trace_memory_budget: 2000,
            ..TraceLimits::default()
        })
        .unwrap();
    let request = pressured(&mut census, 32);
    assert!(matches!(
        census.refine(&mut graph, &empty(), request),
        Err(CensusFault::Heap(HeapFault::Exhausted {
            resource: HostResource::TraceBudget
        }))
    ));
    assert_eq!(census.certificate(), saved);
    assert_eq!(graph.swept, 0);
    census.set_trace_limits(TraceLimits::default()).unwrap();
    assert!(matches!(
        census.refine(&mut graph, &empty(), request).unwrap(),
        RefinedDecision::Granted(_)
    ));
    assert_eq!(graph.swept, 128);

    // Aliased identity roots need one bitmap, not a copied root inventory.
    let mut graph = Dense {
        count: 1,
        roots: 1_000_000,
    };
    let saved = certificate(16_000_032, 1);
    let mut census = Census::restore(saved, saved.upper).unwrap();
    census
        .set_trace_limits(TraceLimits {
            trace_memory_budget: 4096,
            ..TraceLimits::default()
        })
        .unwrap();
    let request = pressured(&mut census, 32);
    assert!(
        matches!(census.refine(&mut graph, &empty(), request).unwrap(),
        RefinedDecision::Refused { reachable } if reachable == saved.upper)
    );
    assert!(census.last_trace_stats().budget_peak <= 4096);

    for count in [1_000_000, 10_000_000] {
        let saved = certificate(count * 48, count);
        let mut graph = Dense { count, roots: 1 };
        let mut census = Census::restore(saved, saved.upper).unwrap();
        census
            .set_trace_limits(TraceLimits {
                trace_memory_budget: 4096,
                ..TraceLimits::default()
            })
            .unwrap();
        let request = pressured(&mut census, 32);
        assert!(matches!(
            census.refine(&mut graph, &empty(), request),
            Err(CensusFault::Heap(HeapFault::Exhausted {
                resource: HostResource::TraceBudget
            }))
        ));
        assert_eq!(census.certificate(), saved);
        census.set_trace_limits(TraceLimits::default()).unwrap();
        let decision = census.refine(&mut graph, &empty(), request).unwrap();
        assert!(
            matches!(decision, RefinedDecision::Refused { reachable } if reachable == saved.upper)
        );
        let stats = census.last_trace_stats();
        eprintln!(
            "census trace entities={count} bitmap_peak={} work_list_peak={} budget_peak={} work={}",
            stats.bitmap_peak, stats.work_list_peak, stats.budget_peak, stats.work
        );
        assert!(stats.budget_peak <= TraceLimits::default().trace_memory_budget);
    }
}
