//! The durable heap (cutover contract §6): which rows a seal writes, under
//! which ids, and what a failed seal leaves for the next; what a load
//! decodes, admits and refuses.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{Args, NativeError, NewObject, Retry};
use lash_kernel_doc::heap::{HeapFault, IntSummary, PageRef, TextSummary};
use lash_kernel_doc::{Float, Integer};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::arena::Summary;
use lash_kernel_state::paged::run::directory::Row;
use lash_kernel_state::paged::run::directory::{Directory, DirectoryRoot};
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::objects::{Head, Heads, ObjectKind, Record, RunValue};
use lash_kernel_state::paged::run::root::HeapRoot;
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};

use super::embedder::Source;

/// Seals what `roots` reach, with no large object opened before.
fn seal(
    arena: &mut Arena,
    roots: &[Value],
    directory: &mut Directory,
    high_water: &mut HighWater,
) -> Result<HeapRoot, SealError> {
    let mut large = crate::heap::large::Large::default();
    crate::heap::seal::seal(
        arena,
        roots,
        directory,
        high_water,
        &mut large,
        &mut Default::default(),
    )
    .map(|(root, _)| root)
}
use crate::heap::slots::SlotHeap;
use crate::heap::{Heap, LoadError};
use crate::paged::SealError;

/// Content-addressed pages that refuse writes while `refuse` is set.
struct Store {
    pages: Source,
    refuse: Rc<Cell<bool>>,
}

impl PageSource for Store {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.pages.read(pages)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        if self.refuse.get() {
            return Err(HeapFault::LeaseLost);
        }
        self.pages.seal(body)
    }
}

/// One store's pages, and whether it refuses writes.
#[derive(Clone, Default)]
struct Pages {
    pages: Rc<RefCell<BTreeMap<lash_kernel_doc::heap::PageId, PageAnswer>>>,
    refuse: Rc<Cell<bool>>,
}

impl Pages {
    /// A directory over these pages: empty, or opened at `root`.
    fn directory(&self, root: Option<DirectoryRoot>) -> Directory {
        let store = Box::new(Store {
            pages: Source(self.pages.clone()),
            refuse: self.refuse.clone(),
        });
        match root {
            None => Directory::new(store),
            Some(root) => Directory::open(root, store),
        }
    }
}

fn list(arena: &mut Arena, members: Vec<Value>) -> Value {
    arena.create(NewObject::List(members)).expect("a list")
}

fn row(directory: &Directory, id: u64) -> Record {
    directory
        .get(ObjectId(id))
        .expect("the directory reads")
        .expect("the row is written")
        .record
}

const LONG: &str = "a text too long to be held inline in a value";

/// Ids follow the canonical walk, depth first and pre-order from the roots,
/// whatever order the objects were allocated in, and a shared object keeps
/// one id however many members hold it.
#[test]
fn ids_follow_the_walk_from_the_roots_not_allocation_order() {
    let members_first = {
        let mut arena = Arena::new();
        let text = arena.text(LONG);
        let tuple = arena.tuple(vec![text, Value::Int(1)], 64).expect("a tuple");
        let inner = list(&mut arena, vec![tuple]);
        let root = list(&mut arena, vec![inner, tuple, tuple]);
        (arena, root)
    };
    let roots_first = {
        let mut arena = Arena::new();
        let root = list(&mut arena, vec![Value::Null; 3]);
        let inner = list(&mut arena, vec![Value::Null]);
        let text = arena.text(LONG);
        let tuple = arena.tuple(vec![text, Value::Int(1)], 64).expect("a tuple");
        let handle = root.handle().expect("a list");
        arena.list_set(handle, 0, inner).expect("a write");
        arena.list_set(handle, 1, tuple).expect("a write");
        arena.list_set(handle, 2, tuple).expect("a write");
        arena
            .list_set(inner.handle().expect("a list"), 0, tuple)
            .expect("a write");
        (arena, root)
    };

    let mut sealed = Vec::new();
    for (mut arena, root) in [members_first, roots_first] {
        let mut directory = Pages::default().directory(None);
        let mut high_water = HighWater(0);
        let heap = seal(&mut arena, &[root], &mut directory, &mut high_water).expect("a seal");
        // root 0, inner 1, the tuple 2 (once), its text 3.
        assert_eq!(
            heap.roots,
            vec![RunValue::Object(ObjectKind::List, ObjectId(0))]
        );
        assert_eq!(
            row(&directory, 0),
            Record::List(vec![
                RunValue::Object(ObjectKind::List, ObjectId(1)),
                RunValue::Object(ObjectKind::Tuple, ObjectId(2)),
                RunValue::Object(ObjectKind::Tuple, ObjectId(2)),
            ])
        );
        assert_eq!(
            row(&directory, 2),
            Record::Tuple(vec![
                RunValue::Object(ObjectKind::LongText, ObjectId(3)),
                RunValue::Int(1),
            ])
        );
        assert_eq!(row(&directory, 3), Record::LongText(LONG.into()));
        assert_eq!(high_water, HighWater(4));
        sealed.push(heap);
    }
    assert_eq!(sealed[0], sealed[1]);
}

