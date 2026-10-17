//! The native ABI over the arena (seam F, decided by measurement in
//! `docs/design/kernel-end-state.md` §F): typed views over call-scoped
//! borrows.
//!
//! A native receives its arguments as `&'call [Value]` and reads what
//! they name through views that borrow the arena for the call: a list's
//! values as `&'call [Value]` chunks, a text as `&'call str` chunks, with
//! no dynamic dispatch per element and no value cloned out. What it builds
//! goes to its [`Nursery`], whose slots the arena reserves without writing;
//! the machine installs them once the call returns. Every borrow ends with
//! the call, so there is no handle release protocol, and reads (`&`) and
//! allocation (`&mut` nursery) never alias.
//!
//! A read that reaches a cold chunk does not fault inside the call: it
//! answers [`Retry::Cold`], the machine decodes that chunk (and, doubling,
//! the ones after it) and runs the call again. A sequence's or a text's
//! view faults only the chunks the call reads, so a slice of a large
//! object decodes the chunks it addresses; a table's view is resident
//! whole, since its key index answers for every key. A reservation past the
//! machine's headroom answers [`Retry::Room`]: the machine collects and
//! runs it again, or ends the run at the memory bound. Natives are pure,
//! so a call run again does what it did, and the nursery of the abandoned
//! attempt is discarded.

use std::sync::Arc;

use std::borrow::Cow;

use super::{
    ArenaHandle, Body, Bytes, Chunk, ErrorBody, LongText, Objects, Sequence, ShortText, Table,
    Value,
};
use crate::KernelDataLimits;
use crate::heap::{IMMUTABLE_INLINE_MAX, LogicalBytes, NormalKey};
use crate::native::{GuardExceeded, WorkCounter};
use crate::number::Integer;

/// What the machine must do before a call can answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// Decode chunk `chunk` of `object`, and run the call again.
    Cold { object: ArenaHandle, chunk: u32 },
    /// Grant the call a holding of `wanted` (or end the run at the memory
    /// bound), and run it again.
    Room { wanted: LogicalBytes },
}

/// How a native call ends without a value.
#[derive(Clone, Debug, thiserror::Error)]
pub enum NativeError {
    /// The function raises this error value, built in its nursery.
    #[error("the native raised {0:?}")]
    Raised(Value),
    /// The function raises an error of `kind` with `message` and null
    /// data, which the machine builds: what a call raises without
    /// allocating, and what the arena's [`Semantics`] refuses a key with.
    #[error("{kind}: {message}")]
    Refused { kind: &'static str, message: String },
    #[error(transparent)]
    Guard(#[from] GuardExceeded),
    /// The arena must do something first; the call has done nothing a run
    /// sees.
    #[error("the native needs {0:?} first")]
    Retry(Retry),
    /// The native declines this input: the definition's kernel body runs
    /// the call (an operation's partial native).
    #[error("the native declined the call")]
    Decline,
    /// A stale handle or an object of the wrong body reached the call: the
    /// arena is malformed, and the run ends as infrastructure.
    #[error("the arena is malformed: {0}")]
    Malformed(&'static str),
}

impl From<Retry> for NativeError {
    fn from(retry: Retry) -> Self {
        Self::Retry(retry)
    }
}

/// The guest semantics a call needs beyond its views, which the arena
/// implements: deep equality and keyed lookup (`K-VAL-020`, `K-KEY-002`).
/// Immediates never reach it; [`Args::equal`] answers them inline.
pub trait Semantics {
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, NativeError>;
    /// Refuses a value that is not a legal key (`invalid_key`,
    /// `too_deep`).
    fn check_key(&self, key: &Value) -> Result<(), NativeError>;
    /// The value `table` holds under `key`.
    fn lookup(&self, table: ArenaHandle, key: &Value) -> Result<Option<&Value>, NativeError>;
    /// `key`'s normal form (`K-KEY-002`), or `None` when it is no legal
    /// key.
    fn normal_key(&self, key: &Value) -> Result<Option<NormalKey>, NativeError>;
    /// The tuple and error nesting depth of `value`, itself included
    /// (`K-VAL-034`): 0 for every other kind.
    fn depth(&self, value: &Value) -> u32;
    /// The run's data limits.
    fn limits(&self) -> KernelDataLimits;
}

/// A call's arguments and the arena they name, borrowed for the call.
#[derive(Clone, Copy)]
pub struct Args<'call> {
    values: &'call [Value],
    objects: &'call Objects,
    semantics: &'call dyn Semantics,
}

/// A list's or a tuple's values.
#[derive(Clone, Copy, Debug)]
pub struct SequenceView<'call> {
    object: ArenaHandle,
    sequence: &'call Sequence,
}

/// A map's, a set's or a record's entries.
#[derive(Clone, Copy, Debug)]
pub struct TableView<'call> {
    object: ArenaHandle,
    table: &'call Table,
}

