//! Guest equality and keys over the arena (`K-VAL-020` to `K-VAL-033`,
//! `K-KEY-002`): the port of the paged heap's `equality.rs` and normal
//! forms. A key's normal form is the token spelling the heap's lookup
//! index keys it by, so a key means the same in the arena and in pages.
//!
//! A read that reaches a cold chunk answers [`Retry::Cold`]: the machine
//! decodes it and asks again. These reads take values, texts and bytes
//! whole, so a chunked object answers for its first cold chunk, or for
//! chunk 0 once every chunk is resident, and the machine joins it into one
//! chunk.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::hash::BuildHasher;

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Objects, Sequence, ShortText, Table};
use lash_kernel_doc::heap::{Identity, NormalKey};
use lash_kernel_doc::heap_values::integral_float;
use lash_kernel_doc::{Integer, NativeError, Retry, Semantics, Timestamp, Value};

use lash_kernel_value::data_limits::KernelDataLimits;
use {lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenWriter};

use super::Facts;
use crate::paged::identity;

/// Bounded, process-local normalized short keys. Colliding cache slots compare
/// the complete short text; a miss replaces the slot, never aliases its key.
#[derive(Debug, Default)]
pub(super) struct KeyCache {
    slots: RefCell<Vec<Option<(ShortText, NormalKey)>>>,
    hash: foldhash::fast::FixedState,
}

impl KeyCache {
    fn with<R>(
        &self,
        objects: &(impl super::Bodies + ?Sized),
        facts: &(impl super::FactTable + ?Sized),
        value: &Value,
        read: impl FnOnce(Option<&NormalKey>) -> Result<R, NativeError>,
    ) -> Result<R, NativeError> {
        let Value::Text(short) = value else {
            return read(normal_key(objects, facts, value)?.as_ref());
        };
        const SLOTS: usize = 64;
        let at = self.hash.hash_one(short.as_bytes()) as usize % SLOTS;
        let mut slots = self.slots.borrow_mut();
        if slots.is_empty() {
            slots.resize_with(SLOTS, || None);
        }
        if !slots[at].as_ref().is_some_and(|(key, _)| key == short) {
            slots[at] = Some((*short, text_key(short.as_str())));
        }
        read(slots[at].as_ref().map(|(_, normal)| normal))
    }
}

/// A key as a resident table's index holds it (`K-KEY-002`): a short
/// text's bytes or a word's integer directly, any other key by its normal
/// form, so a lookup by a short text or an integer normalizes, allocates
/// and hashes no token bytes. Two keys are equal exactly when their normal
/// forms are: every text of at most [`ShortText::MAX`] bytes is a
/// [`Key::Text`], an integral float is the integer it equals, and every
/// other key (a longer text, a larger integer, any other kind) has its
/// normal form only. Process-local: never projected, iterated or ordered.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// A text's [`ShortText::word`], in halves.
    Text(u64, u64),
    Int(i64),
    Normal(Box<NormalKey>),
}

impl Key {
    /// The key of short text `short`.
    #[inline]
    pub fn text(short: &ShortText) -> Self {
        let word = short.word();
        Self::Text(word as u64, (word >> 64) as u64)
    }

    /// The key of `value`, whose normal form is `normal`: a short text or
    /// a word directly, any other key by `normal`.
    pub fn normalized(value: &Value, normal: NormalKey) -> Self {
        match value {
            Value::Text(short) => Self::text(short),
            Value::Int(word) => Self::Int(*word),
            Value::Float(float) => match word_of(float.get()) {
                Some(word) => Self::Int(word),
                None => Self::Normal(Box::new(normal)),
            },
            _ => Self::Normal(Box::new(normal)),
        }
    }

    /// The key of a field name or a text key.
    pub fn of_text(text: &str) -> Self {
        match ShortText::new(text) {
            Some(short) => Self::text(&short),
            None => Self::Normal(Box::new(text_key(text))),
        }
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(low, high) => {
                let packed = (u128::from(*high) << 64 | u128::from(*low)).to_le_bytes();
                let len = usize::from(packed[0]).min(ShortText::MAX);
                write!(f, "Text({:?})", String::from_utf8_lossy(&packed[1..=len]))
            }
            Self::Int(word) => write!(f, "Int({word})"),
            Self::Normal(normal) => write!(f, "Normal({normal:?})"),
        }
    }
}

