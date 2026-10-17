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
    pub seal_fault: Option<HeapFault>,
}
pub(super) struct Source(pub Rc<RefCell<Pages>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
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
        Ok(pages
            .iter()
            .map(|p| {
                store
                    .pages
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        if let Some(fault) = self.0.borrow().seal_fault.clone() {
            return Err(fault);
        }
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
pub(super) fn reopen(heap: &mut PagedHeap, pages: &Rc<RefCell<Pages>>) -> PagedHeap {
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

fn int(v: i64) -> PageValue {
    PageValue::Int(IntRef::Inline(v.into()))
}
fn is_int(value: Option<PageValue>, expected: i64) -> bool {
    matches!(value, Some(PageValue::Int(IntRef::Inline(i))) if i == expected.into())
}
fn text(heap: &mut PagedHeap, t: &str) -> PageValue {
    PageValue::Text(heap.text(t).expect("text"))
}

#[test]
fn aliases_see_one_mutation_across_seal_and_open() {
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .expect("list");
    let aliases = heap
        .allocate_object(ObjectInit::List(vec![
            PageValue::List(list),
            PageValue::List(list),
        ]))
        .expect("aliases");
    heap.list_write(list, 0, int(7)).expect("write");
    let heap = reopen(&mut heap, &pages);
    for i in 0..2 {
        let Some(PageValue::List(id)) = heap.list_get(aliases, i).expect("alias") else {
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
    heap.list_write(list, 0, PageValue::List(list))
        .expect("self edge");
    let heap = reopen(&mut heap, &pages);
    assert!(
        matches!(heap.list_get(list, 0).expect("cycle"), Some(PageValue::List(id)) if id == list)
    );
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
    heap.map_write(
        map,
        PageValue::Float(lash_kernel_doc::Float::new(1.0)),
        int(11),
    )
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
        if let Element::Field { name, .. } = e
            && let Payload::Resident(s) = &name.body
        {
            fields.push(s.to_string());
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

include!("heap_boundaries.rs");

#[test]
fn relative_references_in_error_data_inherit_the_page_family() {
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter};
    let (heap, _) = fresh();
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
    let tree = tree::Tree::<PageValue>::open(Some(page), 1, LogicalBytes(54), true);
    assert!(
        tree.rank_get(0, &heap.access).is_err(),
        "relative references require context"
    );
    for family in [FamilyId(3), FamilyId(4)] {
        let Some((_, PageValue::Tuple(tuple))) = tree
            .clone()
            .in_family(family)
            .rank_get(0, &heap.access)
            .expect("tuple")
        else {
            panic!("tuple");
        };
        let Some(PageValue::Error(error)) = heap.tuple_get(&tuple, 0).expect("error") else {
            panic!("error");
        };
        assert!(
            matches!(heap.error_data(&error).expect("data"), PageValue::List(id) if id == EntityId { family, ordinal: 0 })
        );
    }
}

#[test]
fn errors_at_the_depth_limit_have_bounded_descendants() {
    let (mut heap, pages) = fresh();
    let kind = heap.text(&"k".repeat(256)).expect("kind");
    let message = heap.text(&"m".repeat(256)).expect("message");
    let mut value = PageValue::Null;
    for depth in 1..=128 {
        value = PageValue::Error(Arc::new(ErrorRef {
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
        let PageValue::Error(error) = value else {
            panic!("error at {depth}");
        };
        assert_eq!(error.depth, depth);
        value = heap.error_data(&error).expect("data");
    }
    assert!(matches!(value, PageValue::Null));
    assert!(
        pages
            .borrow()
            .pages
            .values()
            .all(|p| p.tokens.len() + 32 * p.children.len() + 32 <= 65_536)
    );
}

#[test]
fn an_edge_visitor_fault_stops_before_the_next_leaf() {
    let (mut heap, pages) = fresh();
    let tuple = heap
        .tuple(
            (0..65)
                .map(|ordinal| {
                    PageValue::List(EntityId {
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
    let result = heap.value_edges(&PageValue::Tuple(tuple), &mut |_| {
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
    out.sum(2, 3);
    page.encode(&mut out);
    body_bytes.encode(&mut out);
    true.encode(&mut out);
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

/// K-EFF-002: reading outbound integer digits never reserves guest memory.
#[test]
fn outbound_integer_digits_are_read_only_and_keep_their_sign() {
    let (heap, _) = fresh();
    let integer =
        lash_kernel_doc::Integer::parse(&format!("-1{}", "0".repeat(1500))).expect("integer");
    let value = PageValue::Int(heap.int(integer.clone()).expect("paged integer"));
    let PageValue::Int(IntRef::Paged { summary, .. }) = &value else {
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
    error.data = Payload::Resident(Arc::new(PageValue::text("d".repeat(256))));
    let value = heap
        .canonical_value(PageValue::Error(Arc::new(error)))
        .expect("canonical error");
    let PageValue::Error(error) = &value else {
        panic!("error");
    };
    assert!(matches!(error.data, Payload::Paged(_)));
    let site = Site::new(Unit::Main, vec![0]);
    let mut frame = FrameRecord::fresh(SiteRef::new(&(site.clone()), &heap.access).unwrap());
    frame
        .bindings
        .write(
            vec![0],
            BindingRecord {
                name: NameRef::new(&(Name::new("error")), &heap.access).unwrap(),
                declared: SiteRef::new(&(site), &heap.access).unwrap(),
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

/// D-COW: publishing a working tree version gives later edits a private
/// path, even when neither version has been sealed yet.
#[test]
fn a_shared_dirty_directory_keeps_its_prior_version() {
    let (mut heap, _) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(vec![int(1)]))
        .unwrap();
    heap.flush_working().unwrap();
    let prior = heap.directory.clone();
    heap.list_write(list, 0, int(2)).unwrap();
    heap.flush_working().unwrap();
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
    for i in 1..80 {
        tree.write(ordinal_key(i), Ordinal(i), &heap.access)
            .unwrap();
    }
    tree.seal_nested(&heap.access, &mut budget()).unwrap();
    assert!(tree.page().is_some());
    let mut decoded: Tree<Ordinal> = decode_body(&encode_body(&tree)).unwrap();
    let mut children = Vec::new();
    tree.children(&mut children);
    decoded.grants(&children).unwrap();
    for i in 1..80 {
        decoded.remove(&ordinal_key(i), &heap.access).unwrap();
    }
    decoded.seal_nested(&heap.access, &mut budget()).unwrap();
    assert!(decoded.page().is_none());
    assert_eq!(encode_body(&decoded), small);
}

/// S4 inline amendment: a reference inside an inline nested tree is a
/// reference of the page that holds it, so that page's envelope declares
/// it and a read of the page grants it.
#[test]
fn an_inline_nested_trees_references_are_its_holders_children() {
    use super::super::directory::ordinal_key;
    use super::super::tree::{SealWork, Tree};
    use lash_kernel_doc::graph_tokens::{ChildRef, TokenEvent, TokenReader, encode_body};
    let (heap, _) = fresh();
    let held = PageRef {
        id: PageId([9; 32]),
        grant: PageGrant::NONE,
    };
    let mut tree = Tree::new(false);
    tree.write(ordinal_key(0), held, &heap.access).unwrap();
    tree.seal_nested(
        &heap.access,
        &mut SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        }),
    )
    .unwrap();
    assert!(tree.page().is_none(), "the tree is inline");
    let body = encode_body(&tree);
    let mut input = TokenReader::new(&body);
    let mut declared = Vec::new();
    while !input.is_empty() {
        if let TokenEvent::Child(ChildRef(digest)) = input.next_event().unwrap() {
            declared.push(digest);
        }
    }
    assert_eq!(declared, vec![held.id.0]);
}

/// D-EQ: an unequal prefix decides before any later member page is read.
#[test]
fn unequal_prefixes_do_not_read_missing_later_members() {
    use lash_kernel_doc::graph_tokens::TokenReader;
    let (mut heap, pages) = fresh();
    let left = heap
        .allocate_object(ObjectInit::List((0..200).map(int).collect()))
        .unwrap();
    let right = heap
        .allocate_object(ObjectInit::List((0..200).map(|i| int(i + 1)).collect()))
        .unwrap();
    let heap = reopen(&mut heap, &pages);
    let removed = {
        let mut pages = pages.borrow_mut();
        let before = pages.pages.len();
        pages.pages.retain(|_, page| {
            if page.kind != PageKind::SEQUENCE_LEAF {
                return true;
            }
            let mut input = TokenReader::new(&page.tokens);
            input.seq().unwrap();
            input.expect_struct(2).unwrap();
            let first = u64::from_be_bytes(input.bytes().unwrap().try_into().unwrap());
            first == 0
        });
        before - pages.pages.len()
    };
    assert!(removed > 0, "the later leaves really are missing");
    heap.access.clear();
    assert_eq!(
        heap.equal(&PageValue::List(left), &PageValue::List(right)),
        Ok(false)
    );
}

/// D-CAP/D-SEALTIME: private pages spill at 32 MiB without publishing a
/// checkpoint, retaining old working versions and exact new contents.
#[test]
fn dirty_residency_spills_before_checkpoint_and_preserves_working_versions() {
    use super::super::directory::Body;
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(Vec::new()))
        .expect("list");
    for i in 0..10_000 {
        heap.list_write(list, i, int(i as i64))
            .expect("initial row");
    }
    let Body::List { rows: prior, .. } = heap.head(list).expect("head").expect("list exists").body
    else {
        panic!("list")
    };
    heap.list_write(list, 0, int(-1)).expect("new version");
    let mut peak = 0;
    for i in 10_000..250_000 {
        heap.list_write(list, i, int(i as i64)).expect("row");
        peak = peak.max(heap.access.dirty_bytes());
        assert!(
            heap.access.dirty_bytes() <= 32 << 20,
            "dirty resident bytes"
        );
    }
    assert!(pages.borrow().seals > 0, "stage before explicit seal");
    assert!(is_int(
        prior
            .rank_get(0, &heap.access)
            .expect("old working row")
            .map(|(_, v)| v),
        0
    ));
    assert!(is_int(heap.list_get(list, 0).expect("new working row"), -1));
    let heap = reopen(&mut heap, &pages);
    assert!(is_int(
        heap.list_get(list, 249_999).expect("last restored row"),
        249_999
    ));
    assert!(is_int(
        heap.list_get(list, 0).expect("first restored row"),
        -1
    ));
    println!("dirty_limit=33554432 peak_accounted_bytes={peak}");
}

/// D-SEALTIME: a stage quota failure remains an infrastructure cause, and
/// the previously committed root remains readable.
#[test]
fn dirty_spill_quota_failure_preserves_the_committed_root_and_typed_cause() {
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(vec![int(7)]))
        .expect("list");
    heap.release_statement_roots().expect("release");
    seal(&mut heap);
    let committed = heap.sealed_root().expect("committed root");
    let fault = HeapFault::Exhausted {
        resource: HostResource::StagingBytes,
    };
    pages.borrow_mut().seal_fault = Some(fault.clone());
    let mut stopped = false;
    for i in 1..250_000 {
        match heap.list_write(list, i, int(i as i64)) {
            Ok(()) => (),
            Err(HeapWriteError::Fault(actual)) => {
                assert_eq!(actual, fault);
                stopped = true;
                break;
            }
            Err(other) => panic!("stage failure changed cause: {other:?}"),
        }
    }
    assert!(stopped, "dirty pressure must stage");
    pages.borrow_mut().seal_fault = None;
    let old = PagedHeap::open_header(committed, Box::new(Source(pages)), EditEpoch(2))
        .expect("committed open");
    assert_eq!(old.len(list).expect("old length"), 1);
    assert!(is_int(old.list_get(list, 0).expect("old row"), 7));
}

/// K-ITER-005: scalar iteration and positions survive a cold reconstruction,
/// including mixed-width Unicode across stored leaf boundaries.
#[test]
fn paged_text_cursor_linear() {
    for (mixed, parts) in [(false, 10), (true, 10), (false, 36), (true, 36)] {
        let input: String = (0..parts)
            .map(|i| {
                let c = char::from(b'A' + i);
                if mixed {
                    format!("{c}é😀").repeat(600)
                } else {
                    c.to_string().repeat(4096)
                }
            })
            .collect();
        for parked in [false, true] {
            let (heap, _) = fresh();
            let text = heap.text(&input).unwrap();
            let mut cursor = payload::TextCursor::new(text.clone(), 0);
            for (i, expected) in input.chars().enumerate() {
                if parked && i == 71 {
                    heap.access.clear();
                    cursor = payload::TextCursor::new(text.clone(), i as u64);
                }
                assert_eq!(cursor.next(&heap).unwrap(), Some(expected));
                assert_eq!(cursor.position(), i as u64 + 1);
            }
            assert_eq!(cursor.next(&heap).unwrap(), None);
        }
    }
}

/// D-IMM/K-ITER-005: stopping inside a leaf demands no following leaf;
/// the last scalar demands the next leaf before it can be returned.
#[test]
fn paged_text_cursor_preserves_boundary_fault_order() {
    let (heap, pages) = fresh();
    use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter};
    let mut text = heap.text(&format!("{}é😀", "A".repeat(4096))).unwrap();
    let missing = pages
        .borrow()
        .pages
        .values()
        .find(|p| {
            p.kind == PageKind::PAYLOAD_LEAF && !p.tokens.windows(4096).any(|b| b == [b'A'; 4096])
        })
        .unwrap()
        .page
        .id;
    let first = pages
        .borrow()
        .pages
        .values()
        .find(|p| {
            p.kind == PageKind::PAYLOAD_LEAF && p.tokens.windows(4096).any(|b| b == [b'A'; 4096])
        })
        .unwrap()
        .page;
    let last = pages.borrow().pages[&missing].page;
    let empty = PageRef {
        id: PageId([255; 32]),
        grant: PageGrant::NONE,
    };
    let mut out = TokenWriter::new();
    out.seq(3);
    for (summary, page) in [
        (
            lash_kernel_doc::heap_values::text_summary(&"A".repeat(4096)),
            first,
        ),
        (
            TextSummary {
                utf8_bytes: 0,
                chars: 0,
                utf16_units: 0,
            },
            empty,
        ),
        (lash_kernel_doc::heap_values::text_summary("é😀"), last),
    ] {
        out.seq(2);
        summary.encode(&mut out);
        page.encode(&mut out);
    }
    // Rank selection skips zero-extent links without demanding their pages.
    // Advancing the retained frontier must make the same demand decision.
    text.body = Payload::paged(
        heap.access
            .seal(
                PageKind::PAYLOAD_INDEX,
                out.into_bytes(),
                vec![first, empty, last],
            )
            .unwrap(),
    );
    pages.borrow_mut().pages.remove(&missing);
    let mut cursor = payload::TextCursor::new(text.clone(), 0);
    for _ in 0..4095 {
        assert_eq!(cursor.next(&heap).unwrap(), Some('A'));
    }
    assert!(matches!(cursor.next(&heap), Err(HeapFault::Missing { page }) if page == missing));
    assert_eq!(cursor.position(), 4095);
    // A cold reconstruction at the boundary reports the same failure.
    heap.access.clear();
    let mut cursor = payload::TextCursor::new(text, 4095);
    assert!(matches!(cursor.next(&heap), Err(HeapFault::Missing { page }) if page == missing));
}

/// K-VAL-024: appended list contents survive sealing and a cold open.
#[test]
fn a_list_append_neither_locates_nor_reencodes_its_directory_row() {
    const N: u64 = 1024;
    let (mut heap, pages) = fresh();
    let list = heap
        .allocate_object(ObjectInit::List(Vec::new()))
        .expect("list");
    for i in 0..N {
        let len = heap.len(list).expect("length");
        heap.list_write(list, len, int(i as i64)).expect("append");
    }
    let heap = reopen(&mut heap, &pages);
    assert_eq!(heap.len(list).expect("published length"), N);
    assert!(is_int(
        heap.list_get(list, N - 1).expect("last"),
        (N - 1) as i64
    ));
}

/// D-IMM: observing lengths from a cold descriptor does not demand its payload.
#[test]
fn a_cold_text_length_observation_reads_no_payload_leaf() {
    let (mut heap, pages) = fresh();
    // The crab straddles the first 4 KiB boundary; counts are hand-derived.
    let payload = text(&mut heap, &format!("{}🦀é", "x".repeat(4095)));
    let list = heap
        .allocate_object(ObjectInit::List(vec![payload]))
        .unwrap();
    let heap = reopen(&mut heap, &pages);
    heap.pages().clear();
    let missing = {
        let mut store = pages.borrow_mut();
        let id = *store
            .pages
            .iter()
            .find(|(_, p)| p.kind == PageKind::PAYLOAD_LEAF)
            .unwrap()
            .0;
        store.pages.remove(&id);
        id
    };
    let Some(PageValue::Text(t)) = heap
        .list_get(list, 0)
        .expect("cold descriptor without payload")
    else {
        panic!("text");
    };
    assert_eq!(
        (t.summary.utf8_bytes, t.summary.utf16_units, t.summary.chars),
        (4101, 4098, 4097)
    );
    // Demand validation happens only when the payload is actually observed.
    assert!(
        matches!(heap.payload_visit(&PageValue::Text(t), 0..4101, &mut |_| ControlFlow::Continue(())), Err(HeapFault::Missing { page }) if page == missing)
    );
}
