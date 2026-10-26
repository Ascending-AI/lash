//! The persistent semantic heap's page vocabulary (FIG-6187, seams S1 and
//! S2 of `docs/design/persistent-heap-seams.md`; seam C of
//! `docs/design/kernel-end-state.md`).
//!
//! Running state lives in the arena (`crate::arena`); pages are its
//! projection. [`PageValue`] is a value as a page, a saved tree row or a
//! census root holds it: immutable data in place or by descriptor
//! ([`TextRef`], [`IntRef`], [`BytesRef`], [`TupleRef`], [`ErrorRef`]), or
//! the durable identity of a heap entity. Execution and natives never see
//! it: the machine projects an arena value to its page form where it saves
//! one, and admits a page value into the arena where it reads one.
//!
//! A mutable object is an [`EntityId`]: an opaque identity, never a content
//! hash. Immutable stored pages are [`PageId`]s: content hashes, never
//! identities. The two are never interchangeable. A guest cycle runs through
//! entity identities held as data inside pages, so the page graph stays a
//! DAG.
//!
//! Every read that can reach storage is fallible with [`HeapFault`], an
//! infrastructure failure. It is never a guest value: a missing or corrupt
//! page is not `None`, zero length, `false` membership or a `type_error`.

use std::ops::{ControlFlow, Range};
use std::sync::Arc;

use crate::number::{Float, Integer};

use crate::value::{Handle, TaskId, Timestamp};

/// An identity family: the namespace of one allocation range. Family
/// [`FamilyId::PRIMARY`] is a heap's ordinary allocation; every lazily
/// instantiated frozen value (an effect result, a frozen capture's
/// instance) takes the next family (`HeapRoot::next_family`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FamilyId(pub u64);

impl FamilyId {
    /// The family ordinary allocation draws from.
    pub const PRIMARY: Self = Self(0);
}

/// A mutable object's identity: its family and its ordinal within it. Two
/// values alias exactly when they hold equal entity identities; equal but
/// distinct objects never share one. Ordinals are never reused within a
/// family, and a swept ordinal is never resurrected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId {
    pub family: FamilyId,
    pub ordinal: u64,
}

/// The identity of one immutable page: the digest of its complete generic
/// envelope under the blob hash domain, the same 32 bytes as the store's
/// node identity. The kernel treats it as opaque: the embedder computes and
/// checks it ([`PageRef`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub [u8; 32]);

/// The size of a [`PageGrant`].
pub const PAGE_GRANT_BYTES: usize = 48;

/// An embedder's opaque transport authority to read one page: on a worker,
/// the parent's capability (authority, epoch and MAC) for that digest. The
/// heap stores it beside the page's identity and hands it back unread. It
/// is never saved: every persisted page link is a bare [`PageId`], and an
/// activation derives fresh grants from its leased root.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageGrant(pub [u8; PAGE_GRANT_BYTES]);

impl PageGrant {
    /// The grant of an embedder that authorizes nothing per page: an
    /// in-process store with no separate parent.
    pub const NONE: Self = Self([0; PAGE_GRANT_BYTES]);
}

impl std::fmt::Debug for PageGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PageGrant(..)")
    }
}

/// A page the heap may read: its identity and the grant that reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageRef {
    pub id: PageId,
    pub grant: PageGrant,
}

/// Bytes as the kernel's deterministic memory metric counts them
/// (`K-BND-003`): logical occurrence pricing, never resident, physical or
/// encoded bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalBytes(pub u64);

/// An immutable payload of at most this many bytes is held inline in its
/// descriptor; a longer one is a canonical page tree (`D-IMM`). Equal to
/// the graph format's inline token bound.
pub const IMMUTABLE_INLINE_MAX: u64 = 256;

/// The most payload bytes one immutable payload leaf holds (start value,
/// tuned by measurement under `D-CAP`). Text leaves end on scalar
/// boundaries.
pub const PAYLOAD_LEAF_BYTES: u64 = 4_096;

/// Where an immutable payload's bytes are. Both variants are one shared
/// pointer: a cold payload's 80-byte [`PageRef`] is held once behind its
/// `Arc` and shared by every copy of the value, never inlined in each
/// [`PageValue`] (`docs/design/vm-value-layout.md`).
#[derive(Debug)]
pub enum Payload<T: ?Sized> {
    /// At most [`IMMUTABLE_INLINE_MAX`] bytes, or a value built in this
    /// activation and not yet placed in pages.
    Resident(Arc<T>),
    /// A sealed payload tree. Reading it can fault. Build one with
    /// [`Payload::paged`].
    Paged(Arc<PageRef>),
}

