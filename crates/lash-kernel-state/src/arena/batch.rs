//! Detached nursery storage. The registration owns the uninstalled slots until
//! the accepted journal installs exactly this batch or abandons it.
use super::*;
use lash_kernel_doc::heap::{HeapFault, HostResource};

struct Entry {
    handle: ArenaHandle,
    body: Body,
    facts: Facts,
}
struct View<'a> {
    arena: &'a Arena,
    entries: &'a [Entry],
    indices: &'a HashMap<u32, usize>,
}
impl Bodies for View<'_> {
    fn get(&self, handle: ArenaHandle) -> Option<&Body> {
        self.indices
            .get(&handle.index())
            .and_then(|i| (self.entries[*i].handle == handle).then_some(&self.entries[*i].body))
            .or_else(|| self.arena.objects.get(handle))
    }
}
impl FactTable for View<'_> {
    fn get(&self, index: usize) -> Option<&Facts> {
        self.indices
            .get(&(index as u32))
            .map(|i| &self.entries[*i].facts)
            .or_else(|| self.arena.facts.get(index))
    }
}
fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<(), HeapFault> {
    values.try_reserve(additional).map_err(|_| exhausted())
}
fn exhausted() -> HeapFault {
    HeapFault::Exhausted {
        resource: HostResource::StagingBytes,
    }
}

