//! The arena: the objects a run executes on, and the facts the machine
//! keeps beside each one (seam D of `docs/design/kernel-end-state.md`).
//!
//! Bodies are [`Objects`], what a native call borrows. Beside them, by slot
//! index, each object's [`Facts`]: its durable identity once a projection
//! has saved it, whether it changed since it was last projected and, for
//! a list or a table the heap holds, which of its cards changed, the page
//! form it was admitted from or last projected to when it is immutable, its
//! logical cost, a table's key index, and how many members of other objects
//! hold it.
//!
//! The paged heap is the arena's projection. A mutable object read from a
//! page is a proxy, interned by its [`EntityId`] so the run sees one handle
//! for it, which stays cold until a read reaches it. The machine opens its
//! head then, outside any native call: a small object is decoded whole, a
//! large list or table is given chunks that stay cold until a read reaches
//! each, and a long text or bytes on payload pages is decoded node by node.
//! An object whose chunks are all resident is joined into one chunk, so a
//! whole read of it borrows one slice.
//!
//! What changed is on the dirty list: each object with an id whose state
//! the heap does not hold and, for a list or a table the heap holds, its
//! dirty cards ([`CARD`] positions or insertion sequences each). The
//! machine writes it back at a projection point, card by card
//! (`lash-kernel-vm`'s `machine/project.rs` owns those conversions). At a
//! statement boundary, once residency passes a bound, clean large objects
//! drop back to their pages.
//!
//! **Ids at projection** (seam C). A new mutable object has no durable id.
//! A projection gives one to each it saves, in the canonical walk's order
//! ([`Arena::walk`], then [`Arena::walk_dirty`]), so what dies before a
//! projection point never takes one, and temporaries a body allocates where
//! a native allocates none shift no id. A ref used as a map key is keyed
//! by its referent's process-local token, whatever its id: arena keys never
//! reach a page.
//!
//! **Reachability** (seam D). Every member write counts the object it
//! names. An object no member holds waits on the zero-count list, and a
//! release at a statement boundary frees each one the machine's frames do
//! not hold, with what it alone held ([`Arena::release`]). Cycles are freed
//! by [`Arena::collect_cycles`], a mark from what the machine holds, which
//! the machine runs only under pressure and only when a count fell without
//! reaching zero since the last one. A projection marks nothing: it walks
//! the new objects its roots reach and the dirty ones only.

mod bench;
mod chunks;
mod keys;
mod layouts;
mod ops;
mod reach;
mod tables;
mod texts;

pub use crate::paged::stored::CARD;
pub use bench::{Bench, Called, Usage};
pub use chunks::{ColdChunk, PayloadPart};
pub use keys::{ArenaSemantics, Key, text_key};
use ops::composite;
pub use ops::invalid_key;
pub use reach::{Reach, Reached};
pub use tables::{Index, Place, Sequences};

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use lash_kernel_doc::arena::{
    ArenaHandle, Body, Bytes, Chunk, Chunks, ErrorBody, LongText, Objects, Sequence, ShortText,
};
use lash_kernel_doc::heap::{EntityId, PageValue, TextSummary};
use lash_kernel_doc::heap_values::text_summary;
use lash_kernel_doc::{Handle, Integer, NativeError, NewObject, Value};

/// The summary an immutable composite keeps: what its page form's
/// descriptor certifies (`TupleSummary`, `ErrorRef`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// The tuple and error nesting depth inside it, itself included
    /// (`K-VAL-034`).
    pub depth: u32,
    pub task_edges: u64,
    /// Whether a member at any depth names an object or a task.
    pub has_edges: bool,
}

/// What the arena knows of one slot beside its body.
#[derive(Debug, Default)]
pub struct Facts {
    /// A mutable object's durable identity: given by the first projection
    /// that saves it, or the one its page names, and kept from then on.
    pub entity: Option<EntityId>,
    /// The object changed since it was last projected, or was never
    /// projected: the heap does not hold what the arena does.
    pub dirty: bool,
    /// The heap holds a head for the object: it was opened from one or
    /// projected to one. A projection writes such a list's or table's dirty
    /// cards into it, never the whole object.
    pub based: bool,
    /// A based list's or table's dirty cards, ascending: card `c` holds
    /// positions, or insertion sequences, `c * CARD ..`.
    pub cards: Vec<u64>,
    /// An immutable's page form: the descriptor it was admitted from, or
    /// the one its first projection built. Projection reuses it.
    pub page: std::cell::OnceCell<PageValue>,
    /// An immutable composite's summary.
    pub summary: Summary,
    /// A resident map's, set's or record's key index.
    pub index: Option<Index>,
    /// A proxy whose head the machine has not opened: its body is a cold
    /// placeholder of its kind.
    pub cold: bool,
    /// The machine decoded the object this many times since the last
    /// statement boundary: each decode reads twice as many chunks as the
    /// one before.
    streak: u32,
    /// How many members of arena objects hold it. Frames, registers and
    /// pins are not counted: a release scans them.
    count: u32,
    /// It waits on the zero-count list.
    zero: bool,
    /// It waits on the dirty list.
    listed: bool,
    /// What a ref to it is keyed by while it is resident: process-local,
    /// never persisted, independent of where the arena put it.
    token: u64,
    /// Invalidates process-local positions before any edit that adds,
    /// removes or moves an entry, or replaces the body: a value written
    /// over a table entry keeps it.
    revision: u64,
    mark: bool,
}

/// The page a cold chunk of a mutable object names: none of its own. Its
/// rows are read by position, or by insertion sequence, through its
/// object's head in the heap, whose index path reaches the leaf that
/// holds them.
pub const NO_PAGE: lash_kernel_doc::heap::PageRef = lash_kernel_doc::heap::PageRef {
    id: lash_kernel_doc::heap::PageId([0; 32]),
    grant: lash_kernel_doc::heap::PageGrant::NONE,
};

/// The kinds of mutable object a proxy can stand for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    List,
    Map,
    Set,
    Record,
    Closure,
    /// A closure's shared variable.
    Cell,
    /// Named only by a ref: its kind is learned when it is decoded.
    Unknown,
}

/// The site a cold closure's placeholder body holds until it is decoded:
/// one shared empty site, so a proxy allocates none.
static COLD_CLOSURE_SITE: LazyLock<Arc<lash_kernel_doc::Site>> = LazyLock::new(|| {
    Arc::new(lash_kernel_doc::Site::new(
        lash_kernel_doc::Unit::Main,
        &[] as &[u32],
    ))
});

