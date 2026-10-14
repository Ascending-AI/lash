//! S1/S2 counted-work and semantic laws over an in-process page port.
use super::super::census_impl::Census;
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
    pub payload_reads: u64,
    pub seals: u64,
    pub bytes: u64,
}
pub(super) struct Source(pub Rc<RefCell<Pages>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        let mut store = self.0.borrow_mut();
        store.reads += pages.len() as u64;
        store.payload_reads += pages
            .iter()
            .filter(|page| {
                store.pages.get(&page.id).is_some_and(|answer| {
                    matches!(
                        answer.kind,
                        PageKind::PAYLOAD_LEAF | PageKind::PAYLOAD_INDEX
                    )
                })
            })
            .count() as u64;
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
        Census::restore(heap.certificate(), LogicalBytes(1 << 30)).expect("census"),
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
        Census::restore(heap.certificate(), LogicalBytes(1 << 30)).expect("census"),
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
        cost: LogicalBytes(38),
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
    let tree = tree::Tree::<Value>::open(Some(page), 1, LogicalBytes(54), true);
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

#[test]
fn an_allocation_consumes_its_native_reservation() {
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(48)).expect("census"),
    ))
    .expect("install");
    heap.reserve(3, 0).expect("reserve 48");
    let list = heap
        .allocate_object(ObjectInit::List(vec![Value::Null]))
        .expect("consume reservation");
    assert!(matches!(
        heap.list_get(list, 0).expect("get"),
        Some(Value::Null)
    ));
    assert_eq!(heap.certificate().upper, LogicalBytes(48));
    heap.release_statement_roots().expect("release");
    assert_eq!(heap.certificate().upper, LogicalBytes(48));
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(16)).expect("census"),
    ))
    .expect("install");
    heap.reserve(1, 0).expect("reserve value");
    heap.set_execution_roots(
        heap.tasks.clone(),
        heap.joins.clone(),
        &[Value::Null],
        &[],
        &[],
    )
    .expect("consume value reservation");
    assert_eq!(heap.certificate().upper, LogicalBytes(16));
    heap.release_statement_roots().expect("release");
    assert_eq!(heap.certificate().upper, LogicalBytes(0));
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(32)).expect("census"),
    ))
    .expect("install");
    heap.reserve(2, 0).expect("reserve family");
    heap.register_family(frozen(1, true))
        .expect("consume family reservation");
    heap.release_statement_roots().expect("release");
    assert_eq!(heap.certificate().upper, LogicalBytes(32));
}

#[test]
fn replacing_a_reference_checks_the_proposed_reachable_graph() {
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(264)).expect("census"),
    ))
    .expect("install");
    let garbage = heap
        .allocate_object(ObjectInit::List(vec![Value::Null; 10]))
        .expect("large list");
    let root = heap
        .allocate_object(ObjectInit::List(vec![Value::List(garbage)]))
        .expect("root");
    let mut bindings = tree::Tree::new(false);
    bindings
        .write(
            b"root".to_vec(),
            BindingRecord {
                name: "root".into(),
                declared: lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, []),
                value: BoundRecord::Value(Value::List(root)),
            },
            &heap.access,
        )
        .expect("binding");
    heap.set_session_root(bindings).expect("session");
    let text = heap.text(&"x".repeat(200)).expect("text");
    heap.list_write(root, 0, Value::Text(text))
        .expect("proposed graph fits exactly");
    assert_eq!(heap.certificate().upper, LogicalBytes(264));
    assert_eq!(heap.certificate().entities, 1);
    assert!(heap.entity_cost(garbage).expect("swept").is_none());
}

#[test]
fn statement_holding_cost_is_counted_before_fast_reservation() {
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(80)).expect("census"),
    ))
    .expect("install");
    let list = heap
        .allocate_object(ObjectInit::List(vec![Value::Null]))
        .expect("list");
    heap.set_execution_roots(
        heap.tasks.clone(),
        heap.joins.clone(),
        &[Value::List(list)],
        &[],
        &[],
    )
    .expect("register");
    assert_eq!(heap.certificate().upper, LogicalBytes(64));
    assert!(matches!(heap.reserve(2, 0), Err(NativeError::Memory)));
    heap.release_statement_roots().expect("release");
    assert_eq!(heap.certificate().upper, LogicalBytes(48));
}

