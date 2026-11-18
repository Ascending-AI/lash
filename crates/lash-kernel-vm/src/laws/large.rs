//! Large lists and tables on the bytecode core's heap (cutover contract
//! §6, cold access, ruling (A)): a point access to a large object whose
//! row is open reads one index page and one chunk; a seal after a point
//! edit writes W = C + I + E + D pages of heap, the changed chunks, index
//! pages, edge pages with their edge index pages, and the union of the
//! changed directory paths; and the object reopens holding what it held.
//!
//! The objects here hold 200,000 members, so their chunk counts take the
//! index past one page. The ceilings each law asserts are the formula's
//! values for its fixture, not a run's: three writes for an integer-only
//! list at directory depth one; at depth four, the derivation in
//! [`ceiling`] gives six, eight, ten and eleven.

use std::cell::Cell;
use std::rc::Rc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::arena::native::{NativeError, NewObject, Retry};
use lash_kernel_doc::heap::{HeapFault, PageRef};
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::paged::run::chunked::{
    CHUNK, Chunked, EDGE_REFS, EDGES, Members, chunk_page, edge_page, placement, rewrite_edges,
};
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::index::{Index, Seek};
use lash_kernel_state::paged::run::objects::{Record, RunValue};
use lash_kernel_state::paged::run::root::HeapRoot;
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};

use super::embedder::Source;
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;

mod occupancy;

/// Members of each large object.
pub(super) const N: i64 = 200_000;

/// A store that counts the pages read from and written to it, and the
/// bytes written.
#[derive(Clone, Default)]
pub(super) struct Counting {
    pub(super) pages: Source,
    pub(super) reads: Rc<Cell<u64>>,
    pub(super) writes: Rc<Cell<u64>>,
    pub(super) bytes: Rc<Cell<u64>>,
}

impl PageSource for Counting {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(self.reads.get() + pages.len() as u64);
        self.pages.read(pages)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.writes.set(self.writes.get() + 1);
        self.bytes.set(self.bytes.get() + body.tokens.len() as u64);
        self.pages.seal(body)
    }
}

impl Counting {
    /// The pages read and written while `work` runs.
    pub(super) fn during<T>(&self, work: impl FnOnce() -> T) -> (T, u64, u64) {
        let (reads, writes) = (self.reads.get(), self.writes.get());
        let out = work();
        (out, self.reads.get() - reads, self.writes.get() - writes)
    }
}

pub(super) fn fresh(store: &Counting) -> SlotHeap {
    SlotHeap::durable(
        u64::MAX,
        crate::heap::open::Durable {
            directory: Directory::new(Box::new(store.clone())),
            high_water: HighWater(0),
        },
    )
}

pub(super) fn make(heap: &mut SlotHeap, object: NewObject) -> Value {
    heap.admit(crate::heap::slots::schedule(&object))
        .expect("room");
    heap.create(object).expect("an object")
}

/// `root`, sealed, reopened from its store with nothing loaded and no
/// page decoded.
pub(super) fn reopened(store: &Counting, root: &HeapRoot) -> (SlotHeap, Value) {
    let (heap, roots) = SlotHeap::open(
        u64::MAX,
        root,
        Directory::open(root.directory, Box::new(store.clone())),
    )
    .expect("an open");
    (heap, roots[0])
}

/// What `op` answers once every chunk it asks for is loaded.
pub(super) fn warm<T>(
    heap: &mut SlotHeap,
    roots: &[Value],
    op: impl FnMut(&mut SlotHeap) -> Result<T, NativeError>,
) -> T {
    // A whole read loads every chunk once, then joins them: never more
    // loads than an object of `N` members has chunks, and one more.
    warm_within(heap, roots, (N as usize).div_ceil(32), op)
}

/// What `op` answers once every chunk it asks for is loaded, in at most
/// `loads` loads.
pub(super) fn warm_within<T>(
    heap: &mut SlotHeap,
    roots: &[Value],
    loads: usize,
    mut op: impl FnMut(&mut SlotHeap) -> Result<T, NativeError>,
) -> T {
    for _ in 0..=loads {
        match op(heap) {
            Ok(out) => return out,
            Err(NativeError::Retry(Retry::Cold { object, chunk })) => heap
                .load(object, chunk, &mut |visit| {
                    roots.iter().for_each(&mut *visit)
                })
                .expect("a load"),
            Err(other) => panic!("a read failed: {other:?}"),
        }
    }
    panic!("a read asked for more loads than its object has chunks");
}

pub(super) fn item(heap: &mut SlotHeap, roots: &[Value], list: Value, at: i64) -> Option<Value> {
    let handle = list.handle().expect("a list");
    warm(heap, roots, |heap| heap.arena().item(handle, at as u64))
}

pub(super) fn get(heap: &mut SlotHeap, roots: &[Value], table: Value, key: Value) -> Option<Value> {
    let handle = table.handle().expect("a table");
    warm(heap, roots, |heap| heap.arena().table_get(handle, &key))
}

/// The integer `value` holds.
pub(super) fn int(value: Option<Value>) -> Option<i64> {
    value.map(|value| match value {
        Value::Int(int) => int,
        other => panic!("{other:?} is no integer"),
    })
}

/// A list of `0 .. N`.
fn counted(heap: &mut SlotHeap) -> Value {
    make(heap, NewObject::List((0..N).map(Value::Int).collect()))
}

/// A map from each `0 .. N` to its square.
fn squares(heap: &mut SlotHeap) -> Value {
    make(
        heap,
        NewObject::Map((0..N).map(|i| (Value::Int(i), Value::Int(i * i))).collect()),
    )
}

/// `build`'s object, sealed into `store` and reopened with its row read:
/// open, its chunks cold.
fn opened_large(store: &Counting, build: fn(&mut SlotHeap) -> Value) -> (SlotHeap, Value) {
    let (heap, object, _) = opened_root(store, build);
    (heap, object)
}

/// [`opened_large`], with the root it was sealed under.
fn opened_root(store: &Counting, build: fn(&mut SlotHeap) -> Value) -> (SlotHeap, Value, HeapRoot) {
    let mut heap = fresh(store);
    let object = build(&mut heap);
    let root = heap.seal(&[object]).expect("a seal");
    let (mut heap, object) = reopened(store, &root);
    // The first access opens the row; the chunk it reads is not measured.
    let handle = object.handle().expect("an object");
    heap.load(handle, 0, &mut |visit| visit(&object))
        .expect("the row");
    assert!(
        !heap.arena().facts(handle).expect("live").cold,
        "the row opens the object"
    );
    (heap, object, root)
}

/// A tuple of `0 .. N`.
fn counted_tuple(heap: &mut SlotHeap) -> Value {
    make(heap, NewObject::Tuple((0..N).map(Value::Int).collect()))
}

/// Contract §6: a point read of a cold large list or tuple loads one
/// index page and one chunk, wherever the position is.
#[test]
fn a_point_read_of_a_cold_large_sequence_reads_one_index_page_and_one_chunk() {
    for (kind, build) in [
        ("list", counted as fn(&mut SlotHeap) -> Value),
        ("tuple", counted_tuple),
    ] {
        let store = Counting::default();
        let (mut heap, sequence) = opened_large(&store, build);
        for at in [0, 64, 5 * 64 + 63, N / 2 + 3, N - 1] {
            heap.forget_pages();
            let (value, reads, writes) =
                store.during(|| item(&mut heap, &[sequence], sequence, at));
            assert_eq!(int(value), Some(at), "{kind} position {at}");
            assert_eq!((reads, writes), (2, 0), "{kind} position {at}");
        }
    }
}

/// Contract §6, immutable heads: a tuple's key does not depend on how its
/// payload is written or held. A large tuple, and a tuple written chunked
/// for the width of its members, read cold from its head key the same as
/// the equal tuple built anew, and each finds the other as a map's and a
/// set's key.
#[test]
fn a_chunked_tuple_is_the_same_key_warm_and_cold() {
    fn wide(heap: &mut SlotHeap) -> Value {
        let members = (0..256)
            .map(|i| make(heap, NewObject::long_text(format!("{i:0>250}"))))
            .collect();
        make(heap, NewObject::Tuple(members))
    }
    for (kind, build) in [
        (
            "a tuple of N integers",
            counted_tuple as fn(&mut SlotHeap) -> Value,
        ),
        ("a tuple of 256 wide texts", wide),
    ] {
        let store = Counting::default();
        let mut heap = fresh(&store);
        let tuple = build(&mut heap);
        let map = make(&mut heap, NewObject::Map(vec![(tuple, Value::Int(7))]));
        let set = make(&mut heap, NewObject::Set(vec![tuple]));
        let root = heap.seal(&[tuple, map, set]).expect("a seal");
        let (mut heap, roots) = SlotHeap::open(
            u64::MAX,
            &root,
            Directory::open(root.directory, Box::new(store.clone())),
        )
        .expect("an open");
        let [cold, map, set] = roots[..] else {
            panic!("three roots")
        };
        let again = build(&mut heap);
        let held = [cold, map, set, again];
        let key = |heap: &mut SlotHeap, value: Value| {
            warm(heap, &held, |heap| {
                heap.arena().semantics().normal_key(&value)
            })
        };
        let (cold_key, again_key) = (key(&mut heap, cold), key(&mut heap, again));
        assert!(cold_key.is_some(), "{kind}: a tuple of keys is a key");
        assert_eq!(cold_key, again_key, "{kind}: the key warm and cold");
        for probe in [cold, again] {
            assert_eq!(
                int(get(&mut heap, &held, map, probe)),
                Some(7),
                "{kind}: a map key"
            );
            assert!(
                get(&mut heap, &held, set, probe).is_some(),
                "{kind}: a set member"
            );
        }
    }
}