impl ProxyKind {
    /// The cold placeholder body of a proxy of this kind.
    pub fn placeholder(self) -> Body {
        let cold = || {
            Chunks::Split(vec![Chunk::Cold {
                page: NO_PAGE,
                len: 0,
            }])
        };
        match self {
            Self::List => Body::List(Sequence {
                len: 0,
                chunks: cold(),
            }),
            Self::Map => Body::Map(lash_kernel_doc::arena::Table {
                len: 0,
                chunks: cold(),
            }),
            Self::Set => Body::Set(lash_kernel_doc::arena::Table {
                len: 0,
                chunks: cold(),
            }),
            Self::Record => Body::Record(lash_kernel_doc::arena::Table {
                len: 0,
                chunks: cold(),
            }),
            Self::Closure => Body::Closure {
                site: Arc::clone(&*COLD_CLOSURE_SITE),
                captures: Box::new([]),
                code: None,
            },
            Self::Cell | Self::Unknown => Body::Cell(Value::Absent),
        }
    }
}

/// A resident body [`Arena::chill`] set aside, with its key index.
#[derive(Debug)]
pub struct Chilled {
    body: Body,
    index: Option<Index>,
}

/// The objects of one run and their facts.
#[derive(Debug, Default)]
pub struct Arena {
    objects: Objects,
    keys: keys::KeyCache,
    facts: Vec<Facts>,
    /// The shared key layouts of the tables the run built.
    layouts: layouts::Layouts,
    /// The one handle of each durable object the run has seen.
    entities: HashMap<EntityId, ArenaHandle>,
    /// The primary ordinal the next object a projection saves takes.
    next_ordinal: u64,
    /// The objects with ids that changed since they were last projected.
    dirty: Vec<ArenaHandle>,
    /// The objects no member holds, which a release examines.
    zero: Vec<ArenaHandle>,
    /// How long the zero-count list grows before a release is due.
    release_at: usize,
    /// Counts that fell without reaching zero since the last cycle
    /// collection: none means no garbage cycle formed.
    candidates: u64,
    tokens: u64,
    /// Mutable objects the run created.
    allocations: u64,
    /// Objects freed so far: a handle the machine kept outside the arena
    /// still names what it named while this has not moved.
    freed: u64,
    /// Values held by resident bodies, each body one more, long text and
    /// bytes one a [`VALUE_BYTES`]: what triggers a collection, and what
    /// the capacity meter counts (FIG-6791).
    resident: u64,
    /// The control state's bytes the capacity meter counts beside the
    /// resident values: frames and their slots, as the machine grew them
    /// since its last sweep recounted them.
    control: u64,
    /// The held bytes ([`Arena::held`]) at which the capacity meter's
    /// slow path runs next: [`u64::MAX`] when nothing meters.
    capacity_mark: u64,
    /// Objects with a cold chunk, at most: none means nothing needs decoding.
    colds: u64,
    collect_at: u64,
    /// Resident values past which a statement boundary drops clean large
    /// objects back to their pages.
    evict_at: u64,
    /// The resident values below which a statement boundary has neither to
    /// evict nor a cycle collection due ([`Arena::boundary_due`]): the
    /// least of `evict_at` and, once a count fell without reaching zero
    /// since the last collection, `collect_at`.
    boundary_at: u64,
    /// Objects decoded since the last statement boundary.
    streaked: Vec<ArenaHandle>,
    /// The empty tuple, while one is live: every empty tuple the run makes
    /// or decodes is this one object, so evaluating `()` allocates nothing
    /// once one exists. An immutable has no identity a run observes, and
    /// it is freed as any object is, when nothing holds it.
    empty_tuple: Option<ArenaHandle>,
}

/// What a collection's roots are visited with: each value they hold.
pub type Visit<'a> = dyn FnMut(&Value) + 'a;

/// The bytes the capacity meter counts for one resident value slot: the
/// size of a [`Value`]. Long text and bytes count one slot a 16 bytes.
pub const VALUE_BYTES: u64 = std::mem::size_of::<Value>() as u64;

/// A cycle collection is due once resident values pass this, and then each
/// time they double.
const FIRST_COLLECTION: u64 = 1 << 16;

/// A release is due once this many objects wait on the zero-count list,
/// or as many as the last release kept there, if more.
const FIRST_RELEASE: usize = 1 << 10;

fn malformed(problem: &'static str) -> NativeError {
    NativeError::Malformed(problem)
}

impl Arena {
    pub fn new() -> Self {
        Self {
            collect_at: FIRST_COLLECTION,
            release_at: FIRST_RELEASE,
            evict_at: chunks::EVICT_AT,
            boundary_at: chunks::EVICT_AT,
            capacity_mark: u64::MAX,
            ..Self::default()
        }
    }

    /// An arena whose projections give primary ordinals from `next`: the
    /// heap's next ordinal.
    pub fn from_ordinal(next: u64) -> Self {
        Self {
            next_ordinal: next,
            ..Self::new()
        }
    }

    /// The bytes the capacity meter counts the arena holding: its resident
    /// value slots at [`VALUE_BYTES`] each, the control state the machine
    /// counted (FIG-6791), and its tables' shared layouts.
    #[inline(always)]
    pub fn held(&self) -> u64 {
        self.resident
            .saturating_mul(VALUE_BYTES)
            .saturating_add(self.control)
            .saturating_add(self.layouts.held())
    }

    /// Whether what the arena holds reached the capacity mark: the meter's
    /// one compare where storage or control grows.
    #[inline(always)]
    pub fn capacity_due(&self) -> bool {
        self.held() >= self.capacity_mark
    }

    /// Where the capacity meter's slow path runs next.
    pub fn set_capacity_mark(&mut self, mark: u64) {
        self.capacity_mark = mark;
    }

    /// Counts `bytes` more of control state: frames and their slots.
    #[inline(always)]
    pub fn grow_control(&mut self, bytes: u64) {
        self.control = self.control.saturating_add(bytes);
    }

    /// Control state the run let go of: a frame it popped or dropped from
    /// memory.
    pub fn shrink_control(&mut self, bytes: u64) {
        self.control = self.control.saturating_sub(bytes);
    }

    /// The control state as a sweep recounted it.
    pub fn recount_control(&mut self, bytes: u64) {
        self.control = bytes;
    }

    /// Recounts the values its live bodies hold, as a sweep does after it
    /// freed what nothing reaches: the meter counts exactly what is left.
    pub fn recount_resident(&mut self) {
        let objects = &self.objects;
        self.resident = (0..objects.capacity() as u32)
            .filter_map(|index| objects.handle_at(index))
            .filter_map(|handle| objects.get(handle))
            .map(body_values)
            .fold(0, u64::saturating_add);
    }

    /// The bodies, as a native call borrows them.
    #[inline]
    pub fn objects(&self) -> &Objects {
        &self.objects
    }