impl<T: ?Sized> Payload<T> {
    /// The payload of the sealed tree at `page`.
    pub fn paged(page: PageRef) -> Self {
        Self::Paged(Arc::new(page))
    }
}

impl<T: ?Sized> Clone for Payload<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Resident(payload) => Self::Resident(Arc::clone(payload)),
            Self::Paged(page) => Self::Paged(Arc::clone(page)),
        }
    }
}

/// The authenticated lengths of a text, in its descriptor: a length read
/// touches no payload page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextSummary {
    pub utf8_bytes: u64,
    pub chars: u64,
    pub utf16_units: u64,
}

/// A lazily computed runtime normal form. Decoding a descriptor starts empty.
#[derive(Clone, Debug, Default)]
pub struct NormalKeyCache(std::sync::Arc<NormalForms>);
#[derive(Debug, Default)]
struct NormalForms {
    value: std::sync::OnceLock<Option<NormalKey>>,
    function: std::sync::OnceLock<Option<NormalKey>>,
}
impl NormalKeyCache {
    pub fn get(&self) -> Option<&Option<NormalKey>> {
        self.0.value.get()
    }
    pub fn set(&self, key: Option<NormalKey>) {
        let _ = self.0.value.set(key);
    }
    pub fn get_function(&self) -> Option<&Option<NormalKey>> {
        self.0.function.get()
    }
    pub fn set_function(&self, key: Option<NormalKey>) {
        let _ = self.0.function.set(key);
    }
}

/// An immutable text.
#[derive(Clone, Debug)]
pub struct TextRef {
    /// Runtime normal form, shared by descriptor clones and omitted from tokens.
    pub normal: NormalKeyCache,
    pub summary: TextSummary,
    pub body: Payload<str>,
}

/// Immutable bytes.
#[derive(Clone, Debug)]
pub struct BytesRef {
    pub normal: NormalKeyCache,
    pub len: u64,
    pub body: Payload<[u8]>,
}

/// The authenticated facts of an integer, in its descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IntSummary {
    /// How many bits the magnitude takes; zero takes none.
    pub bits: u64,
    pub negative: bool,
}

/// An integer: inline up to [`IMMUTABLE_INLINE_MAX`] bytes of magnitude,
/// otherwise its digits are a payload tree, shared like any paged
/// [`Payload`].
#[derive(Clone, Debug)]
pub enum IntRef {
    Inline(Integer),
    Paged {
        normal: NormalKeyCache,
        summary: IntSummary,
        digits: Arc<PageRef>,
    },
}

macro_rules! word_integer {
    ($($ty:ty),*) => { $(impl From<$ty> for IntRef { fn from(value: $ty) -> Self { Self::Inline(match i64::try_from(value) { Ok(word)=>Integer::from(word), Err(_)=>Integer::new(num_bigint::BigInt::from(value)) }) } })* };
}
word_integer!(i32, i64, u32, u64);

/// What a tuple's descriptor knows without reading its elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TupleSummary {
    /// Exact task-valued edges at immutable nesting depth.
    pub task_edges: u64,
    pub len: u64,
    /// The logical cost of the elements where the tuple is held.
    pub cost: LogicalBytes,
    /// Whether an element, at any depth, names an entity or a task: a
    /// tracer and a data classifier skip a tuple without edges.
    pub has_edges: bool,
    /// The deepest tuple and error nesting inside (`K-VAL-034`).
    pub depth: u32,
}

/// An immutable tuple.
#[derive(Clone, Debug)]
pub struct TupleRef {
    pub normal: NormalKeyCache,
    /// Namespace for untouched frozen tuple pages.
    pub family: Option<FamilyId>,
    pub summary: TupleSummary,
    pub body: Payload<[PageValue]>,
}

/// An immutable error value: kind, message and data, each paged like any
/// other immutable value.
#[derive(Clone, Debug)]
pub struct ErrorRef {
    pub family: Option<FamilyId>,
    pub task_edges: u64,
    pub kind: TextRef,
    pub message: TextRef,
    pub data: Payload<PageValue>,
    /// Authenticated tuple/error nesting depth.
    pub depth: u32,
    pub cost: LogicalBytes,
    pub has_edges: bool,
}

/// What an identity key names (`K-KEY-004`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Identity {
    Object(EntityId),
    Task(TaskId),
}