#[test]
fn an_edge_visitor_fault_stops_before_the_next_leaf() {
    let (mut heap, pages) = fresh();
    let tuple = heap
        .tuple(
            (0..65)
                .map(|ordinal| {
                    Value::List(EntityId {
                        family: FamilyId::PRIMARY,
                        ordinal,
                    })
                })
                .collect(),
        )
        .expect("tuple");
    heap.access.clear();
    pages.borrow_mut().reads = 0;
    let fault = HeapFault::Malformed("visitor stopped".into());
    let mut calls = 0;
    let result = heap.value_edges(&Value::Tuple(tuple), &mut |_| {
        calls += 1;
        Err(fault.clone())
    });
    assert_eq!(result, Err(fault));
    assert_eq!(calls, 1);
    assert_eq!(pages.borrow().reads, 2, "root and first leaf only");
}

#[test]
fn a_forged_membership_aggregate_is_a_corrupt_page() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter, decode_body};
    let (heap, pages) = fresh();
    let mut actual = tree::Tree::new(false);
    for i in 0..32 {
        actual
            .write(
                directory::ordinal_key(i),
                MembershipRun {
                    start: i,
                    len: 1,
                    live: true,
                },
                &heap.access,
            )
            .unwrap();
    }
    actual
        .seal_nested(
            &heap.access,
            &mut tree::SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .unwrap();
    let page = actual.page().expect("above inline threshold");
    let body_bytes = pages.borrow().pages[&page.id].tokens.len() as u64;
    let mut out = TokenWriter::new();
    out.seq(7);
    false.encode(&mut out);
    Option::<FamilyId>::None.encode(&mut out);
    out.sum(2, 2);
    page.encode(&mut out);
    body_bytes.encode(&mut out);
    32u64.encode(&mut out);
    0u64.encode(&mut out);
    0u64.encode(&mut out);
    31u64.encode(&mut out);
    let runs = decode_body::<tree::Tree<MembershipRun>>(&out.into_bytes()).expect("descriptor");
    assert!(matches!(
        runs.rank_get(0, &heap.access),
        Err(HeapFault::Corrupt { .. })
    ));
}

#[test]
fn population_summaries_follow_seeded_directory_edits() {
    use tree::Cell;
    let (mut heap, _) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(vec![Value::Null]))
        .expect("list");
    let variable = heap
        .allocate_object(ObjectInit::Variable(Value::Null))
        .expect("variable");
    let record = heap
        .allocate_object(ObjectInit::Record(vec![]))
        .expect("record");
    let map = heap.allocate_object(ObjectInit::Map(vec![])).expect("map");
    let closure = heap
        .allocate_object(ObjectInit::Closure {
            site: lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, []),
            captures: vec![],
        })
        .expect("closure");
    let mut seed = 417u64;
    for step in 0..64 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let value = if seed & 8 == 0 {
            Value::Null
        } else {
            Value::Task(lash_kernel_doc::TaskId(step))
        };
        match seed % 6 {
            0 => heap.list_write(list, 0, value).expect("list write"),
            1 => heap
                .variable_write(variable, value)
                .expect("variable write"),
            2 => heap
                .record_write(record, FieldName::Static("task"), value)
                .expect("record write"),
            3 => heap
                .record_remove(record, FieldName::Static("task"))
                .expect("record remove"),
            4 => heap.map_write(map, Value::Null, value).expect("map write"),
            _ => heap.table_remove(map, &Value::Null).expect("map remove"),
        }
        let mut closure_entities = 0;
        let mut task_edges = 0;
        heap.directory
            .visit(&heap.access, &mut |_, head| {
                closure_entities += (head.kind() == ObjectKind::Closure) as u64;
                task_edges += head.task_edges();
                true
            })
            .expect("recount");
        assert_eq!(
            heap.data_only_certificate(),
            DataOnlyCertificate {
                closure_entities,
                task_edges
            }
        );
    }
    heap.sweep(&SweepSet {
        entities: vec![list, variable, record, map, closure],
        ended_task_results: vec![],
    })
    .expect("sweep");
    assert_eq!(
        heap.data_only_certificate(),
        DataOnlyCertificate {
            closure_entities: 0,
            task_edges: 0
        }
    );
}

/// K-EFF-002: reading outbound integer digits never reserves guest memory.
#[test]
fn outbound_integer_digits_are_read_only_and_keep_their_sign() {
    let (mut heap, _) = fresh();
    let integer =
        lash_kernel_doc::Integer::parse(&format!("-1{}", "0".repeat(1500))).expect("integer");
    let value = Value::Int(heap.int(integer.clone()).expect("paged integer"));
    let Value::Int(IntRef::Paged { summary, .. }) = &value else {
        panic!("paged");
    };
    assert!(summary.negative);
    let before = heap.certificate();
    let mut digits = Vec::new();
    heap.payload_visit(&value, 0..summary.bits.div_ceil(8), &mut |chunk| {
        let Chunk::Bytes(bytes) = chunk else {
            panic!("integer magnitude bytes");
        };
        digits.extend_from_slice(bytes);
        ControlFlow::Continue(())
    })
    .expect("outbound digits");
    assert_eq!(heap.certificate(), before);
    assert_eq!(
        lash_kernel_doc::heap_values::integer_from_digits(summary.negative, &digits),
        integer
    );
}

