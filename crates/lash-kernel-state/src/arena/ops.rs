//! The machine's own reads and edits of arena objects, outside any native
//! call: what a member read, an assignment, a loop step and a shared
//! variable do. Each answers [`Retry::Cold`] for a proxy or for the cold
//! chunk it reaches, which the machine decodes before it asks again, so a
//! point read or write of a large list or table decodes one chunk. An edit
//! marks its object dirty, with the card it changed, and gives its logical
//! growth, which the machine holds before the heap receives it.

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, LocalCode, Objects, Sequence};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Retry, Site, Value};

use super::keys::normal_key;
use super::{Arena, CARD, Facts, Summary, local_cost, malformed, value_summary};

/// The refusal a value that is no legal key raises (`K-KEY-002`).
pub fn invalid_key() -> NativeError {
    NativeError::refused(
        "invalid_key",
        "a key is null, a bool, a number, a text, bytes, a timestamp, a ref, or a tuple of these",
    )
}

/// Where position `index` of a sequence sits: its chunk and its place in
/// it. A sequence in one chunk holds every position there; a chunked one
/// holds positions `k * CHUNK ..` in chunk `k`.
fn locate(chunks: &[Chunk<Vec<Value>>], index: u64) -> (usize, usize) {
    if chunks.len() <= 1 {
        (0, index as usize)
    } else {
        (
            (index / Sequence::CHUNK as u64) as usize,
            (index % Sequence::CHUNK as u64) as usize,
        )
    }
}

