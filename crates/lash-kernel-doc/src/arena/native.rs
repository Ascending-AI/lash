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

/// Semantic merge positions and callback phases shared by natives and the VM.
pub mod sort;

use std::sync::Arc;

use std::borrow::Cow;

use super::{
    ArenaHandle, Body, Bytes, Chunk, Chunks, ErrorBody, LongText, Objects, Sequence, ShortText,
    Table, Value,
};
use crate::heap::{IMMUTABLE_INLINE_MAX, LogicalBytes, NormalKey};

use crate::native::{GuardExceeded, WorkCounter};

use crate::number::Integer;
use lash_kernel_value::data_limits::KernelDataLimits;

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
    /// Host storage exhaustion remains an infrastructure failure, not a guest bound.
    #[error(transparent)]
    Host(#[from] crate::heap::HeapFault),
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
                Body::LongText(text) if text.moved() => Err(NativeError::Malformed(
                    "a text whose storage moved was read",
                )),
                Body::LongText(text) => Ok(Some(TextView::Long {
                    object: *object,
                    text,
                })),
                _ => Err(NativeError::Malformed("a text handle names another body")),
            },
            _ => Ok(None),
        }
    }

    /// A function reference's admitted identity.
    #[inline]
    pub fn function(
        &self,
        value: &Value,
    ) -> Result<Option<lash_kernel_value::names::CallableId>, NativeError> {
        match value {
            Value::Function(handle) => match self.body(*handle)? {
                Body::Function(callable) => Ok(Some(**callable)),
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
                Body::Cold(_) => Err(Retry::Cold {
                    object: *object,
                    chunk: 0,
                }
                .into()),
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
                Body::Cold(_) => Err(Retry::Cold {
                    object: *object,
                    chunk: 0,
                }
                .into()),
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
            Body::Cold(_) => Err(Retry::Cold { object, chunk: 0 }.into()),
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
        if let Some(equal) = crate::scalar::immediate_equal(a, b) {
            return Ok(equal);
        }
        match (a, b) {
            (Value::Function(_), Value::Function(_)) => Ok(self.function(a)? == self.function(b)?),
            _ => self.semantics.equal(a, b),
        }
    }
}

/// Answers [`Retry::Cold`] for a proxy whose head the machine has not
/// opened: a cold placeholder of no length, which the machine gives its
/// length and its chunks before the call reads any.
#[inline]
fn proxy(object: ArenaHandle, sequence: &Sequence) -> Result<(), NativeError> {
    match sequence.chunks.get(0) {
        Some(Chunk::Cold { .. }) if sequence.len == 0 => {
            Err(Retry::Cold { object, chunk: 0 }.into())
        }
        _ => Ok(()),
    }
}

/// Answers [`Retry::Cold`] for a table with a cold chunk: a table's view
/// reads it resident whole.
#[inline]
fn warm(object: ArenaHandle, chunks: &Chunks) -> Result<(), NativeError> {
    match chunks.first_cold() {
        None => Ok(()),
        Some(chunk) => Err(Retry::Cold { object, chunk }.into()),
    }
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
        let (chunk, at) = self.sequence.locate(index);
        let Some(held) = self.sequence.chunks.get(chunk) else {
            return Err(NativeError::Malformed(
                "a sequence's chunks miss its length",
            ));
        };
        match held {
            Chunk::Resident(values) => Ok(values.get(at)),
            Chunk::Cold { .. } => Err(Retry::Cold {
                object: self.object,
                chunk: chunk as u32,
            }
            .into()),
        }
    }

    /// The values, chunk by chunk.
    pub fn chunks(self) -> impl Iterator<Item = Result<&'call [Value], NativeError>> + 'call {
        values(self.object, &self.sequence.chunks)
    }

    /// The values in `start..end`, chunk by chunk. Chunks outside the range
    /// are never read, including cold chunks before its first member.
    pub fn range(
        self,
        start: u64,
        end: u64,
    ) -> impl Iterator<Item = Result<&'call [Value], NativeError>> + 'call {
        let end = end.min(self.len());
        let start = start.min(end);
        let (mut chunk, mut at) = self.sequence.locate(start);
        let mut left = end - start;
        std::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            let answer = match self.sequence.chunks.get(chunk) {
                Some(Chunk::Resident(values)) => {
                    let count = (values.len() - at).min(left as usize);
                    left -= count as u64;
                    Ok(&values[at..at + count])
                }
                Some(Chunk::Cold { .. }) => {
                    left = 0;
                    Err(Retry::Cold {
                        object: self.object,
                        chunk: chunk as u32,
                    }
                    .into())
                }
                None => {
                    left = 0;
                    Err(NativeError::Malformed(
                        "a sequence's chunks miss its length",
                    ))
                }
            };
            chunk += 1;
            at = 0;
            Some(answer)
        })
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
        values(self.object, &self.table.chunks)
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
            Self::Short(short) => short.as_bytes().len() as u64,
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