/// Bytes' chunks.
#[derive(Clone, Copy, Debug)]
pub struct BytesView<'call> {
    object: ArenaHandle,
    bytes: &'call Bytes,
}

/// A text: inline, or a long text's chunks.
#[derive(Clone, Copy, Debug)]
pub enum TextView<'call> {
    Short(&'call ShortText),
    Long {
        object: ArenaHandle,
        text: &'call LongText,
    },
}

impl<'call> Args<'call> {
    /// The machine's view of a call: its arguments, the arena's bodies and
    /// the arena's semantics.
    pub fn new(
        values: &'call [Value],
        objects: &'call Objects,
        semantics: &'call dyn Semantics,
    ) -> Self {
        Self {
            values,
            objects,
            semantics,
        }
    }

    pub fn values(&self) -> &'call [Value] {
        self.values
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Argument `index`, or `None` past the last.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&'call Value> {
        self.values.get(index)
    }

    #[inline]
    fn body(&self, handle: ArenaHandle) -> Result<&'call Body, NativeError> {
        self.objects
            .get(handle)
            .ok_or(NativeError::Malformed("a stale handle reached a call"))
    }

    /// The list or tuple `value` is.
    #[inline]
    pub fn sequence(&self, value: &Value) -> Result<Option<SequenceView<'call>>, NativeError> {
        let (Value::List(object) | Value::Tuple(object)) = *value else {
            return Ok(None);
        };
        match self.body(object)? {
            Body::List(sequence) | Body::Tuple(sequence) => {
                proxy(object, sequence)?;
                Ok(Some(SequenceView { object, sequence }))
            }
            _ => Err(NativeError::Malformed(
                "a sequence handle names another body",
            )),
        }
    }

    /// The map, set or record `value` is.
    #[inline]
    pub fn table(&self, value: &Value) -> Result<Option<TableView<'call>>, NativeError> {
        let (Value::Map(object) | Value::Set(object) | Value::Record(object)) = *value else {
            return Ok(None);
        };
        match self.body(object)? {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                warm(object, &table.chunks)?;
                Ok(Some(TableView { object, table }))
            }
            _ => Err(NativeError::Malformed("a table handle names another body")),
        }
    }

    /// The text `value` is.
    #[inline]
    pub fn text(&self, value: &'call Value) -> Result<Option<TextView<'call>>, NativeError> {
        match value {
            Value::Text(short) => Ok(Some(TextView::Short(short))),
            Value::LongText(object) => match self.body(*object)? {
                Body::LongText(text) => Ok(Some(TextView::Long {
                    object: *object,
                    text,
                })),
                _ => Err(NativeError::Malformed("a text handle names another body")),
            },
            _ => Ok(None),
        }
    }

    /// A function reference's name.
    #[inline]
    pub fn function(&self, value: &'call Value) -> Result<Option<TextView<'call>>, NativeError> {
        match value {
            Value::Function(short) => Ok(Some(TextView::Short(short))),
            Value::LongFunction(object) => match self.body(*object)? {
                Body::LongFunction(text) => Ok(Some(TextView::Long {
                    object: *object,
                    text,
                })),
                _ => Err(NativeError::Malformed(
                    "a function handle names another body",
                )),
            },
            _ => Ok(None),
        }
    }

    /// The integer `value` is.
    #[inline]
    pub fn integer(&self, value: &Value) -> Result<Option<Cow<'call, Integer>>, NativeError> {
        match value {
            Value::Int(word) => Ok(Some(Cow::Owned(Integer::from(*word)))),
            Value::BigInt(object) => match self.body(*object)? {
                Body::BigInt(int) => Ok(Some(Cow::Borrowed(int))),
                _ => Err(NativeError::Malformed(
                    "an integer handle names another body",
                )),
            },
            _ => Ok(None),
        }
    }

    /// The nanoseconds since the epoch of the timestamp `value` is.
    pub fn timestamp(&self, value: &Value) -> Result<Option<Cow<'call, Integer>>, NativeError> {
        match value {
            Value::Timestamp(word) => Ok(Some(Cow::Owned(Integer::from(*word)))),
            Value::BigTimestamp(object) => match self.body(*object)? {
                Body::Timestamp(int) => Ok(Some(Cow::Borrowed(int))),
                _ => Err(NativeError::Malformed(
                    "a timestamp handle names another body",
                )),
            },
            _ => Ok(None),
        }
    }

    /// The bytes `value` is.
    #[inline]
    pub fn bytes(&self, value: &Value) -> Result<Option<BytesView<'call>>, NativeError> {
        let Value::Bytes(object) = *value else {
            return Ok(None);
        };
        match self.body(object)? {
            Body::Bytes(bytes) => Ok(Some(BytesView { object, bytes })),
            _ => Err(NativeError::Malformed("a bytes handle names another body")),
        }
    }

    /// The parts of the error `value` is.
    pub fn error(&self, value: &Value) -> Result<Option<&'call ErrorBody>, NativeError> {
        let Value::Error(object) = *value else {
            return Ok(None);
        };
        match self.body(object)? {
            Body::Error(error) => Ok(Some(error)),
            _ => Err(NativeError::Malformed("an error handle names another body")),
        }
    }

    /// The host handle `value` is.
    pub fn host_handle(&self, value: &Value) -> Result<Option<&'call crate::Handle>, NativeError> {
        let Value::Handle(object) = *value else {
            return Ok(None);
        };
        match self.body(object)? {
            Body::Handle(handle) => Ok(Some(handle)),
            _ => Err(NativeError::Malformed("a host handle names another body")),
        }
    }

    /// `key`'s normal form (`K-KEY-002`), or `None` when it is no legal
    /// key.
    pub fn normal_key(&self, key: &Value) -> Result<Option<NormalKey>, NativeError> {
        self.semantics.normal_key(key)
    }

    /// The tuple and error nesting depth of `value`, itself included.
    pub fn depth(&self, value: &Value) -> u32 {
        self.semantics.depth(value)
    }

    /// The run's data limits.
    pub fn limits(&self) -> KernelDataLimits {
        self.semantics.limits()
    }

    /// The value `table` holds under `key`.
    pub fn lookup(
        &self,
        table: TableView<'call>,
        key: &Value,
    ) -> Result<Option<&'call Value>, NativeError> {
        self.semantics.lookup(table.object, key)
    }

    pub fn check_key(&self, key: &Value) -> Result<(), NativeError> {
        self.semantics.check_key(key)
    }

    /// Guest equality (`K-VAL-020`): immediates inline, everything else
    /// through the arena's semantics.
    #[inline]
    pub fn equal(&self, a: &Value, b: &Value) -> Result<bool, NativeError> {
        use Value as V;
        Ok(match (a, b) {
            (V::Null, V::Null) | (V::Absent, V::Absent) => true,
            (V::Bool(a), V::Bool(b)) => a == b,
            (V::Int(a), V::Int(b)) => a == b,
            (V::Float(a), V::Float(b)) => a.get() == b.get(),
            (V::Int(i), V::Float(f)) | (V::Float(f), V::Int(i)) => word_float(*i, f.get()),
            (V::Text(a), V::Text(b)) | (V::Function(a), V::Function(b)) => a == b,
            // A short text is never equal to a long one.
            (V::Text(_), V::LongText(_))
            | (V::LongText(_), V::Text(_))
            | (V::Function(_), V::LongFunction(_))
            | (V::LongFunction(_), V::Function(_)) => false,
            (V::Timestamp(a), V::Timestamp(b)) => a == b,
            (V::Task(a), V::Task(b)) | (V::TaskRef(a), V::TaskRef(b)) => a == b,
            (V::Closure(a), V::Closure(b)) | (V::ObjectRef(a), V::ObjectRef(b)) => a == b,
            _ if a.kind() != b.kind() && !(numeric(a) && numeric(b)) => false,
            _ => return self.semantics.equal(a, b),
        })
    }
}