/// The word an integral float equals, when it fits one: `-2^63` and `2^63`
/// are exact in `f64`, and a float in between with no fraction is a word.
#[inline]
fn word_of(float: f64) -> Option<i64> {
    (float.fract() == 0.0 && (-(2f64.powi(63))..2f64.powi(63)).contains(&float))
        .then_some(float as i64)
}

/// `value`'s key, or `None` when it is no legal key: a short text's and a
/// word's inline at the caller.
#[inline(always)]
pub(crate) fn key_of(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    value: &Value,
) -> Result<Option<Key>, NativeError> {
    match value {
        Value::Text(short) => Ok(Some(Key::text(short))),
        Value::Int(word) => Ok(Some(Key::Int(*word))),
        _ => other_key_of(objects, facts, value),
    }
}

/// [`key_of`] of a value that is neither a short text nor a word.
#[inline(never)]
fn other_key_of(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    value: &Value,
) -> Result<Option<Key>, NativeError> {
    if let Some(normal) = certified(facts, value) {
        return Ok(normal.map(|normal| Key::Normal(Box::new(normal.clone()))));
    }
    match value {
        Value::Float(float) => {
            if let Some(word) = word_of(float.get()) {
                return Ok(Some(Key::Int(word)));
            }
        }
        Value::LongText(_) => {
            let text = text(objects, value)?;
            return Ok(Some(Key::of_text(text)));
        }
        Value::BigInt(_) => {
            if let Some(word) = integer(objects, value)?.and_then(|int| int.word()) {
                return Ok(Some(Key::Int(word)));
            }
        }
        _ => {}
    }
    Ok(normal_key(objects, facts, value)?.map(|normal| Key::Normal(Box::new(normal))))
}

/// The normal form the head of the immutable `value` certifies, when the
/// object has a head: `Some(None)` for one that is no legal key. A head
/// answers for its object without its body, so a cold key is indexed
/// without a read.
fn certified<'a>(
    facts: &'a (impl super::FactTable + ?Sized),
    value: &Value,
) -> Option<Option<&'a NormalKey>> {
    match value {
        Value::Tuple(handle)
        | Value::LongText(handle)
        | Value::BigInt(handle)
        | Value::BigTimestamp(handle)
        | Value::Bytes(handle) => facts
            .get(handle.index() as usize)?
            .head()
            .map(super::Head::normal),
        _ => None,
    }
}

/// The tokens a tuple's member contributes to the tuple's normal form: an
/// immutable object's own normal form, embedded by
/// [`identity::embed`](crate::paged::identity::embed), so a tuple's normal
/// form needs no member's body once the member has a head (`D-KEY`).
fn nested(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    member: &Value,
    out: &mut TokenWriter,
) -> Result<bool, NativeError> {
    match member {
        Value::Tuple(_)
        | Value::LongText(_)
        | Value::BigInt(_)
        | Value::BigTimestamp(_)
        | Value::Bytes(_) => match normal_key(objects, facts, member)? {
            None => Ok(false),
            Some(key) => {
                identity::embed(&key, out);
                Ok(true)
            }
        },
        _ => tokens(objects, facts, member, out),
    }
}

/// The retry a read of the cold object `object`'s payload answers.
fn cold(object: ArenaHandle) -> NativeError {
    Retry::Cold { object, chunk: 0 }.into()
}

fn body(
    objects: &(impl super::Bodies + ?Sized),
    handle: ArenaHandle,
) -> Result<&Body, NativeError> {
    objects
        .get(handle)
        .ok_or(NativeError::Malformed("a stale handle reached the arena"))
}

/// The one resident piece of a chunked body, whole.
fn whole<T>(object: ArenaHandle, chunks: &[Chunk<T>]) -> Result<Option<&T>, NativeError> {
    match chunks {
        [] => Ok(None),
        [Chunk::Resident(whole)] => Ok(Some(whole)),
        chunks => {
            super::wants_whole(object);
            Err(Retry::Cold {
                object,
                chunk: super::first_cold(chunks).unwrap_or(0),
            }
            .into())
        }
    }
}

