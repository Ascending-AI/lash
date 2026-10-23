//! Shared ordered key layouts (FIG-6805): the tables a run builds again
//! and again with the same keys in the same order, a record or a string-
//! key map a loop constructs, share one interned layout of those keys.
//! A layout holds each key's position and the keys' order; each table
//! holds its values, its own keys' first spellings beside them, and its
//! insertion sequences, which a table on a layout takes densely, so an
//! entry's sequence is its position (`K-ITER-003`).
//!
//! A layout is the layout before it with one more key. Building a table
//! steps from the empty layout key by key, so tables built with the same
//! keys in the same order end on one layout. Appending a key a table does
//! not hold moves it to the layout that key leads to when another table
//! already built one; writing a value over an entry keeps its layout.
//! Otherwise, and on removing an entry, renumbering its sequences, or
//! growing past what the layouts bound, the table moves to a key index of
//! its own ([`super::tables`]), for good: a table a run grows key by key
//! makes no layouts.
//!
//! Layouts are process-local: nothing projects them, a restored run
//! starts with none, and two runs that build the same tables in another
//! order share differently without either seeing it. A short layout finds
//! a key by comparing its keys; a longer one keeps a hashed index of them.
//! What they hold is metered: each layout's bytes, and its index's when a
//! table first takes it, are counted before they are allocated, and
//! counted no more once no table holds it or a layout after it, so the
//! capacity meter counts the layouts the run's live tables need, with the
//! room a layout keeps for the layouts after it while it lives. The empty layout every run has is
//! not data and is not counted itself. The transitions are bounded
//! ([`MAX_KEYS`], [`MAX_LAYOUTS`]): diverse data falls back to tables' own
//! indexes rather than growing metadata without end.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};

use foldhash::{HashMap, HashMapExt};

use lash_kernel_doc::heap::NormalKey;

use super::keys::Key;

/// The most keys a layout holds: a table that grows past it keeps a key
/// index of its own.
pub(super) const MAX_KEYS: u32 = 16;

/// The most layouts live at once in a run.
const MAX_LAYOUTS: u32 = 1024;

/// The next layouts a layout lists before it hashes them.
const FEW: usize = 8;

/// The most keys a layout finds a key among by comparing them in turn.
const SCANNED: u32 = 8;

/// What the layouts of one run hold.
#[derive(Debug, Default)]
struct Meter {
    /// The bytes the live layouts hold.
    bytes: Cell<u64>,
    /// The live layouts.
    live: Cell<u32>,
}

/// The layouts one more key leads to, while a table holds them or a
/// layout after them: listed while they are few, hashed past that.
#[derive(Debug)]
enum Next {
    Few(Vec<(Key, Weak<Layout>)>),
    Many(HashMap<Key, Weak<Layout>>),
}

/// One ordered key layout: its parent's keys, then its own.
#[derive(Debug)]
pub struct Layout {
    /// The key it holds past its parent's; none for the empty layout.
    key: Option<Key>,
    parent: Option<Rc<Layout>>,
    /// How many keys it holds.
    len: u32,
    /// A table took it: appending its last key to a table on its parent
    /// moves that table to it.
    taken: Cell<bool>,
    /// Each key's position, past [`SCANNED`] keys: built when a table
    /// first takes the layout.
    index: OnceCell<HashMap<Key, u32>>,
    next: RefCell<Next>,
    /// The bytes it holds, counted by `meter`.
    bytes: Cell<u64>,
    meter: Rc<Meter>,
}

impl Drop for Layout {
    fn drop(&mut self) {
        // No table holds it or a layout after it: its parent forgets it.
        if let (Some(parent), Some(key)) = (&self.parent, &self.key) {
            parent.next.borrow_mut().remove(key);
        }
        let meter = &self.meter;
        meter
            .bytes
            .set(meter.bytes.get().saturating_sub(self.bytes.get()));
        meter.live.set(meter.live.get().saturating_sub(1));
    }
}

/// What a layout makes of a key a table appends.
pub(super) enum Step {
    /// The table moves to this layout, the key last.
    Next(Rc<Layout>),
    /// The layout already holds the key, at this position.
    Held(u32),
    /// The layouts' bounds leave no layout for it.
    Full,
}

/// The bytes a key holds beyond its own size.
fn key_bytes(key: &Key) -> u64 {
    match key {
        Key::Normal(normal) => {
            (std::mem::size_of::<NormalKey>()
                + match normal.as_ref() {
                    NormalKey::Inline(bytes) => bytes.capacity(),
                    NormalKey::Digest(_) => 0,
                }) as u64
        }
        Key::Text(..) | Key::Int(_) => 0,
    }
}

/// The bytes a layout holds before its index and its next layouts: itself
/// in its `Rc`, and its key.
fn node_bytes(key: &Key) -> u64 {
    (std::mem::size_of::<Layout>() + 2 * std::mem::size_of::<usize>()) as u64 + key_bytes(key)
}

