//! The arena: the objects a run executes on, and the facts the machine
//! keeps beside each one (seam D of `docs/design/kernel-end-state.md`).
//!
//! Bodies are [`Objects`], what a native call borrows. Beside them, by slot
//! index, each object's [`Facts`]: its durable identity when it is a
//! mutable object, whether it changed since it was last projected (one
//! dirty bit: every object is one resident chunk, swap 2.4(a)), the page
//! form it was admitted from or last projected to when it is immutable,
//! its logical cost, and a table's key index.
//!
//! The paged heap is the arena's projection. A mutable object read from a
//! page is a proxy, interned by its [`EntityId`] so the run sees one handle
//! for it, whose one chunk stays cold until a read reaches it. The machine
//! decodes it then, outside any native call, and writes every dirty object
//! back at a projection point or before an exact refinement
//! (`lash-kernel-vm`'s `machine/project.rs` owns those conversions).
//!
//! Memory is a conservative bound: [`Arena::young`] counts the logical
//! growth not yet written to the heap, which the machine adds to the
//! census's upper bound before every growth. Process memory is reclaimed by
//! [`Arena::collect`], a mark from what the machine holds plus every dirty
//! object (a dirty object is the only holder of its contents).

mod bench;
mod keys;
mod ops;
mod tables;

pub use bench::{Bench, Called, Usage};
pub use keys::{ArenaSemantics, text_key};
pub use ops::invalid_key;
pub use tables::Index;

use std::collections::HashMap;

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
    /// A mutable object's durable identity: assigned at allocation until
    /// ids move to projection (swap 2.4(c)).
    pub entity: Option<EntityId>,
    /// The object changed since it was last projected, or was never
    /// projected: the heap does not hold what the arena does.
    pub dirty: bool,
    /// An immutable's page form: the descriptor it was admitted from, or
    /// the one its first projection built. Projection reuses it.
    pub page: std::cell::OnceCell<PageValue>,
    /// An immutable composite's summary; a mutable object's logical cost
    /// in `summary.cost`, as the heap prices its head.
    pub summary: Summary,
    /// A resident map's, set's or record's key index.
    pub index: Option<Box<Index>>,
    /// A proxy whose contents the machine has not decoded from the heap:
    /// its body is a cold placeholder of its kind.
    pub cold: bool,
    mark: bool,
}

/// The page a proxy's placeholder chunk names: none of its own. In swap
/// 2.4(a) an object is one chunk, decoded whole from the heap by its
/// identity; swap 2.4(b) gives each chunk its page.
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
                site: lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, &[] as &[u32]),
                captures: Box::new([]),
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
    /// Logical growth since the heap last received the arena's objects.
    young: u64,
    /// Values held by resident bodies, roughly: what triggers a collection.
    resident: u64,
    /// Objects with a cold chunk, at most: none means nothing needs decoding.
    colds: u64,
    collect_at: u64,
}

/// What a collection's roots are visited with: each value they hold.
pub type Visit<'a> = dyn FnMut(&Value) + 'a;

/// A collection is due once resident values pass this, and then each time
/// they double.
const FIRST_COLLECTION: u64 = 1 << 16;

fn malformed(problem: &'static str) -> NativeError {
    NativeError::Malformed(problem)
}