/// A value in its page form: what a page, a saved tree row or a census root
/// holds (`docs/design/kernel-end-state.md` §C, the projection column).
/// Immutable data is in place or by descriptor; a mutable object is its
/// durable entity identity. It is the arena value's projection, never a
/// running value.
///
/// It is 56 bytes: every variant is a scalar, an identity, or a descriptor
/// whose cold parts are shared pointers.
///
/// It has no Rust equality. Guest equality and key normalization over page
/// values are the paged heap's: they can read pages and are fallible.
#[derive(Clone, Debug)]
pub enum PageValue {
    Null,
    Absent,
    Bool(bool),
    Int(IntRef),
    Float(Float),
    Text(TextRef),
    Bytes(BytesRef),
    Timestamp(Timestamp),
    Tuple(Arc<TupleRef>),
    List(EntityId),
    Map(EntityId),
    Set(EntityId),
    Record(EntityId),
    Closure(EntityId),
    Error(Arc<ErrorRef>),
    Task(TaskId),
    Function(Box<lash_kernel_value::names::CallableId>),
    Handle(Arc<Handle>),
    Ref(Identity),
}

// Saved rows and census roots move whole page values.
const _: () = assert!(std::mem::size_of::<PageValue>() == 56);

impl PageValue {
    pub fn kind(&self) -> crate::ValueKind {
        use crate::ValueKind;
        match self {
            Self::Null => ValueKind::Null,
            Self::Absent => ValueKind::Absent,
            Self::Bool(_) => ValueKind::Bool,
            Self::Int(_) => ValueKind::Int,
            Self::Float(_) => ValueKind::Float,
            Self::Text(_) => ValueKind::Text,
            Self::Bytes(_) => ValueKind::Bytes,
            Self::Timestamp(_) => ValueKind::Timestamp,
            Self::Tuple(_) => ValueKind::Tuple,
            Self::List(_) => ValueKind::List,
            Self::Map(_) => ValueKind::Map,
            Self::Set(_) => ValueKind::Set,
            Self::Record(_) => ValueKind::Record,
            Self::Closure(_) => ValueKind::Closure,
            Self::Error(_) => ValueKind::Error,
            Self::Task(_) => ValueKind::Task,
            Self::Function(_) => ValueKind::Function,
            Self::Handle(_) => ValueKind::Handle,
            Self::Ref(_) => ValueKind::Ref,
        }
    }
    pub fn object(&self) -> Option<EntityId> {
        match self {
            Self::List(id)
            | Self::Map(id)
            | Self::Set(id)
            | Self::Record(id)
            | Self::Closure(id) => Some(*id),
            _ => None,
        }
    }
    /// A text built in this activation. Publication canonicalizes a long
    /// resident payload into pages before sealing any containing record.
    pub fn text(text: impl Into<Arc<str>>) -> Self {
        let text = text.into();
        Self::Text(TextRef {
            normal: Default::default(),
            summary: crate::heap_values::text_summary(&text),
            body: Payload::Resident(text),
        })
    }
}
impl ErrorRef {
    pub fn new(kind: &str, message: &str) -> Self {
        let text = |s: &str| TextRef {
            normal: Default::default(),
            summary: crate::heap_values::text_summary(s),
            body: Payload::Resident(Arc::from(s)),
        };
        Self {
            family: None,
            task_edges: 0,
            kind: text(kind),
            message: text(message),
            data: Payload::Resident(Arc::new(PageValue::Null)),
            depth: 1,
            cost: LogicalBytes((kind.len() + message.len()) as u64 + 16),
            has_edges: false,
        }
    }
}
/// A record field name: a compiled constant or a runtime text.
#[derive(Clone, Copy, Debug)]
pub enum FieldName<'a> {
    Static(&'a str),
    Text(&'a TextRef),
}
/// A legal key in its normal form (`K-KEY-002`): two keys are the same key
/// exactly when their normal forms are equal. A normal form of at most
/// [`NormalKey::INLINE_MAX`] token bytes is held whole; a longer one is the
/// domain-separated SHA-256 of its full normalized tokens (`D-KEY`). Its
/// order is the lookup index's internal order and is never guest-visible.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NormalKey {
    Inline(Vec<u8>),
    Digest([u8; 32]),
}

impl NormalKey {
    /// The longest normal form held whole.
    pub const INLINE_MAX: usize = 256;
}