/// The bytes a hashbrown table of room for `items` entries of `entry`
/// bytes holds: a power of two of buckets at most 7/8 full, each an entry
/// and a control byte, and a group of control bytes past them.
fn map_bytes(items: usize, entry: usize) -> u64 {
    let buckets = match items {
        0 => return 0,
        1..=3 => 4,
        4..=7 => 8,
        items => (items * 8).div_ceil(7).next_power_of_two(),
    };
    (buckets * (entry + 1) + 16) as u64
}

/// The bytes an index of `len` keys holds.
fn index_bytes(len: u32) -> u64 {
    map_bytes(len as usize, std::mem::size_of::<(Key, u32)>())
}

/// The bytes of next layouts listed with room for `items`.
fn next_bytes(items: usize) -> u64 {
    let entry = std::mem::size_of::<(Key, Weak<Layout>)>();
    if items <= FEW {
        (FEW * entry) as u64
    } else {
        map_bytes(items, entry)
    }
}

impl Next {
    #[inline]
    fn get(&self, key: &Key) -> Option<&Weak<Layout>> {
        match self {
            Self::Few(few) => few
                .iter()
                .find(|(held, _)| held == key)
                .map(|(_, next)| next),
            Self::Many(many) => many.get(key),
        }
    }

    fn live(&self) -> usize {
        match self {
            Self::Few(few) => few.len(),
            Self::Many(many) => many.len(),
        }
    }

    fn room(&self) -> usize {
        match self {
            Self::Few(few) => few.capacity(),
            Self::Many(many) => many.capacity(),
        }
    }

    fn remove(&mut self, key: &Key) {
        match self {
            Self::Few(few) => {
                if let Some(at) = few.iter().position(|(held, _)| held == key) {
                    few.swap_remove(at);
                }
            }
            Self::Many(many) => {
                many.remove(key);
            }
        }
    }

    /// Lists `next` under `key`, the room for it counted in `bytes` and
    /// by `meter` before it is allocated.
    fn insert(&mut self, key: Key, next: Weak<Layout>, bytes: &Cell<u64>, meter: &Meter) {
        let live = self.live();
        if live == self.room() {
            let room = (live * 2).max(FEW);
            let had = if self.room() == 0 {
                0
            } else {
                next_bytes(self.room())
            };
            let grown = next_bytes(room).saturating_sub(had);
            meter.admit(grown);
            bytes.set(bytes.get().saturating_add(grown));
            match self {
                Self::Few(few) if room <= FEW => few.reserve_exact(room - few.len()),
                Self::Few(few) => {
                    let mut many = HashMap::with_capacity(room);
                    many.extend(few.drain(..));
                    *self = Self::Many(many);
                }
                Self::Many(many) => many.reserve(room - many.len()),
            }
        }
        match self {
            Self::Few(few) => few.push((key, next)),
            Self::Many(many) => {
                many.insert(key, next);
            }
        }
    }
}

impl Meter {
    /// Counts `bytes` more, before they are allocated.
    fn admit(&self, bytes: u64) {
        self.bytes.set(self.bytes.get().saturating_add(bytes));
    }
}

impl Layout {
    /// How many keys it holds.
    pub(super) fn len(&self) -> u32 {
        self.len
    }

    /// The keys it holds, last first.
    fn keys(&self) -> impl Iterator<Item = &Key> {
        std::iter::successors(Some(self), |layout| layout.parent.as_deref())
            .filter_map(|layout| layout.key.as_ref())
    }

    /// The layout as a table takes it: its index, if it keeps one, is
    /// built, and counted, when the first table does.
    pub(super) fn taken(self: Rc<Self>) -> Rc<Self> {
        if !self.taken.replace(true) && self.len > SCANNED {
            self.index();
        }
        self
    }

    /// Each key's position.
    fn index(&self) -> &HashMap<Key, u32> {
        self.index.get_or_init(|| {
            let bytes = index_bytes(self.len);
            self.meter.admit(bytes);
            self.bytes.set(self.bytes.get().saturating_add(bytes));
            let mut index = HashMap::with_capacity(self.len as usize);
            for (at, key) in (0..self.len).rev().zip(self.keys()) {
                index.insert(key.clone(), at);
            }
            index
        })
    }

    /// The position of `key`, if the layout holds it: a layout of at most
    /// [`SCANNED`] keys compares its own keys, last first, which costs
    /// less than hashing the key; a longer one asks its index.
    #[inline(always)]
    pub(super) fn position(&self, key: &Key) -> Option<u32> {
        if self.len > SCANNED {
            return self.index().get(key).copied();
        }
        let mut layout = self;
        while let (Some(held), Some(parent)) = (&layout.key, &layout.parent) {
            if held == key {
                return Some(parent.len);
            }
            layout = parent;
        }
        None
    }

    /// Each key's position, as a table's own index of sequences.
    pub(super) fn own(&self) -> HashMap<Key, u64> {
        let mut own = HashMap::with_capacity(self.len as usize);
        for (at, key) in (0..u64::from(self.len)).rev().zip(self.keys()) {
            own.insert(key.clone(), at);
        }
        own
    }

    /// The layout `key`, which the layout does not hold, leads to, when a
    /// table was built on it and still holds it or a layout after it.
    pub(super) fn next(&self, key: &Key) -> Option<Rc<Layout>> {
        let next = self.next.borrow().get(key).and_then(Weak::upgrade);
        next.filter(|layout| layout.taken.get())
    }