/// The values of `chunks`, chunk by chunk.
fn values(
    object: ArenaHandle,
    chunks: &Chunks,
) -> impl Iterator<Item = Result<&[Value], NativeError>> {
    chunks
        .iter()
        .enumerate()
        .map(move |(at, chunk)| match chunk {
            Chunk::Resident(values) => Ok(values),
            Chunk::Cold { .. } => Err(Retry::Cold {
                object,
                chunk: at as u32,
            }
            .into()),
        })
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
    Function(lash_kernel_value::names::CallableId),
    Bytes(Arc<[u8]>),
    BigInt(Integer),
    /// A timestamp whose nanoseconds pass a word.
    Timestamp(Integer),
    Error {
        kind: Value,
        message: Value,
        data: crate::error_object::ErrorObjectValue,
    },
}

impl NewObject {
    /// The value that names the object, by the slot it takes.
    pub fn naming(&self) -> fn(ArenaHandle) -> Value {
        match self {
            Self::List(_) => Value::List,
            Self::Tuple(_) => Value::Tuple,
            Self::Map(_) => Value::Map,
            Self::Set(_) => Value::Set,
            Self::Record(_) => Value::Record,
            Self::LongText(_) => Value::LongText,
            Self::Function(_) => Value::Function,
            Self::Bytes(_) => Value::Bytes,
            Self::BigInt(_) => Value::BigInt,
            Self::Timestamp(_) => Value::BigTimestamp,
            Self::Error { .. } => Value::Error,
        }
    }
}