/// A list's or a tuple's sequence; a proxy not yet decoded retries.
fn sequence_body<'a>(
    objects: &'a (impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    handle: ArenaHandle,
) -> Result<&'a Sequence, NativeError> {
    if facts
        .get(handle.index() as usize)
        .is_none_or(|facts| facts.cold)
    {
        return Err(cold(handle));
    }
    match body(objects, handle)? {
        Body::List(sequence) | Body::Tuple(sequence) => Ok(sequence),
        Body::Cold(_) => Err(cold(handle)),
        _ => Err(NativeError::Malformed(
            "a sequence handle names another body",
        )),
    }
}

/// The values of `handle` from position `at` to the end of the chunk that
/// holds it; a cold chunk retries.
fn values_from<'a>(
    objects: &'a (impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    handle: ArenaHandle,
    at: u64,
) -> Result<&'a [Value], NativeError> {
    let sequence = sequence_body(objects, facts, handle)?;
    let (chunk, offset) = sequence.locate(at);
    match sequence.chunks.resident(chunk) {
        Some(values) => values
            .get(offset..)
            .ok_or(NativeError::Malformed("a sequence chunk is short")),
        None => Err(Retry::Cold {
            object: handle,
            chunk: chunk as u32,
        }
        .into()),
    }
}

/// Compares two sequences by position: unequal when their lengths
/// differ, else one chunk at a time from the first.
fn positions<'a>(
    objects: &'a (impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    left: ArenaHandle,
    right: ArenaHandle,
    pending: &mut Vec<Step<'a>>,
) -> Result<bool, NativeError> {
    let len = sequence_body(objects, facts, left)?.len;
    if len != sequence_body(objects, facts, right)?.len {
        return Ok(false);
    }
    pending.push(Step::Positions {
        left,
        right,
        at: 0,
        len,
    });
    Ok(true)
}

fn sequence(
    objects: &(impl super::Bodies + ?Sized),
    handle: ArenaHandle,
) -> Result<&[Value], NativeError> {
    match body(objects, handle)? {
        Body::List(Sequence { chunks, .. }) | Body::Tuple(Sequence { chunks, .. }) => {
            super::ops::values(handle, chunks)
        }
        _ => Err(NativeError::Malformed(
            "a sequence handle names another body",
        )),
    }
}

fn table(
    objects: &(impl super::Bodies + ?Sized),
    handle: ArenaHandle,
) -> Result<(&Table, &[Value]), NativeError> {
    match body(objects, handle)? {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => {
            Ok((table, super::tables::pairs(handle, table)?))
        }
        _ => Err(NativeError::Malformed("a table handle names another body")),
    }
}

/// A text's or a function name's UTF-8, whole.
pub(crate) fn text<'a>(
    objects: &'a (impl super::Bodies + ?Sized),
    value: &'a Value,
) -> Result<&'a str, NativeError> {
    match value {
        Value::Text(short) => Ok(short.as_str()),
        Value::LongText(handle) => match body(objects, *handle)? {
            Body::LongText(long) => Ok(whole(*handle, &long.chunks)?.map_or("", |text| text)),
            _ => Err(NativeError::Malformed("a text handle names another body")),
        },
        _ => Err(NativeError::Malformed("a text was expected")),
    }
}

/// Bytes, whole.
pub(crate) fn bytes(
    objects: &(impl super::Bodies + ?Sized),
    handle: ArenaHandle,
) -> Result<&[u8], NativeError> {
    match body(objects, handle)? {
        Body::Bytes(bytes) => Ok(whole(handle, &bytes.chunks)?.map_or(&[], |bytes| bytes)),
        _ => Err(NativeError::Malformed("a bytes handle names another body")),
    }
}

/// An integer value's integer.
pub(crate) fn integer(
    objects: &(impl super::Bodies + ?Sized),
    value: &Value,
) -> Result<Option<Integer>, NativeError> {
    Ok(match value {
        Value::Int(word) => Some(Integer::from(*word)),
        Value::BigInt(handle) => match body(objects, *handle)? {
            Body::BigInt(int) => Some(int.clone()),
            Body::Cold(_) => return Err(cold(*handle)),
            _ => {
                return Err(NativeError::Malformed(
                    "an integer handle names another body",
                ));
            }
        },
        _ => None,
    })
}