impl Arena {
    pub fn new() -> Self {
        Self {
            collect_at: FIRST_COLLECTION,
            ..Self::default()
        }
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
    /// dirty through [`Self::touched`].
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

    /// The handle of durable object `entity`, if the run has seen it.
    #[inline]
    pub fn handle_of(&self, entity: EntityId) -> Option<ArenaHandle> {
        self.entities.get(&entity).copied()
    }

    /// The durable identity of the mutable object `handle` names.
    #[inline]
    pub fn entity_of(&self, handle: ArenaHandle) -> Result<EntityId, NativeError> {
        self.facts(handle)?
            .entity
            .ok_or_else(|| malformed("an object has no durable identity"))
    }

    /// Logical growth the heap has not received.
    pub fn young(&self) -> u64 {
        self.young
    }

    /// Counts `bytes` more logical growth.
    pub fn grow(&mut self, bytes: u64) {
        self.young = self.young.saturating_add(bytes);
    }

    /// The heap received every object: their growth is the census's now.
    pub fn flushed(&mut self) {
        self.young = 0;
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

    /// Installs `body` with `facts`.
    pub fn insert(&mut self, body: Body, facts: Facts) -> ArenaHandle {
        self.resident = self.resident.saturating_add(body_values(&body));
        if facts.cold || has_cold(&body) {
            self.colds = self.colds.saturating_add(1);
        }
        let handle = self.objects.insert(body);
        self.place(handle, facts);
        handle
    }

    fn place(&mut self, handle: ArenaHandle, facts: Facts) {
        let index = handle.index() as usize;
        if index >= self.facts.len() {
            self.facts.resize_with(index + 1, Facts::default);
        }
        if let Some(entity) = facts.entity {
            self.entities.insert(entity, handle);
        }
        self.facts[index] = facts;
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

    /// Installs a mutable object `entity` with `body`, dirty: it is fresh,
    /// or a proxy's contents the caller has just decoded.
    pub fn insert_object(&mut self, entity: EntityId, body: Body, dirty: bool) -> ArenaHandle {
        let cost = object_cost(&self.objects, &self.facts, &body);
        self.insert(
            body,
            Facts {
                entity: Some(entity),
                dirty,
                summary: Summary {
                    cost,
                    ..Summary::default()
                },
                ..Facts::default()
            },
        )
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
                ..Facts::default()
            },
        )
    }

    /// Recomputes a resident mutable object's cost after an edit, marks it
    /// dirty, and gives its growth.
    pub fn touched(&mut self, handle: ArenaHandle) -> Result<u64, NativeError> {
        let cost = object_cost(&self.objects, &self.facts, self.body(handle)?);
        let facts = self.facts_mut(handle)?;
        let before = facts.summary.cost;
        facts.summary.cost = cost;
        facts.dirty = true;
        Ok(cost.saturating_sub(before))
    }

    /// The logical cost of `value` where it is held: the heap's local
    /// cost of its page form.
    pub fn cost_of(&self, value: &Value) -> u64 {
        local_cost(&self.objects, &self.facts, value)
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
        let summary = composite(&self.objects, &self.facts, &members);
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
        let mut summary = composite(&self.objects, &self.facts, std::slice::from_ref(&data));
        summary.cost = text_bytes(&self.objects, &kind)
            .saturating_add(text_bytes(&self.objects, &message))
            .saturating_add(local_cost(&self.objects, &self.facts, &data));
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
    /// the logical cost of its mutable objects. Each mutable object takes
    /// its identity from `entity` (swap 2.4(a): at allocation).
    pub fn install(
        &mut self,
        new: Vec<(ArenaHandle, NewObject)>,
        entity: &mut dyn FnMut() -> Result<EntityId, NativeError>,
    ) -> Result<u64, NativeError> {
        let mut grown = 0u64;
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
                    let summary = composite(&self.objects, &self.facts, &members);
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
                    let mut summary =
                        composite(&self.objects, &self.facts, std::slice::from_ref(&data));
                    summary.cost = text_bytes(&self.objects, &kind)
                        .saturating_add(text_bytes(&self.objects, &message))
                        .saturating_add(local_cost(&self.objects, &self.facts, &data));
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
                    .map(|_| tables::index_of(&self.objects, &self.facts, table))
                    .transpose()?,
                _ => None,
            };
            let cost = object_cost(&self.objects, &self.facts, &body);
            grown = grown.saturating_add(cost);
            let entity = entity()?;
            self.install_one(
                handle,
                body,
                Facts {
                    entity: Some(entity),
                    dirty: true,
                    summary: Summary {
                        cost,
                        ..Summary::default()
                    },
                    index: index.map(Box::new),
                    ..Facts::default()
                },
            );
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
                tables::index_of(&self.objects, &self.facts, table)?,
            )),
            _ => None,
        };
        self.resident = self.resident.saturating_add(body_values(&body));
        if self.needs_decode(handle)? && !has_cold(&body) {
            self.colds = self.colds.saturating_sub(1);
        }
        let mutable = self.facts(handle)?.entity.is_some();
        let cost = mutable.then(|| object_cost(&self.objects, &self.facts, &body));
        self.facts_mut(handle)?.cold = false;
        *self
            .objects
            .get_mut(handle)
            .ok_or_else(|| malformed("a stale handle reached the machine"))? = body;
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
        Ok(paged && self.chill(handle)?.is_some())
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

    /// Gives `handle` back the body [`Self::chill`] set aside.
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

    /// Every dirty mutable object, in identity order: what projection
    /// writes to the heap.
    pub fn dirty_objects(&self) -> Vec<(EntityId, ArenaHandle)> {
        let mut dirty: Vec<_> = self
            .entities
            .iter()
            .filter(|(_, handle)| {
                self.facts
                    .get(handle.index() as usize)
                    .is_some_and(|facts| facts.dirty)
            })
            .map(|(entity, handle)| (*entity, *handle))
            .collect();
        dirty.sort_unstable_by_key(|(entity, _)| *entity);
        dirty
    }

    /// Marks `handle` clean: the heap holds what it does.
    pub fn clean(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        self.facts_mut(handle)?.dirty = false;
        Ok(())
    }

    /// Keeps `page` as the page form of the immutable `handle`, unless it
    /// has one.
    pub fn keep_page(&self, handle: ArenaHandle, page: PageValue) -> Result<(), NativeError> {
        let _ = self.facts(handle)?.page.set(page);
        Ok(())
    }

    /// Whether a collection is due.
    pub fn collection_due(&self) -> bool {
        self.resident >= self.collect_at
    }

    /// Releases every object neither reachable from what `roots` visits
    /// nor dirty: a clean proxy is decoded again from the heap if a page
    /// names it later. Call only where every value the machine holds is
    /// visited.
    pub fn collect(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) {
        let mut pending: Vec<ArenaHandle> = Vec::new();
        roots(&mut |value| {
            if let Some(handle) = value.handle() {
                pending.push(handle);
            }
        });
        for (index, facts) in self.facts.iter().enumerate() {
            if facts.dirty
                && let Some(entity) = facts.entity
                && let Some(handle) = self.entities.get(&entity)
                && handle.index() as usize == index
            {
                pending.push(*handle);
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
            each_member(body, &mut |value| {
                if let Some(handle) = value.handle() {
                    pending.push(handle);
                }
            });
            if let Body::Closure { captures, .. } = body {
                pending.extend(captures.iter().copied());
            }
        }
        let mut resident = 0u64;
        for index in 0..self.facts.len() {
            let facts = &mut self.facts[index];
            if std::mem::take(&mut facts.mark) {
                continue;
            }
            let Some(handle) = self.objects.handle_at(index as u32) else {
                continue;
            };
            if let Some(entity) = facts.entity.take() {
                self.entities.remove(&entity);
            }
            *facts = Facts::default();
            self.objects.release(handle);
        }
        let mut colds = 0u64;
        for index in 0..self.facts.len() {
            if let Some(handle) = self.objects.handle_at(index as u32)
                && let Some(body) = self.objects.get(handle)
            {
                resident = resident.saturating_add(body_values(body));
                if self.facts[index].cold || has_cold(body) {
                    colds += 1;
                }
            }
        }
        self.resident = resident;
        self.colds = colds;
        self.collect_at = resident.saturating_mul(2).max(FIRST_COLLECTION);
    }
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

/// The summary of an immutable composite of `members`.
fn composite(objects: &Objects, facts: &[Facts], members: &[Value]) -> Summary {
    let mut summary = Summary {
        depth: 1,
        ..Summary::default()
    };
    for member in members {
        let inner = value_summary(facts, member);
        summary.cost = summary
            .cost
            .saturating_add(local_cost(objects, facts, member));
        summary.depth = summary.depth.max(inner.depth.saturating_add(1));
        summary.task_edges = summary.task_edges.saturating_add(inner.task_edges);
        summary.has_edges |= inner.has_edges;
    }
    summary
}

/// A resident mutable object's logical cost, as the heap prices its head
/// (`ObjectHead::cost`): a minimum plus its rows, a record's field rows
/// less their name references.
fn object_cost(objects: &Objects, facts: &[Facts], body: &Body) -> u64 {
    let rows = |chunks: &[Chunk<Vec<Value>>]| -> u64 {
        chunks
            .iter()
            .filter_map(|chunk| match chunk {
                Chunk::Resident(values) => Some(values),
                Chunk::Cold { .. } => None,
            })
            .flatten()
            .map(|value| local_cost(objects, facts, value))
            .fold(0u64, u64::saturating_add)
    };
    32u64.saturating_add(match body {
        Body::List(sequence) => rows(&sequence.chunks),
        Body::Map(table) => rows(&table.chunks),
        // A set's members are rows of a key and a null.
        Body::Set(table) => rows(&table.chunks),
        Body::Record(table) => rows(&table.chunks).saturating_sub(table.len * 16),
        Body::Closure { captures, .. } => captures.len() as u64 * 8,
        Body::Cell(value) => local_cost(objects, facts, value),
        _ => 0,
    })
}