/// Answers [`Retry::Cold`] for a proxy whose head the machine has not
/// opened: a cold placeholder of no length, which the machine gives its
/// length and its chunks before the call reads any.
#[inline]
fn proxy(object: ArenaHandle, sequence: &Sequence) -> Result<(), NativeError> {
    match sequence.chunks.first() {
        Some(Chunk::Cold { .. }) if sequence.len == 0 => {
            Err(Retry::Cold { object, chunk: 0 }.into())
        }
        _ => Ok(()),
    }
}

/// Answers [`Retry::Cold`] for a table with a cold chunk: a table's view
/// reads it resident whole.
#[inline]
fn warm(object: ArenaHandle, chunks: &[Chunk<Vec<Value>>]) -> Result<(), NativeError> {
    match chunks
        .iter()
        .position(|chunk| matches!(chunk, Chunk::Cold { .. }))
    {
        None => Ok(()),
        Some(chunk) => Err(Retry::Cold {
            object,
            chunk: chunk as u32,
        }
        .into()),
    }
}

fn numeric(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::BigInt(_) | Value::Float(_))
}

/// Whether a word integer equals a float: only an integral one.
#[inline]
fn word_float(int: i64, float: f64) -> bool {
    // 2^63: the first float past every word integer.
    const PAST: f64 = 9_223_372_036_854_775_808.0;
    float.fract() == 0.0 && (-PAST..PAST).contains(&float) && float as i64 == int
}