/// A timestamp value's nanoseconds.
pub(crate) fn nanoseconds(
    objects: &(impl super::Bodies + ?Sized),
    value: &Value,
) -> Result<Option<Integer>, NativeError> {
    Ok(match value {
        Value::Timestamp(word) => Some(Integer::from(*word)),
        Value::BigTimestamp(handle) => match body(objects, *handle)? {
            Body::Timestamp(int) => Some(int.clone()),
            Body::Cold(_) => return Err(cold(*handle)),
            _ => {
                return Err(NativeError::Malformed(
                    "a timestamp handle names another body",
                ));
            }
        },
        _ => None,
    })
}

/// `value`'s normal form, or `None` when it is no legal key.
pub(crate) fn normal_key(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    value: &Value,
) -> Result<Option<NormalKey>, NativeError> {
    if let Some(normal) = certified(facts, value) {
        return Ok(normal.cloned());
    }
    let normal = if let Value::LongText(handle) | Value::Bytes(handle) = value {
        Some(long_key(Some(*handle), body(objects, *handle)?)?)
    } else {
        let mut out = TokenWriter::new();
        tokens(objects, facts, value, &mut out)?.then(|| identity::flat(out))
    };
    certify(objects, facts, value, &normal);
    Ok(normal)
}

/// Installs the head `normal` completes on the immutable `value`, so its
/// key is computed once: a tuple embeds each member's key ([`nested`]),
/// and a member already keyed answers by its head, so keying a shared
/// graph is linear in its distinct objects, not its leaves. Nothing else
/// changes with it: the head is what a seal would certify. A value that
/// names an object or task at any depth is left to its seal: a ref keyed
/// before its referent's first seal is keyed by a token that changes.
fn certify(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    value: &Value,
    normal: &Option<NormalKey>,
) {
    let (Value::Tuple(handle)
    | Value::LongText(handle)
    | Value::BigInt(handle)
    | Value::BigTimestamp(handle)
    | Value::Bytes(handle)) = value
    else {
        return;
    };
    let (Some(fact), Ok(body)) = (facts.get(handle.index() as usize), body(objects, *handle))
    else {
        return;
    };
    if fact.summary.has_edges {
        return;
    }
    if let (Ok(head), Ok(projected)) = (
        super::heads::head_of(body, fact.summary, || Ok(normal.clone())),
        fact.try_projection(),
    ) {
        super::freeze(body);
        let _ = projected.head.set(head);
    }
}

/// The key of a held text or bytes `body`, of `object` when the arena
/// holds it, by its content's length ([`identity`]).
fn long_key(object: Option<ArenaHandle>, body: &Body) -> Result<NormalKey, NativeError> {
    let unread = |chunk: u32| match object {
        Some(object) => {
            super::wants_whole(object);
            NativeError::from(Retry::Cold { object, chunk })
        }
        None => NativeError::Malformed("a detached body holds a cold chunk"),
    };
    fn held<T>(chunks: &[Chunk<T>]) -> Result<Option<&T>, u32> {
        match chunks {
            [] => Ok(None),
            [Chunk::Resident(whole)] => Ok(Some(whole)),
            chunks => Err(super::first_cold(chunks).unwrap_or(0)),
        }
    }
    match body {
        Body::LongText(text) => Ok(identity::text_key(
            held(&text.chunks).map_err(unread)?.map_or("", |text| text),
        )),
        Body::Bytes(bytes) => Ok(identity::bytes_key(
            held(&bytes.chunks)
                .map_err(unread)?
                .map_or(&[], |bytes| bytes),
        )),
        Body::Cold(_) => Err(unread(0)),
        _ => Err(NativeError::Malformed(
            "a text or bytes handle names another body",
        )),
    }
}

/// The normal form of a text key: what a record field name is keyed by.
pub fn text_key(text: &str) -> NormalKey {
    identity::text_key(text)
}