    /// The guest semantics over this arena: what a native's
    /// [`lash_kernel_doc::Args`] and the machine's own equality and keys use.
    #[inline]
    pub fn semantics(&self) -> ArenaSemantics<'_> {
        ArenaSemantics::new(&self.objects, &self.facts, &self.keys)
    }

    #[inline]
    pub fn body(&self, handle: ArenaHandle) -> Result<&Body, NativeError> {
        self.objects
            .get(handle)
            .ok_or_else(|| malformed("a stale handle reached the machine"))
    }

    #[inline]
    pub fn facts(&self, handle: ArenaHandle) -> Result<&Facts, NativeError> {
        self.objects
            .get(handle)
            .and_then(|_| self.facts.get(handle.index() as usize))
            .ok_or_else(|| malformed("a stale handle reached the machine"))
    }

    #[inline]
    fn facts_mut(&mut self, handle: ArenaHandle) -> Result<&mut Facts, NativeError> {
        if self.objects.get(handle).is_none() {
            return Err(malformed("a stale handle reached the machine"));
        }
        self.facts
            .get_mut(handle.index() as usize)
            .ok_or_else(|| malformed("a stale handle reached the machine"))
    }

    /// The body and facts of `handle`, for an edit that may move what it
    /// holds, which revokes its position proofs: the caller marks it dirty.
    #[inline]
    pub fn parts_mut(
        &mut self,
        handle: ArenaHandle,
    ) -> Result<(&mut Body, &mut Facts), NativeError> {
        let facts = self
            .facts
            .get_mut(handle.index() as usize)
            .ok_or_else(|| malformed("a stale handle reached the machine"))?;
        let body = self
            .objects
            .get_mut(handle)
            .ok_or_else(|| malformed("a stale handle reached the machine"))?;
        facts.revision = facts.revision.saturating_add(1);
        Ok((body, facts))
    }

    /// The primary ordinal the next object a projection saves takes.
    pub fn next_ordinal(&self) -> u64 {
        self.next_ordinal
    }

    /// How many mutable objects the run has created.
    pub fn allocations(&self) -> u64 {
        self.allocations
    }

    /// Whether an object of the arena may have a cold chunk.
    #[inline]
    pub fn any_cold(&self) -> bool {
        self.colds > 0
    }

    /// Whether `handle` is a proxy, or has a chunk on its pages.
    pub fn needs_decode(&self, handle: ArenaHandle) -> Result<bool, NativeError> {
        Ok(self.facts(handle)?.cold || has_cold(self.body(handle)?))
    }

    /// Installs `body` with `facts`: its members are held, and it waits on
    /// the zero-count list until a member holds it.
    fn insert(&mut self, body: Body, facts: Facts) -> ArenaHandle {
        self.resident = self.resident.saturating_add(body_values(&body));
        if facts.cold || has_cold(&body) {
            self.colds = self.colds.saturating_add(1);
        }
        self.hold_members(&body);
        let handle = self.objects.insert(body);
        self.place(handle, facts);
        handle
    }

    fn place(&mut self, handle: ArenaHandle, mut facts: Facts) {
        let index = handle.index() as usize;
        if index >= self.facts.len() {
            self.facts.resize_with(index + 1, Facts::default);
        }
        if let Some(entity) = facts.entity {
            self.entities.insert(entity, handle);
        }
        self.tokens += 1;
        facts.token = self.tokens;
        facts.zero = true;
        self.facts[index] = facts;
        self.zero.push(handle);
    }

    /// Installs an immutable `body` with its composite `summary`, and the
    /// page form it was admitted from, if any.
    pub fn immutable(
        &mut self,
        body: Body,
        summary: Summary,
        page: Option<PageValue>,
    ) -> ArenaHandle {
        let facts = Facts {
            summary,
            ..Facts::default()
        };
        if let Some(page) = page {
            let _ = facts.page.set(page);
        }
        if matches!(&body, Body::Tuple(sequence) if sequence.len == 0 && !has_cold(&body)) {
            return self.empty_tuple(facts);
        }
        self.insert(body, facts)
    }

    /// The live empty tuple, made with `facts` when none is.
    fn empty_tuple(&mut self, facts: Facts) -> ArenaHandle {
        if let Some(handle) = self.empty_tuple
            && self.objects.get(handle).is_some()
        {
            return handle;
        }
        let handle = self.insert(Body::Tuple(Sequence::whole(Vec::new())), facts);
        self.empty_tuple = Some(handle);
        handle
    }

    /// The facts of a mutable object the run creates: dirty, and without
    /// a durable id until a projection saves it.
    fn created(&mut self, index: Option<Index>) -> Facts {
        self.allocations += 1;
        Facts {
            dirty: true,
            index,
            ..Facts::default()
        }
    }

    /// Creates the mutable object `body`.
    fn insert_created(&mut self, body: Body) -> ArenaHandle {
        let facts = self.created(None);
        self.insert(body, facts)
    }

    /// The one handle of durable object `entity`: the one the run already
    /// has, else a cold proxy of `kind`. A proxy first named only by a ref
    /// takes the placeholder of the kind a later page names it by.
    pub fn proxy(&mut self, entity: EntityId, kind: ProxyKind) -> ArenaHandle {
        if let Some(handle) = self.entities.get(&entity).copied() {
            let index = handle.index() as usize;
            if kind != ProxyKind::Unknown
                && self.facts.get(index).is_some_and(|facts| facts.cold)
                && let Some(body) = self.objects.get_mut(handle)
                && matches!(body, Body::Cell(Value::Absent))
                && kind != ProxyKind::Cell
            {
                *body = kind.placeholder();
            }
            return handle;
        }
        self.insert(
            kind.placeholder(),
            Facts {
                entity: Some(entity),
                cold: true,
                based: true,
                ..Facts::default()
            },
        )
    }

    /// Counts a member holding `value`.
    #[inline]
    fn hold(&mut self, value: &Value) {
        if let Some(handle) = value.handle() {
            self.hold_handle(handle);
        }
    }

    #[inline]
    fn hold_handle(&mut self, handle: ArenaHandle) {
        if let Some(facts) = self.facts.get_mut(handle.index() as usize) {
            facts.count = facts.count.saturating_add(1);
        }
    }

    /// Uncounts a member that held `value`: an object no member holds any
    /// more waits on the zero-count list.
    #[inline]
    fn unhold(&mut self, value: &Value) {
        if let Some(handle) = value.handle() {
            self.unhold_handle(handle);
        }
    }

    fn unhold_handle(&mut self, handle: ArenaHandle) {
        let Some(facts) = self.facts.get_mut(handle.index() as usize) else {
            return;
        };
        // A saturated count stays: the object is freed by a cycle
        // collection, if ever.
        if facts.count == u32::MAX {
            return;
        }
        facts.count = facts.count.saturating_sub(1);
        if facts.count > 0 {
            self.candidates += 1;
            if self.candidates == 1 {
                self.bound_boundary();
            }
        } else if !facts.zero {
            facts.zero = true;
            self.zero.push(handle);
        }
    }

    fn hold_members(&mut self, body: &Body) {
        each_member(body, &mut |value| {
            if let Some(handle) = value.handle()
                && let Some(facts) = self.facts.get_mut(handle.index() as usize)
            {
                facts.count = facts.count.saturating_add(1);
            }
        });
        if let Body::Closure { captures, .. } = body {
            for cell in captures {
                self.hold_handle(*cell);
            }
        }
    }

    fn unhold_members(&mut self, body: &Body) {
        let mut members = Vec::new();
        each_member(body, &mut |value| members.extend(value.handle()));
        if let Body::Closure { captures, .. } = body {
            members.extend(captures.iter().copied());
        }
        for handle in members {
            self.unhold_handle(handle);
        }
    }

    /// Marks the mutable object `handle` changed since it was last
    /// projected, with `card` dirty when it is a list or a table the heap
    /// holds.
    #[inline]
    pub(crate) fn mark(
        &mut self,
        handle: ArenaHandle,
        card: Option<u64>,
    ) -> Result<(), NativeError> {
        let facts = self.facts_mut(handle)?;
        if facts.based
            && let Some(card) = card
            && let Err(at) = facts.cards.binary_search(&card)
        {
            facts.cards.insert(at, card);
        }
        self.changed(handle);
        Ok(())
    }

    /// Marks the resident object `handle` changed since it was last
    /// projected.
    #[inline]
    fn changed(&mut self, handle: ArenaHandle) {
        let facts = &mut self.facts[handle.index() as usize];
        facts.dirty = true;
        if facts.entity.is_some() && !facts.listed {
            facts.listed = true;
            self.dirty.push(handle);
        }
    }

    /// An immutable composite's summary, as its page form certifies it;
    /// the default for everything else.
    pub fn summary_of(&self, value: &Value) -> Summary {
        value_summary(&self.facts, value)
    }

    /// A text value: inline when short.
    pub fn text(&mut self, text: &str) -> Value {
        match ShortText::new(text) {
            Some(short) => Value::Text(short),
            None => Value::LongText(self.long_text(text.into())),
        }
    }

    /// A function name value: inline when short.
    pub fn function(&mut self, name: &str) -> Value {
        match ShortText::new(name) {
            Some(short) => Value::Function(short),
            None => Value::LongFunction(self.insert(
                Body::LongFunction(resident_text(name.into())),
                Facts::default(),
            )),
        }
    }

    fn long_text(&mut self, text: String) -> ArenaHandle {
        self.insert(Body::LongText(resident_text(text)), Facts::default())
    }

    /// An integer value: inline when it fits a word.
    pub fn int(&mut self, int: Integer) -> Value {
        match int.word() {
            Some(word) => Value::Int(word),
            None => Value::BigInt(self.insert(Body::BigInt(int), Facts::default())),
        }
    }

    /// A timestamp value: inline when its nanoseconds fit a word.
    pub fn timestamp(&mut self, nanoseconds: Integer) -> Value {
        match nanoseconds.word() {
            Some(word) => Value::Timestamp(word),
            None => {
                Value::BigTimestamp(self.insert(Body::Timestamp(nanoseconds), Facts::default()))
            }
        }
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> Value {
        Value::Bytes(self.insert(
            Body::Bytes(Bytes {
                len: bytes.len() as u64,
                chunks: Box::new([Chunk::Resident(bytes.into())]),
            }),
            Facts::default(),
        ))
    }

    pub fn host_handle(&mut self, handle: Handle) -> Value {
        Value::Handle(self.insert(Body::Handle(handle), Facts::default()))
    }

    /// A tuple of `members`, or `too_deep` past `depth_limit`.
    pub fn tuple(&mut self, members: Vec<Value>, depth_limit: usize) -> Result<Value, Raise> {
        let summary = composite(&self.facts, &members);
        if summary.depth as usize > depth_limit {
            return Err(Raise::TooDeep);
        }
        let facts = Facts {
            summary,
            ..Facts::default()
        };
        if members.is_empty() {
            return Ok(Value::Tuple(self.empty_tuple(facts)));
        }
        Ok(Value::Tuple(
            self.insert(Body::Tuple(Sequence::whole(members)), facts),
        ))
    }

    /// An error value of `kind`, `message` and `data`.
    pub fn error(&mut self, kind: &str, message: &str, data: Value) -> Value {
        let kind = self.text(kind);
        let message = self.text(message);
        self.error_of(kind, message, data)
    }

    /// An error value of texts `kind` and `message`, and `data`.
    pub fn error_of(&mut self, kind: Value, message: Value, data: Value) -> Value {
        let summary = composite(&self.facts, std::slice::from_ref(&data));
        Value::Error(self.insert(
            Body::Error(ErrorBody {
                kind,
                message,
                data,
            }),
            Facts {
                summary,
                ..Facts::default()
            },
        ))
    }

    /// Installs what a native call built, in reservation order. None takes
    /// a durable id: a projection that saves one gives it.
    pub fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Result<(), NativeError> {
        let mut built = Vec::with_capacity(new.len());
        for (handle, object) in new {
            let (body, facts) = self.new_body(object)?;
            self.install_one(handle, body, facts);
            built.push(handle);
        }
        // Members are counted once the whole batch is placed: an object
        // may hold one built after it in the same call.
        for handle in built {
            if let Some(body) = self.objects.get(handle) {
                let facts = &mut self.facts;
                each_member(body, &mut |value| {
                    if let Some(handle) = value.handle()
                        && let Some(facts) = facts.get_mut(handle.index() as usize)
                    {
                        facts.count = facts.count.saturating_add(1);
                    }
                });
            }
        }
        Ok(())
    }

    /// Creates `object` in a fresh slot, as [`Self::install`] installs one
    /// a native call built: what a region's construction makes, its
    /// members' own objects already there. Its storage moves into the body.
    pub fn create(&mut self, object: NewObject) -> Result<Value, NativeError> {
        let naming = object.naming();
        let empty = matches!(&object, NewObject::Tuple(members) if members.is_empty());
        let (body, facts) = self.new_body(object)?;
        if empty {
            return Ok(naming(self.empty_tuple(facts)));
        }
        Ok(naming(self.insert(body, facts)))
    }

    /// Creates the map of `entries` in order, as [`Self::create`] creates
    /// [`NewObject::Map`] of them, from entries no vector collected: what
    /// a region's map literal makes of its operands.
    pub fn create_map(
        &mut self,
        entries: impl ExactSizeIterator<Item = (Value, Value)>,
    ) -> Result<Value, NativeError> {
        let (table, index) = tables::build(&self.objects, &self.facts, &self.layouts, entries)?;
        let facts = self.created(Some(index));
        Ok(Value::Map(self.insert(Body::Map(table), facts)))
    }

    /// Creates the list of `values`, as [`Self::create`] creates
    /// [`NewObject::List`] of them: a list a body holds inline is made
    /// without a vector.
    pub fn create_list(&mut self, values: &[Value]) -> Value {
        let chunks = Chunks::of(values).unwrap_or_else(|| Chunks::Whole(values.to_vec()));
        let sequence = Sequence {
            len: values.len() as u64,
            chunks,
        };
        let facts = self.created(None);
        Value::List(self.insert(Body::List(sequence), facts))
    }

    /// The body and facts of a new object: a mutable one created, without
    /// a durable id; an immutable composite with its summary.
    fn new_body(&mut self, object: NewObject) -> Result<(Body, Facts), NativeError> {
        let (body, index) = match object {
            NewObject::List(values) => (Body::List(Sequence::whole(values)), None),
            NewObject::Map(entries) => {
                let (table, index) = tables::build(
                    &self.objects,
                    &self.facts,
                    &self.layouts,
                    entries.into_iter(),
                )?;
                (Body::Map(table), Some(index))
            }
            NewObject::Set(members) => {
                let (table, index) = tables::build(
                    &self.objects,
                    &self.facts,
                    &self.layouts,
                    members.into_iter().map(|m| (m, Value::Absent)),
                )?;
                (Body::Set(table), Some(index))
            }
            NewObject::Record(fields) => {
                let (table, index) = tables::build(
                    &self.objects,
                    &self.facts,
                    &self.layouts,
                    fields.into_iter(),
                )?;
                (Body::Record(table), Some(index))
            }
            NewObject::Tuple(members) => {
                let summary = composite(&self.facts, &members);
                return Ok((
                    Body::Tuple(Sequence::whole(members)),
                    Facts {
                        summary,
                        ..Facts::default()
                    },
                ));
            }
            NewObject::LongText(text) => {
                return Ok((
                    Body::LongText(resident_text(text.as_ref().into())),
                    Facts::default(),
                ));
            }
            NewObject::Bytes(bytes) => {
                return Ok((
                    Body::Bytes(Bytes {
                        len: bytes.len() as u64,
                        chunks: Box::new([Chunk::Resident(bytes.as_ref().into())]),
                    }),
                    Facts::default(),
                ));
            }
            NewObject::BigInt(int) => return Ok((Body::BigInt(int), Facts::default())),
            NewObject::Timestamp(nanoseconds) => {
                return Ok((Body::Timestamp(nanoseconds), Facts::default()));
            }
            NewObject::LongFunction(name) => {
                return Ok((
                    Body::LongFunction(resident_text(name.as_ref().into())),
                    Facts::default(),
                ));
            }
            NewObject::Error {
                kind,
                message,
                data,
            } => {
                let summary = composite(&self.facts, std::slice::from_ref(&data));
                return Ok((
                    Body::Error(ErrorBody {
                        kind,
                        message,
                        data,
                    }),
                    Facts {
                        summary,
                        ..Facts::default()
                    },
                ));
            }
        };
        Ok((body, self.created(index)))
    }

    fn install_one(&mut self, handle: ArenaHandle, body: Body, facts: Facts) {
        self.resident = self.resident.saturating_add(body_values(&body));
        self.objects.install(handle, body);
        self.place(handle, facts);
    }

    /// Replaces a proxy's cold body with the contents the machine decoded,
    /// clean, with its table index.
    pub fn materialized(&mut self, handle: ArenaHandle, body: Body) -> Result<(), NativeError> {
        let index =
            match &body {
                Body::Map(table) | Body::Set(table) | Body::Record(table) => Some(
                    tables::index_of(&self.objects, &self.facts, &self.layouts, handle, table)?,
                ),
                _ => None,
            };
        self.resident = self.resident.saturating_add(body_values(&body));
        if self.needs_decode(handle)? && !has_cold(&body) {
            self.colds = self.colds.saturating_sub(1);
        }
        self.facts_mut(handle)?.cold = false;
        self.hold_members(&body);
        let placeholder = std::mem::replace(
            self.objects
                .get_mut(handle)
                .ok_or_else(|| malformed("a stale handle reached the machine"))?,
            body,
        );
        self.resident = self.resident.saturating_sub(body_values(&placeholder));
        self.unhold_members(&placeholder);
        let facts = self.facts_mut(handle)?;
        facts.revision = facts.revision.saturating_add(1);
        if index.is_some() {
            facts.index = index;
        }
        Ok(())
    }

    /// Puts the resident object `handle` back on its pages, as a collection
    /// that released it and a page that named it again would: a clean
    /// mutable list or table becomes a cold proxy of its kind, and a tuple,
    /// long text or bytes with a page form a cold chunk of it. Gives whether
    /// it did; anything else stays as it is. What the machine's force-cold
    /// laws read.
    pub fn evict(&mut self, handle: ArenaHandle) -> Result<bool, NativeError> {
        let facts = self.facts(handle)?;
        let paged = match facts.entity {
            Some(_) => !facts.dirty,
            None => facts.page.get().is_some(),
        };
        if !paged {
            return Ok(false);
        }
        // The body is dropped: what it held is held no longer.
        let Some(chilled) = self.chill(handle)? else {
            return Ok(false);
        };
        self.unhold_members(&chilled.body);
        Ok(true)
    }

    /// Sets the resident body of the list, table, tuple, long text or bytes
    /// `handle` aside, leaving the cold chunk a page that named it would: a
    /// read of it answers [`lash_kernel_doc::Retry::Cold`]. Gives what it
    /// set aside, for [`Self::thaw`]; nothing for any other body, or one
    /// already cold. What the force-cold laws read.
    pub fn chill(&mut self, handle: ArenaHandle) -> Result<Option<Chilled>, NativeError> {
        if self.needs_decode(handle)? {
            return Ok(None);
        }
        fn cold<T>(len: u64) -> Chunk<T> {
            Chunk::Cold {
                page: NO_PAGE,
                len: u32::try_from(len).unwrap_or(u32::MAX),
            }
        }
        let body = match self.body(handle)? {
            Body::List(_) => ProxyKind::List.placeholder(),
            Body::Map(_) => ProxyKind::Map.placeholder(),
            Body::Set(_) => ProxyKind::Set.placeholder(),
            Body::Record(_) => ProxyKind::Record.placeholder(),
            Body::Tuple(sequence) => Body::Tuple(Sequence {
                len: sequence.len,
                chunks: Chunks::Split(vec![cold(sequence.len)]),
            }),
            Body::LongText(text) => Body::LongText(LongText {
                summary: text.summary,
                chunks: Box::new([cold(text.summary.utf8_bytes)]),
            }),
            Body::Bytes(bytes) => Body::Bytes(Bytes {
                len: bytes.len,
                chunks: Box::new([cold(bytes.len)]),
            }),
            _ => return Ok(None),
        };
        self.resident = self.resident.saturating_add(body_values(&body));
        self.colds = self.colds.saturating_add(1);
        let body = std::mem::replace(
            self.objects
                .get_mut(handle)
                .ok_or_else(|| malformed("a stale handle reached the machine"))?,
            body,
        );
        self.resident = self.resident.saturating_sub(body_values(&body));
        let facts = self.facts_mut(handle)?;
        let mutable = facts.entity.is_some();
        facts.cold = mutable;
        let index = facts.index.take();
        Ok(Some(Chilled { body, index }))
    }

    /// Gives `handle` back the body [`Self::chill`] set aside: what it
    /// holds stayed counted while it was set aside.
    pub fn thaw(&mut self, handle: ArenaHandle, chilled: Chilled) -> Result<(), NativeError> {
        let Chilled { body, index } = chilled;
        self.resident = self.resident.saturating_add(body_values(&body));
        let cold = std::mem::replace(
            self.objects
                .get_mut(handle)
                .ok_or_else(|| malformed("a stale handle reached the machine"))?,
            body,
        );
        self.resident = self.resident.saturating_sub(body_values(&cold));
        self.colds = self.colds.saturating_sub(1);
        let facts = self.facts_mut(handle)?;
        facts.cold = false;
        facts.index = index;
        Ok(())
    }

    /// Sets `seqs` and `next` of a just-materialized table: the insertion
    /// sequences its page form keeps (`K-ITER-003`).
    pub fn set_sequences(
        &mut self,
        handle: ArenaHandle,
        seqs: Vec<u64>,
        next: u64,
    ) -> Result<(), NativeError> {
        let index = self
            .facts_mut(handle)?
            .index
            .as_mut()
            .ok_or_else(|| malformed("a table has no index"))?;
        index.resequence(seqs, next)
    }

    /// Takes the dirty list: every object with an id that changed since it
    /// was last projected, in id order, each still dirty. What a
    /// projection writes back.
    pub fn take_dirty(&mut self) -> Vec<(EntityId, ArenaHandle)> {
        let listed = std::mem::take(&mut self.dirty);
        let mut dirty = Vec::with_capacity(listed.len());
        for handle in listed {
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| work.projection_visits += 1);
            if self.objects.get(handle).is_none() {
                continue;
            }
            let facts = &mut self.facts[handle.index() as usize];
            facts.listed = false;
            if facts.dirty
                && let Some(entity) = facts.entity
            {
                dirty.push((entity, handle));
            }
        }
        dirty.sort_unstable_by_key(|(entity, _)| *entity);
        dirty
    }

    /// Marks `handle` clean: the heap holds what it does, as its head.
    pub fn clean(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        let facts = self.facts_mut(handle)?;
        facts.dirty = false;
        facts.based = true;
        facts.cards.clear();
        Ok(())
    }

    /// Keeps `page` as the page form of the immutable `handle`, unless it
    /// has one.
    pub fn keep_page(&self, handle: ArenaHandle, page: PageValue) -> Result<(), NativeError> {
        let _ = self.facts(handle)?.page.set(page);
        Ok(())
    }

    /// The durable id of the mutable object `handle`, given now if it has
    /// none: the next primary ordinal. It is dirty already, as every object
    /// the run creates is until it is projected.
    pub fn identify(&mut self, handle: ArenaHandle) -> Result<EntityId, NativeError> {
        if let Some(entity) = self.facts(handle)?.entity {
            return Ok(entity);
        }
        if !is_mutable(self.body(handle)?) {
            return Err(malformed("an immutable has no durable identity"));
        }
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.identified += 1);
        let entity = EntityId {
            family: lash_kernel_doc::heap::FamilyId::PRIMARY,
            ordinal: self.next_ordinal,
        };
        self.next_ordinal = self
            .next_ordinal
            .checked_add(1)
            .ok_or_else(|| malformed("identity ordinals overflow"))?;
        self.entities.insert(entity, handle);
        let facts = &mut self.facts[handle.index() as usize];
        facts.entity = Some(entity);
        facts.dirty = true;
        facts.listed = true;
        self.dirty.push(handle);
        Ok(entity)
    }

    /// The canonical walk from `roots`, in order (seam C): depth first and
    /// pre-order, it gives each mutable object without an id the next
    /// ordinal when it first reaches it, descending only into those and
    /// into immutables that have no page form. An object with an id is
    /// not descended: what it reaches that is new, it reaches as a dirty
    /// object, which [`Self::walk_dirty`] walks.
    pub fn walk(&mut self, roots: &[ArenaHandle]) -> Result<(), NativeError> {
        let mut pending: Vec<ArenaHandle> = roots.iter().rev().copied().collect();
        let mut marked = Vec::new();
        let result = self.walk_from(&mut pending, &mut marked);
        for handle in marked {
            self.facts[handle.index() as usize].mark = false;
        }
        result
    }

    /// Walks what every dirty object with an id holds, in id order: the
    /// new objects only a changed object reaches take their ids after
    /// those the roots reach.
    pub fn walk_dirty(&mut self) -> Result<(), NativeError> {
        let mut dirty: Vec<(EntityId, ArenaHandle)> = self
            .dirty
            .iter()
            .filter(|handle| self.objects.get(**handle).is_some())
            .filter_map(|handle| {
                let facts = &self.facts[handle.index() as usize];
                facts.dirty.then_some((facts.entity?, *handle))
            })
            .collect();
        dirty.sort_unstable_by_key(|(entity, _)| *entity);
        dirty.dedup_by_key(|(entity, _)| *entity);
        let mut marked = Vec::new();
        let mut result = Ok(());
        for (_, handle) in dirty {
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| work.projection_visits += 1);
            let mut pending = Vec::new();
            self.push_members(handle, &mut pending)?;
            result = self.walk_from(&mut pending, &mut marked);
            if result.is_err() {
                break;
            }
        }
        for handle in marked {
            self.facts[handle.index() as usize].mark = false;
        }
        result
    }

    fn walk_from(
        &mut self,
        pending: &mut Vec<ArenaHandle>,
        marked: &mut Vec<ArenaHandle>,
    ) -> Result<(), NativeError> {
        while let Some(handle) = pending.pop() {
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| work.projection_visits += 1);
            let Some(body) = self.objects.get(handle) else {
                return Err(malformed("a stale handle reached the machine"));
            };
            let facts = &self.facts[handle.index() as usize];
            if facts.mark || facts.entity.is_some() {
                continue;
            }
            if is_mutable(body) {
                self.identify(handle)?;
            } else if facts.page.get().is_some() {
                continue;
            } else {
                self.facts[handle.index() as usize].mark = true;
                marked.push(handle);
            }
            self.push_members(handle, pending)?;
        }
        Ok(())
    }

    /// Pushes what `handle` holds so that its first member is popped first.
    fn push_members(
        &self,
        handle: ArenaHandle,
        pending: &mut Vec<ArenaHandle>,
    ) -> Result<(), NativeError> {
        let start = pending.len();
        let body = self.body(handle)?;
        each_member(body, &mut |value| pending.extend(value.handle()));
        if let Body::Closure { captures, .. } = body {
            pending.extend(captures.iter().copied());
        }
        pending[start..].reverse();
        Ok(())
    }

    /// How many objects the arena has freed: while it stays put, no handle
    /// was freed, and none was given to another object.
    #[inline(always)]
    pub fn freed(&self) -> u64 {
        self.freed
    }

    /// Whether a release is due: enough objects wait on the zero-count
    /// list.
    #[inline]
    pub fn release_due(&self) -> bool {
        self.zero.len() >= self.release_at
    }

    /// Frees every object on the zero-count list that nothing `roots`
    /// visits holds, and with it what it alone held. An object with an id
    /// that changed since it was projected is kept until a projection
    /// writes it back: a page may name it, and only the arena holds what
    /// it is. Call only where every value the machine holds outside the
    /// arena's members is visited: at a statement boundary.
    pub fn release(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) {
        if self.zero.is_empty() {
            return;
        }
        let rooted = self.mark_roots(roots);
        let mut pending = std::mem::take(&mut self.zero);
        let mut kept = Vec::new();
        let mut freed = Vec::new();
        while let Some(handle) = pending.pop() {
            if self.objects.get(handle).is_none() {
                continue;
            }
            let facts = &mut self.facts[handle.index() as usize];
            if facts.count > 0 {
                facts.zero = false;
                continue;
            }
            if facts.mark || (facts.entity.is_some() && facts.dirty) {
                kept.push(handle);
                continue;
            }
            self.free(handle, &mut freed);
            for member in freed.drain(..) {
                let Some(facts) = self.facts.get_mut(member.index() as usize) else {
                    continue;
                };
                if facts.count == u32::MAX {
                    continue;
                }
                facts.count = facts.count.saturating_sub(1);
                if facts.count == 0 && !facts.zero {
                    facts.zero = true;
                    pending.push(member);
                }
            }
        }
        for handle in rooted {
            self.facts[handle.index() as usize].mark = false;
        }
        self.release_at = kept.len().saturating_mul(2).max(FIRST_RELEASE);
        self.zero = kept;
    }

    /// Marks the objects `roots` visits, each once, and gives them.
    fn mark_roots(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) -> Vec<ArenaHandle> {
        let mut visited = Vec::new();
        roots(&mut |value| visited.extend(value.handle()));
        let mut rooted = Vec::with_capacity(visited.len());
        for handle in visited {
            if self.objects.get(handle).is_none() {
                continue;
            }
            let facts = &mut self.facts[handle.index() as usize];
            if !facts.mark {
                facts.mark = true;
                rooted.push(handle);
            }
        }
        rooted
    }

    /// Frees `handle`: gives what its body held to `members`, uncounted by
    /// the caller.
    fn free(&mut self, handle: ArenaHandle, members: &mut Vec<ArenaHandle>) {
        if self.empty_tuple == Some(handle) {
            self.empty_tuple = None;
        }
        let index = handle.index() as usize;
        let facts = std::mem::take(&mut self.facts[index]);
        if let Some(entity) = facts.entity {
            self.entities.remove(&entity);
        }
        if let Some(body) = self.objects.get(handle) {
            self.resident = self.resident.saturating_sub(body_values(body));
            if facts.cold || has_cold(body) {
                self.colds = self.colds.saturating_sub(1);
            }
            each_member(body, &mut |value| members.extend(value.handle()));
            if let Body::Closure { captures, .. } = body {
                members.extend(captures.iter().copied());
            }
        }
        self.objects.release(handle);
        self.freed += 1;
    }

    /// Whether a cycle collection is due: resident values grew past the
    /// trigger, and some count fell without reaching zero since the last
    /// collection. Without such a fall no garbage cycle can have formed,
    /// and the collection exits at once.
    #[inline]
    pub fn cycles_due(&self) -> bool {
        self.resident >= self.collect_at && self.candidates > 0
    }

    /// Frees every object neither reachable from what `roots` visits nor
    /// from an object with an id that changed since it was projected: the
    /// garbage cycles counts cannot free. A clean proxy is decoded again
    /// from the heap if a page names it later. Call only where every value
    /// the machine holds outside the arena's members is visited.
    pub fn collect_cycles(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) {
        let mut pending: Vec<ArenaHandle> = Vec::new();
        roots(&mut |value| pending.extend(value.handle()));
        for (index, facts) in self.facts.iter().enumerate() {
            if facts.dirty
                && facts.entity.is_some()
                && let Some(handle) = self.objects.handle_at(index as u32)
            {
                pending.push(handle);
            }
        }
        while let Some(handle) = pending.pop() {
            let Some(body) = self.objects.get(handle) else {
                continue;
            };
            let facts = &mut self.facts[handle.index() as usize];
            if facts.mark {
                continue;
            }
            facts.mark = true;
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| work.arena_marks += 1);
            each_member(body, &mut |value| pending.extend(value.handle()));
            if let Body::Closure { captures, .. } = body {
                pending.extend(captures.iter().copied());
            }
        }
        let mut survivors = Vec::new();
        for index in 0..self.facts.len() {
            if std::mem::take(&mut self.facts[index].mark) {
                continue;
            }
            let Some(handle) = self.objects.handle_at(index as u32) else {
                continue;
            };
            self.free(handle, &mut survivors);
        }
        // What a freed object held and survives is held one member less.
        for member in survivors {
            if self.objects.get(member).is_some() {
                self.unhold_handle(member);
            }
        }
        self.zero
            .retain(|handle| self.objects.get(*handle).is_some());
        self.dirty
            .retain(|handle| self.objects.get(*handle).is_some());
        self.candidates = 0;
        self.collect_at = self.resident.saturating_mul(2).max(FIRST_COLLECTION);
        self.bound_boundary();
    }
}