/// What one call allocates: objects in slots the arena reserved, installed
/// by the machine after the call returns, or discarded when the call is
/// run again. Reservations draw on the headroom the machine granted
/// (`K-BND-003`).
pub struct Nursery<'call> {
    objects: &'call Objects,
    room: LogicalBytes,
    unaccounted: bool,
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
            unaccounted: false,
            held: LogicalBytes(0),
            new: Vec::new(),
            log: None,
        }
    }

    /// A nursery without memory reservation accounting or a memory bound.
    pub fn unaccounted(objects: &'call Objects) -> Self {
        Self {
            unaccounted: true,
            ..Self::new(objects, LogicalBytes(0))
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
        if self.unaccounted {
            return Ok(());
        }
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
        if self.unaccounted {
            return Ok(());
        }
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
        let value = object.naming()(handle);
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
    pub fn function(&mut self, callable: lash_kernel_value::names::CallableId) -> Value {
        self.allocate(NewObject::Function(callable))
    }

    /// What the call has reserved: the machine holds it once the call
    /// returns.
    pub fn held(&self) -> LogicalBytes {
        self.held
    }

    /// The objects built so far, in reservation order: what the machine
    /// prices before it installs or discards them.
    pub fn built(&self) -> &[(ArenaHandle, NewObject)] {
        &self.new
    }

    /// The objects built, in reservation order, for the machine to install.
    pub fn into_new(self) -> Vec<(ArenaHandle, NewObject)> {
        self.new
    }

    /// Gives back every slot the call reserved and keeps what it built,
    /// still named by those slots: a detached batch nothing installs, whose
    /// names stand only for each other.
    pub fn detach(self) -> Vec<(ArenaHandle, NewObject)> {
        for (handle, _) in self.new.iter().rev() {
            self.objects.unreserve(*handle);
        }
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

    /// Evaluate the canonical logical obligations from input facts, separately
    /// from execution. `None` is uncertified, never a zero-cost declaration.
    /// A predictor may request cold input chunks; it must not reserve nursery
    /// room, execute the native, or infer a contract from its observed output.
    fn resource_contract(
        &self,
        _args: Args<'_>,
    ) -> Result<Option<crate::NativeResourceContract>, NativeError> {
        Ok(None)
    }

    /// A heap-independent entry over immediates, for an opcode's guarded
    /// fast path: `None` declines to the general call.
    fn prepare(&self) -> Option<PreparedNative> {
        None
    }

    /// Describes this implementation's specializations. Every claim must
    /// agree with `call`; a replacement native supplies its own description.
    /// The machine resolves this once beside the exact function identity.
    fn specialization(&self) -> NativeSpecialization {
        NativeSpecialization {
            immediate: self.prepare(),
            ..NativeSpecialization::default()
        }
    }
}

/// Native-owned facts, including arena reads that have no immediate entry.
/// These are implementation certificates under the native trust boundary,
/// rather than claims inferred from a library function's name.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSpecialization {
    /// R8 operation and argument-kind domain, certified by this exact native
    /// implementation. Resolve it against the registered native version;
    /// neither a function name nor another version authorizes an inline path.
    pub inline: Option<InlineNative>,
    pub immediate: Option<PreparedNative>,
    /// Every successful answer from `immediate` has exactly its immutable
    /// output occurrence as its logical resource obligation, with no scratch
    /// or fresh mutable entity. The claim covers only inputs accepted by that
    /// entry. It must agree with this implementation's `resource_contract`.
    /// Absence of this certificate never implies zero scratch.
    pub immediate_resources: bool,
    pub resident: Option<ResidentRead>,
    /// An allocation-free UTF-16 relation on both inline and heap texts.
    pub text_relation: Option<crate::scalar::ScalarOp>,
    /// Concatenates the two unchanged text operands.
    pub text_append: bool,
    /// Joins a list of texts with a text separator. Resident joins retain
    /// the native's formula and output work reservation; other inputs call it.
    pub text_join: bool,
    /// A semantic projection used by the helper compiler's symbolic proof.
    pub symbolic: Option<SymbolicNative>,
}

/// A native's version-bound inline semantics (`K-LIB-011`, R8).
///
/// Inside the declared domain an implementation must preserve the ordinary
/// native's answer, raise or Decline, work-counter increments, allocation
/// admission, charge and call depth. This is not a totality certificate.
/// Overflow, cold input and allocation still take the ordinary call whenever
/// the inline implementation cannot preserve those obligations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineNative {
    pub native_version: u32,
    pub operation: InlineOperation,
}

impl InlineNative {
    /// Select only the version whose implementation made the declaration.
    pub fn for_version(self, native_version: u32) -> Option<InlineOperation> {
        (self.native_version == native_version).then_some(self.operation)
    }

    /// A graph-free scalar kernel answer. `None` runs the ordinary call;
    /// it does not mean the native itself Declines. The caller still admits
    /// the output and retains the call's charge and depth obligations.
    #[inline]
    pub fn scalar_answer(self, native_version: u32, args: &[Value]) -> Option<Value> {
        self.for_version(native_version)?.answer(args)
    }

    /// Answers through the native's borrowed views, under the version that
    /// made the declaration. Cold or malformed views retain the native error.
    pub fn answer_read(
        self,
        native_version: u32,
        args: Args<'_>,
    ) -> Result<Option<Value>, NativeError> {
        match self.for_version(native_version) {
            Some(operation) => operation.answer_read(args),
            None => Ok(None),
        }
    }
}

/// The operation and its exact argument-kind domain. Unlisted kinds and
/// arities use the ordinary call. Numeric kernels require homogeneous pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineOperation {
    /// One value of any kind. Reads its tag only and returns its inline name.
    Kind,
    /// Shallow identity: inline values, differing kinds, and mutable-object
    /// identities. Immutable bodies needing structural reads run normally.
    Same,
    /// One boolean, with no coercion.
    Not,
    /// One collection of the declared kind, read through the native views.
    /// Cold reads and lengths that cannot be answered inline run normally.
    Length(crate::plan::Collection),
    /// One table of the declared kind and an inline key, read through the
    /// native views: whether the table holds the key. Other keys, record
    /// fields and cold reads run normally.
    Membership(crate::plan::Table),
    Scalar {
        operation: crate::scalar::ScalarOp,
        domain: ScalarDomain,
    },
    IntegerText(IntegerTextDomain),
    /// Two texts, inline or heap-backed. The result is freshly allocated
    /// after admission unless it fits an inline text. No mutation is allowed.
    TextConcat,
}

