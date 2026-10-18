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
mod ops;
mod pricing;
mod tables;

pub use crate::paged::stored::CARD;
pub use bench::{Bench, Called, Usage};
pub use chunks::{ColdChunk, PayloadPart};
pub use keys::{ArenaSemantics, text_key};
pub use ops::invalid_key;
use ops::{composite, object_cost};
pub use pricing::{Pricing, Reached};
pub use tables::{Index, Place};

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use lash_kernel_doc::arena::{
    ArenaHandle, Body, Bytes, Chunk, ErrorBody, LongText, Objects, Sequence, ShortText,
};
use lash_kernel_doc::heap::{EntityId, PageValue, TextSummary};
use lash_kernel_doc::heap_values::text_summary;
use lash_kernel_doc::{Handle, Integer, NativeError, NewObject, Value, ValueKind};

/// The summary an immutable composite keeps: what its page form's
/// descriptor certifies (`TupleSummary`, `ErrorRef`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// The logical cost of its members where it is held.
    pub cost: u64,
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
    /// An immutable composite's summary; a mutable object's logical cost
    /// in `summary.cost`, as the heap prices its head.
    pub summary: Summary,
    /// A resident map's, set's or record's key index.
    pub index: Option<Box<Index>>,
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
    /// The seal it was created after, when it was created by the run:
    /// an object created since the last seal is young (seam I).
    born: Option<u64>,
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
            vec![Chunk::Cold {
                page: NO_PAGE,
                len: 0,
            }]
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
    index: Option<Box<Index>>,
}

/// The objects of one run and their facts.
#[derive(Debug, Default)]
pub struct Arena {
    objects: Objects,
    facts: Vec<Facts>,
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
    /// Seals and exact refinements since the run began: what tells a
    /// young object.
    seals: u64,
    unaccounted: bool,
    /// Mutable objects the run created.
    allocations: u64,
    /// The mutable objects created since the statement began: until a
    /// root holds one, only the arena knows it is live.
    statement: Vec<ArenaHandle>,
    /// The cost of the young mutable objects released since it was last
    /// taken (seam I).
    released_young: u64,
    /// Values held by resident bodies, roughly: what triggers a collection.
    resident: u64,
    /// Objects with a cold chunk, at most: none means nothing needs decoding.
    colds: u64,
    collect_at: u64,
    /// Resident values past which a statement boundary drops clean large
    /// objects back to their pages.
    evict_at: u64,
    /// Objects decoded since the last statement boundary.
    streaked: Vec<ArenaHandle>,
}