/// A reseal writes an object that changed even when every object between
/// it and the roots did not: the walk stops at unchanged objects, so the
/// changed one is found by its dirtiness, not by reaching it.
#[test]
fn a_reseal_writes_a_change_beneath_an_unchanged_object() {
    let mut arena = Arena::new();
    let leaf = list(&mut arena, vec![Value::Int(1)]);
    let root = list(&mut arena, vec![leaf]);
    let mut directory = Pages::default().directory(None);
    let mut high_water = HighWater(0);
    seal(&mut arena, &[root], &mut directory, &mut high_water).expect("a seal");

    arena
        .list_set(leaf.handle().expect("a list"), 0, Value::Int(2))
        .expect("a write");
    seal(&mut arena, &[root], &mut directory, &mut high_water).expect("a reseal");
    assert_eq!(row(&directory, 1), Record::List(vec![RunValue::Int(2)]));
    assert_eq!(high_water, HighWater(2), "the reseal mints no id");
}

/// A seal that fails to write its pages leaves every change it saw dirty,
/// so the next seal writes them: nothing is marked clean before the
/// directory is sealed.
#[test]
fn a_failed_seal_leaves_its_changes_for_the_next() {
    let mut arena = Arena::new();
    let leaf = list(&mut arena, vec![Value::Int(1)]);
    let root = list(&mut arena, vec![leaf]);
    let pages = Pages::default();
    let mut directory = pages.directory(None);
    let mut high_water = HighWater(0);
    let committed = seal(&mut arena, &[root], &mut directory, &mut high_water).expect("a seal");

    arena
        .list_set(leaf.handle().expect("a list"), 0, Value::Int(2))
        .expect("a write");
    let fresh = list(&mut arena, vec![Value::Int(3)]);
    arena
        .list_set(root.handle().expect("a list"), 0, fresh)
        .expect("a write");
    pages.refuse.set(true);
    assert!(matches!(
        seal(&mut arena, &[root], &mut directory, &mut high_water),
        Err(SealError::Heap(HeapFault::LeaseLost))
    ));

    // The run recovers its directory from the committed root. The failed
    // seal minted 2 for `fresh`; the leaf hangs off a second root so that it
    // stays reachable.
    pages.refuse.set(false);
    let mut reopened = pages.directory(Some(committed.directory));
    let heap = seal(&mut arena, &[root, leaf], &mut reopened, &mut high_water).expect("a seal");
    assert_eq!(
        heap.roots,
        vec![
            RunValue::Object(ObjectKind::List, ObjectId(0)),
            RunValue::Object(ObjectKind::List, ObjectId(1)),
        ]
    );
    assert_eq!(
        row(&reopened, 0),
        Record::List(vec![RunValue::Object(ObjectKind::List, ObjectId(2))])
    );
    assert_eq!(row(&reopened, 1), Record::List(vec![RunValue::Int(2)]));
    assert_eq!(row(&reopened, 2), Record::List(vec![RunValue::Int(3)]));
}

/// A list of `members`, sealed as the one root of a fresh store, and that
/// store's pages.
fn sealed(build: impl FnOnce(&mut Arena) -> Value) -> (Pages, HeapRoot) {
    let mut arena = Arena::new();
    let root = build(&mut arena);
    let pages = Pages::default();
    let mut directory = pages.directory(None);
    let heap = seal(&mut arena, &[root], &mut directory, &mut HighWater(0)).expect("a seal");
    (pages, heap)
}

fn opened(pages: &Pages, root: &HeapRoot, bound: u64) -> (SlotHeap, Value) {
    let (heap, roots) =
        SlotHeap::open(bound, root, pages.directory(Some(root.directory))).expect("an open");
    (heap, roots[0])
}

fn cold(heap: &SlotHeap, value: Value) -> bool {
    heap.arena()
        .facts(value.handle().expect("an object"))
        .expect("a live object")
        .cold
}

fn members(heap: &SlotHeap, value: Value) -> Vec<Value> {
    heap.arena()
        .semantics()
        .sequence(value.handle().expect("a list"))
        .expect("a resident list")
        .to_vec()
}

fn load(heap: &mut SlotHeap, value: Value, roots: &[Value]) -> Result<(), LoadError> {
    heap.load(value.handle().expect("an object"), 0, &mut |visit| {
        roots.iter().for_each(&mut *visit)
    })
}

/// A load decodes its object's row only: what the row references opens as
/// proxies, and an object two rows reference opens once.
#[test]
fn a_load_decodes_one_row_and_its_referents_stay_cold() {
    let (pages, root) = sealed(|arena| {
        let shared = list(arena, vec![Value::Int(1), Value::Int(2)]);
        let other = list(arena, vec![shared]);
        list(arena, vec![shared, Value::Int(5), other])
    });
    let (mut heap, root) = opened(&pages, &root, u64::MAX);
    assert!(cold(&heap, root));

    load(&mut heap, root, &[root]).expect("a load");
    let [shared, five, other] = members(&heap, root)[..] else {
        panic!("three members");
    };
    assert!(matches!(five, Value::Int(5)));
    assert!(
        cold(&heap, shared) && cold(&heap, other),
        "referents stay proxies"
    );

    load(&mut heap, other, &[root]).expect("a load");
    let [again] = members(&heap, other)[..] else {
        panic!("one member");
    };
    assert_eq!(again.handle(), shared.handle(), "one object, one handle");
    assert!(cold(&heap, shared));

    load(&mut heap, shared, &[root]).expect("a load");
    assert!(matches!(
        members(&heap, shared)[..],
        [Value::Int(1), Value::Int(2)]
    ));
}