pub(super) fn tokens(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    value: &Value,
    out: &mut TokenWriter,
) -> Result<bool, NativeError> {
    match value {
        Value::Null => out.sum(0, 0),
        Value::Bool(flag) => {
            out.sum(1, 1);
            flag.encode(out);
        }
        Value::Int(word) => {
            out.sum(2, 1);
            Integer::from(*word).encode(out);
        }
        Value::Float(float) => match integral_float(float.get()) {
            Some(int) => {
                out.sum(2, 1);
                int.encode(out);
            }
            None => {
                out.sum(3, 1);
                float.encode(out);
            }
        },
        Value::Text(short) => {
            out.sum(4, 1);
            out.text(short.as_str());
        }
        Value::Timestamp(word) => {
            out.sum(6, 1);
            Timestamp {
                nanoseconds: Integer::from(*word),
            }
            .encode(out);
        }
        Value::TaskRef(task) => {
            out.sum(7, 1);
            Identity::Task(*task).encode(out);
        }
        // A ref is keyed by its referent's durable id, which a sealed key
        // names it by on its pages and after a reopen. Until the referent
        // has one it is keyed by its process-local token, and giving it an
        // id re-keys the tables that key it so ([`super::Arena::assign`]).
        Value::ObjectRef(handle) => {
            let referent = facts
                .get(handle.index() as usize)
                .filter(|_| objects.get(*handle).is_some())
                .ok_or(NativeError::Malformed("a ref names no object of the run"))?;
            match referent.entity() {
                Some(entity) => {
                    out.sum(10, 1);
                    Identity::Object(entity).encode(out);
                }
                None => {
                    referent.token_keyed.set(true);
                    out.sum(12, 1);
                    out.unsigned(referent.token);
                }
            }
        }
        // A text or bytes object is keyed by its identity, which a token
        // stream holds only embedded ([`nested`]).
        Value::LongText(_) | Value::Bytes(_) => {
            return Err(NativeError::Malformed(
                "a text or bytes object is keyed by its identity, not its tokens",
            ));
        }
        Value::BigInt(handle)
        | Value::BigTimestamp(handle)
        | Value::Function(handle)
        | Value::Tuple(handle) => {
            return body_tokens(objects, facts, Some(*handle), body(objects, *handle)?, out);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// The tokens of the immutable `body`, held by `object` when the arena
/// holds it: what [`tokens`] writes for a value naming it. A detached body
/// (one a load decoded and has not installed) is whole.
fn body_tokens(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    object: Option<ArenaHandle>,
    body: &Body,
    out: &mut TokenWriter,
) -> Result<bool, NativeError> {
    let unread = |chunk: u32| match object {
        Some(object) => {
            super::wants_whole(object);
            NativeError::from(Retry::Cold { object, chunk })
        }
        None => NativeError::Malformed("a detached body holds a cold chunk"),
    };
    match body {
        Body::BigInt(int) => {
            out.sum(2, 1);
            int.encode(out);
        }
        Body::Timestamp(nanoseconds) => {
            out.sum(6, 1);
            Timestamp {
                nanoseconds: nanoseconds.clone(),
            }
            .encode(out);
        }
        Body::Function(callable) => {
            out.sum(8, 1);
            callable.encode(out);
        }
        Body::Tuple(sequence) => {
            let members = match object {
                Some(object) => super::ops::values(object, &sequence.chunks)?,
                None => sequence.chunks.values().ok_or_else(|| unread(0))?,
            };
            out.sum(9, 1);
            out.seq(members.len() as u64);
            for member in members {
                if !nested(objects, facts, member, out)? {
                    return Ok(false);
                }
            }
        }
        Body::Cold(_) => return Err(unread(0)),
        _ => return Ok(false),
    }
    Ok(true)
}

/// The normal form of the immutable `body`, which a load decoded and has
/// not installed: what [`normal_key`] would give once it is.
pub(crate) fn detached_normal(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    body: &Body,
) -> Result<Option<NormalKey>, NativeError> {
    if let Body::LongText(_) | Body::Bytes(_) = body {
        return long_key(None, body).map(Some);
    }
    let mut out = TokenWriter::new();
    if !body_tokens(objects, facts, None, body, &mut out)? {
        return Ok(None);
    }
    Ok(Some(identity::flat(out)))
}

enum Step<'a> {
    Pair(Value, Value),
    Sequence(&'a [Value], &'a [Value]),
    /// Two sequences of one length `len`, compared from position `at`
    /// one resident chunk at a time, so a difference in an earlier chunk
    /// ends the comparison before a later chunk is read.
    Positions {
        left: ArenaHandle,
        right: ArenaHandle,
        at: u64,
        len: u64,
    },
    Table {
        left: &'a [Value],
        right: ArenaHandle,
        values: bool,
    },
}

/// Guest equality (`K-VAL-020`): structural for immutables and
/// collections, by identity for closures, tasks and refs.
pub(crate) fn equal(
    objects: &(impl super::Bodies + ?Sized),
    facts: &(impl super::FactTable + ?Sized),
    a: &Value,
    b: &Value,
) -> Result<bool, NativeError> {
    let mut pending = vec![Step::Pair(*a, *b)];
    // Pairs of collections already compared, by slot: the comparison of
    // a cycle assumes equality where it meets one again.
    let mut seen: BTreeSet<(u32, u32)> = BTreeSet::new();
    while let Some(step) = pending.pop() {
        let (a, b) = match step {
            Step::Pair(a, b) => (a, b),
            Step::Sequence(left, right) => {
                for (a, b) in left.iter().zip(right).rev() {
                    pending.push(Step::Pair(*a, *b));
                }
                continue;
            }
            Step::Positions {
                left,
                right,
                at,
                len,
            } => {
                if at < len {
                    let left_values = values_from(objects, facts, left, at)?;
                    let right_values = values_from(objects, facts, right, at)?;
                    let n = left_values.len().min(right_values.len());
                    if n == 0 {
                        return Err(NativeError::Malformed("a sequence chunk holds no position"));
                    }
                    pending.push(Step::Positions {
                        left,
                        right,
                        at: at + n as u64,
                        len,
                    });
                    pending.push(Step::Sequence(&left_values[..n], &right_values[..n]));
                }
                continue;
            }
            Step::Table {
                left,
                right,
                values,
            } => {
                for entry in left.as_chunks::<2>().0.iter().rev() {
                    let key = key_of(objects, facts, &entry[0])?
                        .ok_or(NativeError::Malformed("an entry's key is not a legal key"))?;
                    let Some(other) = lookup(objects, facts, right, &key)? else {
                        return Ok(false);
                    };
                    if values {
                        pending.push(Step::Pair(entry[1], *other));
                    }
                }
                continue;
            }
        };
        use Value as V;
        let same = match (&a, &b) {
            (V::Null, V::Null) | (V::Absent, V::Absent) => true,
            (V::Bool(a), V::Bool(b)) => a == b,
            (V::Int(_) | V::BigInt(_), V::Int(_) | V::BigInt(_)) => {
                integer(objects, &a)? == integer(objects, &b)?
            }
            (V::Float(a), V::Float(b)) => a.get() == b.get(),
            (V::Int(_) | V::BigInt(_), V::Float(f)) | (V::Float(f), V::Int(_) | V::BigInt(_)) => {
                let int = if matches!(a, V::Float(_)) { &b } else { &a };
                match integral_float(f.get()) {
                    Some(f) => integer(objects, int)? == Some(f),
                    None => false,
                }
            }
            (V::Text(_) | V::LongText(_), V::Text(_) | V::LongText(_)) => {
                text(objects, &a)? == text(objects, &b)?
            }
            (V::Function(a), V::Function(b)) => match (body(objects, *a)?, body(objects, *b)?) {
                (Body::Function(a), Body::Function(b)) => a == b,
                _ => return Err(NativeError::Malformed("function body")),
            },
            (V::Bytes(a), V::Bytes(b)) => bytes(objects, *a)? == bytes(objects, *b)?,
            (V::Timestamp(_) | V::BigTimestamp(_), V::Timestamp(_) | V::BigTimestamp(_)) => {
                nanoseconds(objects, &a)? == nanoseconds(objects, &b)?
            }
            (V::Handle(a), V::Handle(b)) => match (body(objects, *a)?, body(objects, *b)?) {
                (Body::Handle(a), Body::Handle(b)) => a == b,
                _ => return Err(NativeError::Malformed("a handle names another body")),
            },
            (V::Task(a), V::Task(b)) | (V::TaskRef(a), V::TaskRef(b)) => a == b,
            (V::Closure(a), V::Closure(b)) | (V::ObjectRef(a), V::ObjectRef(b)) => a == b,
            (V::Tuple(left), V::Tuple(right)) => {
                positions(objects, facts, *left, *right, &mut pending)?
            }
            (V::Error(left), V::Error(right)) => {
                match (body(objects, *left)?, body(objects, *right)?) {
                    (Body::Error(left), Body::Error(right)) => {
                        if text(objects, &left.kind)? != text(objects, &right.kind)?
                            || text(objects, &left.message)? != text(objects, &right.message)?
                        {
                            return Ok(false);
                        }
                        pending.push(Step::Pair(left.data, right.data));
                        true
                    }
                    (Body::Cold(_), _) => return Err(cold(*left)),
                    (_, Body::Cold(_)) => return Err(cold(*right)),
                    _ => return Err(NativeError::Malformed("an error names another body")),
                }
            }
            (V::List(left), V::List(right)) => {
                if !seen.insert((left.index(), right.index())) {
                    continue;
                }
                positions(objects, facts, *left, *right, &mut pending)?
            }
            (V::Map(left), V::Map(right))
            | (V::Set(left), V::Set(right))
            | (V::Record(left), V::Record(right)) => {
                if !seen.insert((left.index(), right.index())) {
                    continue;
                }
                let (left_table, entries) = table(objects, *left)?;
                let (right_table, _) = table(objects, *right)?;
                if left_table.len != right_table.len {
                    return Ok(false);
                }
                pending.push(Step::Table {
                    left: entries,
                    right: *right,
                    values: !matches!(a, V::Set(_)),
                });
                true
            }
            _ => false,
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The value a resident table holds under `key`.
#[inline]
pub(crate) fn lookup<'a>(
    objects: &'a (impl super::Bodies + ?Sized),
    facts: &'a (impl super::FactTable + ?Sized),
    handle: ArenaHandle,
    key: &Key,
) -> Result<Option<&'a Value>, NativeError> {
    let table = match body(objects, handle)? {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
        _ => return Err(NativeError::Malformed("a table handle names another body")),
    };
    let index = facts
        .get(handle.index() as usize)
        .and_then(|facts| facts.index.as_ref())
        .ok_or(NativeError::Malformed("a resident table has no index"))?;
    match super::tables::find(handle, table, index, key)? {
        Some(place) => Ok(Some(&super::tables::pair(table, place)?[1])),
        None => Ok(None),
    }
}

/// The arena's guest semantics, borrowed for a call.
#[derive(Clone, Copy)]
pub struct ArenaSemantics<'a> {
    objects: &'a Objects,
    facts: &'a [Facts],
    keys: &'a KeyCache,
    limits: KernelDataLimits,
}

impl<'a> ArenaSemantics<'a> {
    pub(super) fn new(objects: &'a Objects, facts: &'a [Facts], keys: &'a KeyCache) -> Self {
        Self {
            objects,
            facts,
            keys,
            limits: KernelDataLimits::standard(),
        }
    }

    /// These semantics under the run's data limits.
    pub fn with_limits(self, limits: KernelDataLimits) -> Self {
        Self { limits, ..self }
    }

    /// `value`'s normal form, or `None` when it is no legal key.
    pub fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, NativeError> {
        self.keys.with(
            self.objects,
            self.facts,
            value,
            |normal| Ok(normal.cloned()),
        )
    }

    /// `value`'s key, or `None` when it is no legal key.
    #[inline]
    pub fn key(&self, value: &Value) -> Result<Option<Key>, NativeError> {
        key_of(self.objects, self.facts, value)
    }

    /// A text's or a function name's UTF-8, whole.
    pub fn text(&self, value: &'a Value) -> Result<&'a str, NativeError> {
        text(self.objects, value)
    }

    /// An integer value's integer, `None` for another kind.
    pub fn integer(&self, value: &Value) -> Result<Option<Integer>, NativeError> {
        integer(self.objects, value)
    }

    /// A timestamp value's nanoseconds, `None` for another kind.
    pub fn nanoseconds(&self, value: &Value) -> Result<Option<Integer>, NativeError> {
        nanoseconds(self.objects, value)
    }

    /// Bytes, whole.
    pub fn bytes(&self, handle: ArenaHandle) -> Result<&'a [u8], NativeError> {
        bytes(self.objects, handle)
    }

    /// The values of a resident list or tuple.
    pub fn sequence(&self, handle: ArenaHandle) -> Result<&'a [Value], NativeError> {
        sequence(self.objects, handle)
    }

    /// The pairs of a resident map, set or record, key then value.
    pub fn entries(&self, handle: ArenaHandle) -> Result<&'a [Value], NativeError> {
        Ok(table(self.objects, handle)?.1)
    }

    /// A machine table read refuses an illegal key; native lookup instead
    /// answers absent for one.
    #[inline]
    pub(super) fn table_get(
        &self,
        table: ArenaHandle,
        key: &Value,
    ) -> Result<Option<&'a Value>, NativeError> {
        let key = self.key(key)?.ok_or_else(super::ops::invalid_key)?;
        lookup(self.objects, self.facts, table, &key)
    }
}

