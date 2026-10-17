//! Guest equality and keys over the arena (`K-VAL-020` to `K-VAL-033`,
//! `K-KEY-002`): the port of the paged heap's `equality.rs` and normal
//! forms. A key's normal form is the token spelling the heap's lookup
//! index keys it by, so a key means the same in the arena and in pages.
//!
//! A read that reaches a cold chunk answers [`Retry::Cold`]: the machine
//! decodes it and asks again.

use std::collections::BTreeSet;

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Objects, Sequence, Table};
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenWriter};
use lash_kernel_doc::heap::{Identity, NormalKey};
use lash_kernel_doc::heap_values::{integral_float, key_digest};
use lash_kernel_doc::{Integer, KernelDataLimits, NativeError, Retry, Semantics, Timestamp, Value};

use super::Facts;

fn body(objects: &Objects, handle: ArenaHandle) -> Result<&Body, NativeError> {
    objects
        .get(handle)
        .ok_or(NativeError::Malformed("a stale handle reached the arena"))
}

/// The resident values of a one-chunk sequence or table.
fn resident(object: ArenaHandle, chunks: &[Chunk<Vec<Value>>]) -> Result<&[Value], NativeError> {
    match chunks {
        [] => Ok(&[]),
        [Chunk::Resident(values)] => Ok(values),
        [Chunk::Cold { .. }] => Err(Retry::Cold { object, chunk: 0 }.into()),
        _ => Err(NativeError::Malformed("an object is not one chunk")),
    }
}

fn sequence(objects: &Objects, handle: ArenaHandle) -> Result<&[Value], NativeError> {
    match body(objects, handle)? {
        Body::List(Sequence { chunks, .. }) | Body::Tuple(Sequence { chunks, .. }) => {
            resident(handle, chunks)
        }
        _ => Err(NativeError::Malformed(
            "a sequence handle names another body",
        )),
    }
}

fn table(objects: &Objects, handle: ArenaHandle) -> Result<(&Table, &[Value]), NativeError> {
    match body(objects, handle)? {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => {
            Ok((table, resident(handle, &table.chunks)?))
        }
        _ => Err(NativeError::Malformed("a table handle names another body")),
    }
}

/// A text's or a function name's UTF-8, whole.
pub(crate) fn text<'a>(objects: &'a Objects, value: &'a Value) -> Result<&'a str, NativeError> {
    match value {
        Value::Text(short) | Value::Function(short) => Ok(short.as_str()),
        Value::LongText(handle) | Value::LongFunction(handle) => match body(objects, *handle)? {
            Body::LongText(long) | Body::LongFunction(long) => match &*long.chunks {
                [Chunk::Resident(text)] => Ok(text),
                [Chunk::Cold { .. }] => Err(Retry::Cold {
                    object: *handle,
                    chunk: 0,
                }
                .into()),
                _ => Err(NativeError::Malformed("a text is not one chunk")),
            },
            _ => Err(NativeError::Malformed("a text handle names another body")),
        },
        _ => Err(NativeError::Malformed("a text was expected")),
    }
}

/// Bytes, whole.
pub(crate) fn bytes(objects: &Objects, handle: ArenaHandle) -> Result<&[u8], NativeError> {
    match body(objects, handle)? {
        Body::Bytes(bytes) => match &*bytes.chunks {
            [Chunk::Resident(bytes)] => Ok(bytes),
            [Chunk::Cold { .. }] => Err(Retry::Cold {
                object: handle,
                chunk: 0,
            }
            .into()),
            _ => Err(NativeError::Malformed("bytes are not one chunk")),
        },
        _ => Err(NativeError::Malformed("a bytes handle names another body")),
    }
}