    /// What building a table makes of its next key `key`: the layout after
    /// this one, made if no table holds one yet, or the position this one
    /// already holds the key at (`K-KEY-003`), or none within the bounds.
    #[inline]
    pub(super) fn step(self: &Rc<Self>, key: &Key) -> Step {
        if let Some(layout) = self.next.borrow().get(key).and_then(Weak::upgrade) {
            return Step::Next(layout);
        }
        self.grow(key)
    }

    /// [`Self::step`] where no table holds the layout `key` leads to.
    #[inline(never)]
    fn grow(self: &Rc<Self>, key: &Key) -> Step {
        let mut next = self.next.borrow_mut();
        // A layout one more key leads to never holds that key before it.
        if let Some(at) = (0..self.len)
            .rev()
            .zip(self.keys())
            .find_map(|(at, held)| (held == key).then_some(at))
        {
            return Step::Held(at);
        }
        let meter = &self.meter;
        if self.len >= MAX_KEYS || meter.live.get() >= MAX_LAYOUTS {
            return Step::Full;
        }
        let bytes = node_bytes(key);
        meter.admit(bytes);
        meter.live.set(meter.live.get() + 1);
        let layout = Rc::new(Layout {
            key: Some(key.clone()),
            parent: Some(Rc::clone(self)),
            len: self.len + 1,
            taken: Cell::new(false),
            index: OnceCell::new(),
            next: RefCell::new(Next::Few(Vec::new())),
            bytes: Cell::new(bytes),
            meter: Rc::clone(meter),
        });
        next.insert(key.clone(), Rc::downgrade(&layout), &self.bytes, meter);
        Step::Next(layout)
    }
}

/// The layouts of one run: the empty layout every table on one starts
/// from, and what they hold.
#[derive(Debug)]
pub(super) struct Layouts {
    empty: Rc<Layout>,
    meter: Rc<Meter>,
}

impl Default for Layouts {
    fn default() -> Self {
        let meter = Rc::new(Meter::default());
        Self {
            empty: Rc::new(Layout {
                key: None,
                parent: None,
                len: 0,
                taken: Cell::new(false),
                index: OnceCell::new(),
                next: RefCell::new(Next::Few(Vec::new())),
                bytes: Cell::new(0),
                meter: Rc::clone(&meter),
            }),
            meter,
        }
    }
}

impl Layouts {
    /// The layout of no keys.
    pub(super) fn empty(&self) -> Rc<Layout> {
        Rc::clone(&self.empty)
    }

    /// The bytes the live layouts hold.
    #[inline]
    pub(super) fn held(&self) -> u64 {
        self.meter.bytes.get()
    }
}

#[cfg(test)]
mod tests {
    use lash_kernel_doc::{NewObject, Value};

    use super::super::Arena;
    use super::*;

    fn record(arena: &mut Arena, names: &[&str]) -> Value {
        let fields = names
            .iter()
            .enumerate()
            .map(|(at, name)| (arena.text(name), Value::Int(at as i64)))
            .collect();
        arena.create(NewObject::Record(fields)).unwrap()
    }

    /// FIG-6805: tables built with the same keys in the same order share
    /// one layout, whose bytes the capacity meter counts once, before they
    /// are allocated, and no more once no table holds it. Diverse shapes
    /// stop at the layouts' bound and key their own indexes instead.
    #[test]
    fn tables_built_alike_share_one_metered_bounded_layout() {
        let mut arena = Arena::new();
        assert_eq!(arena.layouts.held(), 0, "the empty layout is not data");
        let held = arena.held();
        record(&mut arena, &["index", "pair", "label"]);
        let shared = arena.layouts.held();
        assert!(shared > 0);
        // One body slot, six key/value slots and 32 bytes of flat-size metadata.
        assert_eq!(arena.held() - held, shared + 7 * 16 + 32);
        for _ in 0..100 {
            record(&mut arena, &["index", "pair", "label"]);
        }
        assert_eq!(
            arena.layouts.held(),
            shared,
            "one layout for every alike table"
        );
        let names: Vec<String> = (0..2 * MAX_LAYOUTS).map(|at| format!("k{at}")).collect();
        for name in &names {
            record(&mut arena, &[name.as_str()]);
        }
        let meter = &arena.layouts.meter;
        assert_eq!(meter.live.get(), MAX_LAYOUTS);
        let most = shared
            + u64::from(MAX_LAYOUTS) * (node_bytes(&Key::Int(0)) + index_bytes(1))
            + next_bytes(2 * MAX_LAYOUTS as usize);
        assert!(arena.layouts.held() <= most, "diverse shapes are bounded");
        arena.release(&mut |_| {});
        assert_eq!(
            arena.layouts.held(),
            arena.layouts.empty.bytes.get(),
            "a released table's layout is not counted, only the room the empty one keeps"
        );
        assert!(arena.layouts.held() <= next_bytes(2 * MAX_LAYOUTS as usize));
        assert_eq!(arena.layouts.meter.live.get(), 0);
    }
}
