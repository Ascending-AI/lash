//! The machine's own reads and edits of arena objects, outside any native
//! call: what a member read, an assignment, a loop step and a shared
//! variable do. Each answers [`Retry::Cold`] for a proxy, which the
//! machine decodes and asks again. An edit marks its object dirty and
//! gives its logical growth, which the machine holds before the heap
//! receives it.

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, Sequence, Table};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{NativeError, Retry, Site, Value};

use super::keys::normal_key;
use super::{Arena, local_cost, malformed};

/// The refusal a value that is no legal key raises (`K-KEY-002`).
pub fn invalid_key() -> NativeError {
    NativeError::refused(
        "invalid_key",
        "a key is null, a bool, a number, a text, bytes, a timestamp, a ref, or a tuple of these",
    )
}

fn values_mut(
    handle: ArenaHandle,
    chunks: &mut Vec<Chunk<Vec<Value>>>,
) -> Result<&mut Vec<Value>, NativeError> {
    if chunks.is_empty() {
        chunks.push(Chunk::Resident(Vec::new()));
    }
    match chunks.as_mut_slice() {
        [Chunk::Resident(values)] => Ok(values),
        [Chunk::Cold { .. }] => Err(Retry::Cold {
            object: handle,
            chunk: 0,
        }
        .into()),
        _ => Err(malformed("an object is not one chunk")),
    }
}

fn values(handle: ArenaHandle, chunks: &[Chunk<Vec<Value>>]) -> Result<&[Value], NativeError> {
    match chunks {
        [] => Ok(&[]),
        [Chunk::Resident(values)] => Ok(values),
        [Chunk::Cold { .. }] => Err(Retry::Cold {
            object: handle,
            chunk: 0,
        }
        .into()),
        _ => Err(malformed("an object is not one chunk")),
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

    /// The values of a list or a tuple.
    pub fn sequence(&self, handle: ArenaHandle) -> Result<&[Value], NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::List(Sequence { chunks, .. }) | Body::Tuple(Sequence { chunks, .. }) => {
                values(handle, chunks)
            }
            _ => Err(malformed("a sequence handle names another body")),
        }
    }

    /// The entries of a map, a set or a record, as key and value pairs.
    pub fn entries(&self, handle: ArenaHandle) -> Result<&[Value], NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::Map(Table { chunks, .. })
            | Body::Set(Table { chunks, .. })
            | Body::Record(Table { chunks, .. }) => values(handle, chunks),
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

    /// The value at `index` of a list or a tuple.
    pub fn item(&self, handle: ArenaHandle, index: u64) -> Result<Option<Value>, NativeError> {
        let values = self.sequence(handle)?;
        Ok(usize::try_from(index)
            .ok()
            .and_then(|index| values.get(index))
            .copied())
    }

    /// Marks a mutable object dirty with its cost moved from `before` to
    /// `after` for one row, and gives the growth.
    fn moved(&mut self, handle: ArenaHandle, before: u64, after: u64) -> Result<u64, NativeError> {
        let facts = self.facts_mut(handle)?;
        facts.summary.cost = facts
            .summary
            .cost
            .saturating_sub(before)
            .saturating_add(after);
        self.changed(handle);
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
        let after = local_cost(&self.objects, &self.facts, &value);
        let before = {
            let (objects, facts) = (&self.objects, &self.facts);
            match objects.get(handle) {
                Some(Body::List(sequence)) => values(handle, &sequence.chunks)?
                    .get(index as usize)
                    .map(|old| local_cost(objects, facts, old)),
                _ => return Err(malformed("a list handle names another body")),
            }
        };
        let Some(Body::List(sequence)) = self.objects.get_mut(handle) else {
            return Err(malformed("a list handle names another body"));
        };
        let held = values_mut(handle, &mut sequence.chunks)?;
        let len = held.len();
        let old = match held.get_mut(index as usize) {
            Some(slot) => Some(std::mem::replace(slot, value)),
            None if index as usize == len => {
                held.push(value);
                sequence.len += 1;
                self.resident = self.resident.saturating_add(1);
                None
            }
            None => return Err(malformed("a list write past its length")),
        };
        self.hold(&value);
        if let Some(old) = old {
            self.unhold(&old);
        }
        self.moved(handle, before.unwrap_or(0), after)
    }

    /// Removes the value at `index` of a list.
    pub fn list_remove(&mut self, handle: ArenaHandle, index: u64) -> Result<(), NativeError> {
        self.warm(handle)?;
        let Some(Body::List(sequence)) = self.objects.get_mut(handle) else {
            return Err(malformed("a list handle names another body"));
        };
        let held = values_mut(handle, &mut sequence.chunks)?;
        if index as usize >= held.len() {
            return Err(malformed("a list removal past its length"));
        }
        let old = held.remove(index as usize);
        sequence.len -= 1;
        let before = local_cost(&self.objects, &self.facts, &old);
        self.moved(handle, before, 0)?;
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
            Some((at, old)) => {
                let spelled = self.entries(handle)?[at * 2];
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
        self.table_insert(handle, normal, key, value)?;
        self.moved(handle, before, after)
    }

    /// Removes the entry under `key` of the table `handle`, if any.
    pub fn table_delete(&mut self, handle: ArenaHandle, key: &Value) -> Result<(), NativeError> {
        self.warm(handle)?;
        let normal = self.key(key)?;
        let Some((at, _)) = self.table_find(handle, &normal)? else {
            return Ok(());
        };
        let pair = &self.entries(handle)?[at * 2..at * 2 + 2];
        let before = self.entry_cost(handle, &pair[0], &pair[1]);
        self.table_remove(handle, &normal)?;
        self.moved(handle, before, 0)?;
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

    /// A closure's code site and the cells it shares.
    pub fn closure(&self, handle: ArenaHandle) -> Result<(&Site, &[ArenaHandle]), NativeError> {
        self.warm(handle)?;
        match self.body(handle)? {
            Body::Closure { site, captures } => Ok((site, captures)),
            _ => Err(malformed("a closure handle names another body")),
        }
    }

    /// Allocates a closure of `site` over `captures`, a fresh mutable
    /// object; gives its handle and logical cost.
    pub fn new_closure(&mut self, site: Site, captures: Vec<ArenaHandle>) -> (ArenaHandle, u64) {
        let handle = self.insert_created(Body::Closure {
            site,
            captures: captures.into_boxed_slice(),
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
        let after = local_cost(&self.objects, &self.facts, &value);
        let Some(Body::Cell(held)) = self.objects.get_mut(handle) else {
            return Err(malformed("a cell handle names another body"));
        };
        let old = std::mem::replace(held, value);
        let before = local_cost(&self.objects, &self.facts, &old);
        self.hold(&value);
        self.unhold(&old);
        self.moved(handle, before, after)
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