impl<'call> SequenceView<'call> {
    pub fn len(&self) -> u64 {
        self.sequence.len
    }

    pub fn is_empty(&self) -> bool {
        self.sequence.len == 0
    }

    /// The value at `index`, or `None` past the end.
    #[inline]
    pub fn get(&self, index: u64) -> Result<Option<&'call Value>, NativeError> {
        if index >= self.sequence.len {
            return Ok(None);
        }
        // A sequence in one chunk holds every value there; chunk `k` of a
        // chunked one holds indexes `k * CHUNK ..`.
        let (chunk, at) = match self.sequence.chunks.len() {
            1 => (0, index),
            _ => (
                index / Sequence::CHUNK as u64,
                index % Sequence::CHUNK as u64,
            ),
        };
        let Some(held) = self.sequence.chunks.get(chunk as usize) else {
            return Err(NativeError::Malformed(
                "a sequence's chunks miss its length",
            ));
        };
        let values = resident(self.object, chunk as u32, held)?;
        Ok(values.get(at as usize))
    }

    /// The values, chunk by chunk.
    pub fn chunks(self) -> impl Iterator<Item = Result<&'call [Value], NativeError>> + 'call {
        let object = self.object;
        self.sequence
            .chunks
            .iter()
            .enumerate()
            .map(move |(at, chunk)| resident(object, at as u32, chunk))
    }
}

impl<'call> TableView<'call> {
    pub fn len(&self) -> u64 {
        self.table.len
    }

    pub fn is_empty(&self) -> bool {
        self.table.len == 0
    }

