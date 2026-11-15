//! Bounded host reads: a prompt never copies an unbounded heap value.

use lash_kernel_doc::arena::Body;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{NativeError, Retry, Value};
use lash_kernel_state::arena::Arena;

use super::{CopyError, SessionEditor, SessionValue};

impl SessionEditor {
    /// Reads at most `budget` bytes of value payload and node allowance.
    /// Values beyond that budget are described without copying their payload.
    /// The limit applies to the whole reachable value, including field names.
    ///
    /// # Errors
    ///
    /// A fault in a page needed to inspect or copy the bounded value.
    pub fn preview(
        &mut self,
        name: &lash_kernel_value::names::Name,
        budget: usize,
    ) -> Result<Option<SessionValue>, HeapFault> {
        let Some(value) = self.bindings.get(name).copied() else {
            return Ok(None);
        };
        let depth = self.depth.min(64);
        let read = self.warm(value, |arena| {
            let (kind, extent) = extent(arena, &value)?;
            let fits = fits(
                arena,
                &value,
                &mut (budget as u64),
                &mut Vec::new(),
                0,
                depth,
            )?;
            Ok((kind, extent, fits))
        });
        let (kind, extent, fits) = match read {
            Ok(read) => read,
            Err(CopyError::Heap(fault)) => return Err(fault),
            Err(other) => return Err(HeapFault::Malformed(format!("{other:?}"))),
        };
        if fits {
            self.get(name)
        } else {
            Ok(Some(SessionValue::Unpreviewed { kind, extent }))
        }
    }
}

fn extent(arena: &Arena, value: &Value) -> Result<(&'static str, u64), NativeError> {
    Ok(match value {
        Value::Text(_) | Value::LongText(_) => ("text", size(arena, value)?),
        Value::Bytes(_) => ("bytes", size(arena, value)?),
        Value::BigInt(handle) => ("integer", bits(arena, *handle)?),
        Value::Tuple(handle)
        | Value::List(handle)
        | Value::Map(handle)
        | Value::Set(handle)
        | Value::Record(handle) => {
            let kind = match value {
                Value::Tuple(_) => "tuple",
                Value::List(_) => "list",
                Value::Map(_) => "map",
                Value::Set(_) => "set",
                _ => "record",
            };
            (kind, arena.len(*handle)?)
        }
        Value::Error(_) => ("error", 1),
        _ => ("value", 1),
    })
}

fn take(remaining: &mut u64, bytes: u64) -> bool {
    match remaining.checked_sub(bytes) {
        Some(left) => {
            *remaining = left;
            true
        }
        None => false,
    }
}

/// The UTF-8 length of a text, or the length of bytes, from its summary:
/// a cold text or bytes object opens with its head's lengths, so sizing it
/// reads no payload.
fn size(arena: &Arena, value: &Value) -> Result<u64, NativeError> {
    match value {
        Value::Text(short) => Ok(short.as_str().len() as u64),
        Value::LongText(handle) => match arena.body(*handle)? {
            Body::LongText(text) => Ok(text.summary.utf8_bytes),
            _ => Err(NativeError::Malformed("a text names another body")),
        },
        Value::Bytes(handle) => match arena.body(*handle)? {
            Body::Bytes(bytes) => Ok(bytes.len),
            _ => Err(NativeError::Malformed("bytes name another body")),
        },
        _ => Err(NativeError::Malformed("a size of no text or bytes")),
    }
}

/// The magnitude bits of a big integer or timestamp object, from its
/// summary when it is cold.
fn bits(arena: &Arena, handle: lash_kernel_doc::arena::ArenaHandle) -> Result<u64, NativeError> {
    use lash_kernel_doc::arena::ColdImmutable;
    match arena.body(handle)? {
        Body::BigInt(int) | Body::Timestamp(int) => Ok(int.bits()),
        Body::Cold(ColdImmutable::BigInt(summary) | ColdImmutable::Timestamp(summary)) => {
            Ok(summary.bits)
        }
        _ => Err(NativeError::Malformed("an integer names another body")),
    }
}

