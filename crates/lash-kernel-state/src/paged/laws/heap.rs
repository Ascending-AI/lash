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
        graph.persistent_roots(&mut |v| {
            raw_edges(v, &mut queue);
            Ok(())
        })?;
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
fn raw_edges(value: &Value, out: &mut Vec<Identity>) {
    match value {
        Value::List(id)
        | Value::Map(id)
        | Value::Set(id)
        | Value::Record(id)
        | Value::Closure(id) => out.push(Identity::Object(*id)),
        Value::Task(t) => out.push(Identity::Task(*t)),
        Value::Ref(i) => out.push(*i),
        Value::Tuple(t) => {
            if let Payload::Resident(v) = &t.body {
                for v in v.iter() {
                    raw_edges(v, out);
                }
            }
        }
        Value::Error(e) => raw_edges(&e.data, out),
        _ => (),
    }
}