/// Contract §6: a point read of a cold large map loads one index page and
/// the one chunk its key's placement names, for a key it holds and one it
/// does not.
#[test]
fn a_point_read_of_a_cold_large_map_reads_one_index_page_and_one_chunk() {
    let store = Counting::default();
    let (mut heap, map) = opened_large(&store, squares);
    for key in [0, 1, N / 2 + 3, N - 1] {
        heap.forget_pages();
        let (value, reads, _) = store.during(|| get(&mut heap, &[map], map, Value::Int(key)));
        assert_eq!(int(value), Some(key * key), "key {key}");
        assert_eq!(reads, 2, "key {key}");
    }
    heap.forget_pages();
    let (value, reads, _) = store.during(|| get(&mut heap, &[map], map, Value::Int(-1)));
    assert_eq!((int(value), reads), (None, 2), "an absent key");
}

/// Contract §6: a point edit of an open large list dirties one chunk, and
/// its seal writes that chunk, its index page and the directory page
/// holding the row; the list reopens holding the edit and nothing else
/// changed.
#[test]
fn a_point_edit_of_a_large_list_seals_one_chunk_its_index_page_and_the_row() {
    let store = Counting::default();
    let (mut heap, list, sealed) = opened_root(&store, counted);
    let (depth, _) = paths(&store, &sealed, id(&heap, list), id(&heap, list));
    assert_eq!(depth, 1, "an integer-only list is the directory's one row");
    let at = N / 2 + 3;
    item(&mut heap, &[list], list, at);
    warm(&mut heap, &[list], |heap| {
        heap.arena_mut()
            .list_set(list.handle().expect("a list"), at as u64, Value::Int(-7))
    });
    let bytes = store.bytes.get();
    let (root, reads, writes) = store.during(|| heap.seal(&[list]).expect("a reseal"));
    let bytes = store.bytes.get() - bytes;
    assert!(
        reads == 0 && writes <= 3,
        "d 1: {reads} reads, {writes} pages ({bytes} bytes) over 3"
    );
    let (mut heap, list) = reopened(&store, &root);
    for probe in [0, at - 1, at, at + 1, N - 1] {
        let expected = if probe == at { -7 } else { probe };
        assert_eq!(int(item(&mut heap, &[list], list, probe)), Some(expected));
    }
}

/// Contract §6: overwriting one entry of an open large map is a point
/// edit; adding one is too, unless its chunk splits.
#[test]
fn a_point_edit_of_a_large_map_seals_one_chunk_its_index_page_and_the_row() {
    let store = Counting::default();
    let (mut heap, map) = opened_large(&store, squares);
    let handle = map.handle().expect("a map");
    let key = Value::Int(N / 2 + 3);
    get(&mut heap, &[map], map, key);
    warm(&mut heap, &[map], |heap| {
        heap.arena_mut().table_write(handle, key, Value::Int(-7))
    });
    let (root, _, writes) = store.during(|| heap.seal(&[map]).expect("a reseal"));
    assert!(writes <= 3, "d 1: {writes} pages over 3");
    let (mut heap, map) = reopened(&store, &root);
    assert_eq!(int(get(&mut heap, &[map], map, key)), Some(-7));
    assert_eq!(
        int(get(&mut heap, &[map], map, Value::Int(N / 2 + 4))),
        Some((N / 2 + 4) * (N / 2 + 4))
    );
}

/// `K-MAP-001` across the page form: entries added to a large map after
/// it was opened are placed by key at the next seal, and the reopened map
/// iterates in insertion order with every value it was given.
#[test]
fn a_large_map_keeps_insertion_order_across_relayout() {
    let store = Counting::default();
    let (mut heap, map) = opened_large(&store, squares);
    let handle = map.handle().expect("a map");
    let added: Vec<i64> = (0..300).map(|i| N + 7 * i).rev().collect();
    for key in &added {
        warm(&mut heap, &[map], |heap| {
            heap.arena_mut()
                .table_write(handle, Value::Int(*key), Value::Int(-key))
        });
    }
    let removed = Value::Int(5);
    warm(&mut heap, &[map], |heap| {
        heap.arena_mut().table_delete(handle, &removed)
    });
    let root = heap.seal(&[map]).expect("a reseal");
    let (mut heap, map) = reopened(&store, &root);
    let handle = map.handle().expect("a map");
    let mut keys = Vec::new();
    let mut last = None;
    while let Some((seq, key)) = warm(&mut heap, &[map], |heap| {
        heap.arena().table_next(handle, last)
    }) {
        let Value::Int(key) = key else {
            panic!("an int key");
        };
        keys.push(key);
        last = Some(seq);
    }
    let expected: Vec<i64> = (0..N).filter(|i| *i != 5).chain(added.clone()).collect();
    assert_eq!(keys.len(), expected.len());
    assert!(keys == expected, "the reopened map iterates out of order");
    assert_eq!(
        int(get(&mut heap, &[map], map, Value::Int(added[17]))),
        Some(-added[17])
    );
}

/// A key whose normal form is 256 bytes: a long text of that length.
fn long_key(i: i64) -> String {
    format!("{i:0>256}")
}

/// Contract §6, measured with 256-byte normals: a point read of a large
/// map keyed by long texts still loads one index page and one chunk; the
/// keys open from the heads the chunk holds, without their rows.
#[test]
fn a_point_read_with_long_keys_reads_one_index_page_and_one_chunk() {
    let store = Counting::default();
    let (mut heap, map) = opened_large(&store, |heap| {
        let pairs = (0..N)
            .map(|i| {
                let key = make(heap, NewObject::long_text(long_key(i)));
                (key, Value::Int(i))
            })
            .collect();
        make(heap, NewObject::Map(pairs))
    });
    for i in [0, N / 2 + 3, N - 1] {
        let key = make(&mut heap, NewObject::long_text(long_key(i)));
        heap.forget_pages();
        let (value, reads, _) = store.during(|| get(&mut heap, &[map, key], map, key));
        assert_eq!(int(value), Some(i), "key {i}");
        assert_eq!(reads, 2, "key {i}");
    }
}

/// Contract §6: a point access through a large list of immutables reads
/// the list's index page and chunk, then the one member's row: the
/// members it does not touch stay unread.
#[test]
fn a_point_access_through_a_large_immutable_graph_reads_one_path() {
    let store = Counting::default();
    let (mut heap, list, root) = opened_root(&store, tuples);
    let at = N / 2 + 3;
    heap.forget_pages();
    let (value, reads, _) = store.during(|| {
        let tuple = item(&mut heap, &[list], list, at).expect("a member");
        let handle = tuple.handle().expect("a tuple");
        warm(&mut heap, &[list, tuple], |heap| {
            heap.arena().item(handle, 0)
        })
    });
    assert_eq!(int(value), Some(at));
    // One row's read costs the directory's depth, measured on the same
    // directory read cold.
    let directory = Directory::open(root.directory, Box::new(store.clone()));
    let (_, depth, _) = store.during(|| directory.get(ObjectId(7)).expect("a row"));
    assert_eq!(reads, 2 + depth, "{depth} directory levels");
}

/// A list of `N` tuples, each its own object: with the list, `N + 1`
/// directory rows (contract §6, the d = 4 fixtures).
fn tuples(heap: &mut SlotHeap) -> Value {
    let label = ShortText::new("member").expect("short");
    let members = (0..N)
        .map(|i| {
            make(
                heap,
                NewObject::Tuple(vec![Value::Int(i), Value::Text(label)]),
            )
        })
        .collect();
    make(heap, NewObject::List(members))
}