/// `handle`'s body, resident.
fn resident(
    arena: &Arena,
    handle: lash_kernel_doc::arena::ArenaHandle,
) -> Result<&Body, NativeError> {
    match arena.body(handle)? {
        Body::Cold(_) => Err(Retry::Cold {
            object: handle,
            chunk: 0,
        }
        .into()),
        body => Ok(body),
    }
}

fn fits(
    arena: &Arena,
    value: &Value,
    remaining: &mut u64,
    path: &mut Vec<lash_kernel_doc::arena::ArenaHandle>,
    depth: usize,
    limit: usize,
) -> Result<bool, NativeError> {
    // Every copied node needs space even when its serialized value is tiny.
    // Depth stops here too, before an adversarial immutable tuple chain can
    // recurse past the host stack. The full reader still classifies cycles
    // and non-data values as opaque when they fit this inspection.
    if depth >= limit || !take(remaining, 64) {
        return Ok(false);
    }
    let semantics = arena.semantics();
    Ok(match value {
        Value::Text(_) | Value::LongText(_) | Value::Bytes(_) => {
            take(remaining, size(arena, value)?)
        }
        Value::Int(_) => take(
            remaining,
            semantics
                .integer(value)?
                .map_or(0, |integer| integer.bits().div_ceil(8)),
        ),
        Value::Timestamp(_) => take(
            remaining,
            semantics
                .nanoseconds(value)?
                .map_or(0, |nanoseconds| nanoseconds.bits().div_ceil(8)),
        ),
        Value::BigInt(handle) | Value::BigTimestamp(handle) => {
            take(remaining, bits(arena, *handle)?.div_ceil(8))
        }
        Value::Handle(handle) => match resident(arena, *handle)? {
            Body::Handle(handle) => take(remaining, (handle.kind.len() + handle.id.len()) as u64),
            _ => true,
        },
        Value::Error(error) => match resident(arena, *error)? {
            Body::Error(error) => {
                take(remaining, size(arena, &error.kind)?)
                    && take(remaining, size(arena, &error.message)?)
                    && fits(arena, &error.data, remaining, path, depth + 1, limit)?
            }
            _ => true,
        },
        Value::Tuple(handle) => {
            if arena.len(*handle)? > *remaining / 64 {
                return Ok(false);
            }
            for member in arena.sequence(*handle)? {
                if !fits(arena, member, remaining, path, depth + 1, limit)? {
                    return Ok(false);
                }
            }
            true
        }
        Value::List(handle) | Value::Map(handle) | Value::Set(handle) | Value::Record(handle) => {
            if path.contains(handle) {
                return Ok(true);
            }
            if arena.len(*handle)? > *remaining / 64 {
                return Ok(false);
            }
            path.push(*handle);
            let all = match value {
                Value::List(_) => members(
                    arena,
                    arena.sequence(*handle)?,
                    remaining,
                    path,
                    depth,
                    limit,
                ),
                Value::Set(_) => {
                    let members_of: Vec<Value> =
                        arena.entries(*handle)?.iter().step_by(2).copied().collect();
                    members(arena, &members_of, remaining, path, depth, limit)
                }
                Value::Map(_) => members(
                    arena,
                    arena.entries(*handle)?,
                    remaining,
                    path,
                    depth,
                    limit,
                ),
                _ => (|| {
                    for pair in arena.entries(*handle)?.as_chunks::<2>().0 {
                        if !(take(remaining, size(arena, &pair[0])?)
                            && fits(arena, &pair[1], remaining, path, depth + 1, limit)?)
                        {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })(),
            }?;
            path.pop();
            all
        }
        _ => true,
    })
}

fn members(
    arena: &Arena,
    values: &[Value],
    remaining: &mut u64,
    path: &mut Vec<lash_kernel_doc::arena::ArenaHandle>,
    depth: usize,
    limit: usize,
) -> Result<bool, NativeError> {
    for value in values {
        if !fits(arena, value, remaining, path, depth + 1, limit)? {
            return Ok(false);
        }
    }
    Ok(true)
}
