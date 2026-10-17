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
fn record(heap: &PagedHeap, name: &Name, value: Value) -> BindingRecord {
    BindingRecord {
        name: NameRef::new(name, &heap.access).unwrap(),
        declared: SiteRef::new(
            &(lash_kernel_doc::Site {
                unit: lash_kernel_doc::Unit::Main,
                path: vec![],
            }),
            &heap.access,
        )
        .unwrap(),
        value: BoundRecord::Value(value),
    }
}
fn bindings(heap: &mut PagedHeap, values: impl IntoIterator<Item = (Name, Value)>) -> KeyedRoot {
    let mut table = crate::paged::tree::Tree::new(false);
    for (name, value) in values {
        table
            .write(
                name.as_str().as_bytes().to_vec(),
                record(heap, &name, value),
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
fn a_swept_child_never_resurrects_while_its_family_stays_live() {
    let (mut heap, pages) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let live = heap
        .allocate_object(ObjectInit::List(vec![Value::List(child)]))
        .expect("live");
    let frozen = heap.freeze(&Value::List(live), true).expect("freeze");
    let root = list(heap.instantiate(&frozen).expect("deliver"));
    let child = list(
        heap.list_get(root, 0)
            .expect("child read")
            .expect("child present"),
    );
    assert_eq!(root.family, child.family);
    assert_ne!(root.ordinal, child.ordinal);
    heap.list_write(child, 0, int(2)).expect("override child");
    heap.list_write(root, 0, int(3)).expect("detach child");
    heap.sweep(&SweepSet {
        entities: vec![child],
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
    assert_eq!(
        reopened.kind(root).expect("live family root"),
        Some(ObjectKind::List)
    );
    assert!(
        matches!(reopened.list_get(root, 0).expect("retained override"), Some(Value::Int(IntRef::Inline(n))) if n == 3.into())
    );
    assert_eq!(reopened.kind(child).expect("dead child membership"), None);
}
#[test]
fn a_lazy_result_length_read_instantiates_nothing() {
    let (mut heap, _) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![Value::List(child); 100_000]))
        .expect("source");
    let frozen = heap.freeze(&Value::List(source), true).expect("freeze");
    let result = list(heap.instantiate(&frozen).expect("deliver"));
    assert_eq!(heap.len(result).expect("length"), 100_000);
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
    let (mut heap, _) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![Value::List(child); 100_000]))
        .expect("source");
    let frozen = heap.freeze(&Value::List(source), true).expect("freeze");
    heap.evict().expect("evict");
    let a = list(heap.instantiate(&frozen).expect("a"));
    let b = list(heap.instantiate(&frozen).expect("b"));
    assert_eq!(heap.len(a).expect("a len"), 100_000);
    assert_eq!(heap.len(b).expect("b len"), 100_000);
    let a_child = list(heap.list_get(a, 55_555).expect("a child").expect("present"));
    let b_child = list(heap.list_get(b, 55_555).expect("b child").expect("present"));
    assert_ne!(a_child, b_child);
    assert_eq!(heap.len(a_child).expect("a child len"), 1);
    assert_eq!(heap.len(b_child).expect("b child len"), 1);
}

#[test]
fn carry_classification_walks_the_union_and_propagates_through_cycles() {
    let (mut heap, _) = fresh();
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
    let result = heap.classify_carry(&root).expect("classify");
    assert_eq!(result.dropped.len(), 100);
    assert_eq!(result.carried.len(), 0);
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
    let (mut heap, _) = fresh();
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
    assert_eq!(heap.frozen_datum(&frozen).expect("report request"), datum);
}