    /// The entries in insertion order, as key and value pairs, chunk by
    /// chunk.
    pub fn chunks(self) -> impl Iterator<Item = Result<&'call [Value], NativeError>> + 'call {
        let object = self.object;
        self.table
            .chunks
            .iter()
            .enumerate()
            .map(move |(at, chunk)| resident(object, at as u32, chunk))
    }
}

impl<'call> BytesView<'call> {
    pub fn len(&self) -> u64 {
        self.bytes.len
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.len == 0
    }

    /// The bytes whole, when they are in one resident piece.
    pub fn as_slice(&self) -> Option<&'call [u8]> {
        match &*self.bytes.chunks {
            [] => Some(&[]),
            [Chunk::Resident(bytes)] => Some(bytes),
            _ => None,
        }
    }

    /// The bytes, chunk by chunk.
    pub fn chunks(self) -> impl Iterator<Item = Result<&'call [u8], NativeError>> + 'call {
        let object = self.object;
        self.bytes
            .chunks
            .iter()
            .enumerate()
            .map(move |(at, chunk)| resident(object, at as u32, chunk))
    }

    /// The bytes whole, borrowed when they are one resident piece.
    pub fn whole(self) -> Result<Cow<'call, [u8]>, NativeError> {
        if let Some(bytes) = self.as_slice() {
            return Ok(Cow::Borrowed(bytes));
        }
        let mut whole = Vec::with_capacity(self.bytes.len as usize);
        for chunk in self.chunks() {
            whole.extend_from_slice(chunk?);
        }
        Ok(Cow::Owned(whole))
    }
}

impl<'call> TextView<'call> {
    /// The text whole, borrowed when it is one resident piece.
    pub fn whole(self) -> Result<Cow<'call, str>, NativeError> {
        if let Some(text) = self.as_str() {
            return Ok(Cow::Borrowed(text));
        }
        let mut whole = String::with_capacity(self.summary().utf8_bytes as usize);
        for chunk in self.chunks() {
            whole.push_str(chunk?);
        }
        Ok(Cow::Owned(whole))
    }

    /// The text's length in UTF-8 bytes.
    pub fn len(&self) -> u64 {
        match self {
            Self::Short(short) => short.as_str().len() as u64,
            Self::Long { text, .. } => text.summary.utf8_bytes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The text's UTF-8 bytes, chars and UTF-16 units.
    pub fn summary(&self) -> crate::heap::TextSummary {
        match self {
            Self::Short(short) => crate::heap_values::text_summary(short.as_str()),
            Self::Long { text, .. } => text.summary,
        }
    }

    /// The text whole, when it is in one resident piece.
    pub fn as_str(&self) -> Option<&'call str> {
        match self {
            Self::Short(short) => Some(short.as_str()),
            Self::Long { text, .. } => match &*text.chunks {
                [Chunk::Resident(text)] => Some(&**text),
                _ => None,
            },
        }
    }

    /// The text's chunks, each ending on a scalar boundary.
    pub fn chunks(self) -> impl Iterator<Item = Result<&'call str, NativeError>> + 'call {
        let (short, long) = match self {
            Self::Short(short) => (Some(short.as_str()), None),
            Self::Long { object, text } => (None, Some((object, text))),
        };
        short
            .into_iter()
            .map(Ok)
            .chain(long.into_iter().flat_map(|(object, text)| {
                text.chunks
                    .iter()
                    .enumerate()
                    .map(move |(at, chunk)| resident(object, at as u32, chunk))
            }))
    }
}

#[inline]
fn resident<T: std::ops::Deref>(
    object: ArenaHandle,
    chunk: u32,
    at: &Chunk<T>,
) -> Result<&T::Target, NativeError> {
    match at {
        Chunk::Resident(values) => Ok(values),
        Chunk::Cold { .. } => Err(Retry::Cold { object, chunk }.into()),
    }
}