fn chunk_mut(
    handle: ArenaHandle,
    chunks: &mut [Chunk<Vec<Value>>],
    chunk: usize,
) -> Result<&mut Vec<Value>, NativeError> {
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

/// The values of a sequence in one resident chunk. A chunked one answers
/// [`Retry::Cold`] for its first cold chunk, or for chunk 0 when every
/// chunk is resident: the machine decodes, and joins the chunks into one.
pub(super) fn values(
    handle: ArenaHandle,
    chunks: &[Chunk<Vec<Value>>],
) -> Result<&[Value], NativeError> {
    match chunks {
        [] => Ok(&[]),
        [Chunk::Resident(values)] => Ok(values),
        chunks => Err(Retry::Cold {
            object: handle,
            chunk: super::first_cold(chunks).unwrap_or(0),
        }
        .into()),
    }
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
        let (chunk, at) = locate(&sequence.chunks, index);
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

    /// Marks a mutable object dirty with its cost moved from `before` to
    /// `after` for one row, and gives the growth. `card` is the card the
    /// row is in, for a list or a table.
    fn moved(
        &mut self,
        handle: ArenaHandle,
        card: Option<u64>,
        before: u64,
        after: u64,
    ) -> Result<u64, NativeError> {
        if self.unaccounted {
            self.mark(handle, card)?;
            return Ok(0);
        }
        let facts = self.facts_mut(handle)?;
        facts.summary.cost = facts
            .summary
            .cost
            .saturating_sub(before)
            .saturating_add(after);
        self.mark(handle, card)?;
        Ok(after.saturating_sub(before))
    }

    /// Writes `value` at `index` of a list, appending at its length.
    pub fn list_set(
        &mut self,
        handle: ArenaHandle,
        index: u64,
        value: Value,
    ) -> Result<u64, NativeError> {
        self.warm(handle)?;
        let after = self.cost_of(&value);
        let (len, before) = {
            let Some(Body::List(sequence)) = self.objects.get(handle) else {
                return Err(malformed("a list handle names another body"));
            };
            let before = if !self.unaccounted && index < sequence.len {
                self.item(handle, index)?
                    .map(|old| self.cost_of_metered(&old))
            } else {
                None
            };
            (sequence.len, before)
        };
        let sequence = list_mut(&mut self.objects, handle)?;
        let mut old = None;
        if index < len {
            let (chunk, at) = locate(&sequence.chunks, index);
            old = Some(std::mem::replace(
                &mut chunk_mut(handle, &mut sequence.chunks, chunk)?[at],
                value,
            ));
        } else if index == len {
            match sequence.chunks.len() {
                0 => sequence.chunks.push(Chunk::Resident(Vec::new())),
                1 => {}
                chunks if len == chunks as u64 * Sequence::CHUNK as u64 => {
                    sequence.chunks.push(Chunk::Resident(Vec::new()));
                }
                _ => {}
            }
            let last = sequence.chunks.len() - 1;
            chunk_mut(handle, &mut sequence.chunks, last)?.push(value);
            sequence.len += 1;
            self.resident = self.resident.saturating_add(1);
        } else {
            return Err(malformed("a list write past its length"));
        }
        self.hold(&value);
        if let Some(old) = old {
            self.unhold(&old);
        }
        self.moved(handle, Some(index / CARD), before.unwrap_or(0), after)
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
            let (chunk, _) = locate(&sequence.chunks, index);
            let held = chunk_mut(handle, &mut sequence.chunks, chunk)?;
            let old = held.pop();
            if held.is_empty() && sequence.chunks.len() > 1 {
                sequence.chunks.pop();
            }
            old
        } else {
            if sequence.chunks.len() > 1 {
                // A removal moves every later value: the list is one chunk
                // for it.
                if let Some(chunk) = super::first_cold(&sequence.chunks) {
                    return Err(Retry::Cold {
                        object: handle,
                        chunk,
                    }
                    .into());
                }
                let joined = super::join(std::mem::take(&mut sequence.chunks));
                sequence.chunks = vec![Chunk::Resident(joined)];
            }
            Some(chunk_mut(handle, &mut sequence.chunks, 0)?.remove(index as usize))
        };
        let old = old.ok_or_else(|| malformed("a list's chunks miss its length"))?;
        sequence.len -= 1;
        let before = self.cost_of(&old);
        for card in index / CARD..=(len - 1) / CARD {
            self.mark(handle, Some(card))?;
        }
        self.moved(handle, None, before, 0)?;
        self.unhold(&old);
        Ok(())
    }

    /// The normal form of a key, refusing a value that is no legal key.
    pub fn key(&self, key: &Value) -> Result<NormalKey, NativeError> {
        normal_key(&self.objects, &self.facts, key)?.ok_or_else(invalid_key)
    }

    /// The row cost of an entry of the table `handle`: a record's field
    /// row is priced without its name reference.
    fn entry_cost(&self, handle: ArenaHandle, key: &Value, value: &Value) -> u64 {
        if self.unaccounted {
            return 0;
        }
        let record = matches!(self.objects.get(handle), Some(Body::Record(_)));
        let cost = local_cost(&self.objects, &self.facts, key).saturating_add(local_cost(
            &self.objects,
            &self.facts,
            value,
        ));
        if record {
            cost.saturating_sub(16)
        } else {
            cost
        }
    }

    /// The value the table `handle` holds under `key`.
    pub fn table_get(
        &self,
        handle: ArenaHandle,
        key: &Value,
    ) -> Result<Option<Value>, NativeError> {
        self.warm(handle)?;
        let normal = self.key(key)?;
        Ok(self.table_find(handle, &normal)?.map(|(_, value)| value))
    }

    /// Writes `value` under `key` of the table `handle`: an existing key
    /// keeps its first spelling and position (`K-KEY-003`).
    pub fn table_write(
        &mut self,
        handle: ArenaHandle,
        key: Value,
        value: Value,
    ) -> Result<u64, NativeError> {
        self.warm(handle)?;
        let normal = self.key(&key)?;
        let (before, after) = match self.table_find(handle, &normal)? {
            Some((place, old)) => {
                let (table, _) = self.table_view(handle)?;
                let spelled = super::tables::pair(table, place)?[0];
                (
                    self.entry_cost(handle, &spelled, &old),
                    self.entry_cost(handle, &spelled, &value),
                )
            }
            None => {
                self.resident = self.resident.saturating_add(2);
                (0, self.entry_cost(handle, &key, &value))
            }
        };
        let seq = self.table_insert(handle, normal, key, value)?;
        self.moved(handle, Some(seq / CARD), before, after)
    }

    /// Removes the entry under `key` of the table `handle`, if any.
    pub fn table_delete(&mut self, handle: ArenaHandle, key: &Value) -> Result<(), NativeError> {
        self.warm(handle)?;
        let normal = self.key(key)?;
        let Some((place, _)) = self.table_find(handle, &normal)? else {
            return Ok(());
        };
        let (table, _) = self.table_view(handle)?;
        let pair = super::tables::pair(table, place)?;
        let before = self.entry_cost(handle, &pair[0], &pair[1]);
        let removed = self.table_remove(handle, &normal)?;
        let card = removed.map(|(seq, _)| seq / CARD);
        self.moved(handle, card, before, 0)?;
        Ok(())
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
    /// object; gives its handle and logical cost.
    pub fn new_closure(
        &mut self,
        site: std::sync::Arc<Site>,
        captures: Vec<ArenaHandle>,
    ) -> (ArenaHandle, u64) {
        let handle = self.insert_created(Body::Closure {
            site,
            captures: captures.into_boxed_slice(),
            code: None,
        });
        let cost = self
            .facts
            .get(handle.index() as usize)
            .map_or(0, |facts| facts.summary.cost);
        (handle, cost)
    }

    /// Allocates a shared variable holding `value`, a fresh mutable object;
    /// gives its handle and logical cost.
    pub fn new_cell(&mut self, value: Value) -> (ArenaHandle, u64) {
        let handle = self.insert_created(Body::Cell(value));
        let cost = self
            .facts
            .get(handle.index() as usize)
            .map_or(0, |facts| facts.summary.cost);
        (handle, cost)
    }

    /// The value a shared variable holds.
    pub fn cell(&self, handle: ArenaHandle) -> Result<Value, NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::Cell(value) => Ok(*value),
            _ => Err(malformed("a cell handle names another body")),
        }
    }

    /// Writes a shared variable; gives its growth.
    pub fn cell_set(&mut self, handle: ArenaHandle, value: Value) -> Result<u64, NativeError> {
        self.warm(handle)?;
        let after = self.cost_of(&value);
        let Some(Body::Cell(held)) = self.objects.get_mut(handle) else {
            return Err(malformed("a cell handle names another body"));
        };
        let old = std::mem::replace(held, value);
        let before = self.cost_of(&old);
        self.hold(&value);
        self.unhold(&old);
        self.moved(handle, None, before, after)
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
pub(super) fn composite<const MEMORY: bool>(
    objects: &Objects,
    facts: &[Facts],
    members: &[Value],
) -> Summary {
    #[cfg(any(test, feature = "perf-witness"))]
    if MEMORY {
        crate::perf_witness::record(|work| work.arena_memory_prices += 1);
    }
    let mut summary = Summary {
        depth: 1,
        ..Summary::default()
    };
    for member in members {
        let inner = value_summary(facts, member);
        if MEMORY {
            summary.cost = summary
                .cost
                .saturating_add(local_cost(objects, facts, member));
        }
        summary.depth = summary.depth.max(inner.depth.saturating_add(1));
        summary.task_edges = summary.task_edges.saturating_add(inner.task_edges);
        summary.has_edges |= inner.has_edges;
    }
    summary
}

/// A resident mutable object's logical cost, as the heap prices its head
/// (`ObjectHead::cost`): a minimum plus its rows, a record's field rows
/// less their name references.
pub(super) fn object_cost(objects: &Objects, facts: &[Facts], body: &Body) -> u64 {
    #[cfg(any(test, feature = "perf-witness"))]
    crate::perf_witness::record(|work| work.arena_memory_prices += 1);
    let rows = |chunks: &[Chunk<Vec<Value>>]| -> u64 {
        chunks
            .iter()
            .filter_map(|chunk| match chunk {
                Chunk::Resident(values) => Some(values),
                Chunk::Cold { .. } => None,
            })
            .flatten()
            .map(|value| local_cost(objects, facts, value))
            .fold(0u64, u64::saturating_add)
    };
    32u64.saturating_add(match body {
        Body::List(sequence) => rows(&sequence.chunks),
        Body::Map(table) => rows(&table.chunks),
        // A set's members are rows of a key and a null.
        Body::Set(table) => rows(&table.chunks),
        Body::Record(table) => rows(&table.chunks).saturating_sub(table.len * 16),
        Body::Closure { captures, .. } => captures.len() as u64 * 8,
        Body::Cell(value) => local_cost(objects, facts, value),
        _ => 0,
    })
}
