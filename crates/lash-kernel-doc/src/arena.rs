//! The arena's value vocabulary and native ABI
//! (`docs/design/kernel-end-state.md`, seams C, D and F): what a run's
//! values and objects are while it executes. [`Value`] and the native ABI
//! ([`native`]) are the crate root's. Pages hold their projection,
//! [`crate::heap::PageValue`].
//!
//! Three namespaces name what a run holds, and none converts to another:
//!
//! 1. [`ArenaHandle`]: an object in this process's arena, by slot index and
//!    generation. It is never persisted, hashed, ordered or compared across
//!    processes.
//! 2. [`EntityId`](crate::heap::EntityId): a mutable object's durable
//!    identity. It is assigned at the projection point that first saves the
//!    object, in canonical walk order (SAM-2), and kept from then on.
//! 3. [`PageRef`](crate::heap::PageRef): an immutable stored page, by
//!    content.
//!
//! A [`Value`] is 16 bytes: immediates inline (null, absent, bool, a word
//! integer, a float, a text or function name of at most
//! [`ShortText::MAX`] bytes, a word timestamp, a task), everything else an
//! arena handle. It is `Copy`: the machine keeps reference counts at the
//! holding locations it writes (registers, slots, object members), so no
//! value a native sees carries a release protocol.

pub mod native;

use crate::number::{Float, Integer};
use crate::value::TaskId;

use crate::heap::{PageRef, TextSummary};

/// An object in this process's arena: its slot and the slot's generation
/// when the object took it. A handle to a released slot is stale, never
/// another object.
///
/// It has no `Hash` or `Ord`: nothing a run observes, keys or saves may
/// depend on where the arena put an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaHandle {
    index: u32,
    generation: u32,
}

impl ArenaHandle {
    pub const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    pub const fn index(self) -> u32 {
        self.index
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// A text of at most [`ShortText::MAX`] UTF-8 bytes, held in the value.
/// Every text that short is a `ShortText`, never an arena object, so a
/// short text and a long one are never equal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ShortText {
    len: u8,
    bytes: [u8; ShortText::MAX],
}

impl ShortText {
    /// The longest text a value holds inline.
    pub const MAX: usize = 14;

    /// `text` inline, when it is short enough.
    pub fn new(text: &str) -> Option<Self> {
        let len = text.len();
        if len > Self::MAX {
            return None;
        }
        let mut bytes = [0; Self::MAX];
        bytes[..len].copy_from_slice(text.as_bytes());
        Some(Self {
            len: len as u8,
            bytes,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    #[expect(
        clippy::expect_used,
        reason = "ShortText::new admits only the bytes of a str"
    )]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.as_bytes()).expect("a short text is UTF-8")
    }
}

impl std::fmt::Debug for ShortText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

/// A kernel value in the end state: one of the 19 kinds of `K-VAL-001`.
/// The table in `docs/design/kernel-end-state.md` §C gives each kind's
/// representation and projection rule.
///
/// It has no Rust equality: guest equality is [`native::Args::equal`].
#[derive(Clone, Copy, Debug)]
pub enum Value {
    Null,
    Absent,
    Bool(bool),
    /// An integer that fits a word; a larger one is [`Value::BigInt`].
    Int(i64),
    Float(Float),
    /// A text of at most [`ShortText::MAX`] bytes; a longer one is
    /// [`Value::LongText`].
    Text(ShortText),
    /// Nanoseconds since the epoch, when they fit a word; otherwise
    /// [`Value::BigTimestamp`].
    Timestamp(i64),
    Task(TaskId),
    /// A ref to a task (`K-KEY-004`).
    TaskRef(TaskId),
    /// A function reference by a name of at most [`ShortText::MAX`] bytes;
    /// a longer one is [`Value::LongFunction`].
    Function(ShortText),
    /// Immutable and shared, by handle.
    BigInt(ArenaHandle),
    LongText(ArenaHandle),
    Bytes(ArenaHandle),
    BigTimestamp(ArenaHandle),
    Tuple(ArenaHandle),
    Error(ArenaHandle),
    LongFunction(ArenaHandle),
    /// A host handle (`K-HOST-004`).
    Handle(ArenaHandle),
    /// Mutable, by identity.
    List(ArenaHandle),
    Map(ArenaHandle),
    Set(ArenaHandle),
    Record(ArenaHandle),
    Closure(ArenaHandle),
    /// A strong ref to an object (`K-KEY-004`).
    ObjectRef(ArenaHandle),
}

// Every register, slot, operand and member moves a whole value.
const _: () = assert!(std::mem::size_of::<Value>() == 16);