/// An integer value's integer.
pub(crate) fn integer(objects: &Objects, value: &Value) -> Result<Option<Integer>, NativeError> {
    Ok(match value {
        Value::Int(word) => Some(Integer::from(*word)),
        Value::BigInt(handle) => match body(objects, *handle)? {
            Body::BigInt(int) => Some(int.clone()),
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
    objects: &Objects,
    value: &Value,
) -> Result<Option<Integer>, NativeError> {
    Ok(match value {
        Value::Timestamp(word) => Some(Integer::from(*word)),
        Value::BigTimestamp(handle) => match body(objects, *handle)? {
            Body::Timestamp(int) => Some(int.clone()),
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
    objects: &Objects,
    facts: &[Facts],
    value: &Value,
) -> Result<Option<NormalKey>, NativeError> {
    let mut out = TokenWriter::new();
    if !tokens(objects, facts, value, &mut out)? {
        return Ok(None);
    }
    let bytes = out.into_bytes();
    Ok(Some(if bytes.len() <= NormalKey::INLINE_MAX {
        NormalKey::Inline(bytes)
    } else {
        NormalKey::Digest(key_digest(&bytes))
    }))
}

/// The normal form of a text key: what a record field name is keyed by.
pub fn text_key(text: &str) -> NormalKey {
    let mut out = TokenWriter::new();
    out.sum(4, 1);
    out.text(text);
    let bytes = out.into_bytes();
    if bytes.len() <= NormalKey::INLINE_MAX {
        NormalKey::Inline(bytes)
    } else {
        NormalKey::Digest(key_digest(&bytes))
    }
}

fn tokens(
    objects: &Objects,
    facts: &[Facts],
    value: &Value,
    out: &mut TokenWriter,
) -> Result<bool, NativeError> {
    match value {
        Value::Null => out.sum(0, 0),
        Value::Bool(flag) => {
            out.sum(1, 1);
            flag.encode(out);
        }
        Value::Int(_) | Value::BigInt(_) => {
            out.sum(2, 1);
            integer(objects, value)?
                .ok_or(NativeError::Malformed("an integer was expected"))?
                .encode(out);
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
        Value::Text(_) | Value::LongText(_) => {
            out.sum(4, 1);
            out.text(text(objects, value)?);
        }
        Value::Bytes(handle) => {
            out.sum(5, 1);
            out.bytes(bytes(objects, *handle)?);
        }
        Value::Timestamp(_) | Value::BigTimestamp(_) => {
            out.sum(6, 1);
            Timestamp {
                nanoseconds: nanoseconds(objects, value)?
                    .ok_or(NativeError::Malformed("a timestamp was expected"))?,
            }
            .encode(out);
        }
        Value::TaskRef(task) => {
            out.sum(7, 1);
            Identity::Task(*task).encode(out);
        }
        // A ref is keyed by its referent's process-local token, whether or
        // not a projection has given it a durable id: an arena key never
        // reaches a page, and a ref keeps its referent resident.
        Value::ObjectRef(handle) => {
            let token = facts
                .get(handle.index() as usize)
                .filter(|_| objects.get(*handle).is_some())
                .map(|facts| facts.token)
                .ok_or(NativeError::Malformed("a ref names no object of the run"))?;
            out.sum(10, 1);
            out.unsigned(token);
        }
        Value::Function(_) | Value::LongFunction(_) => {
            out.sum(8, 1);
            out.text(text(objects, value)?);
        }
        Value::Tuple(handle) => {
            let members = sequence(objects, *handle)?;
            out.sum(9, 1);
            out.seq(members.len() as u64);
            for member in members {
                if !tokens(objects, facts, member, out)? {
                    return Ok(false);
                }
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

enum Step<'a> {
    Pair(Value, Value),
    Sequence(&'a [Value], &'a [Value]),
    Table {
        left: &'a [Value],
        right: ArenaHandle,
        values: bool,
    },
}

/// Guest equality (`K-VAL-020`): structural for immutables and
/// collections, by identity for closures, tasks and refs.
pub(crate) fn equal(
    objects: &Objects,
    facts: &[Facts],
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
            Step::Table {
                left,
                right,
                values,
            } => {
                for entry in left.as_chunks::<2>().0.iter().rev() {
                    let normal = normal_key(objects, facts, &entry[0])?
                        .ok_or(NativeError::Malformed("an entry's key is not a legal key"))?;
                    let Some(other) = lookup(objects, facts, right, &normal)? else {
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
                integral_float(f.get())
                    .is_some_and(|f| integer(objects, int).ok().flatten() == Some(f))
            }
            (V::Text(_) | V::LongText(_), V::Text(_) | V::LongText(_))
            | (V::Function(_) | V::LongFunction(_), V::Function(_) | V::LongFunction(_)) => {
                text(objects, &a)? == text(objects, &b)?
            }
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
                let (left, right) = (sequence(objects, *left)?, sequence(objects, *right)?);
                if left.len() != right.len() {
                    return Ok(false);
                }
                pending.push(Step::Sequence(left, right));
                true
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
                    _ => return Err(NativeError::Malformed("an error names another body")),
                }
            }
            (V::List(left), V::List(right)) => {
                if !seen.insert((left.index(), right.index())) {
                    continue;
                }
                let (left, right) = (sequence(objects, *left)?, sequence(objects, *right)?);
                if left.len() != right.len() {
                    return Ok(false);
                }
                pending.push(Step::Sequence(left, right));
                true
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

/// The value a resident table holds under normal form `normal`.
pub(crate) fn lookup<'a>(
    objects: &'a Objects,
    facts: &'a [Facts],
    handle: ArenaHandle,
    normal: &NormalKey,
) -> Result<Option<&'a Value>, NativeError> {
    let (_, values) = table(objects, handle)?;
    let index = facts
        .get(handle.index() as usize)
        .and_then(|facts| facts.index.as_deref())
        .ok_or(NativeError::Malformed("a resident table has no index"))?;
    Ok(index.find(normal).map(|at| &values[at * 2 + 1]))
}

/// The arena's guest semantics, borrowed for a call.
#[derive(Clone, Copy)]
pub struct ArenaSemantics<'a> {
    objects: &'a Objects,
    facts: &'a [Facts],
    limits: KernelDataLimits,
}

impl<'a> ArenaSemantics<'a> {
    pub(super) fn new(objects: &'a Objects, facts: &'a [Facts]) -> Self {
        Self {
            objects,
            facts,
            limits: KernelDataLimits::standard(),
        }
    }

    /// These semantics under the run's data limits.
    pub fn with_limits(self, limits: KernelDataLimits) -> Self {
        Self { limits, ..self }
    }

    /// `value`'s normal form, or `None` when it is no legal key.
    pub fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, NativeError> {
        normal_key(self.objects, self.facts, value)
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

    /// The value a resident table holds under normal form `normal`.
    pub fn find(
        &self,
        handle: ArenaHandle,
        normal: &NormalKey,
    ) -> Result<Option<&'a Value>, NativeError> {
        lookup(self.objects, self.facts, handle, normal)
    }
}

impl Semantics for ArenaSemantics<'_> {
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, NativeError> {
        equal(self.objects, self.facts, a, b)
    }

    fn check_key(&self, key: &Value) -> Result<(), NativeError> {
        match normal_key(self.objects, self.facts, key)? {
            Some(_) => Ok(()),
            None => Err(NativeError::refused(
                "invalid_key",
                "a key is null, a bool, a number, a text, bytes, a timestamp, a ref, or a tuple of these",
            )),
        }
    }

    fn lookup(&self, table: ArenaHandle, key: &Value) -> Result<Option<&Value>, NativeError> {
        match normal_key(self.objects, self.facts, key)? {
            Some(normal) => lookup(self.objects, self.facts, table, &normal),
            None => Ok(None),
        }
    }

    fn normal_key(&self, key: &Value) -> Result<Option<NormalKey>, NativeError> {
        normal_key(self.objects, self.facts, key)
    }

    fn depth(&self, value: &Value) -> u32 {
        super::value_summary(self.facts, value).depth
    }

    fn limits(&self) -> KernelDataLimits {
        self.limits
    }
}