/// S1/S4: frame bindings contain bounded descriptors; keeping a paged
/// error's immutable data never reads or rewrites its unchanged payload.
#[test]
fn a_long_native_error_in_a_frame_keeps_paged_data_without_reads() {
    use super::super::tree::{Cell, SealWork, Tree};
    use lash_kernel_doc::graph_tokens::encode_body;
    use lash_kernel_doc::{Name, Site, Unit};
    let (mut heap, pages) = fresh();
    let mut error = ErrorRef::new(&"kind".repeat(100_000), &"message".repeat(100_000));
    error.data = Payload::Resident(Arc::new(Value::text("d".repeat(256))));
    let value = heap
        .canonical_value(Value::Error(Arc::new(error)))
        .expect("canonical error");
    let Value::Error(error) = &value else {
        panic!("error");
    };
    assert!(matches!(error.data, Payload::Paged(_)));
    let site = Site::new(Unit::Main, vec![0]);
    let mut frame = FrameRecord::fresh(site.clone());
    frame
        .bindings
        .write(
            vec![0],
            BindingRecord {
                name: Name::new("error"),
                declared: site,
                value: BoundRecord::Value(value.clone()),
            },
            &heap.access,
        )
        .expect("binding");
    frame
        .prepare(
            &heap.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .expect("frame pages");
    let mut frames = Tree::new(true);
    frames.write(vec![0], frame, &heap.access).expect("frame");
    frames
        .seal(
            &heap.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )
        .expect("frames");
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|page| page.tokens.len() + 32 * page.children.len() + 32 <= 65_536)
    );
    heap.access.clear();
    pages.borrow_mut().reads = 0;
    let seals = pages.borrow().seals;
    let kept = heap.canonical_value(value.clone()).expect("already paged");
    assert_eq!(pages.borrow().reads, 0);
    assert_eq!(pages.borrow().seals, seals);
    assert_eq!(encode_body(&kept), encode_body(&value));
}

/// K-BND-003 / S7: a retained ended-task result is a holding even though
/// precise tracing visits it only through a reachable task handle.
#[test]
fn an_ended_task_result_counts_toward_fast_reservation() {
    use super::super::tree::Tree;
    use lash_kernel_doc::{Name, Site, TaskId, TaskIdentity, Unit};
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(100)).expect("census"),
    ))
    .expect("census");
    let list = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("49-byte list");
    let mut session = Tree::new(false);
    session
        .write(
            vec![0],
            BindingRecord {
                name: Name::new("task"),
                declared: Site::new(Unit::Main, [0]),
                value: BoundRecord::Value(Value::Task(TaskId(1))),
            },
            &heap.access,
        )
        .expect("task handle");
    heap.set_session_root(session)
        .expect("16-byte handle holding");
    let mut tasks = Tree::new(false);
    tasks
        .write(
            super::super::directory::ordinal_key(1),
            TaskRecord {
                identity: TaskIdentity::Main,
                state: TaskStateRecord::Ended(EndedTask::Returned(Value::List(list))),
                result_retained: true,
                joiners: Tree::new(true),
                observed: false,
                passed: false,
                failed: false,
                occurrences: Tree::new(false),
                frames: Tree::new(true),
            },
            &heap.access,
        )
        .expect("ended task");
    heap.set_execution_roots(tasks, heap.joins.clone(), &[], &[], &[])
        .expect("16-byte result holding");
    assert!(
        matches!(
            heap.allocate_object(ObjectInit::List(vec![])),
            Err(HeapWriteError::Memory)
        ),
        "49 + 16 + 16 live bytes leave less than a 32-byte object under a 100-byte bound"
    );
}

/// S1/S7: a newly created entity is a zero-cost tracing seed until its
/// enclosing expression installs the completed value in a holding location.
#[test]
fn a_fresh_mid_expression_entity_survives_the_next_allocation_pressure() {
    let (mut heap, _) = fresh();
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(96)).unwrap(),
    ))
    .unwrap();
    let garbage = heap.allocate_object(ObjectInit::List(vec![])).unwrap();
    let fresh = heap.allocate_object(ObjectInit::List(vec![])).unwrap();
    heap.set_execution_roots(heap.tasks.clone(), heap.joins.clone(), &[], &[], &[fresh])
        .unwrap();
    let next = heap
        .allocate_object(ObjectInit::List(vec![Value::Null]))
        .unwrap();
    assert!(heap.certificate().refinements > 0);
    assert_eq!(heap.len(fresh).unwrap(), 0);
    assert_eq!(heap.len(next).unwrap(), 1);
    assert!(heap.kind(garbage).unwrap().is_none());
    assert_eq!(heap.certificate().entities, 2);
}