impl Value {
    pub fn kind(&self) -> crate::ValueKind {
        use crate::ValueKind as Kind;
        match self {
            Self::Null => Kind::Null,
            Self::Absent => Kind::Absent,
            Self::Bool(_) => Kind::Bool,
            Self::Int(_) | Self::BigInt(_) => Kind::Int,
            Self::Float(_) => Kind::Float,
            Self::Text(_) | Self::LongText(_) => Kind::Text,
            Self::Bytes(_) => Kind::Bytes,
            Self::Timestamp(_) | Self::BigTimestamp(_) => Kind::Timestamp,
            Self::Tuple(_) => Kind::Tuple,
            Self::List(_) => Kind::List,
            Self::Map(_) => Kind::Map,
            Self::Set(_) => Kind::Set,
            Self::Record(_) => Kind::Record,
            Self::Closure(_) => Kind::Closure,
            Self::Error(_) => Kind::Error,
            Self::Task(_) => Kind::Task,
            Self::Function(_) | Self::LongFunction(_) => Kind::Function,
            Self::Handle(_) => Kind::Handle,
            Self::TaskRef(_) | Self::ObjectRef(_) => Kind::Ref,
        }
    }

    /// Whether `other` is this very value: the same kind and the same
    /// immediate bit for bit, or the same object. Never guest equality: a
    /// projection that reads it asks only whether a value moved.
    pub fn same(&self, other: &Value) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) | (Self::Absent, Self::Absent) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) | (Self::Timestamp(a), Self::Timestamp(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.get().to_bits() == b.get().to_bits(),
            (Self::Text(a), Self::Text(b)) | (Self::Function(a), Self::Function(b)) => {
                a.as_bytes() == b.as_bytes()
            }
            (Self::Task(a), Self::Task(b)) | (Self::TaskRef(a), Self::TaskRef(b)) => a == b,
            _ => {
                std::mem::discriminant(self) == std::mem::discriminant(other)
                    && self.handle().is_some()
                    && self.handle() == other.handle()
            }
        }
    }

    /// The arena object the value names, if it names one.
    pub fn handle(&self) -> Option<ArenaHandle> {
        match self {
            Self::BigInt(handle)
            | Self::LongText(handle)
            | Self::Bytes(handle)
            | Self::BigTimestamp(handle)
            | Self::Tuple(handle)
            | Self::Error(handle)
            | Self::LongFunction(handle)
            | Self::Handle(handle)
            | Self::List(handle)
            | Self::Map(handle)
            | Self::Set(handle)
            | Self::Record(handle)
            | Self::Closure(handle)
            | Self::ObjectRef(handle) => Some(*handle),
            Self::Null
            | Self::Absent
            | Self::Bool(_)
            | Self::Int(_)
            | Self::Float(_)
            | Self::Text(_)
            | Self::Timestamp(_)
            | Self::Task(_)
            | Self::TaskRef(_)
            | Self::Function(_) => None,
        }
    }
}

/// One chunk of an object's contents (seam D). A resident chunk owns its
/// decoded contents by value: a growable vector for a mutable collection,
/// so an append is amortized O(1), and boxed exact-size storage for an
/// immutable. A cold one is decoded by the machine when a read reaches it,
/// never during a native call: a payload chunk from the page it names, a
/// list's or a table's through its object's head in the heap, by position
/// or by insertion sequence. `len` is the values, or the payload bytes, it
/// holds; a cold table chunk does not know its count and holds 0.
#[derive(Debug)]
pub enum Chunk<T> {
    Resident(T),
    Cold { page: PageRef, len: u32 },
}

/// A list's or a tuple's values. A sequence the run built, or whose chunks
/// are all resident, is one chunk holding all its values, so an append is
/// amortized O(1) and a whole read borrows one slice. A large list opened
/// from its pages is in chunks by position: chunk `k` holds indexes
/// `k * CHUNK ..`, each cold or resident on its own, so a point read or
/// write decodes one chunk and its index path.
#[derive(Debug)]
pub struct Sequence {
    pub len: u64,
    pub chunks: Vec<Chunk<Vec<Value>>>,
}

impl Sequence {
    /// The values one chunk holds: one sequence leaf
    /// (`lash_kernel_state::paged::source::SEQUENCE_LEAF_CELLS`).
    pub const CHUNK: usize = 64;
}

/// A map's, a set's or a record's entries in insertion order, as key and
/// value pairs (a set's values are [`Value::Absent`]). A table the run
/// built, or whose chunks are all resident, is one chunk. A large table
/// opened from its pages is in chunks by insertion sequence, each holding
/// whole pairs, so chunks differ in size. The key index that finds an
/// entry, and the sequences each chunk spans, are the arena's
/// (`native::Semantics::lookup`).
#[derive(Debug)]
pub struct Table {
    pub len: u64,
    pub chunks: Vec<Chunk<Vec<Value>>>,
}

/// A long text: its lengths, and its UTF-8 bytes in chunks that end on
/// scalar boundaries. A text on payload pages starts as one cold chunk of
/// its whole payload; decoding a chunk that names an index page gives one
/// cold chunk per child, and one that names a leaf gives its text, so a
/// read decodes only the leaves it reaches and their index path.
#[derive(Debug)]
pub struct LongText {
    pub summary: TextSummary,
    pub chunks: Box<[Chunk<Box<str>>]>,
}

/// Bytes, in chunks.
#[derive(Debug)]
pub struct Bytes {
    pub len: u64,
    pub chunks: Box<[Chunk<Box<[u8]>>]>,
}

/// An error value's parts (`K-ERR-001`).
#[derive(Clone, Copy, Debug)]
pub struct ErrorBody {
    pub kind: Value,
    pub message: Value,
    pub data: Value,
}