/// The id `value`'s row is keyed by.
fn id(heap: &SlotHeap, value: Value) -> ObjectId {
    let facts = heap
        .arena()
        .facts(value.handle().expect("an object"))
        .expect("live");
    ObjectId(facts.entity().expect("an id").ordinal)
}

/// The depth of the row of `first` read cold from `root`'s directory, and
/// how many of its pages the row of `second` shares: the pages a read of
/// `second` does not load again.
fn paths(store: &Counting, root: &HeapRoot, first: ObjectId, second: ObjectId) -> (u64, u64) {
    let directory = Directory::open(root.directory, Box::new(store.clone()));
    let (_, depth, _) = store.during(|| directory.get(first).expect("a row"));
    let (_, own, _) = store.during(|| directory.get(second).expect("a row"));
    (depth, depth - own)
}

/// The member at `at` of `list`.
fn member(heap: &mut SlotHeap, list: Value, at: i64) -> Value {
    item(heap, &[list], list, at).expect("a member")
}

/// Sets `list[at]` to `value`.
fn set(heap: &mut SlotHeap, roots: &[Value], list: Value, at: i64, value: Value) {
    item(heap, roots, list, at);
    warm(heap, roots, |heap| {
        heap.arena_mut()
            .list_set(list.handle().expect("a list"), at as u64, value)
    });
}