/// What a collection's roots are visited with: each value they hold.
pub type Visit<'a> = dyn FnMut(&Value) + 'a;

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

    /// An arena with lifetime management but no memory-accounting age ledger.
    pub fn without_memory_accounting(mut self) -> Self {
        self.unaccounted = true;
        self
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
        ArenaSemantics::new(&self.objects, &self.facts)
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

    /// The body and facts of `handle`, for an edit: the caller marks it
    /// dirty.
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

    /// A seal or an exact refinement priced what is live: every object
    /// created before it is old, and releasing it refunds nothing.
    pub fn rebased(&mut self) {
        if self.unaccounted {
            return;
        }
        self.seals += 1;
    }

    /// The statement ended: what it created is held by a root or garbage.
    pub fn statement_ended(&mut self) {
        self.statement.clear();
    }

    /// The cost of the young mutable objects released since the last call:
    /// what the memory counter's young part refunds (seam I).
    pub fn take_released_young(&mut self) -> u64 {
        std::mem::take(&mut self.released_young)
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
        self.insert(body, facts)
    }

    /// The facts of a mutable object the run creates: dirty, young, and
    /// without a durable id until a projection saves it.
    fn created(&mut self, cost: u64, index: Option<Box<Index>>) -> Facts {
        self.allocations += 1;
        Facts {
            dirty: true,
            born: (!self.unaccounted).then_some(self.seals),
            summary: Summary {
                cost,
                ..Summary::default()
            },
            index,
            ..Facts::default()
        }
    }

    /// Creates the mutable object `body`.
    fn insert_created(&mut self, body: Body) -> ArenaHandle {
        let cost = if self.unaccounted {
            0
        } else {
            object_cost(&self.objects, &self.facts, &body)
        };
        let facts = self.created(cost, None);
        let handle = self.insert(body, facts);
        if !self.unaccounted {
            self.statement.push(handle);
        }
        handle
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

    /// The logical cost of `value` where it is held: the heap's local
    /// cost of its page form.
    pub fn cost_of(&self, value: &Value) -> u64 {
        if self.unaccounted {
            0
        } else {
            self.cost_of_metered(value)
        }
    }

    /// Logical pricing for a caller already dispatched to the accounted stream.
    #[inline]
    pub fn cost_of_metered(&self, value: &Value) -> u64 {
        local_cost(&self.objects, &self.facts, value)
    }

    fn composite_summary(&self, members: &[Value]) -> Summary {
        if self.unaccounted {
            composite::<false>(&self.objects, &self.facts, members)
        } else {
            composite::<true>(&self.objects, &self.facts, members)
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

    fn long_text(&mut self, text: Box<str>) -> ArenaHandle {
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
        let summary = self.composite_summary(&members);
        if summary.depth as usize > depth_limit {
            return Err(Raise::TooDeep);
        }
        let sequence = Sequence {
            len: members.len() as u64,
            chunks: vec![Chunk::Resident(members)],
        };
        Ok(Value::Tuple(self.insert(
            Body::Tuple(sequence),
            Facts {
                summary,
                ..Facts::default()
            },
        )))
    }

    /// An error value of `kind`, `message` and `data`.
    pub fn error(&mut self, kind: &str, message: &str, data: Value) -> Value {
        let kind = self.text(kind);
        let message = self.text(message);
        self.error_of(kind, message, data)
    }

    /// An error value of texts `kind` and `message`, and `data`.
    pub fn error_of(&mut self, kind: Value, message: Value, data: Value) -> Value {
        let mut summary = self.composite_summary(std::slice::from_ref(&data));
        if !self.unaccounted {
            summary.cost = text_bytes(&self.objects, &kind)
                .saturating_add(text_bytes(&self.objects, &message))
                .saturating_add(local_cost(&self.objects, &self.facts, &data));
        }
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

    /// Installs what a native call built, in reservation order, and gives
    /// the logical cost of its mutable objects. None takes a durable id:
    /// a projection that saves one gives it.
    pub fn install(&mut self, new: Vec<(ArenaHandle, NewObject)>) -> Result<u64, NativeError> {
        let mut grown = 0u64;
        let installed: Vec<ArenaHandle> = new.iter().map(|(handle, _)| *handle).collect();
        for (handle, object) in new {
            let (body, facts) = match object {
                NewObject::List(values) => (
                    Body::List(Sequence {
                        len: values.len() as u64,
                        chunks: vec![Chunk::Resident(values)],
                    }),
                    None,
                ),
                NewObject::Map(entries) => (
                    Body::Map(tables::build(&self.objects, &self.facts, entries)?),
                    Some(ValueKind::Map),
                ),
                NewObject::Set(members) => (
                    Body::Set(tables::build(
                        &self.objects,
                        &self.facts,
                        members.into_iter().map(|m| (m, Value::Absent)).collect(),
                    )?),
                    Some(ValueKind::Set),
                ),
                NewObject::Record(fields) => (
                    Body::Record(tables::build(&self.objects, &self.facts, fields)?),
                    Some(ValueKind::Record),
                ),
                NewObject::Tuple(members) => {
                    let summary = self.composite_summary(&members);
                    self.install_one(
                        handle,
                        Body::Tuple(Sequence {
                            len: members.len() as u64,
                            chunks: vec![Chunk::Resident(members)],
                        }),
                        Facts {
                            summary,
                            ..Facts::default()
                        },
                    );
                    continue;
                }
                NewObject::LongText(text) => {
                    self.install_one(
                        handle,
                        Body::LongText(resident_text(text.as_ref().into())),
                        Facts::default(),
                    );
                    continue;
                }
                NewObject::Bytes(bytes) => {
                    self.install_one(
                        handle,
                        Body::Bytes(Bytes {
                            len: bytes.len() as u64,
                            chunks: Box::new([Chunk::Resident(bytes.as_ref().into())]),
                        }),
                        Facts::default(),
                    );
                    continue;
                }
                NewObject::BigInt(int) => {
                    self.install_one(handle, Body::BigInt(int), Facts::default());
                    continue;
                }
                NewObject::Timestamp(nanoseconds) => {
                    self.install_one(handle, Body::Timestamp(nanoseconds), Facts::default());
                    continue;
                }
                NewObject::LongFunction(name) => {
                    self.install_one(
                        handle,
                        Body::LongFunction(resident_text(name.as_ref().into())),
                        Facts::default(),
                    );
                    continue;
                }
                NewObject::Error {
                    kind,
                    message,
                    data,
                } => {
                    let mut summary = self.composite_summary(std::slice::from_ref(&data));
                    if !self.unaccounted {
                        summary.cost = text_bytes(&self.objects, &kind)
                            .saturating_add(text_bytes(&self.objects, &message))
                            .saturating_add(local_cost(&self.objects, &self.facts, &data));
                    }
                    self.install_one(
                        handle,
                        Body::Error(ErrorBody {
                            kind,
                            message,
                            data,
                        }),
                        Facts {
                            summary,
                            ..Facts::default()
                        },
                    );
                    continue;
                }
            };
            let index = match &body {
                Body::Map(table) | Body::Set(table) | Body::Record(table) => facts
                    .map(|_| tables::index_of(&self.objects, &self.facts, handle, table))
                    .transpose()?,
                _ => None,
            };
            let cost = if self.unaccounted {
                0
            } else {
                object_cost(&self.objects, &self.facts, &body)
            };
            if !self.unaccounted {
                grown = grown.saturating_add(cost);
            }
            let facts = self.created(cost, index.map(Box::new));
            self.install_one(handle, body, facts);
            if !self.unaccounted {
                self.statement.push(handle);
            }
        }
        // Members are counted once the whole batch is placed: an object
        // may hold one built after it in the same call.
        let mut members = Vec::new();
        for handle in installed {
            if let Some(body) = self.objects.get(handle) {
                each_member(body, &mut |value| members.extend(value.handle()));
            }
        }
        for member in members {
            self.hold_handle(member);
        }
        Ok(grown)
    }

    fn install_one(&mut self, handle: ArenaHandle, body: Body, facts: Facts) {
        self.resident = self.resident.saturating_add(body_values(&body));
        self.objects.install(handle, body);
        self.place(handle, facts);
    }

    /// Replaces a proxy's cold body with the contents the machine decoded,
    /// clean, with its table index.
    pub fn materialized(&mut self, handle: ArenaHandle, body: Body) -> Result<(), NativeError> {
        let index = match &body {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => Some(Box::new(
                tables::index_of(&self.objects, &self.facts, handle, table)?,
            )),
            _ => None,
        };
        self.resident = self.resident.saturating_add(body_values(&body));
        if self.needs_decode(handle)? && !has_cold(&body) {
            self.colds = self.colds.saturating_sub(1);
        }
        let mutable = self.facts(handle)?.entity.is_some();
        let cost =
            (mutable && !self.unaccounted).then(|| object_cost(&self.objects, &self.facts, &body));
        self.facts_mut(handle)?.cold = false;
        self.hold_members(&body);
        let placeholder = std::mem::replace(
            self.objects
                .get_mut(handle)
                .ok_or_else(|| malformed("a stale handle reached the machine"))?,
            body,
        );
        self.unhold_members(&placeholder);
        let facts = self.facts_mut(handle)?;
        if let Some(cost) = cost {
            facts.summary.cost = cost;
        }
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
                chunks: vec![cold(sequence.len)],
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

    /// Whether a release is due: enough objects wait on the zero-count
    /// list.
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
            let freed = self.free(handle);
            for member in freed {
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

    /// [`Self::release`] inside a statement, where what the running
    /// statement made is live though no root holds it yet: an ephemeral
    /// run's collection point, which has no published roots to refine.
    pub fn release_inside_statement(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) {
        let mut made = Vec::new();
        for handle in &self.statement {
            if self.objects.get(*handle).is_none() {
                continue;
            }
            let facts = &mut self.facts[handle.index() as usize];
            if !facts.mark {
                facts.mark = true;
                made.push(*handle);
            }
        }
        self.release(roots);
        for handle in made {
            self.facts[handle.index() as usize].mark = false;
        }
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

    /// Frees `handle`: gives what its body held, uncounted by the caller.
    fn free(&mut self, handle: ArenaHandle) -> Vec<ArenaHandle> {
        let index = handle.index() as usize;
        let facts = std::mem::take(&mut self.facts[index]);
        if let Some(entity) = facts.entity {
            self.entities.remove(&entity);
        }
        let mut members = Vec::new();
        if let Some(body) = self.objects.get(handle) {
            self.resident = self.resident.saturating_sub(body_values(body));
            if facts.cold || has_cold(body) {
                self.colds = self.colds.saturating_sub(1);
            }
            // Only an object no projection saved is known dead here: a
            // page may still name one with an id.
            if facts.born == Some(self.seals) && facts.entity.is_none() {
                self.released_young = self.released_young.saturating_add(facts.summary.cost);
            }
            each_member(body, &mut |value| members.extend(value.handle()));
            if let Body::Closure { captures, .. } = body {
                members.extend(captures.iter().copied());
            }
        }
        self.objects.release(handle);
        members
    }

    /// Whether a cycle collection is due: resident values grew past the
    /// trigger, and some count fell without reaching zero since the last
    /// collection. Without such a fall no garbage cycle can have formed,
    /// and the collection exits at once.
    pub fn cycles_due(&self) -> bool {
        self.resident >= self.collect_at && self.candidates > 0
    }

    /// The cost of the young objects reachable from what `roots` visits
    /// or from what the running statement made: what an ephemeral run
    /// holds in the arena, priced without projecting it. Garbage cycles
    /// are not reached, as an exact refinement does not trace them.
    pub fn reachable_young_cost(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) -> u64 {
        let mut pending: Vec<ArenaHandle> = self.statement.clone();
        roots(&mut |value| pending.extend(value.handle()));
        let (mut marked, mut cost) = (Vec::new(), 0_u64);
        while let Some(handle) = pending.pop() {
            let Some(body) = self.objects.get(handle) else {
                continue;
            };
            let facts = &mut self.facts[handle.index() as usize];
            if facts.mark {
                continue;
            }
            facts.mark = true;
            crate::perf_witness::record(|work| work.arena_marks += 1);
            marked.push(handle);
            if facts.born == Some(self.seals) && facts.entity.is_none() {
                cost = cost.saturating_add(facts.summary.cost);
            }
            each_member(body, &mut |value| pending.extend(value.handle()));
            if let Body::Closure { captures, .. } = body {
                pending.extend(captures.iter().copied());
            }
        }
        for handle in marked {
            self.facts[handle.index() as usize].mark = false;
        }
        cost
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
            survivors.extend(self.free(handle));
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

/// The values of resident `chunks`, in order, in one vector.
pub(crate) fn join(chunks: Vec<Chunk<Vec<Value>>>) -> Vec<Value> {
    let mut chunks = chunks.into_iter();
    let mut joined = match chunks.next() {
        Some(Chunk::Resident(values)) => values,
        _ => Vec::new(),
    };
    for chunk in chunks {
        if let Chunk::Resident(values) = chunk {
            joined.extend(values);
        }
    }
    joined
}

/// A guest error the arena's own constructors raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raise {
    /// A value nests deeper than the run's value depth (`K-VAL-034`).
    TooDeep,
}

fn resident_text(text: Box<str>) -> LongText {
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
        Body::List(sequence) | Body::Tuple(sequence) => cold(&sequence.chunks),
        Body::Map(table) | Body::Set(table) | Body::Record(table) => cold(&table.chunks),
        Body::LongText(text) | Body::LongFunction(text) => cold(&text.chunks),
        Body::Bytes(bytes) => cold(&bytes.chunks),
        _ => false,
    }
}

/// How many values a body holds resident, plus one: the collection
/// trigger's measure.
fn body_values(body: &Body) -> u64 {
    let chunked = |chunks: &[Chunk<Vec<Value>>]| -> u64 {
        chunks
            .iter()
            .map(|chunk| match chunk {
                Chunk::Resident(values) => values.len() as u64,
                Chunk::Cold { .. } => 0,
            })
            .sum()
    };
    1 + match body {
        Body::List(sequence) | Body::Tuple(sequence) => chunked(&sequence.chunks),
        Body::Map(table) | Body::Set(table) | Body::Record(table) => chunked(&table.chunks),
        Body::LongText(text) | Body::LongFunction(text) => text.summary.utf8_bytes / 16,
        Body::Bytes(bytes) => bytes.len / 16,
        Body::Closure { captures, .. } => captures.len() as u64,
        _ => 0,
    }
}

/// Visits every value `body` holds resident. A closure's captured cells
/// are handles, not values: its `captures`.
pub fn each_member(body: &Body, visit: &mut dyn FnMut(&Value)) {
    let chunked = |chunks: &[Chunk<Vec<Value>>], visit: &mut dyn FnMut(&Value)| {
        for chunk in chunks {
            if let Chunk::Resident(values) = chunk {
                values.iter().for_each(&mut *visit);
            }
        }
    };
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

fn text_bytes(objects: &Objects, text: &Value) -> u64 {
    match text {
        Value::Text(short) | Value::Function(short) => short.as_bytes().len() as u64,
        Value::LongText(handle) | Value::LongFunction(handle) => match objects.get(*handle) {
            Some(Body::LongText(text) | Body::LongFunction(text)) => text.summary.utf8_bytes,
            _ => 0,
        },
        _ => 0,
    }
}

fn word_bytes(word: i64) -> u64 {
    u64::from(64 - word.unsigned_abs().leading_zeros()).div_ceil(8)
}

/// The heap's local cost of `value`'s page form
/// (`lash_kernel_doc::heap_values::local_cost`).
#[inline]
pub(crate) fn local_cost(objects: &Objects, facts: &[Facts], value: &Value) -> u64 {
    let summary = |handle: &ArenaHandle| {
        facts
            .get(handle.index() as usize)
            .map_or(0, |facts| facts.summary.cost)
    };
    16u64.saturating_add(match value {
        Value::Int(word) | Value::Timestamp(word) => word_bytes(*word),
        Value::BigInt(handle) | Value::BigTimestamp(handle) => match objects.get(*handle) {
            Some(Body::BigInt(int) | Body::Timestamp(int)) => int.bits().div_ceil(8),
            _ => 0,
        },
        Value::Text(_) | Value::Function(_) | Value::LongText(_) | Value::LongFunction(_) => {
            text_bytes(objects, value)
        }
        Value::Bytes(handle) => match objects.get(*handle) {
            Some(Body::Bytes(bytes)) => bytes.len,
            _ => 0,
        },
        Value::Tuple(handle) | Value::Error(handle) => summary(handle),
        Value::Handle(handle) => match objects.get(*handle) {
            Some(Body::Handle(handle)) => (handle.kind.len() + handle.id.len()) as u64,
            _ => 0,
        },
        _ => 0,
    })
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
