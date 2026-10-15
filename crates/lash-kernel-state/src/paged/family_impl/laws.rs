//! S8 semantic and counted-work laws over the authenticated kernel page port.
use crate::paged::family_impl::FrozenBase;
use crate::paged::laws::heap::{Source, fresh, seal};
use crate::paged::*;
use lash_kernel_doc::Name;
use lash_kernel_doc::heap::*;
use std::rc::Rc;

fn int(n: i64) -> Value {
    Value::Int(IntRef::Inline(n.into()))
}
fn list(value: Value) -> EntityId {
    let Value::List(id) = value else {
        panic!("expected list")
    };
    id
}
fn binding(name: &str, value: Value) -> (Name, Value) {
    (Name::new(name), value)
}
fn empty() -> KeyedRoot {
    KeyedRoot::Empty
}
fn bindings(heap: &mut PagedHeap, values: impl IntoIterator<Item = (Name, Value)>) -> KeyedRoot {
    let mut table = crate::paged::tree::Tree::new(false);
    for (name, value) in values {
        table
            .write(
                name.as_str().as_bytes().to_vec(),
                BindingRecord {
                    name: NameRef::new(&name, &heap.access).unwrap(),
                    declared: SiteRef::new(
                        &(lash_kernel_doc::Site {
                            unit: lash_kernel_doc::Unit::Main,
                            path: vec![],
                        }),
                        &heap.access,
                    )
                    .unwrap(),
                    value: BoundRecord::Value(value),
                },
                &heap.access,
            )
            .expect("binding");
    }
    table
        .seal(
            &heap.access,
            &mut crate::paged::tree::SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .expect("binding seal");
    let root = crate::paged::heap_impl::keyed(&table).expect("a sealed root");
    heap.set_session_root(table).expect("install bindings");
    root
}
fn get(heap: &PagedHeap, name: &str) -> Value {
    let record = heap
        .session
        .get(name.as_bytes(), &heap.access)
        .expect("binding read")
        .expect("binding exists");
    let BoundRecord::Value(value) = record.value else {
        panic!("value binding")
    };
    value
}
fn next(
    heap: &mut PagedHeap,
    pages: &Rc<std::cell::RefCell<crate::paged::laws::heap::Pages>>,
    root: KeyedRoot,
) -> (PagedHeap, KeyedRoot) {
    let session = heap
        .carry(CarryRoots {
            bindings: root,
            functions: empty(),
        })
        .expect("carry");
    let mut next = PagedHeap::open_header(
        session.heap,
        Box::new(Source(Rc::clone(pages))),
        EditEpoch(2),
    )
    .expect("open session");
    next.set_census(heap.census.take().expect("census"))
        .expect("next census");
    let bindings = next.adopt(&session).expect("adopt");
    (next, bindings)
}
#[test]
fn a_noop_next_cell_copies_nothing() {
    let (mut heap, pages) = fresh();
    let mut objects = Vec::new();
    for _ in 0..100_000 {
        objects.push(Value::List(
            heap.allocate_object(ObjectInit::List(Vec::new()))
                .expect("object"),
        ));
    }
    let id = heap
        .allocate_object(ObjectInit::List(objects))
        .expect("root");
    let root = bindings(&mut heap, [binding("root", Value::List(id))]);
    heap.release_statement_roots().expect("release");
    seal(&mut heap);
    let before = (pages.borrow().reads, pages.borrow().seals);
    let (next, adopted) = next(&mut heap, &pages, root);
    assert_eq!(adopted.page(), root.page());
    assert_eq!(next.certificate().entities, 100_001);
    let reads = pages.borrow().reads - before.0;
    let writes = pages.borrow().seals - before.1;
    assert!(reads <= 1, "{reads} pages read");
    assert!(writes <= 1, "{writes} pages sealed");
    assert_eq!(list(get(&next, "root")), id);
    eprintln!("100001 objects: next-cell reads={reads}, seals={writes}");
}
#[test]
fn a_noop_cell_with_100k_separate_bindings_reads_only_the_header() {
    let (mut heap, pages) = fresh();
    let mut values = Vec::new();
    for i in 0..100_000 {
        let id = heap
            .allocate_object(ObjectInit::List(Vec::new()))
            .expect("object");
        values.push(binding(&format!("object{i}"), Value::List(id)));
    }
    let root = bindings(&mut heap, values);
    heap.release_statement_roots().expect("release");
    seal(&mut heap);
    let before = (pages.borrow().reads, pages.borrow().seals);
    let (next, adopted) = next(&mut heap, &pages, root);
    assert_eq!(adopted.page(), root.page());
    assert_eq!(adopted.len(), 100_000);
    assert_eq!(next.certificate().entities, 100_000);
    let reads = pages.borrow().reads - before.0;
    let writes = pages.borrow().seals - before.1;
    assert!(reads <= 1, "{reads} pages read");
    assert!(writes <= 1, "{writes} pages sealed");
    eprintln!("100000 separate bindings: next-cell reads={reads}, seals={writes}");
}
#[test]
fn aliases_across_cells_keep_distinct_objects_distinct() {
    let (mut heap, pages) = fresh();
    let a = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("a");
    let b = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("b");
    let root = bindings(
        &mut heap,
        [
            binding("a", Value::List(a)),
            binding("alias", Value::List(a)),
            binding("b", Value::List(b)),
        ],
    );
    let (mut next, _) = next(&mut heap, &pages, root);
    let alias = list(get(&next, "alias"));
    assert_eq!(a, alias);
    assert_ne!(a, b);
    next.list_write(a, 0, int(9)).expect("write");
    assert!(
        matches!(next.list_get(alias, 0).expect("alias"), Some(Value::Int(IntRef::Inline(n))) if n == 9.into())
    );
    assert!(
        matches!(next.list_get(b, 0).expect("distinct"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[test]
fn a_fork_isolates_later_mutations() {
    let (mut heap, pages) = fresh();
    let id = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("list");
    let root = bindings(&mut heap, [binding("a", Value::List(id))]);
    let session = heap
        .carry(CarryRoots {
            bindings: root,
            functions: empty(),
        })
        .expect("carry");
    let mut parent = PagedHeap::open_header(
        session.heap,
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .expect("parent");
    let mut fork = PagedHeap::open_header(
        session.heap,
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(3),
    )
    .expect("fork");
    parent
        .set_census(Box::new(LawCensus(parent.certificate())))
        .expect("parent census");
    fork.set_census(Box::new(LawCensus(fork.certificate())))
        .expect("fork census");
    parent.adopt(&session).expect("parent adopt");
    fork.adopt(&session).expect("fork adopt");
    parent.list_write(id, 0, int(2)).expect("parent write");
    assert!(
        matches!(fork.list_get(id, 0).expect("fork"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
    fork.list_write(id, 0, int(3)).expect("fork write");
    assert!(
        matches!(parent.list_get(id, 0).expect("parent"), Some(Value::Int(IntRef::Inline(n))) if n == 2.into())
    );
}
#[test]
fn a_frozen_capture_ignores_later_live_writes() {
    let (mut heap, _) = fresh();
    let live = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("live");
    let frozen = heap.freeze(&Value::List(live), true).expect("freeze");
    heap.list_write(live, 0, int(2)).expect("live write");
    let capture = list(heap.instantiate(&frozen).expect("capture"));
    assert!(
        matches!(heap.list_get(capture, 0).expect("get"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
    heap.list_write(capture, 0, int(3)).expect("capture write");
    let other = list(heap.instantiate(&frozen).expect("other capture"));
    assert!(
        matches!(heap.list_get(other, 0).expect("get"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[test]
fn a_swept_family_ordinal_never_resurrects() {
    let (mut heap, pages) = fresh();
    let live = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("live");
    let frozen = heap.freeze(&Value::List(live), true).expect("freeze");
    let id = list(heap.instantiate(&frozen).expect("deliver"));
    heap.list_write(id, 0, int(2)).expect("override");
    heap.sweep(&SweepSet {
        entities: vec![id],
        ended_task_results: Vec::new(),
    })
    .expect("sweep");
    seal(&mut heap);
    let mut reopened = PagedHeap::open_header(
        heap.sealed_root().expect("header"),
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .expect("open");
    reopened.set_family_base(Box::new(FrozenBase::default()));
    assert_eq!(reopened.kind(id).expect("membership"), None);
}
#[test]
fn a_lazy_result_length_read_instantiates_nothing() {
    let (mut heap, pages) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![Value::List(child); 100_000]))
        .expect("source");
    let frozen = heap.freeze(&Value::List(source), true).expect("freeze");
    assert_eq!(frozen.summary.entities, 100_001);
    let before = heap.directory.len();
    let result = list(heap.instantiate(&frozen).expect("deliver"));
    let seals = pages.borrow().seals;
    assert_eq!(heap.len(result).expect("length"), 100_000);
    assert_eq!(heap.directory.len(), before);
    assert_eq!(pages.borrow().seals, seals);
    let first = list(heap.list_get(result, 0).expect("first").expect("present"));
    let second = list(heap.list_get(result, 1).expect("second").expect("present"));
    assert_ne!(first, second);
    heap.list_write(first, 0, int(7)).expect("edit first");
    assert!(
        matches!(heap.list_get(second, 0).expect("second unchanged"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[test]
fn repeated_delivery_loads_only_touched_index_paths() {
    let (mut heap, pages) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![Value::List(child); 100_000]))
        .expect("source");
    let frozen = heap.freeze(&Value::List(source), true).expect("freeze");
    heap.evict().expect("evict");
    let before = (
        pages.borrow().reads,
        heap.directory.len(),
        heap.certificate(),
    );
    let a = list(heap.instantiate(&frozen).expect("a"));
    let b = list(heap.instantiate(&frozen).expect("b"));
    assert_ne!(a.family, b.family);
    assert_eq!(
        heap.certificate().entities - before.2.entities,
        2 * frozen.summary.entities
    );
    assert_eq!(
        heap.certificate().upper.0 - before.2.upper.0,
        2 * frozen.summary.cost.0
    );
    assert_eq!(heap.len(a).expect("a len"), 100_000);
    assert_eq!(heap.len(b).expect("b len"), 100_000);
    let a_child = list(heap.list_get(a, 55_555).expect("a child").expect("present"));
    let b_child = list(heap.list_get(b, 55_555).expect("b child").expect("present"));
    assert_ne!(a_child, b_child);
    assert_eq!(heap.len(a_child).expect("a child len"), 1);
    assert_eq!(heap.len(b_child).expect("b child len"), 1);
    assert_eq!(heap.directory.len(), before.1);
    let reads = pages.borrow().reads - before.0;
    assert!(
        reads <= 32,
        "delivery and touched occurrences read {reads} pages"
    );
    eprintln!("100001 occurrences delivered twice: reads={reads}, overrides=0");
}
#[test]
fn data_only_carry_reads_no_binding() {
    let (mut heap, pages) = fresh();
    let large = heap.text(&"x".repeat(1_000_000)).expect("text");
    let root = bindings(&mut heap, [binding("text", Value::Text(large))]);
    heap.evict().expect("evict");
    let before = pages.borrow().reads;
    let result = heap.classify_carry(&root).expect("classify");
    assert_eq!(result.carried.page(), root.page());
    assert_eq!(result.carried.len(), 1);
    assert!(result.dropped.is_empty());
    assert_eq!(pages.borrow().reads, before);
}
#[test]
fn carry_classification_walks_the_union_and_propagates_through_cycles() {
    let (mut heap, pages) = fresh();
    let closure = heap
        .allocate_object(ObjectInit::Closure {
            site: SiteRef::new(
                &(lash_kernel_doc::Site {
                    unit: lash_kernel_doc::Unit::Main,
                    path: vec![],
                }),
                &heap.access,
            )
            .unwrap(),
            captures: Vec::new(),
        })
        .expect("closure");
    let a = heap
        .allocate_object(ObjectInit::List(Vec::new()))
        .expect("a");
    let b = heap
        .allocate_object(ObjectInit::List(vec![
            Value::List(a),
            Value::Closure(closure),
        ]))
        .expect("b");
    heap.list_write(a, 0, Value::List(b)).expect("cycle");
    let root = bindings(
        &mut heap,
        (0..100).map(|i| binding(&format!("alias{i}"), Value::List(a))),
    );
    heap.evict().expect("evict");
    let before = pages.borrow().reads;
    let result = heap.classify_carry(&root).expect("classify");
    assert_eq!(result.dropped.len(), 100);
    assert_eq!(result.carried.len(), 0);
    assert!(pages.borrow().reads - before <= 8, "one union traversal");
}

struct LawCensus(crate::paged::AccountingCertificate);
impl crate::paged::AllocationCensus for LawCensus {
    fn restore(
        c: crate::paged::AccountingCertificate,
        _: LogicalBytes,
    ) -> Result<Self, crate::paged::CensusFault> {
        Ok(Self(c))
    }
    fn certificate(&self) -> crate::paged::AccountingCertificate {
        self.0
    }
    fn added(
        &mut self,
        cost: LogicalBytes,
        entities: u64,
    ) -> Result<(), crate::paged::CensusFault> {
        self.changed(LogicalBytes(0), cost)?;
        self.0.entities = self
            .0
            .entities
            .checked_add(entities)
            .expect("test entity count");
        Ok(())
    }
    fn changed(
        &mut self,
        before: LogicalBytes,
        after: LogicalBytes,
    ) -> Result<(), crate::paged::CensusFault> {
        self.0.upper.0 = self
            .0
            .upper
            .0
            .checked_sub(before.0)
            .and_then(|v| v.checked_add(after.0))
            .expect("test cost");
        Ok(())
    }
    fn reserve(&mut self, bytes: LogicalBytes) -> crate::paged::ReservationDecision {
        crate::paged::ReservationDecision::Granted(crate::paged::LogicalPermit { bytes })
    }
    fn refine(
        &mut self,
        _: &mut dyn crate::paged::CensusGraph,
        _: &crate::paged::TransientRoots<'_>,
        _: crate::paged::PendingReservation,
    ) -> Result<crate::paged::RefinedDecision, crate::paged::CensusFault> {
        unreachable!("unbounded lifecycle law census")
    }
}

#[test]
fn foreign_results_freeze_without_live_allocation_and_keep_the_decode_charge() {
    let (mut heap, _) = fresh();
    let before = (heap.directory.len(), heap.certificate());
    let datum = lash_kernel_doc::Datum::Map(vec![
        (
            lash_kernel_doc::Datum::Int(1.into()),
            lash_kernel_doc::Datum::List(vec![lash_kernel_doc::Datum::Int(1.into())]),
        ),
        (
            lash_kernel_doc::Datum::Float(lash_kernel_doc::Float::new(1.0)),
            lash_kernel_doc::Datum::List(vec![lash_kernel_doc::Datum::Int(2.into())]),
        ),
    ]);
    let frozen = heap.freeze_datum(&datum).expect("freeze foreign data");
    assert_eq!(
        frozen.summary.entities, 2,
        "only the surviving value has an occurrence"
    );
    assert_eq!(heap.directory.len(), before.0);
    assert_eq!(heap.certificate(), before.1);
    assert_eq!(heap.frozen_charge(&frozen).expect("charge"), 9);
    let Value::Map(map) = heap.instantiate(&frozen).expect("instantiate") else {
        panic!("map")
    };
    assert_eq!(heap.len(map).expect("length"), 1);
    let child = list(heap.map_get(map, &int(1)).expect("get").expect("present"));
    assert!(
        matches!(heap.list_get(child, 0).expect("member"), Some(Value::Int(IntRef::Inline(n))) if n == 2.into())
    );
}

#[test]
fn carry_classification_respects_direct_and_immutable_task_roots() {
    let (mut heap, _) = fresh();
    let task = Value::Task(lash_kernel_doc::TaskId(99));
    let tuple = heap.tuple(vec![task.clone()]).expect("tuple");
    let root = bindings(
        &mut heap,
        [
            binding("task", task),
            binding("tuple", Value::Tuple(tuple)),
            binding("data", int(1)),
        ],
    );
    let result = heap.classify_carry(&root).expect("classification");
    assert_eq!(result.dropped, [Name::new("task"), Name::new("tuple")]);
    assert_eq!(result.carried.len(), 1);
}
#[test]
fn freezing_rejects_cycles_and_runtime_identities() {
    let (mut heap, _) = fresh();
    let id = heap
        .allocate_object(ObjectInit::List(Vec::new()))
        .expect("list");
    heap.list_write(id, 0, Value::List(id)).expect("cycle");
    for value in [
        Value::List(id),
        Value::Task(lash_kernel_doc::TaskId(99)),
        Value::Ref(Identity::Object(id)),
    ] {
        assert!(matches!(
            heap.freeze(&value, true),
            Err(HeapWriteError::Raised { .. })
        ));
    }
}

#[test]
fn scalar_delivery_creates_no_dead_family_and_releases_its_temporary_credit() {
    let (mut heap, _) = fresh();
    let frozen = heap
        .freeze_datum(&lash_kernel_doc::Datum::Text("value".into()))
        .expect("freeze");
    let before = heap.certificate();
    let families = heap.families.len();
    for _ in 0..10 {
        let value = heap.instantiate(&frozen).expect("deliver");
        assert!(matches!(value, Value::Text(_)));
        heap.release_statement_roots()
            .expect("release unbound result");
    }
    assert_eq!(heap.families.len(), families);
    assert_eq!(heap.certificate(), before);
}

#[test]
fn a_paged_frozen_error_keeps_its_receiving_family_at_every_immutable_depth() {
    let (mut heap, pages) = fresh();
    let datum = lash_kernel_doc::Datum::Error(Box::new(lash_kernel_doc::ErrorDatum {
        kind: "outer".into(),
        message: "message".into(),
        data: lash_kernel_doc::Datum::Tuple(vec![lash_kernel_doc::Datum::Error(Box::new(
            lash_kernel_doc::ErrorDatum {
                kind: "inner".into(),
                message: "message".into(),
                data: lash_kernel_doc::Datum::List(vec![lash_kernel_doc::Datum::Int(1.into())]),
            },
        ))]),
    }));
    let frozen = heap.freeze_datum(&datum).expect("freeze");
    let a = heap.instantiate(&frozen).expect("first");
    let b = heap.instantiate(&frozen).expect("second");
    fn child(heap: &PagedHeap, value: Value) -> EntityId {
        let Value::Error(error) = value else {
            panic!("outer error")
        };
        let Value::Tuple(tuple) = heap.error_data(&error).expect("outer data") else {
            panic!("tuple")
        };
        let Value::Error(error) = heap
            .tuple_get(&tuple, 0)
            .expect("tuple member")
            .expect("present")
        else {
            panic!("inner error")
        };
        list(heap.error_data(&error).expect("inner data"))
    }
    let a = child(&heap, a);
    let b = child(&heap, b);
    assert_ne!(a.family, b.family);
    heap.list_write(a, 0, int(2)).expect("edit");
    seal(&mut heap);
    let mut reopened = PagedHeap::open_header(
        heap.sealed_root().expect("header"),
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .expect("open");
    reopened.set_family_base(Box::new(FrozenBase::default()));
    assert!(
        matches!(reopened.list_get(a, 0).expect("first"), Some(Value::Int(IntRef::Inline(n))) if n == 2.into())
    );
    assert!(
        matches!(reopened.list_get(b, 0).expect("second"), Some(Value::Int(IntRef::Inline(n))) if n == 1.into())
    );
}

/// S4 Issued requests use the frozen grammar as a host envelope. Reporting
/// reads that envelope without reserving or instantiating guest entities.
#[test]
fn an_issued_intent_projects_identical_data_without_allocating_entities() {
    use lash_kernel_doc::{Datum, ErrorDatum};
    let (mut heap, pages) = fresh();
    let datum = Datum::Tuple(vec![
        Datum::Text("request".repeat(32_000)),
        Datum::Error(Box::new(ErrorDatum {
            kind: "intent".into(),
            message: "issued".into(),
            data: Datum::List(vec![
                Datum::Int(7.into()),
                Datum::Record(vec![("args".into(), Datum::Text("x".repeat(100_000)))]),
            ]),
        })),
    ]);
    let frozen = heap.freeze_datum(&datum).expect("freeze request");
    let before = heap.heap_root().unwrap();
    let seals = pages.borrow().seals;
    assert_eq!(heap.frozen_datum(&frozen).expect("report request"), datum);
    let after = heap.heap_root().unwrap();
    assert_eq!(after.accounting, before.accounting);
    assert_eq!(after.next_family, before.next_family);
    assert_eq!(after.directory.len(), before.directory.len());
    assert_eq!(pages.borrow().seals, seals);
}
/// FIG-6318: a frozen family's pages come from the page port once per
/// residency, however many of its objects a run reads and however often:
/// a second pass over every member reads no page.
#[test]
fn a_frozen_familys_pages_are_read_once_per_residency() {
    let (mut heap, pages) = fresh();
    let children = (0..2_000)
        .map(|n| {
            heap.allocate_object(ObjectInit::List(vec![int(n)]))
                .map(Value::List)
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("children");
    let source = heap
        .allocate_object(ObjectInit::List(children))
        .expect("source");
    let frozen = heap.freeze(&Value::List(source), true).expect("freeze");
    let result = list(heap.instantiate(&frozen).expect("deliver"));
    seal(&mut heap);
    let pass = |heap: &PagedHeap| -> u64 {
        let before = pages.borrow().reads;
        for n in 0..2_000 {
            let child = list(heap.list_get(result, n).expect("read").expect("member"));
            assert!(matches!(
                heap.list_get(child, 0).expect("read"),
                Some(Value::Int(IntRef::Inline(i))) if i == (n as i64).into()
            ));
        }
        pages.borrow().reads - before
    };
    let first = pass(&heap);
    let second = pass(&heap);
    assert_eq!(
        second, 0,
        "a second pass read {second} pages; the first {first}"
    );
}