/// One element of an object, as the paged heap's visit lends it: borrowed
/// from a page pinned for the callback only.
#[derive(Clone, Copy, Debug)]
pub enum Element<'a> {
    Item(&'a PageValue),
    Entry {
        key: &'a PageValue,
        value: &'a PageValue,
    },
    Field {
        name: &'a TextRef,
        value: &'a PageValue,
    },
}

/// One chunk of an immutable payload, as a chunk visit lends it.
#[derive(Clone, Copy, Debug)]
pub enum Chunk<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

/// An infrastructure failure of heap access: never a guest value, never
/// catchable, never `Bound::Memory`. The run stops; the last committed root
/// stands.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeapFault {
    /// A page the heap needs is not available under its grant.
    #[error("page {page:?} is missing")]
    Missing { page: PageId },
    /// A page's bytes do not hash to its identity, or its body is not a
    /// canonical page of its kind.
    #[error("page {page:?} is corrupt: {reason}")]
    Corrupt { page: PageId, reason: String },
    /// The embedder refused the grant: forged, foreign, or of an old epoch.
    #[error("page {page:?} is not authorized")]
    Unauthorized { page: PageId },
    /// The activation's root or stage lease expired or was fenced.
    #[error("the activation's lease is no longer valid")]
    LeaseLost,
    /// A host resource limit: resident pages, pinned pages, staging bytes,
    /// a read batch or the activation's host-work deadline. Not the guest's
    /// memory bound.
    #[error("host resource {resource:?} exhausted")]
    Exhausted { resource: HostResource },
    /// The embedder's transport failed.
    #[error("page transport failed: {0}")]
    Transport(String),
    /// The heap's own state or an admitted certificate is inconsistent.
    #[error("the heap is malformed: {0}")]
    Malformed(String),
}

/// The host resources a heap draws on beside guest memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HostResource {
    /// Quiescent census bitmap, work-list and root scratch budget.
    TraceBudget,
    CleanResidency,
    DirtyResidency,
    PinnedPages,
    StagingBytes,
    ReadBatch,
    HostWork,
}

/// What the paged heap answers about page values without changing
/// anything: the read face projection, materialization and snapshot
/// gathering use (seam D). Execution and natives read the arena instead.
///
/// A read of an identity of another kind than the method asks for is the
/// valid semantic answer (empty, `None`, `false`); only storage failure is
/// a [`HeapFault`].
pub trait PageReader {
    fn normal_key(&self, value: &PageValue) -> Result<Option<NormalKey>, HeapFault>;
    fn equal(&self, a: &PageValue, b: &PageValue) -> Result<bool, HeapFault>;
    fn data_depth_limit(&self) -> usize;
    fn value_depth_limit(&self) -> usize;
    fn len(&self, object: EntityId) -> Result<u64, HeapFault>;
    fn list_get(&self, list: EntityId, index: u64) -> Result<Option<PageValue>, HeapFault>;
    fn map_get(&self, map: EntityId, key: &PageValue) -> Result<Option<PageValue>, HeapFault>;
    fn set_contains(&self, set: EntityId, member: &PageValue) -> Result<bool, HeapFault>;
    fn record_get(
        &self,
        record: EntityId,
        field: FieldName<'_>,
    ) -> Result<Option<PageValue>, HeapFault>;
    /// Visits a list's elements by index, or a map's, set's or record's
    /// contents in insertion order, until the visitor breaks. Each page is
    /// read once per visit; a fault ends the visit with nothing repeated.
    fn visit(
        &self,
        object: EntityId,
        visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault>;
    fn error_data(&self, error: &ErrorRef) -> Result<PageValue, HeapFault>;
    fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<PageValue>, HeapFault>;
    fn tuple_visit(
        &self,
        tuple: &TupleRef,
        visitor: &mut dyn FnMut(&PageValue) -> ControlFlow<()>,
    ) -> Result<(), HeapFault>;
    /// Visits the chunks of `range` in order, until the visitor breaks.
    /// Text ranges cut on scalar boundaries. Bytes and paged integers yield
    /// `Chunk::Bytes`; integer ranges address canonical little-endian
    /// magnitude digits, with the sign retained in the integer descriptor.
    fn payload_visit(
        &self,
        value: &PageValue,
        range: Range<u64>,
        visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault>;
    /// The byte offset of character `index` of a text (`chars` counting).
    fn text_offset(&self, text: &TextRef, index: u64) -> Result<u64, HeapFault>;
}
