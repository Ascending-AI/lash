//! The machine's own reads and edits of arena objects, outside any native
//! call: what a member read, an assignment, a loop step and a shared
//! variable do. Each answers [`Retry::Cold`] for a proxy or for the cold
//! chunk it reaches, which the machine decodes before it asks again, so a
//! point read or write of a large list or table decodes one chunk. An edit
//! marks its object dirty, with the card it changed; what it grew by is in
//! the resident count the capacity meter reads (FIG-6791).

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Chunks, LocalCode, Sequence};
use lash_kernel_doc::{NativeError, Retry, Site, Value};

use super::{Arena, CARD, Facts, Key, Summary, malformed, value_summary};

/// The refusal a value that is no legal key raises (`K-KEY-002`).
pub fn invalid_key() -> NativeError {
    NativeError::refused(
        "invalid_key",
        "a key is null, a bool, a number, a text, bytes, a timestamp, a ref, or a tuple of these",
    )
}

/// Chunk `chunk` of a sequence, to edit its values in place.
fn chunk_mut(
    handle: ArenaHandle,
    chunks: &mut Chunks,
    chunk: usize,
) -> Result<&mut [Value], NativeError> {
    match chunks.get_mut(chunk) {
        Some(Chunk::Resident(values)) => Ok(values),
        Some(Chunk::Cold { .. }) => Err(Retry::Cold {
            object: handle,
            chunk: chunk as u32,
        }
        .into()),
        None => Err(malformed("a sequence's chunks miss its length")),
    }
}

/// Chunk `chunk` of a sequence, to grow or shrink it.
fn chunk_vec(
    handle: ArenaHandle,
    chunks: &mut Chunks,
    chunk: usize,
) -> Result<&mut Vec<Value>, NativeError> {
    match chunks.vec_mut(chunk) {
        Some(Chunk::Resident(values)) => Ok(values),
        Some(Chunk::Cold { .. }) => Err(Retry::Cold {
            object: handle,
            chunk: chunk as u32,
        }
        .into()),
        None => Err(malformed("a sequence's chunks miss its length")),
    }
}

/// The values of a sequence in one resident piece. A chunked one answers
/// [`Retry::Cold`] for its first cold chunk, or for chunk 0 when every
/// chunk is resident: the machine decodes, and joins the chunks into one.
pub(super) fn values(handle: ArenaHandle, chunks: &Chunks) -> Result<&[Value], NativeError> {
    chunks.values().ok_or_else(|| {
        Retry::Cold {
            object: handle,
            chunk: chunks.first_cold().unwrap_or(0),
        }
        .into()
    })
}

fn list_mut(
    objects: &mut lash_kernel_doc::arena::Objects,
    handle: ArenaHandle,
) -> Result<&mut Sequence, NativeError> {
    match objects.get_mut(handle) {
        Some(Body::List(sequence)) => Ok(sequence),
        _ => Err(malformed("a list handle names another body")),
    }
}

impl Arena {
    /// Answers [`Retry::Cold`] for a proxy the machine has not decoded.
    #[inline]
    pub(super) fn warm(&self, handle: ArenaHandle) -> Result<(), NativeError> {
        if self.facts(handle)?.cold {
            return Err(Retry::Cold {
                object: handle,
                chunk: 0,
            }
            .into());
        }
        Ok(())
    }

    /// Whether `handle` is a proxy the machine has not decoded.
    pub fn is_cold(&self, handle: ArenaHandle) -> Result<bool, NativeError> {
        Ok(self.facts(handle)?.cold)
    }