/// Whether `body` is a mutable object's: one a durable id names.
fn is_mutable(body: &Body) -> bool {
    matches!(
        body,
        Body::List(_)
            | Body::Map(_)
            | Body::Set(_)
            | Body::Record(_)
            | Body::Closure { .. }
            | Body::Cell(_)
    )
}

/// The first cold chunk of `chunks`.
pub(crate) fn first_cold<T>(chunks: &[Chunk<T>]) -> Option<u32> {
    chunks
        .iter()
        .position(|chunk| matches!(chunk, Chunk::Cold { .. }))
        .map(|at| at as u32)
}

/// A guest error the arena's own constructors raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raise {
    /// A value nests deeper than the run's value depth (`K-VAL-034`).
    TooDeep,
}

fn resident_text(text: String) -> LongText {
    LongText {
        summary: text_summary(&text),
        chunks: Box::new([Chunk::Resident(text)]),
    }
}

/// A cold one-chunk long text whose page form is paged.
pub fn cold_text(summary: TextSummary, page: lash_kernel_doc::heap::PageRef) -> LongText {
    LongText {
        summary,
        chunks: Box::new([Chunk::Cold {
            page,
            len: u32::try_from(summary.utf8_bytes).unwrap_or(u32::MAX),
        }]),
    }
}

