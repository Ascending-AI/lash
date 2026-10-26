//! S8 semantic and counted-work laws over the authenticated kernel page port.
use crate::paged::family_impl::FrozenBase;
use crate::paged::laws::heap::{Source, fresh, seal, write_list};
use crate::paged::*;
use lash_kernel_doc::heap::*;
use lash_kernel_value::names::Name;
use std::rc::Rc;

fn int(n: i64) -> PageValue {
    PageValue::Int(IntRef::Inline(n.into()))
}
fn list(value: PageValue) -> EntityId {
    let PageValue::List(id) = value else {
        panic!("expected list")
    };
    id
}
fn binding(name: &str, value: PageValue) -> (Name, PageValue) {
    (Name::new(name), value)
}
fn empty() -> KeyedRoot {
    KeyedRoot::Empty
}
fn record(heap: &PagedHeap, name: &Name, value: PageValue) -> BindingRecord {
    BindingRecord {
        name: NameRef::new(name, &heap.access).unwrap(),
        declared: SiteRef::new(
            &(lash_kernel_value::sites::Site {
                unit: lash_kernel_value::sites::Unit::Main,
                path: vec![],
            }),
            &heap.access,
        )
        .unwrap(),
        value: BoundRecord::Value(value),
    }
}
fn bindings(
    heap: &mut PagedHeap,
    values: impl IntoIterator<Item = (Name, PageValue)>,
) -> KeyedRoot {
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
fn get(heap: &PagedHeap, name: &str) -> PageValue {
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
            binding("a", PageValue::List(a)),
            binding("alias", PageValue::List(a)),
            binding("b", PageValue::List(b)),
        ],
    );
    let (mut next, _) = next(&mut heap, &pages, root);
    let alias = list(get(&next, "alias"));
    assert_eq!(a, alias);
    assert_ne!(a, b);
    write_list(&mut next, a, 0..1, |_| int(9)).expect("write");
    assert!(
        matches!(next.list_get(alias, 0).expect("alias"), Some(PageValue::Int(IntRef::Inline(n))) if n == 9.into())
    );
    assert!(
        matches!(next.list_get(b, 0).expect("distinct"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[test]
fn a_fork_isolates_later_mutations() {
    let (mut heap, pages) = fresh();
    let id = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("list");
    let root = bindings(&mut heap, [binding("a", PageValue::List(id))]);
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
    write_list(&mut parent, id, 0..1, |_| int(2)).expect("parent write");
    assert!(
        matches!(fork.list_get(id, 0).expect("fork"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
    write_list(&mut fork, id, 0..1, |_| int(3)).expect("fork write");
    assert!(
        matches!(parent.list_get(id, 0).expect("parent"), Some(PageValue::Int(IntRef::Inline(n))) if n == 2.into())
    );
}
#[test]
fn a_frozen_capture_ignores_later_live_writes() {
    let (mut heap, _) = fresh();
    let live = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("live");
    let frozen = heap.freeze(&PageValue::List(live), true).expect("freeze");
    write_list(&mut heap, live, 0..1, |_| int(2)).expect("live write");
    let capture = list(heap.instantiate(&frozen).expect("capture"));
    assert!(
        matches!(heap.list_get(capture, 0).expect("get"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
    write_list(&mut heap, capture, 0..1, |_| int(3)).expect("capture write");
    let other = list(heap.instantiate(&frozen).expect("other capture"));
    assert!(
        matches!(heap.list_get(other, 0).expect("get"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[test]
fn a_swept_child_never_resurrects_while_its_family_stays_live() {
    let (mut heap, pages) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let live = heap
        .allocate_object(ObjectInit::List(vec![PageValue::List(child)]))
        .expect("live");
    let frozen = heap.freeze(&PageValue::List(live), true).expect("freeze");
    let root = list(heap.instantiate(&frozen).expect("deliver"));
    let child = list(
        heap.list_get(root, 0)
            .expect("child read")
            .expect("child present"),
    );
    assert_eq!(root.family, child.family);
    assert_ne!(root.ordinal, child.ordinal);
    write_list(&mut heap, child, 0..1, |_| int(2)).expect("override child");
    write_list(&mut heap, root, 0..1, |_| int(3)).expect("detach child");
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
        matches!(reopened.list_get(root, 0).expect("retained override"), Some(PageValue::Int(IntRef::Inline(n))) if n == 3.into())
    );
    assert_eq!(reopened.kind(child).expect("dead child membership"), None);
}
/// S8, ADR 0145 §9: a family's membership is exactly its live ordinals,
/// whatever order its members are swept in. A swept ordinal (first, last,
/// interior, adjacent either way, swept twice) reads as absent and never
/// falls back to its frozen body, also after a cold reopen; every other
/// ordinal reads; the census enumerates the live ones in order across its
/// batches; and the family leaves the heap with its last member.
#[test]
fn a_family_holds_exactly_its_live_members_whatever_the_sweep_order() {
    let (mut heap, pages) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![PageValue::List(child); 150]))
        .expect("source");
    let frozen = heap.freeze(&PageValue::List(source), true).expect("freeze");
    let family = list(heap.instantiate(&frozen).expect("deliver")).family;
    let member = |ordinal| EntityId { family, ordinal };
    let sweep = |heap: &mut PagedHeap, ordinal| {
        heap.sweep(&SweepSet {
            entities: vec![member(ordinal)],
            ended_task_results: Vec::new(),
        })
        .expect("sweep");
    };
    let swept = [0, 150, 70, 72, 71, 64, 65, 70];
    for ordinal in swept {
        sweep(&mut heap, ordinal);
    }
    let live: Vec<_> = (0..151)
        .filter(|ordinal| !swept.contains(ordinal))
        .map(member)
        .collect();
    let check = |heap: &mut PagedHeap| {
        for ordinal in 0..151 {
            let expected = (!swept.contains(&ordinal)).then_some(ObjectKind::List);
            assert_eq!(
                heap.kind(member(ordinal)).expect("kind"),
                expected,
                "{ordinal}"
            );
        }
        let mut enumerated = Vec::new();
        let mut after = None;
        loop {
            let batch = heap.census_after(after, 64).expect("census");
            let Some(last) = batch.last().copied() else {
                break;
            };
            enumerated.extend(batch.into_iter().filter(|id| id.family == family));
            after = Some(last);
        }
        assert_eq!(enumerated, live);
    };
    check(&mut heap);
    seal(&mut heap);
    let mut reopened = PagedHeap::open_header(
        heap.sealed_root().expect("header"),
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .expect("open");
    reopened.set_family_base(Box::new(FrozenBase::default()));
    check(&mut reopened);
    for id in &live {
        sweep(&mut reopened, id.ordinal);
    }
    for ordinal in 0..151 {
        assert_eq!(reopened.kind(member(ordinal)).expect("kind"), None);
    }
    assert!(
        reopened
            .census_after(None, usize::MAX)
            .expect("census")
            .iter()
            .all(|id| id.family != family)
    );
}
#[test]
fn a_lazy_result_length_read_instantiates_nothing() {
    let (mut heap, _) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![PageValue::List(child); 100_000]))
        .expect("source");
    let frozen = heap.freeze(&PageValue::List(source), true).expect("freeze");
    let result = list(heap.instantiate(&frozen).expect("deliver"));
    assert_eq!(heap.len(result).expect("length"), 100_000);
    let first = list(heap.list_get(result, 0).expect("first").expect("present"));
    let second = list(heap.list_get(result, 1).expect("second").expect("present"));
    assert_ne!(first, second);
    write_list(&mut heap, first, 0..1, |_| int(7)).expect("edit first");
    assert!(
        matches!(heap.list_get(second, 0).expect("second unchanged"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
    );
}
#[test]
fn repeated_delivery_loads_only_touched_index_paths() {
    let (mut heap, _) = fresh();
    let child = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("child");
    let source = heap
        .allocate_object(ObjectInit::List(vec![PageValue::List(child); 100_000]))
        .expect("source");
    let frozen = heap.freeze(&PageValue::List(source), true).expect("freeze");
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
            callable: lash_kernel_value::names::CallableId {
                link: lash_kernel_value::LinkId::from_bytes([1; 32]),
                function: lash_kernel_value::names::FunctionId::from_bytes([2; 32]),
            },
            site: SiteRef::new(
                &(lash_kernel_value::sites::Site {
                    unit: lash_kernel_value::sites::Unit::Main,
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
            PageValue::List(a),
            PageValue::Closure(closure),
        ]))
        .expect("b");
    write_list(&mut heap, a, 0..1, |_| PageValue::List(b)).expect("cycle");
    let root = bindings(
        &mut heap,
        (0..100).map(|i| binding(&format!("alias{i}"), PageValue::List(a))),
    );
    heap.evict().expect("evict");
    let result = heap.classify_carry(&root).expect("classify");
    assert_eq!(result.dropped.len(), 100);
    assert_eq!(result.carried.len(), 0);
}

struct LawCensus(crate::paged::AccountingCertificate);
impl crate::paged::AllocationCensus for LawCensus {
    fn restore(c: crate::paged::AccountingCertificate) -> Result<Self, crate::paged::CensusFault> {
        Ok(Self(c))
    }
    fn certificate(&self) -> crate::paged::AccountingCertificate {
        self.0
    }
    fn added(&mut self, entities: u64) -> Result<(), crate::paged::CensusFault> {
        self.0.entities = self
            .0
            .entities
            .checked_add(entities)
            .expect("test entity count");
        Ok(())
    }
    fn record_capacity(&mut self, capacity: u64) {
        self.0.capacity = capacity;
    }
    fn refine(
        &mut self,
        _: &mut dyn crate::paged::CensusGraph,
        _: &crate::paged::TransientRoots<'_>,
    ) -> Result<(), crate::paged::CensusFault> {
        unreachable!("lifecycle law census never sweeps")
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
    let PageValue::Map(map) = heap.instantiate(&frozen).expect("instantiate") else {
        panic!("map")
    };
    assert_eq!(heap.len(map).expect("length"), 1);
    let child = list(heap.map_get(map, &int(1)).expect("get").expect("present"));
    assert!(
        matches!(heap.list_get(child, 0).expect("member"), Some(PageValue::Int(IntRef::Inline(n))) if n == 2.into())
    );
}

#[test]
fn carry_classification_respects_direct_and_immutable_task_roots() {
    let (mut heap, _) = fresh();
    let task = PageValue::Task(lash_kernel_doc::TaskId(99));
    let tuple = heap.tuple(vec![task.clone()]).expect("tuple");
    let root = bindings(
        &mut heap,
        [
            binding("task", task),
            binding("tuple", PageValue::Tuple(tuple)),
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
    write_list(&mut heap, id, 0..1, |_| PageValue::List(id)).expect("cycle");
    for value in [
        PageValue::List(id),
        PageValue::Task(lash_kernel_doc::TaskId(99)),
        PageValue::Ref(Identity::Object(id)),
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
    fn child(heap: &PagedHeap, value: PageValue) -> EntityId {
        let PageValue::Error(error) = value else {
            panic!("outer error")
        };
        let PageValue::Tuple(tuple) = heap.error_data(&error).expect("outer data") else {
            panic!("tuple")
        };
        let PageValue::Error(error) = heap
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
    write_list(&mut heap, a, 0..1, |_| int(2)).expect("edit");
    seal(&mut heap);
    let mut reopened = PagedHeap::open_header(
        heap.sealed_root().expect("header"),
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .expect("open");
    reopened.set_family_base(Box::new(FrozenBase::default()));
    assert!(
        matches!(reopened.list_get(a, 0).expect("first"), Some(PageValue::Int(IntRef::Inline(n))) if n == 2.into())
    );
    assert!(
        matches!(reopened.list_get(b, 0).expect("second"), Some(PageValue::Int(IntRef::Inline(n))) if n == 1.into())
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

/// §D incremental binding work: execution edits slots, and only a
/// projection writes the session tree. Repeated reads and writes stay hot.
#[test]
fn session_slots_project_only_their_final_values() {
    let (mut heap, pages) = fresh();
    let name = Name::new("counter");
    let key = crate::paged::metadata::metadata_key(&name);
    let initial = heap.session_root().version();
    for value in 0..100 {
        let record = record(&heap, &name, int(value));
        heap.write_session_binding(&key, |_, _| Ok(Some(record)))
            .unwrap();
        assert!(
            heap.equal(&heap.session_binding(&name).unwrap().unwrap(), &int(value))
                .unwrap()
        );
    }
    assert!(
        initial.matches(heap.session_root()),
        "execution must not edit the projected tree"
    );
    assert_eq!(pages.borrow().seals, 0, "no pages before projection");
    heap.seal_session_root(&mut crate::paged::tree::SealWork::new(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    }))
    .unwrap();
    let projected = heap
        .session_root()
        .get(&key, heap.pages())
        .unwrap()
        .unwrap();
    let BoundRecord::Value(value) = projected.value else {
        panic!("value binding")
    };
    assert!(heap.equal(&value, &int(99)).unwrap());
    assert_eq!(heap.session_root().len(), 1);
}

/// §D incremental carry: even a million untouched names remain cold.
#[test]
fn a_no_op_carry_over_a_million_names_reads_no_pages() {
    let (mut heap, pages) = fresh();
    let root = bindings(
        &mut heap,
        (0..1_000_000).map(|i| binding(&format!("name{i:07}"), int(i))),
    );
    let session = heap
        .carry(CarryRoots {
            bindings: root,
            functions: empty(),
        })
        .unwrap();
    let mut cold = PagedHeap::open_header(
        session.heap,
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .unwrap();
    cold.set_census(Box::new(LawCensus(cold.certificate())))
        .unwrap();
    cold.adopt(&session).unwrap();
    let before = pages.borrow().reads;
    let carried = cold
        .carry(CarryRoots {
            bindings: session.bindings,
            functions: session.functions,
        })
        .unwrap();
    assert_eq!(pages.borrow().reads, before);
    assert_eq!(carried, session);
    assert_eq!(carried.bindings.len(), 1_000_000);
}

/// K-SES-003 / K-MACH-008: the census sees current slot values, including
/// replacements, before any session tree has been projected.
#[test]
fn session_slot_roots_and_task_edges_follow_replacements() {
    let (mut heap, _) = fresh();
    let name = Name::new("live");
    let key = crate::paged::metadata::metadata_key(&name);
    let id = heap
        .allocate_object(ObjectInit::List(vec![int(7)]))
        .unwrap();
    let value = record(&heap, &name, PageValue::List(id));
    heap.write_session_binding(&key, |_, _| Ok(Some(value)))
        .unwrap();
    heap.collect().unwrap();
    assert_eq!(heap.kind(id).unwrap(), Some(ObjectKind::List));
    assert!(heap.session_root().is_empty());
    let value = record(&heap, &name, PageValue::Task(lash_kernel_doc::TaskId::MAIN));
    heap.write_session_binding(&key, |_, _| Ok(Some(value)))
        .unwrap();
    assert_eq!(heap.data_only_certificate().task_edges, 1);
    let site = SiteRef::new(
        &lash_kernel_value::sites::Site::new(lash_kernel_value::sites::Unit::Main, [0]),
        heap.pages(),
    )
    .unwrap();
    let closure = heap
        .allocate_object(ObjectInit::Closure {
            callable: lash_kernel_value::names::CallableId {
                link: lash_kernel_value::LinkId::from_bytes([1; 32]),
                function: lash_kernel_value::names::FunctionId::from_bytes([2; 32]),
            },
            site,
            captures: Vec::new(),
        })
        .unwrap();
    let value = record(&heap, &name, PageValue::Closure(closure));
    heap.write_session_binding(&key, |_, _| Ok(Some(value)))
        .unwrap();
    assert_eq!(heap.data_only_certificate().task_edges, 0);
    heap.collect().unwrap();
    assert_eq!(heap.kind(id).unwrap(), None);
    assert_eq!(heap.kind(closure).unwrap(), Some(ObjectKind::Closure));
    let value = record(&heap, &name, int(9));
    heap.write_session_binding(&key, |_, _| Ok(Some(value)))
        .unwrap();
    heap.collect().unwrap();
    assert_eq!(heap.data_only_certificate().closure_entities, 0);
    heap.seal_session_root(&mut crate::paged::tree::SealWork::new(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    }))
    .unwrap();
    heap.collect().unwrap();
    let root = heap.session_root().keyed_root().unwrap();
    assert!(heap.classify_carry(&root).unwrap().dropped.is_empty());
}

/// K-SES-004: saved closure environments remain distinct roots even when
/// their code is identical and no current binding names them.
#[test]
fn saved_environments_keep_identity_and_shared_cells_across_carry() {
    use std::collections::{BTreeMap, BTreeSet};
    use {
        lash_kernel_value::CallableId, lash_kernel_value::FunctionId, lash_kernel_value::LinkId,
        lash_kernel_value::sites::Site, lash_kernel_value::sites::Unit,
    };
    let (mut heap, pages) = fresh();
    let callable = CallableId {
        link: LinkId::from_bytes([1; 32]),
        function: FunctionId::from_bytes([2; 32]),
    };
    let site = SiteRef::new(&Site::new(Unit::Main, [0]), heap.pages()).unwrap();
    let captured = NameRef::new(&Name::new("x"), heap.pages()).unwrap();
    let first_cell = heap.allocate_object(ObjectInit::Variable(int(1))).unwrap();
    let second_cell = heap.allocate_object(ObjectInit::Variable(int(2))).unwrap();
    let mut make = |cell| {
        heap.allocate_object(ObjectInit::Closure {
            callable,
            site: site.clone(),
            captures: vec![(captured.clone(), cell)],
        })
        .unwrap()
    };
    let first = make(first_cell);
    let second = make(second_cell);
    let main_name = Name::new("main_global");
    let main_cell = heap
        .bind_closure_capture(first, &main_name, int(3))
        .unwrap();
    assert_eq!(
        heap.bind_closure_capture(first, &main_name, int(99))
            .unwrap(),
        main_cell
    );
    let named = BTreeSet::from([first, second]);
    let (frozen, references) = heap
        .freeze_named(&PageValue::Closure(first), &named)
        .unwrap();
    assert_eq!(references, BTreeSet::from([first]));
    assert!(matches!(heap.instantiate(&frozen).unwrap(), PageValue::Closure(id) if id == first));
    seal(&mut heap);
    let anchor = heap.sealed_root().unwrap();
    let mut functions = crate::paged::tree::Tree::new(false);
    for (name, environment) in [("first", first), ("second", second)] {
        functions
            .write(
                name.as_bytes().to_vec(),
                FunctionRecord {
                    environment,
                    callable,
                    name: NameRef::new(&Name::new(name), heap.pages()).unwrap(),
                    meta: PageValue::Absent,
                    closure: anchor,
                    captures: BTreeMap::new(),
                    before: Vec::new(),
                    after: Vec::new(),
                },
                heap.pages(),
            )
            .unwrap();
    }
    functions
        .seal(
            heap.pages(),
            &mut crate::paged::tree::SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .unwrap();
    let functions = crate::paged::heap_impl::keyed(&functions).unwrap();
    let session = heap
        .carry(CarryRoots {
            bindings: empty(),
            functions,
        })
        .unwrap();
    let mut cold = PagedHeap::open_header(
        session.heap,
        Box::new(Source(Rc::clone(&pages))),
        EditEpoch(2),
    )
    .unwrap();
    cold.adopt(&session).unwrap();
    cold.set_census(heap.census.take().unwrap()).unwrap();
    cold.collect().unwrap();
    assert!(
        cold.equal(&cold.variable_get(main_cell).unwrap(), &int(3))
            .unwrap()
    );
    assert_ne!(first, second);
    assert_eq!(cold.closure(first).unwrap().callable, callable);
    assert_eq!(cold.closure(second).unwrap().callable, callable);
    assert!(
        cold.equal(&cold.variable_get(first_cell).unwrap(), &int(1))
            .unwrap()
    );
    assert!(
        cold.equal(&cold.variable_get(second_cell).unwrap(), &int(2))
            .unwrap()
    );
}