/// Two members, each a one-member list: what the heap holds once the root
/// is loaded is its body (an object of two values) and a proxy object per
/// member; the root's own proxy is replaced.
const ROOT_LOADED: u64 = 24 + 2 * 16 + 2 * 24;

fn pair() -> (Pages, HeapRoot) {
    sealed(|arena| {
        let first = list(arena, vec![Value::Int(1)]);
        let second = list(arena, vec![Value::Int(2)]);
        list(arena, vec![first, second])
    })
}

/// A load the bound refuses collects what its roots do not reach, and asks
/// again.
#[test]
fn a_load_collects_and_asks_again_before_it_refuses() {
    let (pages, root) = pair();
    let (mut heap, root) = opened(&pages, &root, ROOT_LOADED);
    let garbage = NewObject::List(vec![Value::Int(0)]);
    heap.admit(crate::heap::slots::schedule(&garbage))
        .expect("room for garbage");
    heap.create(garbage).expect("garbage");

    load(&mut heap, root, &[root]).expect("a load after a collection");
    assert!(!cold(&heap, root));
    assert_eq!(heap.held(), ROOT_LOADED);
}

/// A load the bound refuses after a collection is the guest's memory
/// refusal, and leaves the heap as it was: no body, no proxies, no units.
#[test]
fn a_refused_load_changes_nothing() {
    let (pages, root) = pair();
    let (mut heap, root) = opened(&pages, &root, ROOT_LOADED - 1);
    let held = heap.held();
    assert!(matches!(
        load(&mut heap, root, &[root]),
        Err(LoadError::Memory(_))
    ));
    assert!(cold(&heap, root));
    assert_eq!(heap.held(), held);
    for id in [1, 2] {
        assert!(
            heap.arena()
                .entity_handle(crate::heap::open::entity(ObjectId(id)))
                .is_none()
        );
    }
}

/// A row the store cannot give is an infrastructure failure, not a memory
/// refusal, and the object stays cold.
#[test]
fn a_store_fault_is_not_a_memory_refusal() {
    let (pages, root) = pair();
    let (mut heap, root) = opened(&pages, &root, u64::MAX);
    pages.pages.borrow_mut().clear();
    assert!(matches!(
        load(&mut heap, root, &[root]),
        Err(LoadError::Store(HeapFault::Missing { .. }))
    ));
    assert!(cold(&heap, root));
}

/// Paging a heap out and loading it back holds exactly what it held: each
/// body replaces its proxy, and a shared object comes back once.
#[test]
fn paging_out_and_loading_back_holds_what_the_bodies_hold() {
    let mut heap = SlotHeap::durable(
        u64::MAX,
        crate::heap::open::Durable {
            directory: Pages::default().directory(None),
            high_water: HighWater(0),
        },
    );
    let create = |heap: &mut SlotHeap, object: NewObject| {
        heap.admit(crate::heap::slots::schedule(&object))
            .expect("room");
        heap.create(object).expect("an object")
    };
    let shared = create(
        &mut heap,
        NewObject::List(vec![Value::Int(1), Value::Int(2)]),
    );
    let root = create(
        &mut heap,
        NewObject::List(vec![shared, Value::Int(3), shared]),
    );
    let resident = heap.held();
    assert_eq!(resident, (24 + 2 * 16) + (24 + 3 * 16));

    heap.page_out(&[root]).expect("a page-out");
    assert!(cold(&heap, root) && cold(&heap, shared));
    assert_eq!(heap.held(), 2 * 24, "two proxies");

    load(&mut heap, root, &[root]).expect("a load");
    load(&mut heap, shared, &[root]).expect("a load");
    let [first, three, second] = members(&heap, root)[..] else {
        panic!("three members");
    };
    assert!(matches!(three, Value::Int(3)));
    assert_eq!(first.handle(), shared.handle());
    assert_eq!(second.handle(), shared.handle());
    assert!(matches!(
        members(&heap, shared)[..],
        [Value::Int(1), Value::Int(2)]
    ));
    assert_eq!(heap.held(), resident);
}

/// A heap that seals to a fresh store, and that store's pages.
fn durable() -> (SlotHeap, Pages) {
    let pages = Pages::default();
    let heap = SlotHeap::durable(
        u64::MAX,
        crate::heap::open::Durable {
            directory: pages.directory(None),
            high_water: HighWater(0),
        },
    );
    (heap, pages)
}

fn make(heap: &mut SlotHeap, object: NewObject) -> Value {
    heap.admit(crate::heap::slots::schedule(&object))
        .expect("room");
    heap.create(object).expect("an object")
}

const UNICODE: &str = "ünïcødé, then 😀 past the inline bound";
/// -(2^100): 101 bits of magnitude.
const BIG: &str = "-1267650600228229401496703205376";
/// 2^70 nanoseconds: 71 bits.
const LATE: &str = "1180591620717411303424";
const BYTES: [u8; 20] = [7; 20];

/// The six immutable kinds a row names by head, in this order: a Unicode
/// text, a bigint, a timestamp past a word, bytes, a tuple nesting a tuple
/// and a list, and an error whose data holds that list. The list is the
/// root's last member, so the error's data aliases it.
struct Six {
    root: Value,
    members: [Value; 6],
    list: Value,
}