/// Whether a body has a chunk on its pages.
fn has_cold(body: &Body) -> bool {
    fn cold<T>(chunks: &[Chunk<T>]) -> bool {
        chunks
            .iter()
            .any(|chunk| matches!(chunk, Chunk::Cold { .. }))
    }
    match body {
        Body::List(sequence) | Body::Tuple(sequence) => sequence.chunks.first_cold().is_some(),
        Body::Map(table) | Body::Set(table) | Body::Record(table) => {
            table.chunks.first_cold().is_some()
        }
        Body::LongText(text) | Body::LongFunction(text) => cold(&text.chunks),
        Body::Bytes(bytes) => cold(&bytes.chunks),
        _ => false,
    }
}

/// How many values a body holds resident, plus one: the collection
/// trigger's measure.
fn body_values(body: &Body) -> u64 {
    1 + match body {
        Body::List(sequence) | Body::Tuple(sequence) => sequence.chunks.resident_len(),
        Body::Map(table) | Body::Set(table) | Body::Record(table) => table.chunks.resident_len(),
        Body::LongText(text) | Body::LongFunction(text) => text.summary.utf8_bytes / 16,
        Body::Bytes(bytes) => bytes.len / 16,
        Body::Closure { captures, .. } => captures.len() as u64,
        _ => 0,
    }
}

