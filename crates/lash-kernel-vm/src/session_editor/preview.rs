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
    let semantics = arena.semantics();
    Ok(match value {
        Value::Text(_) | Value::LongText(_) => ("text", semantics.text(value)?.len() as u64),
        Value::Bytes(handle) => ("bytes", semantics.bytes(*handle)?.len() as u64),
        Value::BigInt(_) => (
            "integer",
            semantics
                .integer(value)?
                .map_or(0, |integer| integer.bits()),
        ),
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
        Value::Text(_) | Value::LongText(_) => take(remaining, semantics.text(value)?.len() as u64),
        Value::Bytes(handle) => take(remaining, semantics.bytes(*handle)?.len() as u64),
        Value::Int(_) | Value::BigInt(_) => take(
            remaining,
            semantics
                .integer(value)?
                .map_or(0, |integer| integer.bits().div_ceil(8)),
        ),
        Value::Timestamp(_) | Value::BigTimestamp(_) => take(
            remaining,
            semantics
                .nanoseconds(value)?
                .map_or(0, |nanoseconds| nanoseconds.bits().div_ceil(8)),
        ),
        Value::Handle(handle) => match resident(arena, *handle)? {
            Body::Handle(handle) => take(remaining, (handle.kind.len() + handle.id.len()) as u64),
            _ => true,
        },
        Value::Error(error) => match resident(arena, *error)? {
            Body::Error(error) => {
                take(remaining, semantics.text(&error.kind)?.len() as u64)
                    && take(remaining, semantics.text(&error.message)?.len() as u64)
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
                        if !(take(remaining, semantics.text(&pair[0])?.len() as u64)
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