fn six(heap: &mut SlotHeap) -> Six {
    let text = make(heap, NewObject::LongText(UNICODE.into()));
    let big = make(
        heap,
        NewObject::BigInt(Integer::parse(BIG).expect("an int")),
    );
    let late = make(
        heap,
        NewObject::Timestamp(Integer::parse(LATE).expect("an int")),
    );
    let bytes = make(heap, NewObject::Bytes(BYTES.into()));
    let list = make(heap, NewObject::List(vec![Value::Int(1)]));
    let inner = make(heap, NewObject::Tuple(vec![text, Value::Int(1)]));
    let tuple = make(heap, NewObject::Tuple(vec![inner, list]));
    let data = make(
        heap,
        NewObject::Record(vec![(
            Value::Text(lash_kernel_doc::arena::ShortText::new("list").expect("short")),
            list,
        )]),
    );
    let error = heap.error("value", "boom", data).expect("an error");
    let members = [text, big, late, bytes, tuple, error];
    let mut all = members.to_vec();
    all.push(list);
    let root = make(heap, NewObject::List(all));
    Six {
        root,
        members,
        list,
    }
}

/// [`six`], sealed and reopened from its pages with its root loaded:
/// every member is a cold proxy opened from the head the root's row names
/// it by, and the store's pages.
fn six_cold() -> (SlotHeap, Six, Pages) {
    let (mut heap, pages) = durable();
    let six = six(&mut heap);
    let sealed = heap.seal(&[six.root]).expect("a seal");
    let (mut heap, root) = opened(&pages, &sealed, u64::MAX);
    load(&mut heap, root, &[root]).expect("a load");
    let [text, big, late, bytes, tuple, error, list] = members(&heap, root)[..] else {
        panic!("seven members");
    };
    let members = [text, big, late, bytes, tuple, error];
    for member in members {
        assert!(
            heap.arena()
                .needs_decode(member.handle().expect("an object"))
                .expect("live")
        );
    }
    (
        heap,
        Six {
            root,
            members,
            list,
        },
        pages,
    )
}

fn head(heap: &SlotHeap, value: Value) -> Head {
    heap.arena()
        .head(value.handle().expect("an object"))
        .expect("a head")
}

fn counts(text: &str) -> TextSummary {
    TextSummary {
        utf8_bytes: text.len() as u64,
        chars: text.chars().count() as u64,
        utf16_units: text.encode_utf16().count() as u64,
    }
}

/// Astra §5: the head a row names an immutable by says of it what its
/// resident body does: Unicode counts as `str` counts them, the bigint's
/// magnitude, nesting depth, and whether it holds an edge. A load checks
/// the row's body against it before the run sees it, so loading each
/// agrees. Mutating the list a tuple and an error hold leaves their heads
/// as they were: heads describe immutable structure.
#[test]
fn immutable_head_summary_equals_resident_body() {
    let (mut heap, six, pages) = six_cold();
    let [text, big, late, bytes, tuple, error] = six.members;
    let Head::LongText { summary, .. } = head(&heap, text) else {
        panic!("a text head");
    };
    assert_eq!(summary, counts(UNICODE));
    let Head::BigInt { summary, .. } = head(&heap, big) else {
        panic!("a bigint head");
    };
    assert_eq!(
        summary,
        IntSummary {
            bits: 101,
            negative: true
        }
    );
    let Head::Timestamp { summary, .. } = head(&heap, late) else {
        panic!("a timestamp head");
    };
    assert_eq!(
        summary,
        IntSummary {
            bits: 71,
            negative: false
        }
    );
    let Head::Bytes { len, .. } = head(&heap, bytes) else {
        panic!("a bytes head");
    };
    assert_eq!(len, 20);
    let edges = |depth| Summary {
        depth,
        task_edges: 0,
        has_edges: true,
    };
    // (( text, 1 ), list): two tuples deep, and a list is no key.
    assert_eq!(
        head(&heap, tuple),
        Head::Tuple {
            len: 2,
            shape: edges(2),
            normal: None,
        }
    );
    // The error over a data record holding the list.
    assert_eq!(head(&heap, error), Head::Error(edges(1)));

    let heads = six.members.map(|member| head(&heap, member));
    for member in six.members {
        load(&mut heap, member, &[six.root]).expect("a load agrees with its head");
    }
    assert_eq!(
        heap.arena().semantics().text(&text).expect("resident"),
        UNICODE
    );

    load(&mut heap, six.list, &[six.root]).expect("a load");
    let list = six.list.handle().expect("a list");
    let deeper = make(&mut heap, NewObject::Tuple(vec![tuple]));
    heap.arena_mut().list_set(list, 0, deeper).expect("a write");
    let resealed = heap.seal(&[six.root]).expect("a reseal");
    let (mut heap, root) = opened(&pages, &resealed, u64::MAX);
    load(&mut heap, root, &[root]).expect("a load");
    for (member, held) in members(&heap, root).iter().zip(&heads) {
        assert_eq!(&head(&heap, *member), held);
    }
}

fn retry_on(result: Result<impl std::fmt::Debug, NativeError>, object: Value) {
    match result {
        Err(NativeError::Retry(Retry::Cold { object: cold, .. })) => {
            assert_eq!(Some(cold), object.handle());
        }
        other => panic!("a payload read answered {other:?}"),
    }
}

