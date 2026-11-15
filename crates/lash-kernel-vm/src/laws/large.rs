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
use lash_kernel_state::paged::run::chunked::{CHUNK, EDGE_REFS};
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::index::NODE_REFS;
use lash_kernel_state::paged::run::root::HeapRoot;
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};

use super::embedder::Source;
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;

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
/// level (nodes over pages 0–95, 96–191, 192–260); d = 4. `heap.seal`
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
    use std::collections::BTreeSet;
    let n = N as u64;
    let fanout = NODE_REFS as u64;
    // An edge list past one page is cut into pages three quarters full.
    let edge_width = EDGE_REFS as u64 * 3 / 4;
    // Index levels below a row over `leaves` pages: none for up to one
    // node's worth, which the row holds; else one per factor of the fanout.
    let levels = |leaves: u64| {
        let mut levels = 0;
        let mut top = leaves;
        while top > fanout {
            top = top.div_ceil(fanout);
            levels += 1;
        }
        levels
    };
    // C: the one chunk both edited positions share.
    let c = 1;
    // I: the chunk index path, one page per level below the row
    // (3,125 chunks: one level).
    let i_index = levels(n.div_ceil(CHUNK as u64));
    // E: the changed edge pages, and the distinct edge index pages on
    // their paths, one set per level below the row (261 edge pages: one
    // level).
    let edge_pages: BTreeSet<u64> = changed
        .iter()
        .map(|&at| u64::try_from(at).expect("a position") / edge_width)
        .collect();
    let edge_levels = levels(n.div_ceil(edge_width));
    let mut e = edge_pages.len() as u64;
    let mut nodes = edge_pages;
    for _ in 0..edge_levels {
        nodes = nodes.iter().map(|page| page / fanout).collect();
        e += nodes.len() as u64;
    }
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
