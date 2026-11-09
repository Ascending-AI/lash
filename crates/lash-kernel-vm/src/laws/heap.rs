//! The durable heap's seal (cutover contract §6): which rows a seal
//! writes, under which ids, and what a failed seal leaves for the next.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::paged::run::directory::{Directory, DirectoryRoot};
use lash_kernel_state::paged::run::ids::{HighWater, ObjectId};
use lash_kernel_state::paged::run::objects::{ObjectKind, Record, RunValue};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};

use super::embedder::Source;
use crate::heap::seal::seal;
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