/// Astra §3: every read of a cold immutable's payload answers a retry for
/// that object, never a placeholder's contents: the integer, the
/// timestamp, the error's fields, the text, the bytes and the tuple's
/// members, and integer-against-float equality.
#[test]
fn every_immutable_payload_read_retries_without_dummy_data() {
    let (heap, six, _) = six_cold();
    let [text, big, late, bytes, tuple, error] = six.members;
    let objects = heap.arena().objects();
    let semantics = heap.arena().semantics();
    let args = Args::new(&[], objects, &semantics);
    retry_on(args.integer(&big), big);
    retry_on(args.timestamp(&late), late);
    retry_on(args.error(&error), error);
    retry_on(
        args.text(&text)
            .and_then(|view| view.expect("a text").whole()),
        text,
    );
    retry_on(
        args.bytes(&bytes)
            .and_then(|view| view.expect("bytes").whole()),
        bytes,
    );
    retry_on(
        args.sequence(&tuple)
            .and_then(|view| view.expect("a tuple").get(0)),
        tuple,
    );
    let float = Value::Float(Float::new(-(2f64.powi(100))));
    retry_on(args.equal(&big, &float), big);
    retry_on(args.equal(&float, &big), big);
}

/// Astra §3: a cold immutable's length, depth and text counts answer from
/// its head, without a load.
#[test]
fn cold_metadata_reads_preserve_len_depth_and_text_counts() {
    let (heap, six, _) = six_cold();
    let [text, _, _, bytes, tuple, error] = six.members;
    let objects = heap.arena().objects();
    let semantics = heap.arena().semantics();
    let args = Args::new(&[], objects, &semantics);
    let view = args.text(&text).expect("a read").expect("a text");
    assert_eq!(view.summary(), counts(UNICODE));
    assert_eq!(view.len(), UNICODE.len() as u64);
    assert_eq!(
        args.bytes(&bytes).expect("a read").expect("bytes").len(),
        20
    );
    assert_eq!(
        args.sequence(&tuple)
            .expect("a read")
            .expect("a tuple")
            .len(),
        2
    );
    assert_eq!(args.depth(&tuple), 2);
    assert_eq!(args.depth(&error), 1);
    for member in six.members {
        assert!(
            heap.arena()
                .needs_decode(member.handle().expect("an object"))
                .expect("live")
        );
    }
}

/// A store holding a root list whose one member is a text: its row, and
/// the root list's row naming it by `head`.
fn text_store(head: Head, text: &str) -> (Pages, HeapRoot) {
    let pages = Pages::default();
    let mut directory = pages.directory(None);
    directory
        .put(
            ObjectId(0),
            Row::of(
                Record::List(vec![RunValue::Object(ObjectKind::LongText, ObjectId(1))]),
                Heads(vec![(ObjectId(1), head)]),
            ),
        )
        .expect("a put");
    directory
        .put(
            ObjectId(1),
            Row::of(Record::LongText(text.into()), Heads::default()),
        )
        .expect("a put");
    let root = HeapRoot {
        high_water: HighWater(2),
        directory: directory.seal().expect("a seal"),
        roots: vec![RunValue::Object(ObjectKind::List, ObjectId(0))],
        heads: Heads::default(),
    };
    (pages, root)
}

fn text_head(text: &str) -> Head {
    Head::LongText {
        summary: counts(text),
        normal: lash_kernel_state::arena::text_key(text),
    }
}

/// Contract §2 and astra §1: loading a cold immutable admits its growth
/// before it installs a word, refuses as memory when the bound will not
/// hold it, leaving it cold and the holding unchanged; and a row its head
/// does not certify is a store fault, not a body. The root keeps its one
/// handle throughout.
#[test]
fn cold_load_preserves_roots_and_admits_before_install() {
    let (pages, root) = text_store(text_head(UNICODE), UNICODE);
    // The root's body (one value), and the text's opened head.
    let opened_root = 24 + 16 + 24;
    let (mut heap, root) = opened(&pages, &root, opened_root);
    load(&mut heap, root, &[root]).expect("a load");
    assert_eq!(heap.held(), opened_root);
    let [text] = members(&heap, root)[..] else {
        panic!("one member");
    };
    assert!(matches!(
        load(&mut heap, text, &[root]),
        Err(LoadError::Memory(_))
    ));
    assert!(
        heap.arena()
            .needs_decode(text.handle().expect("a text"))
            .expect("live")
    );
    assert_eq!(heap.held(), opened_root);

    let (pages, sealed) = text_store(text_head(UNICODE), UNICODE);
    let (mut heap, root) = opened(&pages, &sealed, u64::MAX);
    load(&mut heap, root, &[root]).expect("a load");
    let [text] = members(&heap, root)[..] else {
        panic!("one member");
    };
    load(&mut heap, text, &[root]).expect("a load");
    assert_eq!(heap.held(), 24 + 16 + 24 + UNICODE.len() as u64);
    assert_eq!(members(&heap, root)[0].handle(), text.handle());

    let (pages, sealed) = text_store(text_head(UNICODE), &UNICODE.replace('😀', "😁"));
    let (mut heap, root) = opened(&pages, &sealed, u64::MAX);
    load(&mut heap, root, &[root]).expect("a load");
    let [text] = members(&heap, root)[..] else {
        panic!("one member");
    };
    assert!(matches!(
        load(&mut heap, text, &[root]),
        Err(LoadError::Store(HeapFault::Corrupt { .. }))
    ));
    assert!(
        heap.arena()
            .needs_decode(text.handle().expect("a text"))
            .expect("live")
    );
}