impl NativeError {
    /// The call raises an error of `kind` with `message` and null data.
    pub fn refused(kind: &'static str, message: impl Into<String>) -> Self {
        Self::Refused {
            kind,
            message: message.into(),
        }
    }
}

/// What a native builds, in the shape the machine installs.
#[derive(Clone, Debug)]
pub enum NewObject {
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    Record(Vec<(Value, Value)>),
    LongText(Arc<str>),
    /// A function reference by a name past [`ShortText::MAX`] bytes.
    LongFunction(Arc<str>),
    Bytes(Arc<[u8]>),
    BigInt(Integer),
    /// A timestamp whose nanoseconds pass a word.
    Timestamp(Integer),
    Error {
        kind: Value,
        message: Value,
        data: Value,
    },
}

/// What one call allocates: objects in slots the arena reserved, installed
/// by the machine after the call returns, or discarded when the call is
/// run again. Reservations draw on the headroom the machine granted
/// (`K-BND-003`).
pub struct Nursery<'call> {
    objects: &'call Objects,
    room: LogicalBytes,
    held: LogicalBytes,
    new: Vec<(ArenaHandle, NewObject)>,
    /// Every reservation asked for, and whether it was held: kept only
    /// for a probe that certifies what a native reserves.
    log: Option<Vec<Reservation>>,
}

/// One reservation a call asked for ([`Nursery::recording`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub values: u64,
    pub bytes: u64,
    pub held: bool,
}

impl<'call> Nursery<'call> {
    /// A nursery that may hold `room` before it asks for more.
    pub fn new(objects: &'call Objects, room: LogicalBytes) -> Self {
        Self {
            objects,
            room,
            held: LogicalBytes(0),
            new: Vec::new(),
            log: None,
        }
    }

    /// A nursery that also keeps every reservation the call asks for
    /// ([`Self::reservations`]).
    pub fn recording(objects: &'call Objects, room: LogicalBytes) -> Self {
        Self {
            log: Some(Vec::new()),
            ..Self::new(objects, room)
        }
    }

    /// The reservations asked for, in order, when the nursery records.
    pub fn reservations(&self) -> &[Reservation] {
        self.log.as_deref().unwrap_or(&[])
    }