/// Each kind's name as an inline text, in [`crate::ValueKind::NAMES`]
/// order.
const KIND_TEXTS: [Option<ShortText>; crate::ValueKind::NAMES.len()] = {
    let mut texts = [None; crate::ValueKind::NAMES.len()];
    let mut at = 0;
    while at < texts.len() {
        texts[at] = ShortText::new(crate::ValueKind::NAMES[at].1);
        at += 1;
    }
    texts
};

impl InlineOperation {
    /// Kind and arity eligibility only. This does not admit resources or
    /// authorize an answer when the native would raise or Decline.
    #[inline]
    pub fn accepts(self, args: &[Value]) -> bool {
        match (self, args) {
            (Self::Kind, [_]) => true,
            (Self::Same, [left, right]) => shallow_same(left, right).is_some(),
            (Self::Not, [Value::Bool(_)]) => true,
            (Self::Length(kind), [value]) => matches!(
                (kind, value),
                (crate::plan::Collection::List, Value::List(_))
                    | (crate::plan::Collection::Tuple, Value::Tuple(_))
                    | (crate::plan::Collection::Map, Value::Map(_))
                    | (crate::plan::Collection::Set, Value::Set(_))
                    | (crate::plan::Collection::Record, Value::Record(_))
            ),
            (Self::Membership(table), [target, key]) => {
                matches!(
                    (table, target),
                    (crate::plan::Table::Map, Value::Map(_))
                        | (crate::plan::Table::Set, Value::Set(_))
                ) && inline_key(key)
            }
            (Self::Scalar { domain, .. }, [Value::Int(_), Value::Int(_)]) => {
                matches!(domain, ScalarDomain::Words | ScalarDomain::WordsOrFloats)
            }
            (Self::Scalar { domain, .. }, [Value::Float(_), Value::Float(_)]) => {
                matches!(domain, ScalarDomain::Floats | ScalarDomain::WordsOrFloats)
            }
            (Self::IntegerText(IntegerTextDomain::Decimal), [Value::Int(_)]) => true,
            (
                Self::IntegerText(IntegerTextDomain::RadixArgument),
                [Value::Int(_), Value::Int(radix)],
            ) => (2..=36).contains(radix),
            (Self::TextConcat, [left, right]) => {
                matches!(left, Value::Text(_) | Value::LongText(_))
                    && matches!(right, Value::Text(_) | Value::LongText(_))
            }
            _ => false,
        }
    }

    /// The allocation-free answer within this operation's immediate domain.
    /// `None` retains the ordinary call, including any allocation admission.
    #[inline]
    pub fn answer(self, args: &[Value]) -> Option<Value> {
        match (self, args) {
            (Self::Kind, [value]) => KIND_TEXTS[value.kind() as usize].map(Value::Text),
            (Self::Same, [left, right]) => shallow_same(left, right).map(Value::Bool),
            (Self::Not, [Value::Bool(value)]) => Some(Value::Bool(!value)),
            (Self::Scalar { operation, .. }, [Value::Int(a), Value::Int(b)])
                if self.accepts(args) =>
            {
                crate::scalar::ints(operation, *a, *b)
            }
            (Self::Scalar { operation, .. }, [Value::Float(a), Value::Float(b)])
                if self.accepts(args) =>
            {
                crate::scalar::floats(operation, *a, *b)
            }
            _ => None,
        }
    }

    /// Allocation-free answers using the same zero-copy views as the native.
    pub fn answer_read(self, args: Args<'_>) -> Result<Option<Value>, NativeError> {
        if !self.accepts(args.values()) {
            return Ok(None);
        }
        if let Self::Length(kind) = self {
            let [value] = args.values() else {
                return Ok(None);
            };
            let length = match kind {
                crate::plan::Collection::List | crate::plan::Collection::Tuple => {
                    args.sequence(value)?.map(|view| view.len())
                }
                crate::plan::Collection::Map
                | crate::plan::Collection::Set
                | crate::plan::Collection::Record => args.table(value)?.map(|view| view.len()),
            };
            return Ok(length
                .and_then(|length| usize::try_from(length).ok())
                .and_then(|length| i64::try_from(length).ok())
                .map(Value::Int));
        }
        if let Self::Membership(_) = self {
            let [target, key] = args.values() else {
                return Ok(None);
            };
            return match args.table(target)? {
                Some(table) => Ok(Some(Value::Bool(args.lookup(table, key)?.is_some()))),
                None => Ok(None),
            };
        }
        Ok(self.answer(args.values()))
    }
}