/// Contract §6 (astra 3): a paged payload's row commits its counts and
/// root id, so a load compares them with the head its referrers name it by
/// before it reads a payload page. A row holding another text of equal
/// counts is a store fault even when its payload pages are gone.
#[test]
fn a_paged_row_its_head_does_not_certify_is_refused_before_its_pages_are_read() {
    let (named, stored) = ("a".repeat(9000), "b".repeat(9000));
    let pages = Pages::default();
    let mut directory = pages.directory(None);
    let paged = lash_kernel_state::paged::run::objects::Paged::write(
        directory.access(),
        stored.as_bytes(),
        true,
    )
    .expect("a payload");
    directory
        .put(
            ObjectId(0),
            Row::of(
                Record::List(vec![RunValue::Object(ObjectKind::LongText, ObjectId(1))]),
                Heads(vec![(ObjectId(1), text_head(&named))]),
            ),
        )
        .expect("a put");
    directory
        .put(ObjectId(1), Row::of(Record::Paged(paged), Heads::default()))
        .expect("a put");
    let sealed = HeapRoot {
        high_water: HighWater(2),
        directory: directory.seal().expect("a seal"),
        roots: vec![RunValue::Object(ObjectKind::List, ObjectId(0))],
        heads: Heads::default(),
    };
    pages.pages.borrow_mut().retain(|_, answer| {
        !matches!(
            answer.kind,
            lash_kernel_state::paged::PageKind::PAYLOAD_LEAF
                | lash_kernel_state::paged::PageKind::PAYLOAD_INDEX
        )
    });
    let (mut heap, root) = opened(&pages, &sealed, u64::MAX);
    load(&mut heap, root, &[root]).expect("a load");
    let [text] = members(&heap, root)[..] else {
        panic!("one member");
    };
    let refused = load(&mut heap, text, &[root]);
    assert!(
        matches!(refused, Err(LoadError::Store(HeapFault::Corrupt { .. }))),
        "{refused:?}"
    );
}

/// Contract §6 (astra 4): a seal publishes a payload's row only under the
/// identity its builder computed. An object whose cached head names
/// another text of equal counts is refused, never published under a key
/// its content does not have.
#[test]
fn a_seal_refuses_a_payload_whose_cached_head_names_other_content() {
    let pages = Pages::default();
    let mut directory = pages.directory(None);
    let mut high_water = HighWater(0);
    let mut arena = Arena::new();
    let text = arena.text(&"a".repeat(9000));
    arena
        .certify(text.handle().expect("a text"), text_head(&"b".repeat(9000)))
        .expect("a head");
    let root = list(&mut arena, vec![text]);
    let refused = seal(&mut arena, &[root], &mut directory, &mut high_water);
    assert!(
        matches!(&refused, Err(SealError::Fault { problem }) if problem.contains("identity")),
        "{refused:?}"
    );
}

/// Astra §1: an immutable two rows name opens once, as one slot the
/// heap holds once, however many loads name it.
#[test]
fn shared_immutable_reopens_as_one_slot_and_one_holding() {
    let (mut heap, _pages) = durable();
    let text = make(&mut heap, NewObject::LongText(UNICODE.into()));
    let tuple = make(&mut heap, NewObject::Tuple(vec![text, Value::Int(1)]));
    let inner = make(&mut heap, NewObject::List(vec![tuple]));
    let root = make(&mut heap, NewObject::List(vec![tuple, inner, tuple]));
    heap.page_out(&[root]).expect("a page-out");
    load(&mut heap, root, &[root]).expect("a load");
    load(&mut heap, inner, &[root]).expect("a load");
    let [first, _, third] = members(&heap, root)[..] else {
        panic!("three members");
    };
    let [nested] = members(&heap, inner)[..] else {
        panic!("one member");
    };
    assert_eq!(first.handle(), tuple.handle());
    assert_eq!(third.handle(), tuple.handle());
    assert_eq!(nested.handle(), tuple.handle());
    // Root (three values), inner (one), one cold tuple of no resident
    // values and the text's proxy, which the cold tuple's row still names:
    // a collection traces a cold object by its row's edges.
    heap.collect(&mut |visit| visit(&root));
    assert_eq!(heap.held(), (24 + 3 * 16) + (24 + 16) + 24 + 24);
}

/// A map keyed by a tuple holding a ref to `list`, which it also holds as
/// the key's value (the ref's referent stays reachable), between two text
/// keys. A first entry is deleted, so the map keys its own index, with
/// sequences that are not its positions.
fn keyed(arena: &mut Arena) -> (Value, Value) {
    let list = list(arena, vec![Value::Int(1)]);
    let key = arena
        .tuple(
            vec![
                Value::ObjectRef(list.handle().expect("a list")),
                Value::Int(2),
            ],
            64,
        )
        .expect("a tuple");
    let map = arena
        .create(NewObject::Map(vec![
            (short("gone"), Value::Int(0)),
            (short("first"), Value::Int(10)),
            (key, list),
            (short("last"), Value::Int(30)),
        ]))
        .expect("a map");
    arena
        .table_delete(map.handle().expect("a map"), &short("gone"))
        .expect("a delete");
    (map, list)
}