    /// Holds room for `values` values and `bytes` bytes the call is about
    /// to build, before it builds them.
    pub fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let wanted = values
            .checked_mul(std::mem::size_of::<Value>() as u64)
            .and_then(|values| values.checked_add(bytes))
            .and_then(|wanted| wanted.checked_add(self.held.0));
        let answer = match wanted {
            Some(held) if held <= self.room.0 => {
                self.held = LogicalBytes(held);
                Ok(())
            }
            _ => Err(Retry::Room {
                wanted: LogicalBytes(wanted.unwrap_or(u64::MAX)),
            }
            .into()),
        };
        if let Some(log) = &mut self.log {
            log.push(Reservation {
                values,
                bytes,
                held: answer.is_ok(),
            });
        }
        answer
    }

    /// Holds room for `values` values and `bytes` bytes the call needs
    /// without asking for it as a reservation: a copy the heap makes for
    /// the call (a whole paged payload read, a concatenation, a slice).
    /// It draws on the room as [`Self::reserve`] does and is not recorded.
    pub fn hold(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let wanted = values
            .checked_mul(std::mem::size_of::<Value>() as u64)
            .and_then(|values| values.checked_add(bytes))
            .and_then(|wanted| wanted.checked_add(self.held.0));
        match wanted {
            Some(held) if held <= self.room.0 => {
                self.held = LogicalBytes(held);
                Ok(())
            }
            _ => Err(Retry::Room {
                wanted: LogicalBytes(wanted.unwrap_or(u64::MAX)),
            }
            .into()),
        }
    }

    /// Holds what reading a payload of `bytes` whole costs: the heap copies
    /// a payload longer than [`IMMUTABLE_INLINE_MAX`] out of its pages.
    pub fn read_whole(&mut self, bytes: u64) -> Result<(), NativeError> {
        if bytes > IMMUTABLE_INLINE_MAX {
            self.hold(0, bytes)
        } else {
            Ok(())
        }
    }

    /// Builds `object` in a reserved slot, and gives the value naming it.
    pub fn allocate(&mut self, object: NewObject) -> Value {
        let handle = self.objects.reserve();
        let value = match &object {
            NewObject::List(_) => Value::List(handle),
            NewObject::Tuple(_) => Value::Tuple(handle),
            NewObject::Map(_) => Value::Map(handle),
            NewObject::Set(_) => Value::Set(handle),
            NewObject::Record(_) => Value::Record(handle),
            NewObject::LongText(_) => Value::LongText(handle),
            NewObject::LongFunction(_) => Value::LongFunction(handle),
            NewObject::Bytes(_) => Value::Bytes(handle),
            NewObject::BigInt(_) => Value::BigInt(handle),
            NewObject::Timestamp(_) => Value::BigTimestamp(handle),
            NewObject::Error { .. } => Value::Error(handle),
        };
        self.new.push((handle, object));
        value
    }

    /// A text: inline when it is short, otherwise built.
    pub fn text(&mut self, text: &str) -> Value {
        match ShortText::new(text) {
            Some(short) => Value::Text(short),
            None => self.allocate(NewObject::LongText(Arc::from(text))),
        }
    }

    /// An integer: inline when it fits a word, otherwise built.
    pub fn integer(&mut self, integer: Integer) -> Value {
        match integer.word() {
            Some(word) => Value::Int(word),
            None => self.allocate(NewObject::BigInt(integer)),
        }
    }

    /// A timestamp of `nanoseconds` since the epoch: inline when they fit
    /// a word, otherwise built.
    pub fn timestamp(&mut self, nanoseconds: Integer) -> Value {
        match nanoseconds.word() {
            Some(word) => Value::Timestamp(word),
            None => self.allocate(NewObject::Timestamp(nanoseconds)),
        }
    }

    /// Bytes, built.
    pub fn bytes(&mut self, bytes: &[u8]) -> Value {
        self.allocate(NewObject::Bytes(Arc::from(bytes)))
    }

    /// A function reference by `name`: inline when it is short, otherwise
    /// built.
    pub fn function(&mut self, name: &str) -> Value {
        match ShortText::new(name) {
            Some(short) => Value::Function(short),
            None => self.allocate(NewObject::LongFunction(Arc::from(name))),
        }
    }

    /// What the call has reserved: the machine holds it once the call
    /// returns.
    pub fn held(&self) -> LogicalBytes {
        self.held
    }

    /// The objects built, in reservation order, for the machine to install.
    pub fn into_new(self) -> Vec<(ArenaHandle, NewObject)> {
        self.new
    }

    /// Gives back every slot the call reserved: its attempt is abandoned.
    pub fn discard(self) {
        for (handle, _) in self.new.iter().rev() {
            self.objects.unreserve(*handle);
        }
    }
}

/// One native call: its arguments, its nursery and its guard counter.
pub struct NativeCall<'call, 'n> {
    pub args: Args<'call>,
    pub new: &'n mut Nursery<'call>,
    pub counter: &'n mut WorkCounter,
}

/// A native implementation over the arena. It is pure: it reads only its
/// arguments and what they name, changes no existing object (`K-LIB-007`)
/// and answers the same for the same input (`K-LIB-006`). A call
/// that answers [`NativeError::Retry`] or [`NativeError::Decline`] has done
/// nothing a run sees.
pub trait NativeFunction: Send + Sync {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError>;

    /// A heap-independent entry over immediates, for an opcode's guarded
    /// fast path: `None` declines to the general call.
    fn prepare(&self) -> Option<PreparedNative> {
        None
    }
}

/// A native's certified entry that reads no arena object.
#[derive(Clone, Copy, Debug)]
pub struct PreparedNative {
    pub entry: fn(&[Value]) -> Option<Value>,
    pub operation: &'static str,
}
