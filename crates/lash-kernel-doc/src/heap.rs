//! The persistent semantic heap's value vocabulary and native contract
//! (FIG-6187, seams S1 and S2 of `docs/design/persistent-heap-seams.md`).
//!
//! These are the frozen target declarations of the heap cutover. Nothing
//! uses them yet: the cutover moves [`Value`], [`Object`], [`Identity`],
//! [`Element`], [`NativeHeap`], [`NativeCall`], [`NativeError`] and
//! [`NativeFunction`] to the crate root in place of the flat-heap ones in
//! `value.rs` and `native.rs`, which it deletes (layer hp-heap, FIG-6188).
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

use crate::name::Name;
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

/// Where an immutable payload's bytes are.
#[derive(Debug)]
pub enum Payload<T: ?Sized> {
    /// At most [`IMMUTABLE_INLINE_MAX`] bytes, or a value built in this
    /// activation and not yet placed in pages.
    Resident(Arc<T>),
    /// A sealed payload tree. Reading it can fault.
    Paged(PageRef),
}

impl<T: ?Sized> Clone for Payload<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Resident(payload) => Self::Resident(Arc::clone(payload)),
            Self::Paged(page) => Self::Paged(*page),
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

/// An immutable text.
#[derive(Clone, Debug)]
pub struct TextRef {
    pub summary: TextSummary,
    pub body: Payload<str>,
}

/// Immutable bytes.
#[derive(Clone, Debug)]
pub struct BytesRef {
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
/// otherwise its digits are a payload tree.
#[derive(Clone, Debug)]
pub enum IntRef {
    Inline(Integer),
    Paged {
        summary: IntSummary,
        digits: PageRef,
    },
}

/// What a tuple's descriptor knows without reading its elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TupleSummary {
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
    pub summary: TupleSummary,
    pub body: Payload<[Value]>,
}

/// An immutable error value: kind, message and data, each paged like any
/// other immutable value.
#[derive(Clone, Debug)]
pub struct ErrorRef {
    pub kind: TextRef,
    pub message: TextRef,
    pub data: Arc<Value>,
    pub cost: LogicalBytes,
    pub has_edges: bool,
}

/// What an identity key names (`K-KEY-004`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Identity {
    Object(EntityId),
    Task(TaskId),
}

/// A kernel value after the cutover: immutable data in place or by
/// descriptor, or the identity of a heap entity.
///
/// It has no Rust equality. Guest equality, ordering and key normalization
/// are the heap's (`SemanticHeap::equal`, `SemanticHeap::normal_key`):
/// they can read pages and are fallible.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Absent,
    Bool(bool),
    Int(IntRef),
    Float(Float),
    Text(TextRef),
    Bytes(BytesRef),
    Timestamp(Timestamp),
    Tuple(TupleRef),
    List(EntityId),
    Map(EntityId),
    Set(EntityId),
    Record(EntityId),
    Closure(EntityId),
    Error(Arc<ErrorRef>),
    Task(TaskId),
    Function(Name),
    Handle(Arc<Handle>),
    Ref(Identity),
}

/// What a native allocates: a fresh list, map, set or record.
#[derive(Clone, Debug)]
pub enum Object {
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    Record(Vec<(TextRef, Value)>),
}

/// One element of an object, as [`NativeHeap::visit`] lends it: borrowed
/// from a page pinned for the callback only.
#[derive(Clone, Copy, Debug)]
pub enum Element<'a> {
    Item(&'a Value),
    Entry { key: &'a Value, value: &'a Value },
    Field { name: &'a TextRef, value: &'a Value },
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
    CleanResidency,
    DirtyResidency,
    PinnedPages,
    StagingBytes,
    ReadBatch,
    HostWork,
}