fn short(text: &str) -> Value {
    Value::Text(lash_kernel_doc::arena::ShortText::new(text).expect("short"))
}

/// The map's entries in iteration order, each as the key and value it
/// holds (handles included), and what each key looks up: one entry each.
fn entries(arena: &mut Arena, map: Value, list: Value) -> Vec<String> {
    let handle = map.handle().expect("a map");
    let pairs: Vec<Value> = {
        let objects = arena.objects();
        let semantics = arena.semantics();
        let args = Args::new(&[], objects, &semantics);
        let table = args.table(&map).expect("a read").expect("a map");
        table
            .chunks()
            .map(|chunk| chunk.expect("resident").to_vec())
            .collect::<Vec<_>>()
            .concat()
    };
    let mut seen: Vec<String> = pairs
        .chunks(2)
        .map(|pair| format!("{:?} => {:?}", pair[0], pair[1]))
        .collect();
    for key in ["first", "last"] {
        let got = arena.table_get(handle, &short(key)).expect("a lookup");
        seen.push(format!("{key} looks up {got:?}"));
    }
    seen.push(format!("the tuple looks up {:?}", lookup(arena, map, list)));
    seen
}

fn lookup(arena: &mut Arena, map: Value, list: Value) -> Option<Value> {
    let query = arena
        .tuple(
            vec![
                Value::ObjectRef(list.handle().expect("a list")),
                Value::Int(2),
            ],
            64,
        )
        .expect("a tuple");
    arena
        .table_get(map.handle().expect("a map"), &query)
        .expect("a lookup")
}

/// Astra §1: a key holding a ref means its referent, whether the referent
/// has a durable id yet or not: the same map answers the same query
/// before the first seal, after it, and reopened from its pages.
#[test]
fn reference_tuple_keys_survive_first_seal_and_reopen() {
    let mut arena = Arena::new();
    let (map, list) = keyed(&mut arena);
    assert!(lookup(&mut arena, map, list).is_some(), "before the seal");
    let before = entries(&mut arena, map, list);
    let pages = Pages::default();
    let mut directory = pages.directory(None);
    let root = seal(&mut arena, &[map], &mut directory, &mut HighWater(0)).expect("a seal");
    assert!(lookup(&mut arena, map, list).is_some(), "after the seal");
    // The re-key keeps each entry's place and what each key finds.
    assert_eq!(entries(&mut arena, map, list), before);
    assert!(before[3].ends_with("Some(Int(10))") && before[4].ends_with("Some(Int(30))"));

    let (mut heap, map) = opened(&pages, &root, u64::MAX);
    load(&mut heap, map, &[map]).expect("a load");
    let list = heap
        .arena()
        .entity_handle(crate::heap::open::entity(ObjectId(2)))
        .map(Value::List)
        .expect("the list opened");
    assert!(
        lookup(heap.arena_mut(), map, list).is_some(),
        "reopened, the key answers from its head"
    );
}

/// Astra §1: a seal that fails keeps the ids and heads it gave, so its
/// retry writes the rows one clean seal of the same graph would, whatever
/// the run allocated before: no row or root records a process-local
/// token.
#[test]
fn failed_seal_preserves_assigned_heads_for_retry() {
    let rows = |pages: &Pages, root: &HeapRoot| {
        let directory = pages.directory(Some(root.directory));
        (0..root.high_water.0)
            .map(|id| directory.get(ObjectId(id)).expect("a read"))
            .collect::<Vec<_>>()
    };

    let mut once = Arena::new();
    let (map, _) = keyed(&mut once);
    let pages = Pages::default();
    let sealed = seal(
        &mut once,
        &[map],
        &mut pages.directory(None),
        &mut HighWater(0),
    )
    .expect("a seal");
    let expected = rows(&pages, &sealed);

    let mut retried = Arena::new();
    for _ in 0..5 {
        list(&mut retried, Vec::new());
    }
    let (map, _) = keyed(&mut retried);
    let pages = Pages::default();
    let mut high_water = HighWater(0);
    pages.refuse.set(true);
    assert!(
        seal(
            &mut retried,
            &[map],
            &mut pages.directory(None),
            &mut high_water
        )
        .is_err()
    );
    pages.refuse.set(false);
    let again = seal(
        &mut retried,
        &[map],
        &mut pages.directory(None),
        &mut high_water,
    )
    .expect("a retry");
    assert_eq!(again.roots, sealed.roots);
    assert_eq!(rows(&pages, &again), expected);
}

/// What `value` copies out as once every chunk the copy reaches is
/// loaded.
fn copied(heap: &mut SlotHeap, value: Value, roots: &[Value]) -> lash_kernel_doc::Datum {
    for _ in 0..10_000 {
        match crate::data::copy_out(heap.arena(), 100_000, &value) {
            Ok(datum) => return datum,
            Err(crate::data::CopyError::Cold { object, chunk }) => heap
                .load(object, chunk, &mut |visit| {
                    roots.iter().for_each(&mut *visit)
                })
                .expect("a load"),
            Err(_) => panic!("the copy-out failed"),
        }
    }
    panic!("the copy-out never finished");
}