/// Prepared bodies and indexes, with every installation buffer already reserved.
/// The matching registration must remain live until publication or abandonment.
pub struct PreparedBatch {
    identity: std::rc::Rc<()>,
    tokens: u64,
    freed: u64,
    allocations: u64,
    next_ordinal: u64,
    registration_epoch: u64,
    entries: Vec<Entry>,
    dependencies: Vec<ContentVersion>,
}
/// An exclusive preparation guard. Splitting it transfers the registration to
/// the canonical journal and the storage recipe to its infallible closure.
pub struct PreparedInstallation<'a> {
    registration: Registration<'a>,
    batch: PreparedBatch,
}
impl<'a> PreparedInstallation<'a> {
    /// Binds storage to a prospective identity in the canonical journal. The
    /// journal must grant it before publication; this method grants nothing.
    pub fn bind_entity(
        &mut self,
        handle: ArenaHandle,
        entity: EntityId,
    ) -> Result<(), NativeError> {
        let arena = self.registration.arena();
        if entity.family != lash_kernel_doc::heap::FamilyId::PRIMARY
            || entity.ordinal < self.batch.next_ordinal
            || self
                .batch
                .entries
                .iter()
                .any(|entry| entry.handle != handle && entry.facts.entity == Some(entity))
        {
            return Err(malformed("a batch identity is not fresh and unique"));
        }
        let count = self
            .batch
            .entries
            .iter()
            .filter(|entry| entry.facts.entity.is_some())
            .count()
            + 1;
        let entry = self
            .batch
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle)
            .ok_or_else(|| malformed("an identity target is absent from the batch"))?;
        if !is_mutable(&entry.body) || entry.facts.entity.is_some_and(|old| old != entity) {
            return Err(malformed("a batch identity does not name its mutable body"));
        }
        arena.entities.try_reserve(count).map_err(|_| exhausted())?;
        reserve(&mut arena.dirty, count)?;
        entry.facts.entity = Some(entity);
        Ok(())
    }
    pub fn into_parts(self) -> (Registration<'a>, PreparedBatch) {
        (self.registration, self.batch)
    }
    pub fn publish(self) {
        let (mut registration, batch) = self.into_parts();
        batch.publish(registration.arena());
        registration.commit();
    }
}
impl Arena {
    /// Builds the complete native batch without installing a guest body or
    /// changing counters. Canonical key normalization and charge summaries read
    /// a view of the existing arena plus the batch, including forward references.
    pub fn prepare_install(
        &mut self,
        new: Vec<(ArenaHandle, NewObject)>,
    ) -> Result<PreparedInstallation<'_>, NativeError> {
        let mut registration = self.registration()?;
        // Transfer the nursery's existing vector before the first fallible
        // reservation, so even host exhaustion returns every valid slot.
        registration.reserve_batch(new)?;
        Self::prepare_registered(registration)
    }

    /// Prepare a fresh shared variable without installing a body or granting
    /// its logical identity. Abandonment returns its reserved slot.
    pub fn prepare_cell(
        &mut self,
        value: Value,
    ) -> Result<(ArenaHandle, PreparedInstallation<'_>), NativeError> {
        let mut registration = self.registration()?;
        let handle = registration.reserve_body(Body::Cell(value))?;
        Ok((handle, Self::prepare_registered(registration)?))
    }

    fn prepare_registered(
        mut registration: Registration<'_>,
    ) -> Result<PreparedInstallation<'_>, NativeError> {
        let len = registration.batch_len();
        let tokens = registration.arena().tokens;
        let mut entries = Vec::new();
        reserve(&mut entries, len)?;
        let mut indices = HashMap::default();
        indices.try_reserve(len).map_err(|_| exhausted())?;
        let mut allocations = 0u64;
        for at in 0..len {
            let (handle, body) = registration.take_batch_body(at)?;
            let mutable = is_mutable(&body);
            let token = tokens
                .checked_add(entries.len() as u64 + 1)
                .ok_or_else(|| malformed("object tokens overflow"))?;
            indices.insert(handle.index(), entries.len());
            entries.push(Entry {
                handle,
                body,
                facts: Facts {
                    dirty: mutable,
                    token,
                    ..Facts::default()
                },
            });
            allocations += u64::from(mutable);
        }
        let arena = registration.arena();
        arena
            .allocations
            .checked_add(allocations)
            .ok_or_else(|| malformed("object allocations overflow"))?;
        // Immutable summaries follow dependencies, rather than reservation order.
        let mut done = Vec::new();
        reserve(&mut done, entries.len())?;
        done.resize(entries.len(), false);
        let mut remaining = entries.len();
        while remaining != 0 {
            let before = remaining;
            for at in 0..entries.len() {
                if done[at] {
                    continue;
                }
                let mut ready = true;
                let mut summary = Summary {
                    depth: 1,
                    ..Summary::default()
                };
                let immutable = matches!(entries[at].body, Body::Tuple(_) | Body::Error(_));
                each_member(&entries[at].body, &mut |value| {
                    if immutable
                        && matches!(value, Value::Tuple(_) | Value::Error(_))
                        && let Some(child) = value.handle().and_then(|h| indices.get(&h.index()))
                        && !done[*child]
                    {
                        ready = false;
                    }
                    let view = View {
                        arena,
                        entries: &entries,
                        indices: &indices,
                    };
                    let inner = value_summary(&view, value);
                    summary.depth = summary.depth.max(inner.depth.saturating_add(1));
                    summary.task_edges = summary.task_edges.saturating_add(inner.task_edges);
                    summary.has_edges |= inner.has_edges;
                });
                if ready {
                    if immutable {
                        entries[at].facts.summary = summary;
                    }
                    done[at] = true;
                    remaining -= 1;
                }
            }
            if before == remaining {
                return Err(malformed("an immutable batch contains a cycle"));
            }
        }
        // Normalize each table using the same key owner as ordinary creation.
        for at in 0..entries.len() {
            let view = View {
                arena,
                entries: &entries,
                indices: &indices,
            };
            let table = match &entries[at].body {
                Body::Map(table) | Body::Record(table) | Body::Set(table) => table,
                _ => continue,
            };
            let Some(values) = table.chunks.values() else {
                return Err(malformed("a staged table is not resident"));
            };
            let (table, index) = tables::build(
                &view,
                &view,
                &arena.layouts,
                values
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[key, value]| (*key, *value)),
            )?;
            match &mut entries[at].body {
                Body::Map(body) | Body::Record(body) | Body::Set(body) => *body = table,
                _ => unreachable!(),
            }
            entries[at].facts.index = Some(index);
        }
        let mut dependencies = Vec::new();
        for at in 0..entries.len() {
            let view = View {
                arena,
                entries: &entries,
                indices: &indices,
            };
            let mut error = None;
            each_member(&entries[at].body, &mut |value| {
                if let Some(handle) = value.handle() {
                    if Bodies::get(&view, handle).is_none_or(|body| matches!(body, Body::Free)) {
                        error = Some(malformed("a batch member names a stale object"));
                    } else if !indices.contains_key(&handle.index()) {
                        if let Some(version) = arena.content_version(handle) {
                            if let Err(fault) = reserve(&mut dependencies, 1) {
                                error = Some(fault.into());
                            } else {
                                dependencies.push(version);
                            }
                        } else {
                            error = Some(malformed("a batch member has no fresh content fact"));
                        }
                    }
                }
            });
            if let Some(error) = error {
                return Err(error);
            }
            entries[at].facts.flat = sizes::summarize_flat(&view, &entries[at].body);
        }
        let extent = entries
            .iter()
            .map(|entry| entry.handle.index() as usize + 1)
            .max()
            .unwrap_or(arena.facts.len());
        let additional = extent.saturating_sub(arena.facts.len());
        reserve(&mut arena.facts, additional)?;
        reserve(&mut arena.zero, entries.len())?;
        let batch = PreparedBatch {
            identity: std::rc::Rc::clone(&arena.identity),
            tokens: arena.tokens,
            freed: arena.freed,
            allocations,
            next_ordinal: arena.next_ordinal,
            registration_epoch: arena.registration_epoch,
            entries,
            dependencies,
        };
        Ok(PreparedInstallation {
            registration,
            batch,
        })
    }
}
impl PreparedBatch {
    /// Journal preparation may grant identities, but may not change a body,
    /// release a generation, or install another allocation before this batch.
    pub fn is_current(&self, arena: &Arena) -> bool {
        arena.registration_active()
            && self.registration_epoch == arena.registration_epoch
            && std::rc::Rc::ptr_eq(&self.identity, &arena.identity)
            && self.tokens == arena.tokens
            && self.freed == arena.freed
            && self.entries.iter().all(|entry| {
                entry
                    .facts
                    .entity
                    .is_none_or(|id| id.ordinal < arena.next_ordinal)
            })
            && arena.entities.capacity() - arena.entities.len()
                >= self
                    .entries
                    .iter()
                    .filter(|entry| entry.facts.entity.is_some())
                    .count()
            && self
                .dependencies
                .iter()
                .all(|version| arena.content_is(version))
            && arena.zero.capacity() - arena.zero.len() >= self.entries.len()
            && arena.dirty.capacity() - arena.dirty.len()
                >= self
                    .entries
                    .iter()
                    .filter(|entry| entry.facts.entity.is_some())
                    .count()
            && self
                .entries
                .iter()
                .all(|entry| (entry.handle.index() as usize) < arena.facts.capacity())
    }
    /// Called only by the matching accepted journal. No key lookup, summary
    /// rebuild, host allocation or recoverable failure remains after this check.
    pub fn publish(self, arena: &mut Arena) {
        assert!(arena.registration_active() && self.is_current(arena));
        arena.allocations += self.allocations;
        for entry in &self.entries {
            arena.resident = arena.resident.saturating_add(body_values(&entry.body));
        }
        let start = arena.zero.len();
        for mut entry in self.entries {
            if entry.facts.entity.is_some() {
                entry.facts.listed = true;
                arena.dirty.push(entry.handle);
            }
            arena.objects.install(entry.handle, entry.body);
            arena.place(entry.handle, entry.facts);
        }
        // Every forward target exists before a member count is applied.
        for handle in &arena.zero[start..] {
            let Some(body) = arena.objects.get(*handle) else {
                unreachable!("the prepared batch lost an installed target");
            };
            each_member(body, &mut |value| {
                if let Some(handle) = value.handle() {
                    let facts = &mut arena.facts[handle.index() as usize];
                    facts.count = facts.count.saturating_add(1);
                }
            });
        }
    }
}
pub(super) fn raw_body(object: NewObject) -> Result<Body, NativeError> {
    let pairs = |entries: Vec<(Value, Value)>| -> Result<_, NativeError> {
        let mut values = Vec::new();
        reserve(
            &mut values,
            entries.len().checked_mul(2).ok_or_else(exhausted)?,
        )?;
        values.extend(entries.into_iter().flat_map(|(key, value)| [key, value]));
        Ok(lash_kernel_doc::arena::Table::whole(values))
    };
    Ok(match object {
        NewObject::List(values) => Body::List(Sequence::whole(values)),
        NewObject::Tuple(values) => Body::Tuple(Sequence::whole(values)),
        NewObject::Map(entries) => Body::Map(pairs(entries)?),
        NewObject::Record(entries) => Body::Record(pairs(entries)?),
        NewObject::Set(members) => {
            let mut values = Vec::new();
            reserve(
                &mut values,
                members.len().checked_mul(2).ok_or_else(exhausted)?,
            )?;
            values.extend(members.into_iter().flat_map(|value| [value, Value::Absent]));
            Body::Set(lash_kernel_doc::arena::Table::whole(values))
        }
        NewObject::LongText(text) => Body::LongText(resident_text(text.as_ref().into())),
        NewObject::Bytes(bytes) => Body::Bytes(Bytes {
            len: bytes.len() as u64,
            chunks: Box::new([Chunk::Resident(bytes.as_ref().into())]),
        }),
        NewObject::BigInt(integer) => Body::BigInt(integer),
        NewObject::Timestamp(integer) => Body::Timestamp(integer),
        NewObject::Function(callable) => Body::Function(Box::new(callable)),
        NewObject::Error {
            kind,
            message,
            data,
        } => Body::Error(ErrorBody {
            kind,
            message,
            data,
        }),
    })
}