/// A key legal by its kind alone, with no normal form to build: the keys
/// a table's membership answers inline.
fn inline_key(key: &Value) -> bool {
    matches!(
        key,
        Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Timestamp(_)
            | Value::Function(_)
            | Value::TaskRef(_)
    )
}

fn shallow_same(left: &Value, right: &Value) -> Option<bool> {
    if left.kind() != right.kind() {
        return Some(false);
    }
    match (left, right) {
        (Value::Null, Value::Null)
        | (Value::Absent, Value::Absent)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Int(_), Value::Int(_))
        | (Value::Float(_), Value::Float(_))
        | (Value::Text(_), Value::Text(_))
        | (Value::Timestamp(_), Value::Timestamp(_))
        | (Value::Task(_), Value::Task(_))
        | (Value::TaskRef(_), Value::TaskRef(_))
        | (Value::ObjectRef(_), Value::ObjectRef(_))
        | (Value::List(_), Value::List(_))
        | (Value::Map(_), Value::Map(_))
        | (Value::Set(_), Value::Set(_))
        | (Value::Record(_), Value::Record(_))
        | (Value::Closure(_), Value::Closure(_)) => Some(left.same(right)),
        _ => None,
    }
}

/// Homogeneous immediate pairs. Big integers and mixed pairs run normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarDomain {
    Words,
    Floats,
    WordsOrFloats,
}

/// Integer formatting's word-only domains. Longer answers still need the
/// native's allocation admission; an immediate entry may answer only the
/// short outputs it certifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegerTextDomain {
    Decimal,
    RadixArgument,
}

/// A read that neither allocates nor mutates its resident operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentRead {
    Length(crate::plan::Collection),
    Item(crate::ValueKind),
    Membership(crate::plan::Table),
}

/// Narrow native semantics used to reduce a source path under explicit
/// operand facts. Other operations keep their prepared or general entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolicNative {
    Not,
    Kind,
    Same,
    ToFloat,
    Floor,
    /// Mathematical numeric semantics on both word integers and floats,
    /// including mixed pairs. No coercion or callback is involved.
    Numeric(crate::scalar::ScalarOp),
    /// On two floats, a whole nonnegative key below the second operand
    /// returns that key, with negative zero normalized to positive zero.
    NumberIndex,
}

type ChargedEntry = fn(&[Value]) -> Option<PreparedAnswer>;

/// A native's certified entry that reads no arena object.
#[derive(Clone, Copy, Debug)]
pub struct PreparedNative {
    /// The value-only guard, with no charge measurements or metadata.
    pub entry: fn(&[Value]) -> Option<Value>,
    /// The same guard with certified scalar sizes, used only when the call
    /// is charged. It must answer and decline exactly as `entry` does.
    pub charged_entry: Option<ChargedEntry>,
    pub operation: &'static str,
    /// The kernels of [`crate::scalar`] that are the entry's own paths on
    /// two floats and on two word integers.
    pub pair: Option<crate::scalar::Pair>,
}

/// A prepared answer and the scalar sizes established by its guard.
#[derive(Clone, Copy, Debug)]
pub struct PreparedAnswer {
    pub value: Value,
    /// Present only for at most three immediate scalar arguments and an
    /// immediate scalar result. Bits 0..2 describe arguments, bit 3 the
    /// result: a set bit means a nonzero word integer (size 2); every
    /// other certified value has size 1. Deep size equals size and nested
    /// size is zero. A guard that cannot certify this leaves it absent.
    pub scalar_sizes: Option<u8>,
}

impl From<Value> for PreparedAnswer {
    fn from(value: Value) -> Self {
        Self {
            value,
            scalar_sizes: None,
        }
    }
}

impl PreparedNative {
    /// Selects charge metadata only for a charged call (K-CHG-007).
    /// An uncharged helper body runs the value-only guard.
    #[inline]
    pub fn answer(&self, args: &[Value], charging: bool) -> Option<PreparedAnswer> {
        if charging && let Some(entry) = self.charged_entry {
            entry(args)
        } else {
            (self.entry)(args).map(Into::into)
        }
    }
}