/// Visits every value `body` holds resident. A closure's captured cells
/// are handles, not values: its `captures`.
#[inline]
pub fn each_member(body: &Body, visit: &mut impl FnMut(&Value)) {
    fn chunked(chunks: &Chunks, visit: &mut impl FnMut(&Value)) {
        for chunk in 0..chunks.count() {
            if let Some(values) = chunks.resident(chunk) {
                values.iter().for_each(&mut *visit);
            }
        }
    }
    match body {
        Body::List(sequence) | Body::Tuple(sequence) => chunked(&sequence.chunks, visit),
        Body::Map(table) | Body::Set(table) | Body::Record(table) => chunked(&table.chunks, visit),
        Body::Error(error) => {
            visit(&error.kind);
            visit(&error.message);
            visit(&error.data);
        }
        Body::Cell(value) => visit(value),
        Body::Closure { .. }
        | Body::Free
        | Body::BigInt(_)
        | Body::LongText(_)
        | Body::Bytes(_)
        | Body::Timestamp(_)
        | Body::LongFunction(_)
        | Body::Handle(_) => {}
    }
}

fn value_summary(facts: &[Facts], value: &Value) -> Summary {
    match value {
        Value::Tuple(handle) | Value::Error(handle) => facts
            .get(handle.index() as usize)
            .map_or_else(Summary::default, |facts| facts.summary),
        Value::List(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Record(_)
        | Value::Closure(_)
        | Value::ObjectRef(_) => Summary {
            has_edges: true,
            ..Summary::default()
        },
        Value::Task(_) | Value::TaskRef(_) => Summary {
            task_edges: 1,
            has_edges: true,
            ..Summary::default()
        },
        _ => Summary::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::heap::FamilyId;

    /// FIG-6791: a decoded body takes its placeholder's place in what the
    /// capacity meter counts, so the run holds what a recount finds.
    #[test]
    fn a_decoded_body_replaces_its_placeholder_in_the_count() {
        let mut arena = Arena::new();
        let entity = EntityId {
            family: FamilyId(1),
            ordinal: 1,
        };
        let handle = arena.proxy(entity, ProxyKind::List);
        let body = Body::List(Sequence::whole(vec![Value::Null, Value::Null]));
        arena.materialized(handle, body).unwrap();
        let counted = arena.held();
        arena.recount_resident();
        assert_eq!(counted, arena.held());
    }

    /// FIG-6814: while an empty tuple is live, every empty tuple the run
    /// makes or decodes is that one object, so evaluating `()` allocates
    /// nothing and the meter counts it once. Once nothing holds it, it is
    /// freed as any object is, and the next is made afresh.
    #[test]
    fn an_empty_tuple_is_one_object_while_one_is_live() {
        let mut arena = Arena::new();
        let Ok(Value::Tuple(first)) = arena.tuple(Vec::new(), 8) else {
            panic!("an empty tuple")
        };
        let held = arena.held();
        let decoded = arena.immutable(
            Body::Tuple(Sequence::whole(Vec::new())),
            Summary {
                depth: 1,
                ..Summary::default()
            },
            None,
        );
        assert_eq!(decoded, first);
        for made in [
            arena.tuple(Vec::new(), 8).unwrap(),
            arena.create(NewObject::Tuple(Vec::new())).unwrap(),
        ] {
            assert!(matches!(made, Value::Tuple(handle) if handle == first));
        }
        assert_eq!(arena.held(), held);
        assert!(
            arena.tuple(Vec::new(), 0).is_err(),
            "depth is checked first"
        );
        assert!(matches!(
            arena.tuple(vec![Value::Null], 8),
            Ok(Value::Tuple(handle)) if handle != first
        ));
        arena.release(&mut |_| {});
        assert!(arena.body(first).is_err());
        let Ok(Value::Tuple(next)) = arena.tuple(Vec::new(), 8) else {
            panic!("an empty tuple")
        };
        assert_ne!(next, first);
        assert!(arena.body(next).is_ok());
    }

    /// FIG-7001: a statement boundary is due exactly where its full tests
    /// say, and has nothing left to end or evict where it is not: resident
    /// values past the collection mark make no collection due until a count
    /// falls without reaching zero, the first such fall makes it due, a
    /// collection clears it, and a full zero-count list makes a release due.
    #[test]
    fn a_boundary_is_due_exactly_where_its_full_tests_say() {
        fn boundary(arena: &mut Arena) -> bool {
            let due = arena.boundary_due();
            assert!(arena.streaked.is_empty() && arena.resident < arena.evict_at);
            assert_eq!(due, arena.release_due() || arena.cycles_due());
            due
        }
        let mut arena = Arena::new();
        assert!(!boundary(&mut arena));
        let big = arena.create_list(&vec![Value::Null; FIRST_COLLECTION as usize]);
        let shared = arena.create_list(&[Value::Null]);
        let holders = [arena.create_list(&[shared]), arena.create_list(&[shared])];
        assert!(arena.resident >= arena.collect_at);
        assert!(
            !boundary(&mut arena),
            "no count fell since the last collection"
        );
        let Value::List(holder) = holders[0] else {
            panic!("a list")
        };
        arena.list_set(holder, 0, Value::Null).unwrap();
        assert!(boundary(&mut arena), "a count fell without reaching zero");
        let roots = [big, shared, holders[0], holders[1]];
        arena.collect_cycles(&mut |visit| roots.iter().for_each(&mut *visit));
        assert!(!boundary(&mut arena), "the collection cleared what was due");
        for _ in 0..FIRST_RELEASE {
            arena.create_list(&[Value::Null]);
        }
        assert!(boundary(&mut arena), "the zero-count list is full");
    }
}