/// What an arena slot holds, as a native call reads it. The arena keeps
/// its own per-slot facts (count, dirty chunks, durable id, base page) in
/// tables beside these bodies, so a call's reads touch only what it reads.
#[derive(Debug)]
pub enum Body {
    /// A released slot, waiting to be taken again.
    Free,
    BigInt(Integer),
    LongText(LongText),
    Bytes(Bytes),
    Timestamp(Integer),
    Tuple(Sequence),
    Error(ErrorBody),
    LongFunction(LongText),
    Handle(crate::Handle),
    List(Sequence),
    Map(Table),
    Set(Table),
    Record(Table),
    /// A closure: its code's site, shared with every closure of that code,
    /// its captured variables' cells, and the code a call last found for
    /// the site.
    Closure {
        site: std::sync::Arc<crate::Site>,
        captures: Box<[ArenaHandle]>,
        code: Option<LocalCode>,
    },
    /// A variable a closure shares (`K-CLO-001`), or a `for` iteration's
    /// fresh one (`K-CLO-002`): not a value kind, only a holder of one.
    Cell(Value),
}

/// The code a closure runs, as one prepared executable numbers its codes:
/// a process-local cache of the lookup by the closure's site. Like an
/// [`ArenaHandle`] it is never persisted, keyed or observed, and it names
/// its executable, so another executable's numbering never answers for
/// it. A closure admitted from pages starts without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalCode {
    /// The executable's process-local identity.
    pub executable: u64,
    pub code: u32,
}

/// The arena's bodies by handle: what a native call borrows for its
/// duration. Slots a call allocates into are reserved, not written, until
/// the machine installs them after the call returns.
#[derive(Debug, Default)]
pub struct Objects {
    slots: Vec<Slot>,
    /// Released slots, the next allocation's first choice.
    free: std::cell::RefCell<Vec<u32>>,
    /// Slots past the end reserved by the call running.
    reserved: std::cell::Cell<u32>,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    body: Body,
}

impl Objects {
    /// The body `handle` names, or `None` for a stale handle.
    #[inline]
    pub fn get(&self, handle: ArenaHandle) -> Option<&Body> {
        self.slots
            .get(handle.index as usize)
            .filter(|slot| slot.generation == handle.generation)
            .map(|slot| &slot.body)
    }

    /// The body `handle` names, for the machine's own edits outside a
    /// native call; `None` for a stale handle.
    #[inline]
    pub fn get_mut(&mut self, handle: ArenaHandle) -> Option<&mut Body> {
        self.slots
            .get_mut(handle.index as usize)
            .filter(|slot| slot.generation == handle.generation)
            .map(|slot| &mut slot.body)
    }

    /// Allocates `body` outside a native call: reserves a slot and installs
    /// it at once.
    pub fn insert(&mut self, body: Body) -> ArenaHandle {
        let handle = self.reserve();
        self.install(handle, body);
        handle
    }

    /// The handle of the live object in slot `index`, if one is there.
    pub fn handle_at(&self, index: u32) -> Option<ArenaHandle> {
        self.slots
            .get(index as usize)
            .filter(|slot| !matches!(slot.body, Body::Free))
            .map(|slot| ArenaHandle::new(index, slot.generation))
    }

    /// One past the highest slot index ever taken: the bound of a table
    /// kept beside the bodies by slot index.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Reserves a slot for an object a call allocates: a released one if
    /// any, else one past the end. Nothing is written until
    /// [`Self::install`].
    pub(crate) fn reserve(&self) -> ArenaHandle {
        if let Some(index) = self.free.borrow_mut().pop() {
            let generation = self.slots[index as usize].generation;
            return ArenaHandle::new(index, generation);
        }
        let index = self.slots.len() as u32 + self.reserved.get();
        self.reserved.set(self.reserved.get() + 1);
        ArenaHandle::new(index, 0)
    }

    /// Gives back a slot [`Self::reserve`] took, last reserved first.
    pub(crate) fn unreserve(&self, handle: ArenaHandle) {
        if (handle.index as usize) < self.slots.len() {
            self.free.borrow_mut().push(handle.index);
        } else {
            self.reserved.set(self.reserved.get().saturating_sub(1));
        }
    }

    /// Writes `body` to the slot reserved as `handle`, in reservation
    /// order.
    pub fn install(&mut self, handle: ArenaHandle, body: Body) {
        let index = handle.index as usize;
        if index == self.slots.len() {
            self.reserved.set(self.reserved.get().saturating_sub(1));
            self.slots.push(Slot {
                generation: handle.generation,
                body,
            });
        } else {
            self.slots[index].body = body;
        }
    }

    /// Releases the slot `handle` names: its next taker gets the next
    /// generation.
    pub fn release(&mut self, handle: ArenaHandle) {
        if let Some(slot) = self.slots.get_mut(handle.index as usize)
            && slot.generation == handle.generation
        {
            slot.generation = slot.generation.wrapping_add(1);
            slot.body = Body::Free;
            self.free.get_mut().push(handle.index);
        }
    }
}
