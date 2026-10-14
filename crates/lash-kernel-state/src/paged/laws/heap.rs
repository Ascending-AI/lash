//! S1/S2 counted-work and semantic laws over an in-process page port.
use super::super::*;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::key_digest;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Pages {
    pub pages: BTreeMap<PageId, PageAnswer>,
    pub reads: u64,
    pub seals: u64,
    pub bytes: u64,
}
pub(super) struct Source(pub Rc<RefCell<Pages>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        let mut store = self.0.borrow_mut();
        store.reads += pages.len() as u64;
        pages
            .iter()
            .map(|p| {
                store
                    .pages
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect()
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        let mut store = self.0.borrow_mut();
        store.seals += 1;
        store.bytes += body.tokens.len() as u64 + 32 * body.children.len() as u64;
        store.pages.insert(
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
pub(super) fn fresh() -> (PagedHeap, Rc<RefCell<Pages>>) {
    let pages = Rc::new(RefCell::new(Pages::default()));
    let mut heap = PagedHeap::fresh(Box::new(Source(Rc::clone(&pages))), EditEpoch(1));
    heap.set_census(Box::new(
        TestCensus::restore(heap.certificate(), LogicalBytes(1 << 30)).expect("census"),
    ))
    .expect("install census");
    (heap, pages)
}
pub(super) fn seal(heap: &mut PagedHeap) -> SealedHeap {
    heap.seal(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
    .expect("seal")
    .expect("complete")
}
fn reopen(heap: &mut PagedHeap, pages: &Rc<RefCell<Pages>>) -> PagedHeap {
    heap.release_statement_roots().expect("release temporaries");
    seal(heap);
    let mut heap = PagedHeap::open_header(
        heap.sealed_root().expect("heap header"),
        Box::new(Source(Rc::clone(pages))),
        EditEpoch(2),
    )
    .expect("open");
    heap.set_census(Box::new(
        TestCensus::restore(heap.certificate(), LogicalBytes(1 << 30)).expect("census"),
    ))
    .expect("install census");
    heap
}
fn int(v: i64) -> Value {
    Value::Int(IntRef::Inline(v.into()))
}
fn is_int(value: Option<Value>, expected: i64) -> bool {
    matches!(value, Some(Value::Int(IntRef::Inline(i))) if i == expected.into())
}
fn text(heap: &mut PagedHeap, t: &str) -> Value {
    Value::Text(heap.text(t).expect("text"))
}

#[test]
fn aliases_see_one_mutation_across_seal_and_open() {
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("list");
    let aliases = heap
        .allocate_object(ObjectInit::List(vec![Value::List(list), Value::List(list)]))
        .expect("aliases");
    heap.list_write(list, 0, int(7)).expect("write");
    let heap = reopen(&mut heap, &pages);
    for i in 0..2 {
        let Some(Value::List(id)) = heap.list_get(aliases, i).expect("alias") else {
            panic!("list alias");
        };
        assert_eq!(id, list);
        assert!(is_int(heap.list_get(id, 0).expect("get"), 7));
    }
}
#[test]
fn cycles_seal_as_a_page_dag() {
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(Vec::new()))
        .expect("list");
    heap.list_write(list, 0, Value::List(list))
        .expect("self edge");
    let heap = reopen(&mut heap, &pages);
    assert!(matches!(heap.list_get(list, 0).expect("cycle"), Some(Value::List(id)) if id == list));
    // Every storage child existed before its parent was sealed; entity cycles
    // therefore cannot create a storage cycle.
    fn walk(id: PageId, pages: &Pages, active: &mut Vec<PageId>) {
        assert!(!active.contains(&id));
        active.push(id);
        for p in &pages.pages[&id].children {
            walk(p.id, pages, active);
        }
        active.pop();
    }
    walk(
        heap.sealed_root().expect("header").id,
        &pages.borrow(),
        &mut Vec::new(),
    );
}
#[test]
fn map_iteration_survives_removal_and_reinsertion_across_seal() {
    let (mut heap, pages) = fresh();
    let map = heap
        .allocate_object(ObjectInit::Map(vec![(int(1), int(10)), (int(2), int(20))]))
        .expect("map");
    let (cursor, first) = heap.table_after(map, None).expect("first").expect("entry");
    assert!(heap.equal(&first, &int(1)).expect("equal"));
    heap.map_write(map, Value::Float(lash_kernel_doc::Float::new(1.0)), int(11))
        .expect("replace");
    heap.table_remove(map, &int(1)).expect("remove");
    heap.map_write(map, int(1), int(12)).expect("reinsert");
    let mut heap = reopen(&mut heap, &pages);
    let (second_cursor, second) = heap
        .table_after(map, Some(cursor))
        .expect("next")
        .expect("entry");
    assert!(heap.equal(&second, &int(2)).expect("equal"));
    let (new_cursor, last) = heap
        .table_after(map, Some(second_cursor))
        .expect("last")
        .expect("entry");
    assert!(new_cursor > second_cursor);
    assert!(heap.equal(&last, &int(1)).expect("equal"));
    let field = heap.text("z").expect("field");
    let record = heap
        .allocate_object(ObjectInit::Record(vec![(field, int(3))]))
        .expect("record");
    heap.record_write(record, FieldName::Static("a"), int(4))
        .expect("record write");
    let heap = reopen(&mut heap, &pages);
    let mut fields = Vec::new();
    heap.visit(record, &mut |e| {
        if let Element::Field { name, .. } = e {
            if let Payload::Resident(s) = &name.body {
                fields.push(s.to_string());
            }
        }
        ControlFlow::Continue(())
    })
    .expect("visit");
    assert_eq!(fields, ["z", "a"]);
}
#[test]
fn a_point_write_in_a_huge_list_seals_one_leaf_path() {
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List((0..100_000).map(int).collect()))
        .expect("list");
    seal(&mut heap);
    let before = pages.borrow().bytes;
    heap.list_write(list, 55_555, int(-1)).expect("write");
    let sealed = seal(&mut heap);
    assert!(sealed.sealed_pages <= 6, "{} pages", sealed.sealed_pages);
    let bytes = pages.borrow().bytes - before;
    let heap = reopen(&mut heap, &pages);
    let before_reads = pages.borrow().reads;
    assert!(is_int(heap.list_get(list, 55_555).expect("get"), -1));
    let reads = pages.borrow().reads - before_reads;
    assert!(reads <= 5, "{reads} lookup pages");
    println!(
        "list: fanout=32 cells=64 sealed_pages={} sealed_bytes={bytes} cold_lookup_pages={reads}",
        sealed.sealed_pages
    );
}
#[test]
fn a_tiny_update_among_many_objects_seals_one_directory_path() {
    let (mut heap, pages) = fresh();
    let mut chosen = None;
    for i in 0..100_000 {
        let id = heap
            .allocate_object(ObjectInit::List(vec![int(i)]))
            .expect("small object");
        if i == 55_555 {
            chosen = Some(id);
        }
    }
    seal(&mut heap);
    let before = pages.borrow().bytes;
    heap.list_write(chosen.expect("chosen"), 0, int(-1))
        .expect("write");
    let sealed = seal(&mut heap);
    assert!(sealed.sealed_pages <= 5, "{} pages", sealed.sealed_pages);
    let bytes = pages.borrow().bytes - before;
    let heap = reopen(&mut heap, &pages);
    let before_reads = pages.borrow().reads;
    assert!(is_int(
        heap.list_get(chosen.expect("chosen"), 0).expect("get"),
        -1
    ));
    let reads = pages.borrow().reads - before_reads;
    assert!(reads <= 4, "{reads} lookup pages");
    println!(
        "objects: fanout=32 rows=64 target_bytes=8192 sealed_pages={} sealed_bytes={bytes} cold_lookup_pages={reads}",
        sealed.sealed_pages
    );
}
#[test]
fn a_text_length_read_touches_no_payload_page() {
    let (mut heap, pages) = fresh();
    let payload = text(&mut heap, &"x".repeat(16 << 20));
    let list = heap
        .allocate_object(ObjectInit::List(vec![payload]))
        .expect("holder");
    let heap = reopen(&mut heap, &pages);
    let Some(Value::Text(t)) = heap.list_get(list, 0).expect("descriptor") else {
        panic!("text");
    };
    let before_reads = pages.borrow().reads;
    assert_eq!(t.summary.utf8_bytes, 16 << 20);
    assert_eq!(t.summary.chars, 16 << 20);
    assert_eq!(pages.borrow().reads, before_reads);
    println!("text: payload_leaf_bytes=4096 length_payload_pages=0");
}
#[test]
fn equal_payloads_share_one_canonical_root() {
    let (mut heap, _) = fresh();
    let a = heap.text(&"é🦀".repeat(8_000)).expect("text");
    let b = heap.text(&"é🦀".repeat(8_000)).expect("text");
    let (Payload::Paged(a), Payload::Paged(b)) = (a.body, b.body) else {
        panic!("paged text");
    };
    assert_eq!(a.id, b.id);
}

/// Exact test-only census; production policy is injected by composition.
struct TestCensus {
    certificate: AccountingCertificate,
    limit: u64,
}
impl AllocationCensus for TestCensus {
    fn restore(
        certificate: AccountingCertificate,
        bound: LogicalBytes,
    ) -> Result<Self, CensusFault> {
        Ok(Self {
            certificate,
            limit: bound.0,
        })
    }
    fn certificate(&self) -> AccountingCertificate {
        self.certificate
    }
    fn added(&mut self, cost: LogicalBytes, entities: u64) -> Result<(), CensusFault> {
        if entities
            .checked_mul(32)
            .is_none_or(|minimum| minimum > cost.0)
        {
            return Err(CensusFault::Malformed("entity cost below minimum".into()));
        }
        self.changed(LogicalBytes(0), cost)?;
        self.certificate.entities += entities;
        Ok(())
    }
    fn changed(&mut self, before: LogicalBytes, after: LogicalBytes) -> Result<(), CensusFault> {
        self.certificate.upper.0 = self
            .certificate
            .upper
            .0
            .checked_sub(before.0)
            .and_then(|v| v.checked_add(after.0))
            .ok_or_else(|| CensusFault::Malformed("test census overflow".into()))?;
        Ok(())
    }
    fn reserve(&mut self, bytes: LogicalBytes) -> ReservationDecision {
        if self
            .certificate
            .upper
            .0
            .checked_add(bytes.0)
            .is_some_and(|v| v <= self.limit)
        {
            ReservationDecision::Granted(LogicalPermit { bytes })
        } else {
            ReservationDecision::NeedsPrecision(PendingReservation {
                bytes,
                overflowed: self.certificate.upper.0.checked_add(bytes.0).is_none(),
            })
        }
    }
    fn refine(
        &mut self,
        graph: &mut dyn CensusGraph,
        roots: &TransientRoots<'_>,
        pending: PendingReservation,
    ) -> Result<RefinedDecision, CensusFault> {
        use std::collections::BTreeSet;
        let mut cost = graph.root_cost()?.0;
        let mut queue = Vec::new();
        let mut persistent = Vec::new();
        graph.persistent_roots(&mut |v| {
            persistent.push(v.clone());
            Ok(())
        })?;
        for v in persistent {
            graph.value_edges(&v, &mut |i| {
                queue.push(i);
                Ok(())
            })?;
        }
        for v in roots
            .registers
            .iter()
            .chain(roots.pinned)
            .chain(roots.native_args)
        {
            cost = cost
                .checked_add(graph.value_cost(v)?.0)
                .ok_or_else(|| CensusFault::Malformed("trace cost overflow".into()))?;
            graph.value_edges(v, &mut |i| {
                queue.push(i);
                Ok(())
            })?;
        }
        queue.extend(roots.fresh.iter().map(|id| Identity::Object(*id)));
        cost = cost
            .checked_add(roots.reserved.0)
            .ok_or_else(|| CensusFault::Malformed("reservation cost overflow".into()))?;
        let mut marked = BTreeSet::new();
        let mut entities = 0;
        while let Some(identity) = queue.pop() {
            if !marked.insert(identity) {
                continue;
            }
            let local = match identity {
                Identity::Object(id) => {
                    let local = graph
                        .entity_cost(id)?
                        .ok_or_else(|| {
                            CensusFault::Malformed("trace reaches absent entity".into())
                        })?
                        .local
                        .0;
                    graph.entity_edges(id, &mut |i| {
                        queue.push(i);
                        Ok(())
                    })?;
                    entities += 1;
                    local
                }
                Identity::Task(id) => {
                    graph
                        .task_result(id, &mut |i| {
                            queue.push(i);
                            Ok(())
                        })?
                        .0
                }
            };
            cost = cost
                .checked_add(local)
                .ok_or_else(|| CensusFault::Malformed("trace overflow".into()))?;
        }
        let all = graph.census_after(None, usize::MAX)?;
        let ended = graph.ended_tasks_after(None, usize::MAX)?;
        graph.sweep(&SweepSet {
            entities: all
                .into_iter()
                .filter(|id| !marked.contains(&Identity::Object(*id)))
                .collect(),
            ended_task_results: ended
                .into_iter()
                .filter(|id| !marked.contains(&Identity::Task(*id)))
                .collect(),
        })?;
        self.certificate.upper = LogicalBytes(cost);
        self.certificate.entities = entities;
        self.certificate.refinements += 1;
        Ok(
            if cost
                .checked_add(pending.bytes.0)
                .is_some_and(|v| v <= self.limit)
            {
                RefinedDecision::Granted(LogicalPermit {
                    bytes: pending.bytes,
                })
            } else {
                RefinedDecision::Refused {
                    reachable: LogicalBytes(cost),
                }
            },
        )
    }
}
fn frozen(entities: u64, data_only: bool) -> FrozenRef {
    FrozenRef {
        root: PageRef {
            id: PageId([19; 32]),
            grant: PageGrant::NONE,
        },
        summary: FrozenSummary {
            entities,
            cost: LogicalBytes(entities * 32),
            data_only,
        },
    }
}
#[test]
fn the_last_swept_member_retires_its_family_base() {
    let (mut heap, pages) = fresh();
    let family = heap.register_family(frozen(3, false)).expect("family");
    assert_eq!(heap.census_after(None, 2).expect("first batch").len(), 2);
    for ordinal in 0..3 {
        heap.sweep(&SweepSet {
            entities: vec![EntityId { family, ordinal }],
            ended_task_results: vec![],
        })
        .expect("sweep");
    }
    assert_eq!(heap.families.len(), 0);
    assert!(heap.census_after(None, 10).expect("census").is_empty());
    assert_eq!(
        heap.data_only_certificate(),
        DataOnlyCertificate {
            closure_entities: 0,
            task_edges: 0
        }
    );
    let mut heap = reopen(&mut heap, &pages);
    assert!(
        heap.census_after(None, 10)
            .expect("reopened census")
            .is_empty()
    );
    let next = heap.register_family(frozen(0, true)).expect("empty family");
    assert!(next.0 > family.0);
    assert_eq!(heap.families.len(), 0);
}
#[test]
fn sweeping_a_dead_member_preserves_the_membership_count() {
    let (mut heap, pages) = fresh();
    let family = heap.register_family(frozen(3, true)).expect("family");
    let dead = SweepSet {
        entities: vec![EntityId { family, ordinal: 1 }],
        ended_task_results: vec![],
    };
    heap.sweep(&dead).expect("first sweep");
    let mut heap = reopen(&mut heap, &pages);
    heap.sweep(&dead).expect("repeated sweep");
    assert_eq!(
        heap.census_after(None, 10).expect("census"),
        vec![
            EntityId { family, ordinal: 0 },
            EntityId { family, ordinal: 2 }
        ]
    );
    heap.sweep(&SweepSet {
        entities: vec![EntityId { family, ordinal: 0 }],
        ended_task_results: vec![],
    })
    .expect("sweep");
    assert_eq!(heap.families.len(), 1);
    heap.sweep(&SweepSet {
        entities: vec![EntityId { family, ordinal: 2 }],
        ended_task_results: vec![],
    })
    .expect("sweep last");
    assert_eq!(heap.families.len(), 0);
}
#[test]
fn a_direct_task_binding_prevents_the_data_only_certificate() {
    let (mut heap, _) = fresh();
    let mut bindings = tree::Tree::new(false);
    let name = lash_kernel_doc::Name::from("task");
    bindings
        .write(
            b"task".to_vec(),
            BindingRecord {
                name,
                declared: lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, []),
                value: BoundRecord::Value(Value::Task(lash_kernel_doc::TaskId(7))),
            },
            &heap.access,
        )
        .expect("binding");
    heap.set_session_root(bindings).expect("session");
    assert_eq!(heap.data_only_certificate().task_edges, 1);
    heap.set_session_root(tree::Tree::new(false))
        .expect("drop session");
    assert_eq!(heap.data_only_certificate().task_edges, 0);
    assert_eq!(heap.certificate().upper, LogicalBytes(0));
}

#[test]
fn relative_references_in_error_data_inherit_the_page_family() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter};
    let (mut heap, _) = fresh();
    let mut out = TokenWriter::new();
    out.seq(1);
    out.seq(2);
    out.bytes(&0u64.to_be_bytes());
    out.sum(8, 1);
    out.seq(3);
    TupleSummary {
        len: 1,
        depth: 2,
        cost: LogicalBytes(22),
        has_edges: true,
        task_edges: 0,
    }
    .encode(&mut out);
    Option::<FamilyId>::None.encode(&mut out);
    out.sum(0, 1);
    out.seq(1);
    out.sum(14, 1);
    out.seq(8);
    heap.text("err").expect("kind").encode(&mut out);
    heap.text("msg").expect("message").encode(&mut out);
    out.sum(0, 1);
    lash_kernel_doc::heap_values::encode_relative(0, 0, &mut out);
    1u32.encode(&mut out);
    LogicalBytes(22).encode(&mut out);
    true.encode(&mut out);
    Option::<FamilyId>::None.encode(&mut out);
    0u64.encode(&mut out);
    let page = heap
        .access
        .seal(PageKind::SEQUENCE_LEAF, out.into_bytes(), vec![])
        .expect("relative leaf");
    let tree = tree::Tree::<Value>::open(Some(page), 1, LogicalBytes(38), true);
    assert!(
        tree.rank_get(0, &heap.access).is_err(),
        "relative references require context"
    );
    for family in [FamilyId(3), FamilyId(4)] {
        let Some((_, Value::Tuple(tuple))) = tree
            .clone()
            .in_family(family)
            .rank_get(0, &heap.access)
            .expect("tuple")
        else {
            panic!("tuple");
        };
        let Some(Value::Error(error)) = heap.tuple_get(&tuple, 0).expect("error") else {
            panic!("error");
        };
        assert!(
            matches!(heap.error_data(&error).expect("data"), Value::List(id) if id == EntityId { family, ordinal: 0 })
        );
    }
}

#[test]
fn errors_at_the_depth_limit_have_bounded_descendants() {
    let (mut heap, pages) = fresh();
    let kind = heap.text(&"k".repeat(256)).expect("kind");
    let message = heap.text(&"m".repeat(256)).expect("message");
    let mut value = Value::Null;
    for depth in 1..=128 {
        value = Value::Error(Arc::new(ErrorRef {
            family: None,
            task_edges: 0,
            kind: kind.clone(),
            message: message.clone(),
            data: Payload::Resident(Arc::new(value)),
            depth,
            cost: LogicalBytes(0),
            has_edges: false,
        }));
    }
    let list = heap
        .allocate_object(ObjectInit::List(vec![value]))
        .expect("deep error");
    let heap = reopen(&mut heap, &pages);
    let mut value = heap.list_get(list, 0).expect("get").expect("value");
    for depth in (1..=128).rev() {
        let Value::Error(error) = value else {
            panic!("error at {depth}");
        };
        assert_eq!(error.depth, depth);
        value = heap.error_data(&error).expect("data");
    }
    assert!(matches!(value, Value::Null));
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|p| p.tokens.len() + 32 * p.children.len() + 32 <= 65_536)
    );
}