    /// The values of a list or a tuple, whole.
    pub fn sequence(&self, handle: ArenaHandle) -> Result<&[Value], NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::List(Sequence { chunks, .. }) | Body::Tuple(Sequence { chunks, .. }) => {
                values(handle, chunks)
            }
            _ => Err(malformed("a sequence handle names another body")),
        }
    }

    /// The entries of a map, a set or a record, whole, as key and value
    /// pairs.
    pub fn entries(&self, handle: ArenaHandle) -> Result<&[Value], NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                super::tables::pairs(handle, table)
            }
            _ => Err(malformed("a table handle names another body")),
        }
    }

    /// How many values a list or a tuple holds, or entries a table.
    pub fn len(&self, handle: ArenaHandle) -> Result<u64, NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::List(sequence) | Body::Tuple(sequence) => Ok(sequence.len),
            Body::Map(table) | Body::Set(table) | Body::Record(table) => Ok(table.len),
            _ => Err(malformed("a collection handle names another body")),
        }
    }

    /// The value at `index` of a list or a tuple: one chunk read.
    pub fn item(&self, handle: ArenaHandle, index: u64) -> Result<Option<Value>, NativeError> {
        self.warm(handle)?;
        let (Body::List(sequence) | Body::Tuple(sequence)) = self.body(handle)? else {
            return Err(malformed("a sequence handle names another body"));
        };
        if index >= sequence.len {
            return Ok(None);
        }
        let (chunk, at) = sequence.locate(index);
        match sequence.chunks.get(chunk) {
            Some(Chunk::Resident(values)) => Ok(values.get(at).copied()),
            Some(Chunk::Cold { .. }) => Err(Retry::Cold {
                object: handle,
                chunk: chunk as u32,
            }
            .into()),
            None => Err(malformed("a sequence's chunks miss its length")),
        }
    }

    /// Writes `value` at `index` of a list, appending at its length.
    pub fn list_set(
        &mut self,
        handle: ArenaHandle,
        index: u64,
        value: Value,
    ) -> Result<(), NativeError> {
        self.warm(handle)?;
        let len = match self.objects.get(handle) {
            Some(Body::List(sequence)) => sequence.len,
            _ => return Err(malformed("a list handle names another body")),
        };
        let sequence = list_mut(&mut self.objects, handle)?;
        let mut old = None;
        if index < len {
            let (chunk, at) = sequence.locate(index);
            old = Some(std::mem::replace(
                &mut chunk_mut(handle, &mut sequence.chunks, chunk)?[at],
                value,
            ));
        } else if index == len {
            if let Chunks::Split(chunks) = &mut sequence.chunks {
                match chunks.len() {
                    0 => chunks.push(Chunk::Resident(Vec::new())),
                    1 => {}
                    count if len == count as u64 * Sequence::CHUNK as u64 => {
                        chunks.push(Chunk::Resident(Vec::new()));
                    }
                    _ => {}
                }
            }
            if !sequence.chunks.push_inline(value) {
                let last = sequence.chunks.count() - 1;
                chunk_vec(handle, &mut sequence.chunks, last)?.push(value);
            }
            sequence.len += 1;
            self.resident = self.resident.saturating_add(1);
        } else {
            return Err(malformed("a list write past its length"));
        }
        self.hold(&value);
        if let Some(old) = old {
            self.unhold(&old);
        }
        self.mark(handle, Some(index / CARD))
    }

    /// Removes the value at `index` of a list. Removing the last value
    /// changes its chunk alone; removing another moves every later one, in
    /// one chunk.
    pub fn list_remove(&mut self, handle: ArenaHandle, index: u64) -> Result<(), NativeError> {
        self.warm(handle)?;
        let sequence = list_mut(&mut self.objects, handle)?;
        let len = sequence.len;
        if index >= len {
            return Err(malformed("a list removal past its length"));
        }
        let old = if index + 1 == len {
            let (chunk, _) = sequence.locate(index);
            let held = chunk_vec(handle, &mut sequence.chunks, chunk)?;
            let old = held.pop();
            if held.is_empty()
                && let Chunks::Split(chunks) = &mut sequence.chunks
                && chunks.len() > 1
            {
                chunks.pop();
            }
            old
        } else {
            if sequence.chunks.count() > 1 {
                // A removal moves every later value: the list is one chunk
                // for it.
                if let Some(chunk) = sequence.chunks.first_cold() {
                    return Err(Retry::Cold {
                        object: handle,
                        chunk,
                    }
                    .into());
                }
                sequence.chunks.join();
            }
            Some(chunk_vec(handle, &mut sequence.chunks, 0)?.remove(index as usize))
        };
        let old = old.ok_or_else(|| malformed("a list's chunks miss its length"))?;
        sequence.len -= 1;
        self.resident = self.resident.saturating_sub(1);
        for card in index / CARD..=(len - 1) / CARD {
            self.mark(handle, Some(card))?;
        }
        self.unhold(&old);
        Ok(())
    }

    /// The resident key of `key`, refusing a value that is no legal key.
    #[inline]
    pub fn key(&self, key: &Value) -> Result<Key, NativeError> {
        self.semantics().key(key)?.ok_or_else(invalid_key)
    }

    /// The value the table `handle` holds under `key`.
    #[inline]
    pub fn table_get(
        &self,
        handle: ArenaHandle,
        key: &Value,
    ) -> Result<Option<Value>, NativeError> {
        self.warm(handle)?;
        Ok(self.semantics().table_get(handle, key)?.copied())
    }

    /// Writes `value` under `key` of the table `handle`: an existing key
    /// keeps its first spelling and position (`K-KEY-003`).
    pub fn table_write(
        &mut self,
        handle: ArenaHandle,
        key: Value,
        value: Value,
    ) -> Result<(), NativeError> {
        self.warm(handle)?;
        let resident = self.key(&key)?;
        let seq = match self.table_overwrite(handle, &resident, value)? {
            Some(seq) => seq,
            None => {
                self.resident = self.resident.saturating_add(2);
                self.table_append(handle, resident, key, value)?
            }
        };
        self.mark(handle, Some(seq / CARD))
    }

    /// Removes the entry under `key` of the table `handle`, if any.
    pub fn table_delete(&mut self, handle: ArenaHandle, key: &Value) -> Result<(), NativeError> {
        self.warm(handle)?;
        let resident = self.key(key)?;
        if self.table_find(handle, &resident)?.is_none() {
            return Ok(());
        }
        let removed = self.table_remove(handle, &resident)?;
        if removed.is_some() {
            self.resident = self.resident.saturating_sub(2);
        }
        self.mark(handle, removed.map(|(seq, _)| seq / CARD))
    }

    /// The first entry of a table after cursor `after`, warm.
    pub fn table_next(
        &self,
        handle: ArenaHandle,
        after: Option<u64>,
    ) -> Result<Option<(u64, Value)>, NativeError> {
        self.warm(handle)?;
        self.table_after(handle, after)
    }

    /// A closure's code site, the cells it shares and the code a call last
    /// found for the site.
    pub fn closure(
        &self,
        handle: ArenaHandle,
    ) -> Result<(&Site, &[ArenaHandle], Option<LocalCode>), NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::Closure {
                site,
                captures,
                code,
            } => Ok((site, captures, *code)),
            _ => Err(malformed("a closure handle names another body")),
        }
    }

    /// Keeps the code a call found for a closure's site for its next calls.
    /// The cache is the process's alone: nothing projects it, so the
    /// object does not change.
    pub fn found_closure_code(
        &mut self,
        handle: ArenaHandle,
        found: LocalCode,
    ) -> Result<(), NativeError> {
        self.warm(handle)?;
        match self.objects.get_mut(handle) {
            Some(Body::Closure { code, .. }) => {
                *code = Some(found);
                Ok(())
            }
            _ => Err(malformed("a closure handle names another body")),
        }
    }

    /// Allocates a closure of `site` over `captures`, a fresh mutable
    /// object.
    pub fn new_closure(
        &mut self,
        site: std::sync::Arc<Site>,
        captures: Vec<ArenaHandle>,
    ) -> ArenaHandle {
        self.insert_created(Body::Closure {
            site,
            captures: captures.into_boxed_slice(),
            code: None,
        })
    }

    /// Allocates a shared variable holding `value`, a fresh mutable object.
    pub fn new_cell(&mut self, value: Value) -> ArenaHandle {
        self.insert_created(Body::Cell(value))
    }

    /// The value a shared variable holds.
    pub fn cell(&self, handle: ArenaHandle) -> Result<Value, NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::Cell(value) => Ok(*value),
            _ => Err(malformed("a cell handle names another body")),
        }
    }

    /// Writes a shared variable.
    pub fn cell_set(&mut self, handle: ArenaHandle, value: Value) -> Result<(), NativeError> {
        self.warm(handle)?;
        let Some(Body::Cell(held)) = self.objects.get_mut(handle) else {
            return Err(malformed("a cell handle names another body"));
        };
        let old = std::mem::replace(held, value);
        self.hold(&value);
        self.unhold(&old);
        self.mark(handle, None)
    }

    /// The kind of value the mutable object `handle` is, by its body.
    pub fn object_value(&self, handle: ArenaHandle) -> Result<Option<Value>, NativeError> {
        self.warm(handle)?;
        Ok(match self.body(handle)? {
            Body::List(_) => Some(Value::List(handle)),
            Body::Map(_) => Some(Value::Map(handle)),
            Body::Set(_) => Some(Value::Set(handle)),
            Body::Record(_) => Some(Value::Record(handle)),
            Body::Closure { .. } => Some(Value::Closure(handle)),
            _ => None,
        })
    }
}

/// The summary of an immutable composite of `members`.
pub(super) fn composite(facts: &[Facts], members: &[Value]) -> Summary {
    let mut summary = Summary {
        depth: 1,
        ..Summary::default()
    };
    for member in members {
        let inner = value_summary(facts, member);
        summary.depth = summary.depth.max(inner.depth.saturating_add(1));
        summary.task_edges = summary.task_edges.saturating_add(inner.task_edges);
        summary.has_edges |= inner.has_edges;
    }
    summary
}