/// D-IMM/D-KEY: an immutable descriptor retains its computed normal form.
/// Payload-page eviction does not force repeated normalization of a live key.
#[test]
fn a_thousand_lookups_normalize_a_resident_paged_key_at_most_once() {
    let (mut heap, pages) = fresh();
    let mut key = text(&mut heap, &"k".repeat(1 << 20));
    let map = heap
        .allocate_object(ObjectInit::Map(vec![(key.clone(), int(7))]))
        .unwrap();
    // Model a newly decoded descriptor with an empty runtime key cache.
    if let Value::Text(text) = &mut key {
        text.normal = Default::default();
    }
    let before = pages.borrow().payload_reads;
    for _ in 0..1000 {
        heap.access.clear();
        assert!(is_int(heap.map_get(map, &key).unwrap(), 7));
    }
    let reads = pages.borrow().payload_reads - before;
    assert!(
        reads > 0 && reads <= 270,
        "one canonical 1 MiB tree read, got {reads}"
    );
}

/// D-COW: publishing a working tree version gives later edits a private
/// path, even when neither version has been sealed yet.
#[test]
fn a_shared_dirty_directory_keeps_its_prior_version() {
    let (mut heap, _) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .unwrap();
    let prior = heap.directory.clone();
    heap.list_write(list, 0, int(2)).unwrap();
    let head = prior
        .get(&super::super::directory::entity_key(list), &heap.access)
        .unwrap()
        .unwrap();
    let super::super::directory::Body::List { rows, .. } = head.body else {
        panic!("small list");
    };
    assert!(is_int(
        rows.rank_get(0, &heap.access).unwrap().map(|(_, v)| v),
        1
    ));
    assert!(is_int(heap.list_get(list, 0).unwrap(), 2));
}

/// D-CAP: tiny maps use the same bounded inline tree as every collection.
#[test]
fn a_small_map_needs_no_collection_pages() {
    let (mut heap, pages) = fresh();
    let map = heap
        .allocate_object(ObjectInit::Map(vec![(int(1), int(2))]))
        .unwrap();
    let before = pages.borrow().seals;
    let mut heap = reopen(&mut heap, &pages);
    assert_eq!(
        pages.borrow().seals - before,
        2,
        "directory and heap header only"
    );
    assert!(is_int(heap.map_get(map, &int(1)).unwrap(), 2));
    heap.map_write(map, int(1), int(3)).unwrap();
    let before = pages.borrow().seals;
    let heap = reopen(&mut heap, &pages);
    assert_eq!(pages.borrow().seals - before, 2);
    assert!(is_int(heap.map_get(map, &int(1)).unwrap(), 3));
}

/// S4 inline amendment: below-threshold nested roots have one grammar,
/// including after a larger indexed tree shrinks back to a tiny leaf.
#[test]
fn nested_trees_choose_one_form_across_growth_and_shrink() {
    use super::super::directory::{Ordinal, ordinal_key};
    use super::super::tree::{SealWork, Tree};
    use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
    let (heap, _) = fresh();
    let budget = || {
        SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
    };
    let mut tree = Tree::new(false);
    tree.write(ordinal_key(0), Ordinal(7), &heap.access)
        .unwrap();
    tree.seal_nested(&heap.access, &mut budget()).unwrap();
    let small = encode_body(&tree);
    assert!(tree.page().is_none());
    let mut tree: Tree<Ordinal> = decode_body(&small).unwrap();
    for i in 1..200 {
        tree.write(ordinal_key(i), Ordinal(i), &heap.access)
            .unwrap();
    }
    tree.seal_nested(&heap.access, &mut budget()).unwrap();
    assert!(tree.page().is_some());
    let mut decoded: Tree<Ordinal> = decode_body(&encode_body(&tree)).unwrap();
    let mut children = Vec::new();
    tree.children(&mut children);
    decoded.grants(&children).unwrap();
    for i in 1..200 {
        decoded.remove(&ordinal_key(i), &heap.access).unwrap();
    }
    decoded.seal_nested(&heap.access, &mut budget()).unwrap();
    assert!(decoded.page().is_none());
    assert_eq!(encode_body(&decoded), small);
}