/// Contract §6, the d = 4 fixtures of ruling (A): in a heap of 200,001
/// rows, whose directory is four deep, the seal of a point edit writes
/// W = C + I + E + D + s + i + 2 pages. Each fixture's ceiling is that
/// formula's value for it, term by term from the fixture's geometry (see
/// [`ceiling`]); the law records each fixture's depth and path overlap,
/// and the bytes written.
///
/// The fixture: 3,125 chunks, one chunk index level below the row
/// (I = 1); 200,000 edges at 768 a page, 261 edge pages, one edge index
/// level of four nodes, as wide as the edge index makes them for 261
/// pages (66: pages 0–65, 66–131, 132–197, 198–260); d = 4. `heap.seal`
/// writes no run record, so s, i and the two roots count 0.
///
/// | case | C | I | E: pages + index nodes | D | W |
/// |---|---|---|---|---|---|
/// | unchanged-edge overwrite | 1 | 1 | 0 + 0 | 4 | 6 |
/// | one edge page (page 130, node 1) | 1 | 1 | 1 + 1 | 4 | 8 |
/// | two edge pages (pages 0 and 130, nodes 0 and 1) | 1 | 1 | 2 + 2 | 4 | 10 |
/// | a single dead row | 1 | 1 | 1 + 1 | 4 + 3 | 11 |
#[test]
fn a_point_edit_in_a_four_deep_directory_seals_within_its_fixture_ceiling() {
    let at = N / 2 + 3;
    // Each fixture: its name, the list positions whose referents' edge
    // counts it changes, and the edit. The edit returns the values the
    // run still holds besides the list.
    type Edit = fn(&mut SlotHeap, Value, i64) -> Vec<Value>;
    type Changed = fn(i64) -> Vec<i64>;
    let fixtures: [(&str, Changed, Edit); 4] = [
        // Two members of one chunk swap: every edge count is unchanged.
        // W = C 1 + I 1 + E 0 + D 4 = 6.
        (
            "an unchanged-edge overwrite",
            |_| vec![],
            |heap, list, at| {
                let (a, b) = (member(heap, list, at), member(heap, list, at + 1));
                set(heap, &[list, a, b], list, at, b);
                set(heap, &[list, a, b], list, at + 1, a);
                vec![]
            },
        ),
        // A member is replaced by its neighbour, whose id shares its edge
        // page; the run still holds the member replaced. W = C 1 + I 1 +
        // E (1 page + 1 index node) + D 4 = 8.
        (
            "one edge page",
            |at| vec![at, at + 1],
            |heap, list, at| {
                let (a, b) = (member(heap, list, at), member(heap, list, at + 1));
                assert_eq!(id(heap, b).0, id(heap, a).0 + 1, "neighbouring ids");
                set(heap, &[list, a, b], list, at, b);
                vec![a]
            },
        ),
        // A member is replaced by one whose id lies more than an edge
        // page's references away: two edge pages change, under two edge
        // index nodes. W = C 1 + I 1 + E (2 + 2) + D 4 = 10.
        (
            "two edge pages",
            |at| vec![at, 5],
            |heap, list, at| {
                let (a, b) = (member(heap, list, at), member(heap, list, 5));
                assert!(
                    id(heap, a).0 - id(heap, b).0 > EDGE_REFS as u64,
                    "ids an edge page apart"
                );
                set(heap, &[list, a, b], list, at, b);
                vec![a]
            },
        ),
        // The only reference to a member is overwritten: its row leaves
        // the directory at the seal. W = C 1 + I 1 + E (1 + 1) + D (4 + 3,
        // the dead row's path sharing only the root) = 11.
        (
            "a single dead row",
            |at| vec![at, at + 1],
            |heap, list, at| {
                let b = member(heap, list, at + 1);
                set(heap, &[list, b], list, at, b);
                vec![]
            },
        ),
    ];
    let mut failed = Vec::new();
    for (fixture, changed, edit) in fixtures {
        let store = Counting::default();
        let (mut heap, list, root) = opened_root(&store, tuples);
        let dead = fixture == "a single dead row";
        // The dead row's fixture is the one whose two directory paths
        // share only the root: the first such member from `at` on.
        let at = if dead {
            // Its neighbour shares its chunk.
            (at..N - 1)
                .filter(|at| at % CHUNK as i64 != CHUNK as i64 - 1)
                .find(|&at| {
                    let member = member(&mut heap, list, at);
                    paths(&store, &root, id(&heap, list), id(&heap, member)).1 == 1
                })
                .expect("a member whose path shares only the root")
        } else {
            at
        };
        let (depth, overlap) = {
            let member = member(&mut heap, list, at);
            paths(&store, &root, id(&heap, list), id(&heap, member))
        };
        assert_eq!(depth, 4, "{fixture}: 200,001 rows are four levels deep");
        // The members' ids run with their positions, so a member's rank in
        // the list's edge set is its position.
        let first = member(&mut heap, list, 0);
        let last = member(&mut heap, list, N - 1);
        assert_eq!(
            id(&heap, last).0 - id(&heap, first).0,
            N as u64 - 1,
            "{fixture}: the members' ids are consecutive"
        );
        let ceiling = ceiling(&changed(at), depth, if dead { depth - overlap } else { 0 });
        let held = edit(&mut heap, list, at);
        let mut roots = vec![list];
        roots.extend(held);
        let bytes = store.bytes.get();
        let (_, _, writes) = store.during(|| heap.seal(&roots).expect("a reseal"));
        let bytes = store.bytes.get() - bytes;
        eprintln!(
            "{fixture}: d {depth}, path overlap {overlap}, {writes} pages, {bytes} bytes, ceiling {ceiling}"
        );
        if writes > ceiling {
            failed.push(format!(
                "{fixture}: d {depth}, path overlap {overlap}: {writes} pages ({bytes} bytes) over {ceiling}"
            ));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// W = C + I + E + D + s + i + 2 for a point edit of the [`tuples`] list
/// that changes the edge counts of the members at `changed`, with the
/// list's row `depth` directory pages down and `dead` more directory
/// pages on a dead row's path (contract §6, ruling (A); `seal-design.md`
/// run record).
fn ceiling(changed: &[i64], depth: u64, dead: u64) -> u64 {
    use lash_kernel_state::paged::run::index::Shape;
    use lash_kernel_state::paged::run::objects::ObjectKind;
    let n = N as u64;
    // An edge list past one page is cut into pages three quarters full.
    let edge_width = EDGE_REFS as u64 * 3 / 4;
    // The index pages on the paths to the leaves at `at` of an index of
    // `shape` over `leaves`: per level below its top, the distinct nodes
    // holding them, each level's nodes as wide as `shape` makes them for
    // that level's count.
    let paths = |shape: Shape, leaves: u64, at: BTreeSet<u64>| {
        let (mut pages, mut count, mut nodes) = (0, leaves, at);
        for _ in 0..shape.height(leaves) {
            let width = shape.width(count);
            nodes = nodes.iter().map(|place| place / width).collect();
            pages += nodes.len() as u64;
            count = count.div_ceil(width);
        }
        pages
    };
    // C: the one chunk both edited positions share.
    let c = 1;
    let chunk = changed.first().map_or(0, |&at| {
        u64::try_from(at).expect("a position") / CHUNK as u64
    });
    // I: the chunk index path, one page per level below the row
    // (3,125 chunks: one level).
    let i_index = paths(
        Chunked::shape_of(ObjectKind::List, n),
        n.div_ceil(CHUNK as u64),
        BTreeSet::from([chunk]),
    );
    // E: the changed edge pages, and the distinct edge index pages on
    // their paths (261 edge pages: one level of four nodes).
    let edge_pages: BTreeSet<u64> = changed
        .iter()
        .map(|&at| u64::try_from(at).expect("a position") / edge_width)
        .collect();
    let e = edge_pages.len() as u64 + paths(EDGES, n.div_ceil(edge_width), edge_pages);
    // D: the union of the changed directory paths: the list row's, and a
    // dead row's pages it does not share.
    let d = depth + dead;
    // s, i, 2: the run record's segments, segment index pages and two
    // roots, which the run's checkpoint writes and a heap seal does not.
    let (s, i, roots) = (0, 0, 0);
    c + i_index + e + d + s + i + roots
}

/// `K-VAL-020` on cold lists: two large lists of one length that differ
/// at their first member compare unequal from their first chunks alone,
/// so the comparison reads no later chunk of either.
#[test]
fn unequal_prefixes_do_not_read_later_chunks() {
    use lash_kernel_doc::arena::native::Semantics;
    let store = Counting::default();
    let mut heap = fresh(&store);
    let left = counted(&mut heap);
    let right = make(
        &mut heap,
        NewObject::List((1..=N).map(Value::Int).collect()),
    );
    let root = heap.seal(&[left, right]).expect("a seal");
    let (mut heap, roots) = SlotHeap::open(
        u64::MAX,
        &root,
        Directory::open(root.directory, Box::new(store.clone())),
    )
    .expect("an open");
    let (equal, reads, _) = store.during(|| {
        warm(&mut heap, &roots, |heap| {
            heap.arena().semantics().equal(&roots[0], &roots[1])
        })
    });
    assert!(!equal);
    // Each list's row and its first chunk through its index: no later
    // chunk is read.
    assert!(reads <= 2 * 3, "the comparison read {reads} pages");
}

/// The record of the `at`th object `root` seals, read from its store.
fn record(store: &Counting, root: &HeapRoot, at: usize) -> Result<Record, String> {
    let handle = root
        .roots
        .get(at)
        .and_then(|root| root.id())
        .ok_or("the root names no object")?;
    Ok(Directory::open(root.directory, Box::new(store.clone()))
        .get(handle)
        .map_err(|error| format!("{error:?}"))?
        .ok_or("the object has no row")?
        .record)
}

/// The chunked row of the first object `root` seals, read from its store.
fn row(store: &Counting, root: &HeapRoot) -> Result<Chunked, String> {
    match record(store, root, 0)? {
        Record::Chunked(chunked) => Ok(*chunked),
        other => Err(format!("the row is no chunked record: {other:?}")),
    }
}

/// The roots `root` seals, opened from `store`.
fn opened_all(store: &Counting, root: &HeapRoot) -> Result<(SlotHeap, Vec<Value>), String> {
    SlotHeap::open(
        u64::MAX,
        root,
        Directory::open(root.directory, Box::new(store.clone())),
    )
    .map_err(|error| format!("the reopen fails: {error:?}"))
}

/// What a mutable container's row is whatever its history (lead ruling,
/// l65e item 4): its chunk index and its edge index each at the least
/// height their leaves give, every index page, read cold, the one its
/// parent names and naming between one and `NODE_REFS` references, and
/// no chunk empty. Its chunk layout and the widths of its index pages
/// may depend on its history, and need not be a direct build's, but each
/// index keeps the occupancy rule ([`occupancy`], lead ruling, l65f
/// item 1).
fn well_formed(store: &Counting, chunked: &Chunked) -> Result<(), String> {
    chunked.check()?;
    chunked.index.check(&chunked.shape())?;
    chunked.edges.check(&EDGES)?;
    occupancy::occupied(store, &chunked.index).map_err(|error| format!("chunks: {error}"))?;
    occupancy::occupied(store, &chunked.edges).map_err(|error| format!("edges: {error}"))?;
    let access = Access::new(Box::new(store.clone()));
    let mut empty = 0;
    chunked
        .index
        .each(&access, &chunked.shape(), &mut |_, chunk| {
            empty += u64::from(chunk.len == 0);
            Ok(false)
        })
        .map_err(|error| format!("the chunk index reads otherwise: {error:?}"))?;
    chunked
        .edges
        .each(&access, &EDGES, &mut |_, _| Ok(false))
        .map_err(|error| format!("the edge index reads otherwise: {error:?}"))?;
    if empty > 0 {
        return Err(format!("{empty} chunks are empty"));
    }
    Ok(())
}

/// The list case: a list of `n` grown by one member and shrunk back, each
/// step sealed and reopened cold, holds `0 .. len`, its row
/// [`well_formed`] with its chunk index `heights` high before and after
/// the growth.
fn list_case(n: i64, heights: (u8, u8)) -> Result<(), String> {
    let store = Counting::default();
    let mut heap = fresh(&store);
    let list = make(&mut heap, NewObject::List((0..n).map(Value::Int).collect()));
    let mut root = heap.seal(&[list]).map_err(|error| format!("{error:?}"))?;
    for (step, len) in [("grown", n + 1), ("shrunk back", n)] {
        let (mut heap, list) = reopened(&store, &root);
        let handle = list.handle().ok_or("no list")?;
        if len > n {
            warm_within(&mut heap, &[list], 4, |heap| {
                heap.arena_mut().list_set(handle, n as u64, Value::Int(n))
            });
        } else {
            warm_within(&mut heap, &[list], 4, |heap| {
                heap.arena_mut().list_remove(handle, n as u64)
            });
        }
        root = heap
            .seal(&[list])
            .map_err(|error| format!("{step}: {error:?}"))?;
        let (mut cold, list) = reopened(&store, &root);
        let handle = list.handle().ok_or("no list")?;
        let held = warm_within(&mut cold, &[list], 4, |heap| heap.arena().len(handle));
        if held != len as u64 {
            return Err(format!("{n}, {step}: {held} members, not {len}"));
        }
        let probes = (0..len).step_by(4_099).chain([len - 2, len - 1]);
        for at in probes {
            if int(item(&mut cold, &[list], list, at)) != Some(at) {
                return Err(format!("{n}, {step}: member {at} is not {at}"));
            }
        }
        let chunked = row(&store, &root)?;
        well_formed(&store, &chunked).map_err(|error| format!("{n}, {step}: {error}"))?;
        let height = if len > n { heights.1 } else { heights.0 };
        if chunked.index.height() != height {
            return Err(format!(
                "{n}, {step}: the chunk index is {} high, not {height}",
                chunked.index.height()
            ));
        }
    }
    Ok(())
}

/// A tuple of `0 .. n`, sealed alone into a store of its own: the record
/// a tuple built directly at `n` has.
fn direct_tuple(n: i64) -> Result<Record, String> {
    let store = Counting::default();
    let mut heap = fresh(&store);
    let tuple = make(
        &mut heap,
        NewObject::Tuple((0..n).map(Value::Int).collect()),
    );
    let root = heap.seal(&[tuple]).map_err(|error| format!("{error:?}"))?;
    record(&store, &root, 0)
}

/// The tuple case (lead ruling, l65e item 4): a tuple is a
/// content-addressed immutable, so however it is built its record is the
/// direct build's, every page of it (a page's id commits to its
/// children's), and its key is the key of the tuple built anew. A tuple
/// of `0 .. n` whose chunk index is `height` high (none: inline) is built
/// in a store holding another object, sealed again after it is reopened
/// cold and read and another object edited, and copied from its cold
/// members; each is the direct build.
fn tuple_case(n: i64, height: Option<u8>) -> Result<(), String> {
    let seal = |heap: &mut SlotHeap, roots: &[Value]| {
        heap.seal(roots)
            .map_err(|error| format!("{n}: the seal fails: {error:?}"))
    };
    let direct = direct_tuple(n)?;
    let direct_height = match &direct {
        Record::Chunked(chunked) => Some(chunked.index.height()),
        _ => None,
    };
    if direct_height != height {
        return Err(format!(
            "{n}: the direct build's index is {direct_height:?} high, not {height:?}"
        ));
    }
    let same = |route: &str, got: Record| -> Result<(), String> {
        if got == direct {
            Ok(())
        } else {
            Err(format!(
                "{n}, {route}: the record is not the direct build's"
            ))
        }
    };
    let store = Counting::default();
    let mut heap = fresh(&store);
    let list = make(
        &mut heap,
        NewObject::List((0..300).map(|i| Value::Int(-i)).collect()),
    );
    let first = seal(&mut heap, &[list])?;
    let (mut heap, list) = reopened(&store, &first);
    let tuple = make(
        &mut heap,
        NewObject::Tuple((0..n).map(Value::Int).collect()),
    );
    let root = seal(&mut heap, &[tuple, list])?;
    same("built in a store with history", record(&store, &root, 0)?)?;
    let (mut heap, roots) = opened_all(&store, &root)?;
    let [tuple, list] = roots[..] else {
        return Err("two roots".into());
    };
    if int(item(&mut heap, &roots, tuple, n - 1)) != Some(n - 1) {
        return Err(format!("{n}: the cold tuple's last member reads otherwise"));
    }
    let handle = list.handle().ok_or("no list")?;
    warm(&mut heap, &roots, |heap| {
        heap.arena_mut().list_set(handle, 0, Value::Int(1))
    });
    let root = seal(&mut heap, &[tuple, list])?;
    same("sealed again cold", record(&store, &root, 0)?)?;
    let (mut heap, roots) = opened_all(&store, &root)?;
    let tuple = roots[0];
    let members = (0..n)
        .map(|at| item(&mut heap, &roots, tuple, at).ok_or(format!("{n}: no member {at}")))
        .collect::<Result<Vec<_>, _>>()?;
    let copy = make(&mut heap, NewObject::Tuple(members));
    let held = [tuple, copy];
    let key = |heap: &mut SlotHeap, value: Value| {
        warm(heap, &held, |heap| {
            heap.arena().semantics().normal_key(&value)
        })
    };
    let (cold_key, copy_key) = (key(&mut heap, tuple), key(&mut heap, copy));
    if cold_key.is_none() || cold_key != copy_key {
        return Err(format!("{n}: the copy's key is not the cold tuple's"));
    }
    let root = seal(&mut heap, &[copy, tuple])?;
    same("copied from its cold members", record(&store, &root, 0)?)?;
    same("its source, sealed beside it", record(&store, &root, 1)?)
}

/// The number a table key of the table cases names: `i`, or `k{i}`.
fn number(key: Value) -> Option<i64> {
    match key {
        Value::Int(i) => Some(i),
        Value::Text(text) => text.as_str().strip_prefix('k')?.parse().ok(),
        _ => None,
    }
}

/// The table cases: a map, set or record of `start` entries is grown one
/// entry at a time, each step sealed and reopened cold, until a chunk
/// splits, then shrunk back by one chunk, every entry of the last chunk
/// holding a new one removed. The table before the growth and at each
/// end holds exactly its entries and its row is [`well_formed`], its
/// chunk index `heights` high before and after the split.
fn table_case(kind: &str, start: i64, heights: (u8, u8)) -> Result<(), String> {
    let key = |i: i64| -> Value {
        match kind {
            "record" => Value::Text(ShortText::new(&format!("k{i}")).expect("short")),
            _ => Value::Int(i),
        }
    };
    let value = |i: i64| -> Value {
        match kind {
            "set" => Value::Absent,
            _ => Value::Int(-i),
        }
    };
    let entries = |keys: &[i64]| {
        keys.iter()
            .map(|i| (key(*i), value(*i)))
            .collect::<Vec<_>>()
    };
    let mut model: Vec<i64> = (0..start).collect();
    // A write may load every chunk once, and join them.
    let loads = usize::try_from(2 * start / 32 + 64).expect("a bound");
    let store = Counting::default();
    let mut heap = fresh(&store);
    let object = match kind {
        "map" => NewObject::Map(entries(&model)),
        "record" => NewObject::Record(entries(&model)),
        _ => NewObject::Set(model.iter().map(|i| key(*i)).collect()),
    };
    let table = make(&mut heap, object);
    let mut root = heap.seal(&[table]).map_err(|error| format!("{error:?}"))?;
    let check =
        |root: &HeapRoot, model: &[i64], step: &str, height: u8| -> Result<Chunked, String> {
            let (mut cold, table) = reopened(&store, root);
            let handle = table.handle().ok_or("no table")?;
            let mut keys = Vec::new();
            let mut last = None;
            while let Some((seq, held)) = warm_within(&mut cold, &[table], loads, |heap| {
                heap.arena().table_next(handle, last)
            }) {
                keys.push(number(held));
                last = Some(seq);
            }
            let expected: Vec<Option<i64>> = model.iter().map(|i| Some(*i)).collect();
            if keys != expected {
                return Err(format!(
                    "{kind}, {step}: the entries are not the ones written"
                ));
            }
            for i in [model[0], model[model.len() / 2], model[model.len() - 1]] {
                let probe = key(i);
                let held = warm_within(&mut cold, &[table], loads, |heap| {
                    heap.arena().table_get(handle, &probe)
                });
                let right = match (kind, held) {
                    ("set", Some(Value::Absent)) => true,
                    (_, Some(Value::Int(held))) => held == -i,
                    _ => false,
                };
                if !right {
                    return Err(format!("{kind}, {step}: entry {i} reads otherwise"));
                }
            }
            let chunked = row(&store, root)?;
            well_formed(&store, &chunked).map_err(|error| format!("{kind}, {step}: {error}"))?;
            if chunked.index.height() != height {
                return Err(format!(
                    "{kind}, {step}: the chunk index is {} high, not {height}",
                    chunked.index.height()
                ));
            }
            Ok(chunked)
        };
    let built = check(&root, &model, "built", heights.0)?;
    let before = built.chunks();
    // The new entries' keys, past every key built, each placed in the
    // last chunk, so that the growth splits it.
    let new = start + 1_000;
    let access = Access::new(Box::new(store.clone()));
    let (_, last) = built
        .index
        .get(&access, &built.shape(), Seek::At(before - 1))
        .map_err(|error| format!("{error:?}"))?
        .ok_or("no last chunk")?;
    let last = last.first.ok_or("the last chunk has no placement")?;
    let scratch = fresh(&Counting::default());
    let mut placed = (new..).filter(|i| {
        scratch
            .arena()
            .semantics()
            .normal_key(&key(*i))
            .ok()
            .flatten()
            .is_some_and(|normal| placement(&normal) >= last)
    });
    let mut steps = 0;
    loop {
        let next = placed.next().ok_or("no key is placed in the last chunk")?;
        let (mut heap, table) = reopened(&store, &root);
        let handle = table.handle().ok_or("no table")?;
        warm_within(&mut heap, &[table], loads, |heap| {
            heap.arena_mut().table_write(handle, key(next), value(next))
        });
        model.push(next);
        root = heap.seal(&[table]).map_err(|error| format!("{error:?}"))?;
        if row(&store, &root)?.chunks() > before {
            break;
        }
        steps += 1;
        if steps > 400 {
            return Err(format!("{kind}: 400 entries split no chunk"));
        }
    }
    let split = check(&root, &model, "grown", heights.1)?;
    // The chunk the split made: the later of the two, the first whose
    // first placement no chunk before the growth had.
    let access = Access::new(Box::new(store.clone()));
    let mut firsts = Vec::new();
    split
        .index
        .each(&access, &split.shape(), &mut |_, chunk| {
            firsts.push(*chunk);
            Ok(false)
        })
        .map_err(|error| format!("{error:?}"))?;
    let mut removed = Vec::new();
    for chunk in firsts.iter().rev() {
        let (page, _) = chunk_page(&access, chunk).map_err(|error| format!("{error:?}"))?;
        let Members::Table(held) = page.members else {
            return Err(format!("{kind}: a chunk holds a list's members"));
        };
        let keys: Vec<i64> = held
            .iter()
            .filter_map(|entry| match &entry.key {
                RunValue::Int(i) => Some(*i),
                RunValue::Text(text) => text.strip_prefix('k').and_then(|i| i.parse().ok()),
                _ => None,
            })
            .collect();
        if keys.iter().any(|i| *i >= new) {
            removed = keys;
            break;
        }
    }
    if removed.is_empty() {
        return Err(format!("{kind}: no chunk holds the new entries"));
    }
    let (mut heap, table) = reopened(&store, &root);
    let handle = table.handle().ok_or("no table")?;
    for i in &removed {
        let gone = key(*i);
        warm_within(&mut heap, &[table], loads, |heap| {
            heap.arena_mut().table_delete(handle, &gone)
        });
    }
    model.retain(|i| !removed.contains(i));
    root = heap.seal(&[table]).map_err(|error| format!("{error:?}"))?;
    let shrunk = check(&root, &model, "shrunk back", heights.0)?;
    if shrunk.chunks() + 1 != split.chunks() {
        return Err(format!(
            "{kind}: emptying a chunk left {} chunks of {}",
            shrunk.chunks(),
            split.chunks()
        ));
    }
    Ok(())
}

/// The edge list case, through the row's edge writer: 73,728 ids, 96
/// pages of 768, one level below no index page; 256 more in the first
/// page fill it to 1,024; one more splits it, 97 pages under one index
/// level; the second page's ids removed, 96 pages and no index page
/// again. Each step is read back cold: the counts written, and the
/// index at its height.
fn edge_case() -> Result<(), String> {
    let store = Counting::default();
    let mut model: BTreeMap<ObjectId, u64> = BTreeMap::new();
    let mut edges = Index::empty(&EDGES);
    // Each step: its name, the (id, count change) pairs, and the pages
    // and index height it leaves.
    type Step = (&'static str, Vec<(u64, i64)>, u64, u8);
    let steps: [Step; 4] = [
        ("built", (0..73_728).map(|i| (2 * i, 1)).collect(), 96, 0),
        ("filled", (0..256).map(|i| (2 * i + 1, 1)).collect(), 96, 0),
        ("grown", vec![(513, 1)], 97, 1),
        ("shrunk back", Vec::new(), 96, 0),
    ];
    for (step, mut changes, pages, height) in steps {
        let access = Access::new(Box::new(store.clone()));
        if changes.is_empty() {
            let (_, second) = edges
                .get(&access, &EDGES, Seek::At(1))
                .map_err(|error| format!("{error:?}"))?
                .ok_or("no second edge page")?;
            let page = edge_page(&access, &second).map_err(|error| format!("{error:?}"))?;
            changes = page
                .0
                .iter()
                .map(|(id, n)| (id.0, -i64::try_from(*n).expect("a count")))
                .collect();
        }
        let delta: BTreeMap<ObjectId, i64> =
            changes.iter().map(|(id, n)| (ObjectId(*id), *n)).collect();
        for (id, n) in &delta {
            let count = model.entry(*id).or_insert(0);
            *count = count.checked_add_signed(*n).expect("a count");
            if *count == 0 {
                model.remove(id);
            }
        }
        edges =
            rewrite_edges(&access, &edges, &delta).map_err(|error| format!("{step}: {error:?}"))?;
        let cold = Access::new(Box::new(store.clone()));
        let mut held = BTreeMap::new();
        edges
            .each(&cold, &EDGES, &mut |_, part| {
                held.extend(edge_page(&cold, part)?.0);
                Ok(false)
            })
            .map_err(|error| format!("{step}: {error:?}"))?;
        if held != model {
            return Err(format!(
                "edges, {step}: the counts read back are not the ones written"
            ));
        }
        edges
            .check(&EDGES)
            .map_err(|error| format!("edges, {step}: {error}"))?;
        if (edges.leaves(), edges.height()) != (pages, height) {
            return Err(format!(
                "edges, {step}: {} pages at height {}, not {pages} at {height}",
                edges.leaves(),
                edges.height()
            ));
        }
    }
    Ok(())
}

/// The list's members, read cold, or the fault that stops the read.
fn list_members(heap: &mut SlotHeap, list: Value) -> Result<Vec<i64>, String> {
    let handle = list.handle().ok_or("no list")?;
    let roots = [list];
    let len = try_warm(heap, &roots, |heap| heap.arena().len(handle))?;
    (0..len)
        .map(
            |at| match try_warm(heap, &roots, |heap| heap.arena().item(handle, at))? {
                Some(Value::Int(i)) => Ok(i),
                other => Err(format!("member {at} is {other:?}")),
            },
        )
        .collect()
}

/// What `op` answers once every chunk it asks for is loaded, or the
/// fault a load or the op meets.
fn try_warm<T>(
    heap: &mut SlotHeap,
    roots: &[Value],
    mut op: impl FnMut(&mut SlotHeap) -> Result<T, NativeError>,
) -> Result<T, String> {
    for _ in 0..=(N as usize).div_ceil(32) {
        match op(heap) {
            Ok(out) => return Ok(out),
            Err(NativeError::Retry(Retry::Cold { object, chunk })) => heap
                .load(object, chunk, &mut |visit| {
                    roots.iter().for_each(&mut *visit)
                })
                .map_err(|error| format!("a load fails: {error:?}"))?,
            Err(other) => return Err(format!("a read fails: {other:?}")),
        }
    }
    Err("a read asked for more loads than its object has chunks".into())
}

/// The list's last members removed until it holds `len`.
fn truncate(heap: &mut SlotHeap, list: Value, len: u64) {
    let handle = list.handle().expect("a list");
    while warm(heap, &[list], |heap| heap.arena().len(handle)) > len {
        let last = warm(heap, &[list], |heap| heap.arena().len(handle)) - 1;
        warm(heap, &[list], |heap| {
            heap.arena_mut().list_remove(handle, last)
        });
    }
}

/// `from .. to` appended to the list.
fn append(heap: &mut SlotHeap, list: Value, from: i64, to: i64) {
    let handle = list.handle().expect("a list");
    for i in from..to {
        warm(heap, &[list], |heap| {
            heap.arena_mut().list_set(handle, i as u64, Value::Int(i))
        });
    }
}

/// One step done to a list in place.
type ListStep = dyn Fn(&mut SlotHeap, Value);

/// Review astra-l65d item 1: a large list of five chunks shrunk to one
/// chunk, grown past it, cleared, and grown again past 256, each
/// step sealed and reopened cold, holds `0 .. len`.
fn list_cleared_case() -> Result<(), String> {
    let store = Counting::default();
    let mut heap = fresh(&store);
    let list = make(
        &mut heap,
        NewObject::List((0..320).map(Value::Int).collect()),
    );
    let steps: [(&str, &ListStep, i64); 4] = [
        (
            "shrunk to one chunk",
            &|heap, list| truncate(heap, list, 64),
            64,
        ),
        (
            "grown past one chunk",
            &|heap, list| append(heap, list, 64, 65),
            65,
        ),
        ("shrunk to zero", &|heap, list| truncate(heap, list, 0), 0),
        (
            "cleared, then grown again",
            &|heap, list| append(heap, list, 0, 320),
            320,
        ),
    ];
    let mut root = heap.seal(&[list]).map_err(|error| format!("{error:?}"))?;
    for (step, edit, len) in steps {
        let (mut heap, list) = reopened(&store, &root);
        edit(&mut heap, list);
        root = heap
            .seal(&[list])
            .map_err(|error| format!("{step}: {error:?}"))?;
        let (mut cold, list) = SlotHeap::open(
            u64::MAX,
            &root,
            Directory::open(root.directory, Box::new(store.clone())),
        )
        .map(|(heap, roots)| (heap, roots[0]))
        .map_err(|error| format!("{step}: the reopen fails: {error:?}"))?;
        let held = list_members(&mut cold, list).map_err(|error| format!("{step}: {error}"))?;
        if held != (0..len).collect::<Vec<_>>() {
            return Err(format!("{step}: {} members, not 0 .. {len}", held.len()));
        }
    }
    Ok(())
}

/// Review astra-l65d item 1, for tables: a map, set or record of 300
/// entries, each entry deleted, then 300 new ones written, each step
/// sealed and reopened cold, holds exactly its entries.
fn table_cleared_case(kind: &str) -> Result<(), String> {
    let key = |i: i64| -> Value {
        match kind {
            "record" => Value::Text(ShortText::new(&format!("k{i}")).expect("short")),
            _ => Value::Int(i),
        }
    };
    let value = |i: i64| -> Value {
        match kind {
            "set" => Value::Absent,
            _ => Value::Int(-i),
        }
    };
    let store = Counting::default();
    let mut heap = fresh(&store);
    let entries: Vec<(Value, Value)> = (0..300).map(|i| (key(i), value(i))).collect();
    let object = match kind {
        "map" => NewObject::Map(entries),
        "record" => NewObject::Record(entries),
        _ => NewObject::Set((0..300).map(key).collect()),
    };
    let table = make(&mut heap, object);
    let mut root = heap.seal(&[table]).map_err(|error| format!("{error:?}"))?;
    for (step, model) in [
        ("shrunk to zero", Vec::new()),
        (
            "cleared, then grown again",
            (1_000..1_300).collect::<Vec<i64>>(),
        ),
    ] {
        let (mut heap, table) = reopened(&store, &root);
        let handle = table.handle().ok_or("no table")?;
        if model.is_empty() {
            for i in 0..300 {
                let gone = key(i);
                warm(&mut heap, &[table], |heap| {
                    heap.arena_mut().table_delete(handle, &gone)
                });
            }
        } else {
            for i in &model {
                warm(&mut heap, &[table], |heap| {
                    heap.arena_mut().table_write(handle, key(*i), value(*i))
                });
            }
        }
        root = heap
            .seal(&[table])
            .map_err(|error| format!("{kind}, {step}: {error:?}"))?;
        let (mut cold, table) = SlotHeap::open(
            u64::MAX,
            &root,
            Directory::open(root.directory, Box::new(store.clone())),
        )
        .map(|(heap, roots)| (heap, roots[0]))
        .map_err(|error| format!("{kind}, {step}: the reopen fails: {error:?}"))?;
        let handle = table.handle().ok_or("no table")?;
        let mut keys = Vec::new();
        let mut last = None;
        while let Some((seq, held)) = try_warm(&mut cold, &[table], |heap| {
            heap.arena().table_next(handle, last)
        })
        .map_err(|error| format!("{kind}, {step}: {error}"))?
        {
            keys.push(number(held));
            last = Some(seq);
        }
        if keys != model.iter().map(|i| Some(*i)).collect::<Vec<_>>() {
            return Err(format!(
                "{kind}, {step}: {} entries, not the {} written",
                keys.len(),
                model.len()
            ));
        }
    }
    Ok(())
}

/// Review astra-l65d item 1, for edge lists: 73,729 ids (97 pages under
/// one index level) all removed, then 73,729 others added, each read back
/// cold: the counts written.
fn edge_cleared_case() -> Result<(), String> {
    let store = Counting::default();
    let mut edges = Index::empty(&EDGES);
    let built: BTreeMap<ObjectId, i64> = (0..73_729).map(|i| (ObjectId(2 * i), 1)).collect();
    let cleared: BTreeMap<ObjectId, i64> = built.keys().map(|id| (*id, -1)).collect();
    let regrown: BTreeMap<ObjectId, i64> = (0..73_729).map(|i| (ObjectId(2 * i + 1), 2)).collect();
    for (step, delta) in [
        ("built", &built),
        ("shrunk to zero", &cleared),
        ("cleared, then grown again", &regrown),
    ] {
        let access = Access::new(Box::new(store.clone()));
        edges =
            rewrite_edges(&access, &edges, delta).map_err(|error| format!("{step}: {error:?}"))?;
        edges
            .check(&EDGES)
            .map_err(|error| format!("edges, {step}: {error}"))?;
        let cold = Access::new(Box::new(store.clone()));
        let mut held = BTreeMap::new();
        edges
            .each(&cold, &EDGES, &mut |_, part| {
                held.extend(edge_page(&cold, part)?.0);
                Ok(false)
            })
            .map_err(|error| format!("{step}: {error:?}"))?;
        let model: BTreeMap<ObjectId, u64> = match step {
            "shrunk to zero" => BTreeMap::new(),
            _ => delta.iter().map(|(id, n)| (*id, *n as u64)).collect(),
        };
        if held != model {
            return Err(format!(
                "edges, {step}: {} ids read back, not {}",
                held.len(),
                model.len()
            ));
        }
    }
    Ok(())
}

/// The run record case: a parked run whose encoded record takes 96, 97
/// and 96 segments, then one (its nearest to cleared) and 97 again,
/// written and read back cold each time: its state whole, and its
/// segment index at height 0, 1, 0, 0 and 1.
fn run_case() -> Result<(), String> {
    use lash_kernel_state::paged::run::continuation::{self, RunPage, SEGMENT_BYTES};
    use lash_kernel_value::tokens::{decode_body, encode_body};
    let mut embedder =
        super::embedder::Embedder::new("main { return perform 0(\"pause\") as Any }");
    if !matches!(embedder.run(), crate::Step::Parked(_)) {
        return Err("the run did not park".into());
    }
    let mut record = embedder.run_record();
    let store = Counting {
        pages: embedder.source.clone(),
        ..Counting::default()
    };
    record.sites.push(Vec::new());
    let mut previous = None;
    for (count, height) in [(96usize, 0u8), (97, 1), (96, 0), (1, 0), (97, 1)] {
        // Two bytes per zero coordinate, half a segment past `count - 1`.
        *record.sites.last_mut().expect("a site table") =
            vec![0; ((count - 1) * SEGMENT_BYTES + SEGMENT_BYTES / 2) / 2];
        if encode_body(&record).len().div_ceil(SEGMENT_BYTES) != count {
            return Err(format!("the record does not take {count} segments"));
        }
        let access = Access::new(Box::new(store.clone()));
        // Written over the run root before it, it is the run written
        // anew: its segment index is full, one shape for its count.
        let page = continuation::write(&access, &record, previous)
            .map_err(|error| format!("{error:?}"))?;
        let anew =
            continuation::write(&access, &record, None).map_err(|error| format!("{error:?}"))?;
        if page.id != anew.id {
            return Err(format!(
                "run of {count} segments: written over its last root, it is not the run written anew"
            ));
        }
        previous = Some(page);
        let cold = Access::new(Box::new(store.clone()));
        let root: RunPage = decode_body(
            &cold
                .read(page)
                .map_err(|error| format!("{error:?}"))?
                .tokens,
        )
        .map_err(|error| format!("{error:?}"))?;
        if (root.segments.leaves(), root.segments.height()) != (count as u64, height) {
            return Err(format!(
                "run of {count} segments: {} at height {}, not {count} at {height}",
                root.segments.leaves(),
                root.segments.height()
            ));
        }
        let opened = continuation::read(&cold, page).map_err(|error| format!("{error:?}"))?;
        if opened.sites != record.sites {
            return Err(format!(
                "run of {count} segments: its state reads back otherwise"
            ));
        }
    }
    Ok(())
}

/// The index level the objects above do not reach: a full index (a
/// tuple's, a run's) of 9,216 leaves, 96 full pages under its top, and a
/// filled one (a list's, a table's, an edge list's) of 6,912, 96 pages of
/// 72, are grown by one leaf, to height 2, and shrunk back, to height 1,
/// each read back cold whole, a full one's pages those its direct build
/// seals.
fn index_case() -> Result<(), String> {
    use lash_kernel_state::paged::PageKind;
    use lash_kernel_state::paged::run::continuation::{SEGMENTS, Segment};
    use lash_kernel_state::paged::run::index::Shape;
    let filled = Shape {
        full: false,
        ..SEGMENTS
    };
    for (name, shape, n) in [("full", SEGMENTS, 9_216u64), ("filled", filled, 6_912)] {
        let store = Counting::default();
        let access = Access::new(Box::new(store.clone()));
        let leaves: Vec<Segment> = (0..=n)
            .map(|i| {
                access
                    .seal(PageKind::RUN_SEGMENT, i.to_le_bytes().to_vec(), Vec::new())
                    .map(Segment)
            })
            .collect::<Result<_, _>>()
            .map_err(|error| format!("{error:?}"))?;
        let at = |len: u64| leaves[..len as usize].to_vec();
        let built = |len: u64| Index::build(&access, &shape, at(len));
        let top = |index: &Index<Segment>| match index {
            Index::Leaves(leaves) => leaves.iter().map(|leaf| (1, leaf.0.id)).collect::<Vec<_>>(),
            Index::Nodes { refs, .. } => {
                refs.iter().map(|part| (part.count, part.page.id)).collect()
            }
        };
        let mut index = built(n).map_err(|error| format!("{error:?}"))?;
        for (step, len, height) in [("grown", n + 1, 2u8), ("shrunk back", n, 1)] {
            index = if len > n {
                index.rebuild(
                    &access,
                    &shape,
                    &BTreeSet::new(),
                    &mut |_, leaf| Ok(vec![leaf]),
                    at(len)[n as usize..].to_vec(),
                )
            } else {
                index.rebuild(
                    &access,
                    &shape,
                    &BTreeSet::from([n]),
                    &mut |_, _| Ok(Vec::new()),
                    Vec::new(),
                )
            }
            .map_err(|error| format!("{name}, {step}: {error:?}"))?;
            index
                .check(&shape)
                .map_err(|error| format!("{name}, {step}: {error}"))?;
            let cold = Access::new(Box::new(store.clone()));
            let mut held = Vec::new();
            index
                .each(&cold, &shape, &mut |_, leaf| {
                    held.push(*leaf);
                    Ok(false)
                })
                .map_err(|error| format!("{name}, {step}: {error:?}"))?;
            if held != at(len) || index.height() != height {
                return Err(format!(
                    "{name}, {step}: {} leaves at height {}, not {len} at {height}",
                    held.len(),
                    index.height()
                ));
            }
            let direct = built(len).map_err(|error| format!("{error:?}"))?;
            if shape.full && top(&index) != top(&direct) {
                return Err(format!(
                    "{name}, {step}: the pages are not the direct build's"
                ));
            }
        }
    }
    Ok(())
}

/// Contract §6 index rule (review astra-l65c items 2 and 3; lead ruling,
/// l65e item 4): an object grown by one across each boundary its
/// structure has, the chunk count and each index height, and shrunk
/// back, sealed and reopened cold at each step, holds what was written.
/// A mutable container (a list, map, set, record or edge list) is
/// identified by its object id, so its layout may depend on its history:
/// its row is [`well_formed`], each index at the height its leaves give,
/// and need not be a direct build's. Slack it may keep: a chunk with at
/// least one member, and an index page with at least half of `NODE_REFS`
/// references, or fewer when it is the last of its level, also when a map
/// is thinned to one chunk in each index page, regrows below its first
/// surviving boundary after its first four chunks are emptied, and when an index of
/// height 2 is thinned to one leaf under an upper page. A tuple
/// is content-addressed: built in a store with history, sealed again
/// cold, or copied from its cold members, inline and at index heights 1
/// and 2, its record is the direct build's and its key
/// the key of the tuple built anew. The run record, its segment index
/// full, written over its last root is the run written anew. The index's
/// second level is grown and shrunk for both widths at the index itself.
#[test]
fn an_object_grown_and_shrunk_across_each_boundary_keeps_its_contents_and_a_tuple_is_its_direct_build()
 {
    let mut failed = Vec::new();
    // Five chunks to six; 6,912 chunks (96 index pages of 72) to 6,913.
    for (n, heights) in [(5 * 64, (1, 1)), (6_912 * 64, (1, 2))] {
        if let Err(error) = list_case(n, heights) {
            failed.push(format!("list: {error}"));
        }
    }
    // Seven chunks to eight; a map and a record of 6,912 chunks (96 index
    // pages of 72) to 6,913.
    for (kind, start, heights) in [
        ("map", 300, (1, 1)),
        ("set", 300, (1, 1)),
        ("record", 300, (1, 1)),
        ("map", 331_776, (1, 2)),
        ("record", 331_776, (1, 2)),
    ] {
        if let Err(error) = table_case(kind, start, heights) {
            failed.push(error);
        }
    }
    for kind in ["map", "set", "record"] {
        if let Err(error) = table_cleared_case(kind) {
            failed.push(error);
        }
    }
    for (kind, is_set) in [("map", false), ("set", true)] {
        if let Err(error) = occupancy::table_regrown_below_first_case(is_set) {
            failed.push(format!("{kind}, regrown below first: {error}"));
        }
    }
    if let Err(error) = list_cleared_case() {
        failed.push(format!("list: {error}"));
    }
    if let Err(error) = occupancy::table_thinned_case() {
        failed.push(format!("map: {error}"));
    }
    if let Err(error) = occupancy::index_thinned_case() {
        failed.push(format!("index thinned: {error}"));
    }
    // Inline; one index level from 257 members; 96 full index pages of
    // 96 chunks of 64 members to one chunk more.
    for (n, height) in [
        (256, None),
        (257, Some(1)),
        (589_824, Some(1)),
        (589_825, Some(2)),
    ] {
        if let Err(error) = tuple_case(n, height) {
            failed.push(format!("tuple: {error}"));
        }
    }
    if let Err(error) = edge_cleared_case() {
        failed.push(error);
    }
    for (name, case) in [
        ("edges", edge_case as fn() -> Result<(), String>),
        ("run", run_case),
        ("index", index_case),
    ] {
        if let Err(error) = case() {
            failed.push(format!("{name}: {error}"));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// An edit in one segment of a run seals that segment's index path and no
/// other index page. Through the run writer, at heights 0 and 1, the
/// writer seals every segment again (it encodes the record whole, and an
/// unchanged segment is the same page), the one changed index page and
/// the root: 2 + 0 + 1 and 97 + 1 + 1. At height 2, through the index
/// alone, it seals one page for each level below the embedded top. A
/// large object's chunk edits are counted at each height by
/// [`a_point_edit_in_a_four_deep_directory_seals_within_its_fixture_ceiling`].
#[test]
fn an_edit_in_one_segment_seals_only_its_index_path() {
    use lash_kernel_state::paged::PageKind;
    use lash_kernel_state::paged::run::continuation::{
        self, RunPage, SEGMENT_BYTES, SEGMENTS, Segment,
    };
    use lash_kernel_value::tokens::decode_body;
    let mut embedder =
        super::embedder::Embedder::new("main { return perform 0(\"pause\") as Any }");
    assert!(matches!(embedder.run(), crate::Step::Parked(_)));
    let mut record = embedder.run_record();
    let store = Counting {
        pages: embedder.source.clone(),
        ..Counting::default()
    };
    let access = Access::new(Box::new(store.clone()));
    let top = |page| -> Vec<_> {
        let root: RunPage = decode_body(&access.read(page).unwrap().tokens).unwrap();
        match root.segments {
            Index::Leaves(leaves) => leaves.iter().map(|leaf| leaf.0.id).collect(),
            Index::Nodes { refs, .. } => refs.iter().map(|part| part.page.id).collect(),
        }
    };
    record.sites.push(Vec::new());
    for (count, index_pages) in [(2usize, 0u64), (97, 1)] {
        // Two bytes per zero coordinate, half a segment past `count - 1`.
        *record.sites.last_mut().unwrap() =
            vec![0; ((count - 1) * SEGMENT_BYTES + SEGMENT_BYTES / 2) / 2];
        let before = continuation::write(&access, &record, None).unwrap();
        *record.sites.last_mut().unwrap().last_mut().unwrap() = 1;
        let (after, _, writes) =
            store.during(|| continuation::write(&access, &record, Some(before)).unwrap());
        assert_eq!(
            writes,
            count as u64 + index_pages + 1,
            "an edit in the last of {count} segments"
        );
        let (old, new) = (top(before), top(after));
        let kept = old.len() - 1;
        assert_eq!((old.len(), &old[..kept]), (new.len(), &new[..kept]));
        assert_ne!(old[kept], new[kept]);
        let cold = Access::new(Box::new(store.clone()));
        assert_eq!(
            continuation::read(&cold, after).unwrap().sites,
            record.sites
        );
    }
    let leaves: Vec<Segment> = (0..9_217u64)
        .map(|i| {
            access
                .seal(PageKind::RUN_SEGMENT, i.to_le_bytes().to_vec(), Vec::new())
                .map(Segment)
        })
        .collect::<Result<_, _>>()
        .unwrap();
    let index = Index::build(&access, &SEGMENTS, leaves.clone()).unwrap();
    assert_eq!(index.height(), 2);
    let mut edited = leaves;
    edited[9_000] = edited[0];
    let (revised, _, writes) =
        store.during(|| index.revise(&access, &SEGMENTS, edited.clone()).unwrap());
    assert_eq!(writes, 2, "an edit at height 2");
    assert_eq!(revised, Index::build(&access, &SEGMENTS, edited).unwrap());
}

/// Schemasmash K03, `docs/kernel/semantics.md` (admission before
/// placement): a cold load of a large map's chunk either refuses for
/// memory or leaves the heap holding no more than its bound. Swept over
/// every bound from what the open row holds to what the loaded chunk
/// holds with ample room.
#[test]
fn a_cold_map_chunk_load_never_settles_past_the_bound() {
    const PAIRS: i64 = 2_000;
    let store = Counting::default();
    let mut heap = fresh(&store);
    let map = make(
        &mut heap,
        NewObject::Map(
            (0..PAIRS)
                .map(|i| (Value::Int(i), Value::Int(i * i)))
                .collect(),
        ),
    );
    let root = heap.seal(&[map]).expect("a seal");
    // Opens `root` under `bound` with the map's row and first chunk read:
    // the heap, the map, and what loading chunk 1 then answers.
    let attempt = |bound: u64| {
        let (mut heap, roots) = SlotHeap::open(
            bound,
            &root,
            Directory::open(root.directory, Box::new(store.clone())),
        )
        .ok()?;
        let map = roots[0];
        let handle = map.handle().expect("a map");
        heap.load(handle, 0, &mut |visit| visit(&map)).ok()?;
        let before = heap.held();
        let loaded = heap.load(handle, 1, &mut |visit| visit(&map));
        Some((before, loaded.is_ok(), heap.held()))
    };
    let (row_held, loaded, all_held) = attempt(u64::MAX).expect("an ample open");
    assert!(
        loaded && all_held > row_held,
        "chunk 1 is cold after the row"
    );
    let mut over = Vec::new();
    let mut admitted = 0;
    for bound in row_held..=all_held {
        if let Some((_, true, held)) = attempt(bound) {
            admitted += 1;
            if held > bound {
                over.push((bound, held));
            }
        }
    }
    assert!(admitted > 0, "no bound admits the load");
    assert!(
        over.is_empty(),
        "{} of {} bounds in {row_held}..={all_held} admitted a load settling past the bound; first {:?}, last {:?}",
        over.len(),
        all_held - row_held + 1,
        over.first(),
        over.last()
    );
}