impl Semantics for ArenaSemantics<'_> {
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, NativeError> {
        equal(self.objects, self.facts, a, b)
    }

    fn check_key(&self, key: &Value) -> Result<(), NativeError> {
        self.key(key)?
            .map(|_| ())
            .ok_or_else(super::ops::invalid_key)
    }

    fn lookup(&self, table: ArenaHandle, key: &Value) -> Result<Option<&Value>, NativeError> {
        match self.key(key)? {
            Some(key) => lookup(self.objects, self.facts, table, &key),
            None => Ok(None),
        }
    }

    fn normal_key(&self, key: &Value) -> Result<Option<NormalKey>, NativeError> {
        self.normal_key(key)
    }

    fn depth(&self, value: &Value) -> u32 {
        super::value_summary(self.facts, value).depth
    }

    fn limits(&self) -> KernelDataLimits {
        self.limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::Float;

    /// K-KEY-002: a bounded normalization cache collision never aliases keys.
    /// The cache must agree with the canonical grammar, including UTF-8 and
    /// every inline text length, after more distinct keys than cache slots.
    #[test]
    fn normalized_short_key_cache_checks_full_keys_on_collisions() {
        let objects = Objects::default();
        let cache = KeyCache::default();
        let names: Vec<_> = (0..256)
            .map(|i| format!("clé{i}"))
            .chain((0..=ShortText::MAX).map(|len| "x".repeat(len)))
            .collect();
        for name in names.iter().chain(names.iter().rev()) {
            let value = Value::Text(ShortText::new(name).unwrap());
            let expected = normal_key(&objects, &[], &value).unwrap();
            cache
                .with(&objects, &[], &value, |actual| {
                    assert_eq!(actual, expected.as_ref(), "{name}");
                    Ok(())
                })
                .unwrap();
        }
    }

    /// K-KEY-002: a resident key is the same key as another exactly when
    /// their normal forms are equal, over every spelling a key may take:
    /// short texts of every length and UTF-8, the same text as a function
    /// name, words and the integral floats that equal them, signed zero,
    /// floats past a word, booleans and null.
    #[test]
    fn resident_keys_agree_with_normal_forms() {
        let objects = Objects::default();
        let texts: Vec<String> = (0..=ShortText::MAX)
            .map(|len| "x".repeat(len))
            .chain(["clé".into(), "é".into(), "0".into(), "1".into()])
            .collect();
        let mut values: Vec<Value> = texts
            .iter()
            .flat_map(|text| {
                let short = ShortText::new(text).unwrap();
                [Value::Text(short)]
            })
            .collect();
        for word in [0, 1, -1, 2, i64::MAX, i64::MIN, 1 << 53] {
            values.push(Value::Int(word));
        }
        for float in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            2.0,
            0.5,
            -(2f64.powi(63)),
            2f64.powi(63),
            2f64.powi(64),
            2f64.powi(53),
            f64::INFINITY,
        ] {
            values.push(Value::Float(Float::new(float)));
        }
        values.extend([Value::Bool(true), Value::Bool(false), Value::Null]);
        for a in &values {
            for b in &values {
                let (Some(normal_a), Some(normal_b)) = (
                    normal_key(&objects, &[], a).unwrap(),
                    normal_key(&objects, &[], b).unwrap(),
                ) else {
                    continue;
                };
                let (key_a, key_b) = (
                    key_of(&objects, &[], a).unwrap().unwrap(),
                    key_of(&objects, &[], b).unwrap().unwrap(),
                );
                assert_eq!(key_a == key_b, normal_a == normal_b, "{a:?} {b:?}");
            }
        }
    }
}