/// How a native call ends without a value, after the cutover.
#[derive(Clone, Debug, thiserror::Error)]
pub enum NativeError {
    /// The function raises this error value.
    #[error("{0:?}")]
    Raised(Arc<ErrorRef>),
    #[error(transparent)]
    Guard(#[from] crate::native::GuardExceeded),
    /// The run's memory bound refused a reservation after exact refinement.
    #[error("the run's memory bound refused an allocation")]
    Memory,
    /// Heap access failed: the call ends the run as infrastructure.
    #[error(transparent)]
    Heap(#[from] HeapFault),
}

/// What a native function sees of the heap after the cutover: fallible
/// reads of what exists, range access to immutable payloads, reservation
/// and allocation. It cannot change an entity that exists (`K-LIB-007`).
///
/// A read of an identity of another kind than the method asks for is the
/// valid semantic answer it is today (empty, `None`, `false`); only storage
/// failure is a [`HeapFault`].
///
/// Implemented by the paged heap (layer hp-heap, FIG-6188).
pub trait NativeHeap {
    fn data_depth_limit(&self) -> usize;
    fn value_depth_limit(&self) -> usize;
    fn len(&self, object: EntityId) -> Result<u64, HeapFault>;
    fn list_get(&self, list: EntityId, index: u64) -> Result<Option<Value>, HeapFault>;
    fn map_get(&self, map: EntityId, key: &Value) -> Result<Option<Value>, HeapFault>;
    fn set_contains(&self, set: EntityId, member: &Value) -> Result<bool, HeapFault>;
    fn record_get(&self, record: EntityId, field: &str) -> Result<Option<Value>, HeapFault>;
    /// Visits a list's elements by index, or a map's, set's or record's
    /// contents in insertion order, until the visitor breaks. Each page is
    /// read once per visit; a fault ends the visit with nothing repeated.
    fn visit(
        &self,
        object: EntityId,
        visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault>;
    fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<Value>, HeapFault>;
    fn tuple_visit(
        &self,
        tuple: &TupleRef,
        visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>,
    ) -> Result<(), HeapFault>;
    /// Visits the chunks of `range` (bytes of a text or a bytes value) in
    /// order, until the visitor breaks. A text range cuts on scalar
    /// boundaries.
    fn payload_visit(
        &self,
        value: &Value,
        range: Range<u64>,
        visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault>;
    /// The byte offset of character `index` of a text (`chars` counting).
    fn text_offset(&self, text: &TextRef, index: u64) -> Result<u64, HeapFault>;
    /// A contiguous copy of a text, for an algorithm that needs one (a
    /// regex, a foreign codec). It reserves the copy's size first.
    fn materialize_text(&mut self, text: &TextRef) -> Result<Arc<str>, NativeError>;
    fn materialize_bytes(&mut self, bytes: &BytesRef) -> Result<Arc<[u8]>, NativeError>;
    fn materialize_int(&mut self, int: &IntRef) -> Result<Integer, NativeError>;
    /// Builds an immutable value from a contiguous payload the call
    /// reserved. A long one is placed in payload pages.
    fn text(&mut self, text: &str) -> Result<TextRef, NativeError>;
    fn bytes(&mut self, bytes: &[u8]) -> Result<BytesRef, NativeError>;
    fn int(&mut self, int: Integer) -> Result<IntRef, NativeError>;
    fn tuple(&mut self, members: Vec<Value>) -> Result<TupleRef, NativeError>;
    /// Joins immutable payloads without copying the pages they share.
    fn concat(&mut self, parts: &[Value]) -> Result<Value, NativeError>;
    fn slice(&mut self, value: &Value, range: Range<u64>) -> Result<Value, NativeError>;
    fn allocate(&mut self, object: Object) -> Result<EntityId, NativeError>;
    /// Reserves room for what the call is about to hold, as today
    /// (`K-BND-003`): `values` values and `bytes` bytes. A refusal comes only
    /// after the heap's exact refinement (`NativeError::Memory`).
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError>;
}

/// One native call after the cutover.
pub struct NativeCall<'a> {
    pub args: &'a [Value],
    pub heap: &'a mut dyn NativeHeap,
    pub counter: &'a mut crate::native::WorkCounter,
}

/// A native implementation after the cutover: the purity rules of
/// `crate::NativeFunction` hold unchanged. A [`NativeError::Heap`] is not a
/// result: a call that faulted is never resumed as one.
pub trait NativeFunction: Send + Sync {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError>;

    /// A heap-independent prepared entry, as today. It reads no page: an
    /// operand that is paged makes it return `None`.
    fn prepare(&self) -> Option<PreparedNative> {
        None
    }
}

/// A native's certified, heap-independent entry after the cutover.
#[derive(Clone, Copy, Debug)]
pub struct PreparedNative {
    pub entry: fn(&[Value]) -> Option<Value>,
    pub operation: &'static str,
}