/// The cutover contract's chunking rule (`chunked::LARGE`, 256): a list
/// or table of at most 256 members is one directory row, and one of more
/// is chunks under an index. A run of 40,000 small records, a map of 256
/// entries and a list of 257 seals the list's 5 chunks and its index page
/// and otherwise directory pages only: no small object stages a page of
/// its own rows.
#[test]
fn a_small_object_is_one_row_and_only_a_large_one_takes_chunk_pages() {
    use lash_kernel_doc::arena::ShortText;
    use lash_kernel_state::paged::PageKind;
    let (mut heap, pages) = durable();
    let id = Value::Text(ShortText::new("id").expect("short"));
    let key = Value::Text(ShortText::new("key").expect("short"));
    let mut roots: Vec<Value> = (0..40_000)
        .map(|i| {
            make(
                &mut heap,
                NewObject::Record(vec![
                    (id, Value::Int(i)),
                    (key, Value::Int((i * 7_919) % 40_000)),
                ]),
            )
        })
        .collect();
    let map = make(
        &mut heap,
        NewObject::Map((0..256).map(|i| (Value::Int(i), Value::Int(-i))).collect()),
    );
    let list = make(
        &mut heap,
        NewObject::List((0..257).map(Value::Int).collect()),
    );
    roots.extend([map, list]);
    let sealed = heap.seal(&roots).expect("a seal");
    let mut kinds = BTreeMap::<PageKind, u64>::new();
    for answer in pages.pages.borrow().values() {
        *kinds.entry(answer.kind).or_default() += 1;
    }
    assert_eq!(kinds.remove(&PageKind::SEQUENCE_LEAF), Some(5), "{kinds:?}");
    assert_eq!(
        kinds.remove(&PageKind::SEQUENCE_INDEX),
        Some(1),
        "{kinds:?}"
    );
    assert!(
        kinds
            .keys()
            .all(|kind| [PageKind::KEYED_LEAF, PageKind::KEYED_INDEX].contains(kind)),
        "{kinds:?}"
    );
    let directory = pages.directory(Some(sealed.directory));
    let RunValue::Object(ObjectKind::Map, map_id) = sealed.roots[40_000] else {
        panic!("a map root: {:?}", sealed.roots[40_000]);
    };
    let Record::Map(entries) = row(&directory, map_id.0) else {
        panic!("the map is not one row");
    };
    assert_eq!(entries.len(), 256);
    let (mut heap, roots) =
        SlotHeap::open(u64::MAX, &sealed, pages.directory(Some(sealed.directory)))
            .expect("an open");
    assert_eq!(
        copied(&mut heap, roots[39_999], &roots),
        lash_kernel_doc::Datum::Record(vec![
            (
                "id".into(),
                lash_kernel_doc::Datum::Int(Integer::from(39_999))
            ),
            (
                "key".into(),
                lash_kernel_doc::Datum::Int(Integer::from((39_999 * 7_919) % 40_000))
            ),
        ])
    );
}

/// Errors nested to the value depth (128) with 256-byte kinds and
/// messages seal with every page inside the 64 KiB page bound, and reopen
/// cold holding every level, its kind, message and depth.
#[test]
fn errors_at_the_depth_limit_seal_in_bounded_pages_and_reopen_whole() {
    let (mut heap, pages) = durable();
    let (kind, message) = ("k".repeat(256), "m".repeat(256));
    let mut value = Value::Null;
    for _ in 0..128 {
        value = heap.error(&kind, &message, value).expect("an error");
    }
    let root = make(&mut heap, NewObject::List(vec![value]));
    let sealed = heap.seal(&[root]).expect("a seal");
    for answer in pages.pages.borrow().values() {
        let size = answer.tokens.len() + 32 * answer.children.len() + 32;
        assert!(size <= 65_536, "a {:?} page of {size} bytes", answer.kind);
    }
    let (mut heap, root) = opened(&pages, &sealed, u64::MAX);
    let lash_kernel_doc::Datum::List(members) = copied(&mut heap, root, &[root]) else {
        panic!("a list");
    };
    let mut datum = &members[0];
    let mut depth = 0;
    while let lash_kernel_doc::Datum::Error(error) = datum {
        assert_eq!(
            (error.kind.as_str(), error.message.as_str()),
            (kind.as_str(), message.as_str())
        );
        depth += 1;
        datum = error.raw();
    }
    assert_eq!(depth, 128);
    assert_eq!(datum, &lash_kernel_doc::Datum::Null);
}

/// A negative integer of 1,501 digits seals and copies out of a cold
/// reopen with its sign and every digit.
#[test]
fn a_cold_integer_copies_out_with_its_sign_and_every_digit() {
    let integer = Integer::parse(&format!("-1{}", "0".repeat(1500))).expect("an integer");
    let (mut heap, pages) = durable();
    let big = make(&mut heap, NewObject::BigInt(integer.clone()));
    let root = make(&mut heap, NewObject::List(vec![big, Value::Int(1)]));
    let sealed = heap.seal(&[root]).expect("a seal");
    let (mut heap, root) = opened(&pages, &sealed, u64::MAX);
    assert_eq!(
        copied(&mut heap, root, &[root]),
        lash_kernel_doc::Datum::List(vec![
            lash_kernel_doc::Datum::Int(integer),
            lash_kernel_doc::Datum::Int(Integer::from(1)),
        ])
    );
}
