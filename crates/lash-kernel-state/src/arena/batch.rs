//! Detached nursery storage. The registration owns the uninstalled slots until
//! the accepted journal installs exactly this batch or abandons it.
use super::*;
use crate::paged::logical::{Holding, RootValue, VirtualBody, VirtualEntity};
use crate::paged::tree::Access;
use lash_kernel_doc::heap::Identity;
use lash_kernel_doc::heap::{HeapFault, HostResource};
use lash_kernel_doc::{LogicalHeldV1 as Cost, NativeResourceHolding, NativeResourceOrigin};

/// Immutable observations of one detached installation. Only the storage
/// owner can build these facts; clones retain the same batch authority.
#[derive(Clone)]
pub struct PreparedGraphProof(std::rc::Rc<GraphFacts>);
struct InstalledFact {
    handle: ArenaHandle,
    token: u64,
    revision: u64,
}
struct GraphFacts {
    published: std::cell::Cell<bool>,
    installed: Vec<InstalledFact>,
    arena: std::rc::Rc<()>,
    epoch: u64,
    tokens: u64,
    freed: u64,
    dependencies: Vec<ContentVersion>,
    aliases: Vec<(ArenaHandle, EntityId)>,
    entities: Vec<VirtualEntity>,
    bodies: Vec<Holding>,
    members: Vec<Option<NativeMembers>>,
    handles: Vec<ArenaHandle>,
    values: Vec<ValueFacts>,
}
type NativeMembers = (lash_kernel_doc::NativeResourceEntity, Vec<Occurrence>);
#[derive(Clone, Copy)]
struct Occurrence {
    bytes: u64,
    entity: Option<EntityId>,
    alias: bool,
}
impl Occurrence {
    fn native(
        &self,
        owners: &std::collections::BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> NativeResourceHolding {
        NativeResourceHolding {
            bytes: self.bytes,
            origin: if self.alias {
                self.entity
                    .and_then(|id| owners.get(&id).copied())
                    .map_or(NativeResourceOrigin::Alias, NativeResourceOrigin::Fresh)
            } else {
                NativeResourceOrigin::Immutable
            },
        }
    }
}
struct ValueFacts {
    value: Value,
    holding: Holding,
    native: Occurrence,
    depth: u32,
    collection: Option<NewObject>,
    literal: Option<lash_kernel_doc::Literal>,
    callable: Option<lash_kernel_value::names::CallableId>,
    closure: Option<(
        std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
        Vec<ArenaHandle>,
    )>,
}
/// An exact output occurrence of a storage-owned detached graph.
#[derive(Clone)]
pub struct PreparedValueProof {
    graph: PreparedGraphProof,
    index: usize,
}
impl PreparedGraphProof {
    /// Existing alias registration may consume the index capacity reserved
    /// while binding fresh bodies. Finish that reservation before commit.
    pub fn reserve_for_publication(&self, arena: &mut Arena) -> Result<(), HeapFault> {
        if !self.is_current(arena) {
            return Err(proof_fault("stale prepared graph capacity reservation"));
        }
        arena
            .entities
            .try_reserve(self.0.entities.len())
            .map_err(|_| exhausted())
    }
    pub fn is_current(&self, arena: &Arena) -> bool {
        arena.registration_active()
            && std::rc::Rc::ptr_eq(&self.0.arena, &arena.identity)
            && self.0.epoch == arena.registration_epoch
            && self.0.tokens == arena.tokens
            && self.0.freed == arena.freed
            && self
                .0
                .dependencies
                .iter()
                .all(|version| arena.content_is(version))
            && self.0.aliases.iter().all(|(handle, entity)| {
                arena
                    .facts(*handle)
                    .is_ok_and(|facts| facts.ledger_entity() == Some(*entity))
            })
    }
    pub fn same(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.0, &other.0)
    }
    pub fn entities(&self) -> &[VirtualEntity] {
        &self.0.entities
    }
    pub fn body(&self, index: usize) -> &Holding {
        &self.0.bodies[index]
    }
    pub fn native_members(
        &self,
        index: usize,
        owners: &std::collections::BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> Result<
        (
            lash_kernel_doc::NativeResourceEntity,
            Vec<NativeResourceHolding>,
        ),
        HeapFault,
    > {
        let (kind, members) = self.0.members[index]
            .as_ref()
            .ok_or_else(|| proof_fault("prepared body has no native resource declaration"))?;
        Ok((
            *kind,
            members.iter().map(|member| member.native(owners)).collect(),
        ))
    }
    pub fn dependencies(&self) -> &[ArenaHandle] {
        &self.0.handles
    }
    pub fn value(&self, index: usize) -> PreparedValueProof {
        assert!(index < self.0.values.len());
        PreparedValueProof {
            graph: self.clone(),
            index,
        }
    }
    pub fn len(&self) -> usize {
        self.0.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.values.is_empty()
    }
}
impl PreparedValueProof {
    /// A paid source receipt may restore a native value only after this exact
    /// batch installed it. Representation cooling preserves content versions.
    pub fn restore_value(&self, arena: &Arena) -> Result<Value, HeapFault> {
        let graph = &self.graph.0;
        if !graph.published.get()
            || !std::rc::Rc::ptr_eq(&graph.arena, &arena.identity)
            || !graph
                .dependencies
                .iter()
                .all(|version| arena.content_is(version))
            || !graph.installed.iter().all(|version| {
                arena.facts(version.handle).is_ok_and(|facts| {
                    facts.token == version.token && facts.content_revision == version.revision
                })
            })
        {
            return Err(proof_fault("unpublished or stale prepared recovery value"));
        }
        Ok(self.value())
    }
    pub fn graph(&self) -> &PreparedGraphProof {
        &self.graph
    }
    pub fn value(&self) -> Value {
        self.graph.0.values[self.index].value
    }
    pub fn same(&self, other: &Self) -> bool {
        self.index == other.index && self.graph.same(&other.graph)
    }
    pub fn holding(&self, arena: &Arena) -> Result<Holding, HeapFault> {
        if !self.graph.is_current(arena) {
            return Err(HeapFault::Malformed(
                "stale or foreign prepared value proof".into(),
            ));
        }
        Ok(self.graph.0.values[self.index].holding.clone())
    }
    pub fn native_holding(
        &self,
        owners: &std::collections::BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> NativeResourceHolding {
        self.graph.0.values[self.index].native.native(owners)
    }
    pub fn entity(&self) -> Option<EntityId> {
        self.graph.0.values[self.index].native.entity
    }
    pub fn depth(&self) -> u32 {
        self.graph.0.values[self.index].depth
    }
    pub fn collection(&self) -> Option<&NewObject> {
        self.graph.0.values[self.index].collection.as_ref()
    }
    pub fn callable(&self) -> Option<lash_kernel_value::names::CallableId> {
        self.graph.0.values[self.index].callable
    }
    pub fn literal(&self) -> Option<&lash_kernel_doc::Literal> {
        self.graph.0.values[self.index].literal.as_ref()
    }
    pub fn closure(&self) -> Option<(&lash_kernel_doc::arena::ClosureIdentity, &[ArenaHandle])> {
        self.graph.0.values[self.index]
            .closure
            .as_ref()
            .map(|(identity, captures)| (identity.as_ref(), captures.as_slice()))
    }
}

/// Batch members by arena slot index. Slots are reused, so the keys are
/// sparse; the crate's fast hasher replaces SipHash for these u32 keys.
type Slots<V> = HashMap<u32, V, foldhash::fast::RandomState>;

pub(super) struct Entry {
    pub(super) handle: ArenaHandle,
    body: Body,
    pub(super) facts: Facts,
}
struct View<'a> {
    arena: &'a Arena,
    entries: &'a [Entry],
    indices: &'a Slots<usize>,
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
    proof: Option<PreparedGraphProof>,
    identity: std::rc::Rc<()>,
    tokens: u64,
    freed: u64,
    allocations: u64,
    next_provisional: u64,
    registration_epoch: u64,
    pub(super) entries: Vec<Entry>,
    dependencies: Vec<ContentVersion>,
    empty_tuple: Option<(Option<ArenaHandle>, ArenaHandle)>,
}
/// An exclusive preparation guard. Splitting it transfers the registration to
/// the canonical journal and the storage recipe to its infallible closure.
pub struct PreparedInstallation<'a> {
    holdings: Slots<Holding>,
    observed: Vec<ArenaHandle>,
    pub(super) registration: Registration<'a>,
    pub(super) batch: PreparedBatch,
}
impl Bodies for PreparedInstallation<'_> {
    fn get(&self, handle: ArenaHandle) -> Option<&Body> {
        self.batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
            .map(|entry| &entry.body)
            .or_else(|| self.registration.read_arena().objects.get(handle))
    }
}
impl FactTable for PreparedInstallation<'_> {
    fn get(&self, index: usize) -> Option<&Facts> {
        self.batch
            .entries
            .iter()
            .find(|entry| entry.handle.index() as usize == index)
            .map(|entry| &entry.facts)
            .or_else(|| self.registration.read_arena().facts.get(index))
    }
}
impl Arena {
    /// Stage the actual code field key and the complete table edit together.
    pub fn prepare_field_member(
        &mut self,
        handle: ArenaHandle,
        name: std::sync::Arc<str>,
        next: Option<Value>,
    ) -> Result<Option<(PreparedInstallation<'_>, PreparedTableMember)>, HeapFault> {
        let Some(mut patch) = self.prepare_table_patch(
            handle,
            crate::paged::logical::Member::Field(name.clone()),
            None,
            next,
            Key::of_text(&name),
        )?
        else {
            return Ok(None);
        };
        if !patch.is_record() {
            return Ok(None);
        }
        let flat = self
            .facts(handle)
            .map_err(|error| proof_fault(&error.to_string()))?
            .flat;
        let old = patch.old_key();
        let mut nursery = lash_kernel_doc::Nursery::unaccounted(self.objects());
        let key = old.or_else(|| {
            next.map(|_| match ShortText::new(&name) {
                Some(short) => Value::Text(short),
                None => nursery.allocate(NewObject::LongText(name)),
            })
        });
        let mut installation =
            self.prepare_install(nursery.into_new())
                .map_err(|error| match error {
                    NativeError::Host(error) => error,
                    error => proof_fault(&error.to_string()),
                })?;
        patch.bind_field_key(key);
        patch.seal_flat(&installation, flat);
        let count = installation.batch.entries.len();
        patch.reserve(installation.registration.arena(), count)?;
        Ok(Some((installation, patch)))
    }
}
impl<'a> PreparedInstallation<'a> {
    /// Reads a detached body or an existing dependency without installing it.
    /// The exclusive registration keeps every returned generation stable.
    pub fn body(&self, handle: ArenaHandle) -> Result<&Body, NativeError> {
        if let Some(entry) = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
        {
            return Ok(&entry.body);
        }
        self.registration.read_arena().body(handle)
    }

    /// The canonical immutable descriptor, if projection already produced it.
    pub fn page(&mut self, handle: ArenaHandle) -> Result<Option<PageValue>, NativeError> {
        if let Some(entry) = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
        {
            return Ok(entry.facts.page.get().cloned());
        }
        Ok(self.registration.arena().facts(handle)?.page.get().cloned())
    }

    /// A new mutable body must already be bound to its prospective journal
    /// key. Existing dependencies take their ledger keys under this
    /// registration.
    pub fn ledger_key(&mut self, handle: ArenaHandle) -> Result<EntityId, NativeError> {
        if let Some(entry) = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
        {
            return entry
                .facts
                .provisional
                .ok_or_else(|| malformed("a prepared body has no journal key"));
        }
        self.registration.arena().ledger_key(handle)
    }

    /// Canonical integer interpretation over detached and existing bodies.
    pub fn integer(&self, value: &Value) -> Result<Option<lash_kernel_doc::Integer>, NativeError> {
        keys::integer(self, value)
    }
    /// Resident sequence members, using the ordinary chunk reader.
    pub fn sequence(&self, handle: ArenaHandle) -> Result<&[Value], NativeError> {
        if let Some(entry) = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
        {
            return match &entry.body {
                Body::List(sequence) | Body::Tuple(sequence) => {
                    ops::values(handle, &sequence.chunks)
                }
                _ => Err(malformed("a sequence handle names another body")),
            };
        }
        self.registration.read_arena().sequence(handle)
    }
    /// Normalized table pairs, using the ordinary canonical table reader.
    pub fn entries(&self, handle: ArenaHandle) -> Result<&[Value], NativeError> {
        if let Some(entry) = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
        {
            return match &entry.body {
                Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                    tables::pairs(handle, table)
                }
                _ => Err(malformed("a table handle names another body")),
            };
        }
        self.registration.read_arena().entries(handle)
    }
    /// The exact collection extent; existing cold dependencies retain their reader.
    pub fn len(&self, handle: ArenaHandle) -> Result<u64, NativeError> {
        if let Some(entry) = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
        {
            return match &entry.body {
                Body::List(sequence) | Body::Tuple(sequence) => Ok(sequence.len),
                Body::Map(table) | Body::Set(table) | Body::Record(table) => Ok(table.len),
                _ => Err(malformed("a collection handle names another body")),
            };
        }
        self.registration.read_arena().len(handle)
    }
    /// Copies the normalized collection recipe from detached storage. Tables
    /// retain canonical key spelling, insertion order and last-write values.
    pub fn collection(&self, handle: ArenaHandle) -> Result<NewObject, NativeError> {
        let entry = self
            .batch
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
            .ok_or_else(|| malformed("a collection target is absent from the batch"))?;
        match &entry.body {
            Body::List(sequence) => Ok(NewObject::List(
                sequence
                    .chunks
                    .values()
                    .ok_or_else(|| malformed("a prepared list is not resident"))?
                    .to_vec(),
            )),
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                let values = tables::pairs(handle, table)?;
                if matches!(entry.body, Body::Set(_)) {
                    return Ok(NewObject::Set(
                        values
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|[key, _]| *key)
                            .collect(),
                    ));
                }
                let entries = values
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[key, value]| (*key, *value))
                    .collect();
                Ok(if matches!(entry.body, Body::Record(_)) {
                    NewObject::Record(entries)
                } else {
                    NewObject::Map(entries)
                })
            }
            _ => Err(malformed("a prepared body is not a collection")),
        }
    }
    /// Binds storage to a prospective identity in the canonical journal. The
    /// journal must grant it before publication; this method grants nothing.
    pub fn bind_entity(
        &mut self,
        handle: ArenaHandle,
        entity: EntityId,
    ) -> Result<(), NativeError> {
        if self.batch.proof.is_some() {
            return Err(malformed("a sealed batch cannot change identities"));
        }
        let arena = self.registration.arena();
        if entity.family != lash_kernel_doc::heap::FamilyId::PROVISIONAL
            || entity.ordinal < self.batch.next_provisional
            || self
                .batch
                .entries
                .iter()
                .any(|entry| entry.handle != handle && entry.facts.provisional == Some(entity))
        {
            return Err(malformed("a batch identity is not fresh and unique"));
        }
        let count = self
            .batch
            .entries
            .iter()
            .filter(|entry| entry.facts.provisional.is_some())
            .count()
            + 1;
        let entry = self
            .batch
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle)
            .ok_or_else(|| malformed("an identity target is absent from the batch"))?;
        if !is_mutable(&entry.body) || entry.facts.provisional.is_some_and(|old| old != entity) {
            return Err(malformed("a batch identity does not name its mutable body"));
        }
        arena.entities.try_reserve(count).map_err(|_| exhausted())?;
        entry.facts.provisional = Some(entity);
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

    /// Prepare the live shared empty tuple, including its cache update and
    /// generation dependency, for the accepted batch's infallible installation.
    pub fn prepare_empty_tuple(
        &mut self,
    ) -> Result<(Value, PreparedInstallation<'_>), NativeError> {
        let cached = self.resident_empty_tuple();
        let expected = self.empty_tuple;
        let mut registration = self.registration()?;
        let (handle, dependency) = if let Some(Value::Tuple(handle)) = cached {
            let dependency = registration
                .arena()
                .content_version(handle)
                .ok_or_else(|| malformed("the shared empty tuple has no content version"))?;
            (handle, Some(dependency))
        } else {
            (
                registration.reserve_body(Body::Tuple(Sequence::whole(Vec::new())))?,
                None,
            )
        };
        let mut installation = Self::prepare_registered(registration)?;
        if let Some(dependency) = dependency {
            reserve(&mut installation.batch.dependencies, 1)?;
            installation.batch.dependencies.push(dependency);
        }
        installation.batch.empty_tuple = Some((expected, handle));
        Ok((Value::Tuple(handle), installation))
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

    /// Prepare every shared parameter cell in one atomic installation.
    pub fn prepare_cells(
        &mut self,
        values: &[Value],
    ) -> Result<(Vec<ArenaHandle>, PreparedInstallation<'_>), NativeError> {
        let mut registration = self.registration()?;
        let handles = registration.reserve_cells(values)?;
        Ok((handles, Self::prepare_registered(registration)?))
    }

    /// Prepare a closure and its actual capture cells before source acceptance.
    /// The local code cache is implementation state, installed with the body.
    pub fn prepare_closure(
        &mut self,
        identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
        captures: Vec<ArenaHandle>,
        code: lash_kernel_doc::arena::LocalCode,
    ) -> Result<(ArenaHandle, PreparedInstallation<'_>), NativeError> {
        let mut registration = self.registration()?;
        let handle = registration.reserve_body(Body::Closure {
            identity,
            captures: captures.into_boxed_slice(),
            code: Some(code),
        })?;
        Ok((handle, Self::prepare_registered(registration)?))
    }

    pub(super) fn prepare_registered(
        mut registration: Registration<'_>,
    ) -> Result<PreparedInstallation<'_>, NativeError> {
        let len = registration.batch_len();
        let tokens = registration.arena().tokens;
        let mut entries = Vec::new();
        reserve(&mut entries, len)?;
        let mut indices = Slots::default();
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
            if let Body::Closure { captures, .. } = &entries[at].body
                && captures
                    .iter()
                    .any(|handle| !matches!(Bodies::get(&view, *handle), Some(Body::Cell(_))))
            {
                return Err(malformed("a closure capture is not a variable cell"));
            }
            each_handle(&entries[at].body, &mut |handle| {
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
            proof: None,
            identity: std::rc::Rc::clone(&arena.identity),
            tokens: arena.tokens,
            freed: arena.freed,
            allocations,
            next_provisional: arena.next_provisional().ordinal,
            registration_epoch: arena.registration_epoch,
            entries,
            dependencies,
            empty_tuple: None,
        };
        Ok(PreparedInstallation {
            holdings: Slots::default(),
            observed: Vec::new(),
            registration,
            batch,
        })
    }
}
fn proof_fault(reason: &str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}

impl PreparedInstallation<'_> {
    pub fn values_fit_depth(&self, values: &[Value], limit: usize) -> bool {
        self.batch
            .entries
            .iter()
            .map(|entry| naming(&entry.body, entry.handle))
            .chain(values.iter().copied())
            .all(|value| value_summary(self, &value).depth as usize <= limit)
    }
    /// Seal the exact detached view. Every allocation is validated, including
    /// bodies that no output reaches. Mutable ordinals follow reservation order.
    pub fn prove_values(
        &mut self,
        pages: &Access<'_>,
        values: &[Value],
        depth_limit: usize,
        literals: bool,
    ) -> Result<PreparedGraphProof, HeapFault> {
        self.prove_region_values(pages, values, depth_limit, literals, None)
    }
    pub(crate) fn prove_region_values(
        &mut self,
        pages: &Access<'_>,
        values: &[Value],
        depth_limit: usize,
        literals: bool,
        region: Option<(&[(ArenaHandle, u64)], u64)>,
    ) -> Result<PreparedGraphProof, HeapFault> {
        if self.batch.proof.is_some() {
            return Err(proof_fault("a batch proof is already sealed"));
        }
        if let Some((_, count)) = region {
            self.registration
                .reserve_region_identities(count)
                .map_err(|error| proof_fault(&error.to_string()))?;
            self.registration
                .read_arena()
                .allocations
                .checked_add(count)
                .ok_or_else(|| proof_fault("region allocation counter overflow"))?;
            self.batch.allocations = count;
        }
        let mutable: Vec<_> = self
            .batch
            .entries
            .iter()
            .filter(|entry| is_mutable(&entry.body))
            .map(|entry| entry.handle)
            .collect();
        for (index, handle) in mutable.iter().enumerate() {
            let entity = EntityId {
                family: lash_kernel_doc::heap::FamilyId::PROVISIONAL,
                ordinal: self
                    .batch
                    .next_provisional
                    .checked_add(if let Some((ordinals, _)) = region {
                        ordinals
                            .iter()
                            .find(|(target, _)| target == handle)
                            .ok_or_else(|| proof_fault("region omitted a fresh identity"))?
                            .1
                    } else {
                        index as u64
                    })
                    .ok_or_else(|| proof_fault("batch identity overflow"))?,
            };
            if region.is_none()
                && self
                    .registration
                    .reserve_entity()
                    .map_err(|error| proof_fault(&error.to_string()))?
                    != entity
            {
                return Err(proof_fault("prepared allocation ordinals changed"));
            }
            self.bind_entity(*handle, entity)
                .map_err(|error| proof_fault(&error.to_string()))?;
        }
        let allocations: Vec<_> = self
            .batch
            .entries
            .iter()
            .map(|entry| naming(&entry.body, entry.handle))
            .collect();
        for value in &allocations {
            if value_summary(self, value).depth as usize > depth_limit {
                return Err(proof_fault("prepared value exceeds the depth limit"));
            }
            self.proof_holding(pages, value)?;
        }
        let mut entities = Vec::new();
        let mut bodies = Vec::new();
        let mut members = Vec::new();
        for handle in mutable {
            let entity = self
                .ledger_key(handle)
                .map_err(|error| proof_fault(&error.to_string()))?;
            let body = match self
                .body(handle)
                .map_err(|error| proof_fault(&error.to_string()))?
            {
                Body::List(_) => match self
                    .collection(handle)
                    .map_err(|error| proof_fault(&error.to_string()))?
                {
                    NewObject::List(values) => VirtualBody::NativeList(values),
                    _ => unreachable!(),
                },
                Body::Map(_) => match self
                    .collection(handle)
                    .map_err(|error| proof_fault(&error.to_string()))?
                {
                    NewObject::Map(values) => VirtualBody::NativeMap(values),
                    _ => unreachable!(),
                },
                Body::Set(_) => match self
                    .collection(handle)
                    .map_err(|error| proof_fault(&error.to_string()))?
                {
                    NewObject::Set(values) => VirtualBody::NativeSet(values),
                    _ => unreachable!(),
                },
                Body::Record(_) => match self
                    .collection(handle)
                    .map_err(|error| proof_fault(&error.to_string()))?
                {
                    NewObject::Record(values) => VirtualBody::NativeRecord(values),
                    _ => unreachable!(),
                },
                Body::Cell(value) => VirtualBody::NativeVariable(*value),
                Body::Closure { captures, .. } => {
                    let captures = captures.to_vec();
                    VirtualBody::Closure(
                        captures
                            .into_iter()
                            .map(|handle| {
                                self.ledger_key(handle)
                                    .map_err(|error| proof_fault(&error.to_string()))
                            })
                            .collect::<Result<_, _>>()?,
                    )
                }
                _ => return Err(proof_fault("a mutable batch body changed kind")),
            };
            let (holding, native) = self.proof_body(pages, &body)?;
            entities.push(VirtualEntity { entity, body });
            bodies.push(holding);
            members.push(native);
        }
        let mut outputs = Vec::new();
        for value in values {
            if value_summary(self, value).depth as usize > depth_limit {
                return Err(proof_fault("prepared output exceeds the depth limit"));
            }
            let holding = self.proof_holding(pages, value)?;
            let native = self.proof_occurrence(value, holding.bytes())?;
            let collection = match value {
                Value::Tuple(handle) => {
                    if self
                        .len(*handle)
                        .map_err(|error| proof_fault(&error.to_string()))?
                        == 0
                    {
                        Some(NewObject::Tuple(Vec::new()))
                    } else {
                        self.sequence(*handle)
                            .ok()
                            .map(|values| NewObject::Tuple(values.to_vec()))
                    }
                }
                Value::List(handle)
                | Value::Map(handle)
                | Value::Set(handle)
                | Value::Record(handle)
                    if self
                        .batch
                        .entries
                        .iter()
                        .any(|entry| entry.handle == *handle) =>
                {
                    Some(
                        self.collection(*handle)
                            .map_err(|error| proof_fault(&error.to_string()))?,
                    )
                }
                _ => None,
            };
            let closure = if let Value::Closure(handle) = value {
                if let Body::Closure {
                    identity, captures, ..
                } = self
                    .body(*handle)
                    .map_err(|error| proof_fault(&error.to_string()))?
                {
                    Some((identity.clone(), captures.to_vec()))
                } else {
                    return Err(proof_fault("a closure proof names another body"));
                }
            } else {
                None
            };
            outputs.push(ValueFacts {
                value: *value,
                holding,
                native,
                depth: value_summary(self, value).depth,
                collection,
                literal: if literals {
                    self.proof_literal(value)?
                } else {
                    None
                },
                callable: if let Value::Function(handle) = value {
                    match self
                        .body(*handle)
                        .map_err(|error| proof_fault(&error.to_string()))?
                    {
                        Body::Function(callable) => Some(**callable),
                        _ => None,
                    }
                } else {
                    None
                },
                closure,
            });
        }
        let mut handles = Vec::new();
        for entry in &self.batch.entries {
            each_handle(&entry.body, &mut |handle| {
                if !self
                    .batch
                    .entries
                    .iter()
                    .any(|entry| entry.handle == handle)
                {
                    handles.push(handle);
                }
            });
        }
        handles.extend_from_slice(&self.observed);
        handles.extend(values.iter().filter_map(Value::handle).filter(|handle| {
            !self
                .batch
                .entries
                .iter()
                .any(|entry| entry.handle == *handle)
        }));
        let mut dependencies = self.batch.dependencies.clone();
        let mut aliases = Vec::new();
        for handle in &handles {
            let arena = self.registration.read_arena();
            dependencies.push(
                arena
                    .content_version(*handle)
                    .ok_or_else(|| proof_fault("prepared dependency is stale"))?,
            );
            if let Some(entity) = arena.facts(*handle).ok().and_then(Facts::ledger_entity) {
                aliases.push((*handle, entity));
            }
        }
        let arena = self.registration.read_arena();
        let installed = self
            .batch
            .entries
            .iter()
            .map(|entry| InstalledFact {
                handle: entry.handle,
                token: entry.facts.token,
                revision: entry.facts.content_revision,
            })
            .collect();
        let graph = PreparedGraphProof(std::rc::Rc::new(GraphFacts {
            published: std::cell::Cell::new(false),
            installed,
            arena: std::rc::Rc::clone(&arena.identity),
            epoch: arena.registration_epoch,
            tokens: arena.tokens,
            freed: arena.freed,
            dependencies,
            aliases,
            entities,
            bodies,
            members,
            values: outputs,
            handles,
        }));
        graph.reserve_for_publication(self.registration.arena())?;
        self.batch.proof = Some(graph.clone());
        Ok(graph)
    }
    fn observe_value(&mut self, value: &Value) -> Result<(), HeapFault> {
        let mut pending = vec![*value];
        while let Some(value) = pending.pop() {
            let Some(handle) = value.handle() else {
                continue;
            };
            if self.observed.contains(&handle) {
                continue;
            }
            self.observed.push(handle);
            let arena = self.registration.read_arena();
            let body = arena
                .body(handle)
                .map_err(|error| proof_fault(&error.to_string()))?;
            if matches!(value, Value::Tuple(_) | Value::Error(_))
                && arena
                    .facts(handle)
                    .map_err(|error| proof_fault(&error.to_string()))?
                    .page
                    .get()
                    .is_none()
            {
                each_member(body, &mut |value| pending.push(*value));
            }
        }
        Ok(())
    }
    fn proof_occurrence(&mut self, value: &Value, bytes: u64) -> Result<Occurrence, HeapFault> {
        let alias = matches!(
            value,
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) | Value::Closure(_)
        );
        let entity = if alias || matches!(value, Value::ObjectRef(_)) {
            Some(
                self.ledger_key(
                    value
                        .handle()
                        .ok_or_else(|| proof_fault("prepared entity has no handle"))?,
                )
                .map_err(|error| proof_fault(&error.to_string()))?,
            )
        } else {
            None
        };
        Ok(Occurrence {
            bytes,
            entity,
            alias,
        })
    }
    fn proof_holding(&mut self, pages: &Access<'_>, value: &Value) -> Result<Holding, HeapFault> {
        let Some(handle) = value.handle() else {
            return match value {
                Value::Task(task) | Value::TaskRef(task) => {
                    Holding::with_edges(Cost::REFERENCE, [Identity::Task(*task)], pages)
                }
                _ => Ok(Holding::scalar(
                    self.registration.read_arena().logical_local(value)?,
                )),
            };
        };
        if matches!(value, Value::Tuple(_) | Value::Error(_))
            && let Some(holding) = self.holdings.get(&handle.index())
        {
            return Ok(holding.clone());
        }
        let detached = self
            .batch
            .entries
            .iter()
            .any(|entry| entry.handle == handle);
        let body = self
            .body(handle)
            .map_err(|error| proof_fault(&error.to_string()))?;
        let valid = matches!(
            (value, body),
            (Value::List(_), Body::List(_))
                | (Value::Map(_), Body::Map(_))
                | (Value::Set(_), Body::Set(_))
                | (Value::Record(_), Body::Record(_))
                | (Value::Closure(_), Body::Closure { .. })
                | (
                    Value::ObjectRef(_),
                    Body::Cell(_)
                        | Body::List(_)
                        | Body::Map(_)
                        | Body::Set(_)
                        | Body::Record(_)
                        | Body::Closure { .. }
                )
                | (Value::Tuple(_), Body::Tuple(_))
                | (Value::Error(_), Body::Error(_))
                | (Value::BigInt(_), Body::BigInt(_))
                | (Value::BigTimestamp(_), Body::Timestamp(_))
                | (Value::LongText(_), Body::LongText(_))
                | (Value::Bytes(_), Body::Bytes(_))
                | (Value::Function(_), Body::Function(_))
                | (Value::Handle(_), Body::Handle(_))
        );
        if !valid {
            return Err(proof_fault("prepared value has another body kind"));
        }
        if !detached {
            self.observe_value(value)?;
            return Holding::from_native(self.registration.arena(), pages, value);
        }
        if is_mutable(body) {
            let entity = self
                .ledger_key(handle)
                .map_err(|error| proof_fault(&error.to_string()))?;
            return Holding::with_edges(Cost::REFERENCE, [Identity::Object(entity)], pages);
        }
        let mut children = Vec::new();
        let base = match body {
            Body::BigInt(integer) | Body::Timestamp(integer) => {
                Cost::REFERENCE.checked_add(integer.bits().div_ceil(8))
            }
            Body::LongText(text) => Cost::REFERENCE.checked_add(text.summary.utf8_bytes),
            Body::Bytes(bytes) => Cost::REFERENCE.checked_add(bytes.len),
            Body::Function(_) => Cost::REFERENCE.checked_add(64),
            Body::Handle(handle) => Cost::REFERENCE
                .checked_add(handle.kind.len() as u64)
                .and_then(|bytes| bytes.checked_add(handle.id.len() as u64)),
            Body::Tuple(sequence) => {
                children.extend_from_slice(
                    ops::values(handle, &sequence.chunks)
                        .map_err(|error| proof_fault(&error.to_string()))?,
                );
                Some(Cost::REFERENCE)
            }
            Body::Error(error) => {
                if !matches!(error.kind, Value::Text(_) | Value::LongText(_))
                    || !matches!(error.message, Value::Text(_) | Value::LongText(_))
                {
                    return Err(proof_fault("prepared error text is malformed"));
                }
                children.extend([error.kind, error.message, error.data]);
                Some(Cost::REFERENCE)
            }
            _ => return Err(proof_fault("prepared immutable has another body kind")),
        }
        .ok_or_else(|| proof_fault("prepared occurrence cost overflow"))?;
        let error = matches!(value, Value::Error(_));
        let mut bytes = base;
        let mut edges = Vec::new();
        for (index, child) in children.iter().enumerate() {
            let holding = self.proof_holding(pages, child)?;
            bytes = bytes
                .checked_add(
                    holding
                        .bytes()
                        .checked_sub(if error && index < 2 {
                            Cost::REFERENCE
                        } else {
                            0
                        })
                        .ok_or_else(|| proof_fault("prepared error cost underflow"))?,
                )
                .ok_or_else(|| proof_fault("prepared occurrence cost overflow"))?;
            holding.visit_edges(pages, &mut |identity, count| {
                edges.push((identity, count));
                Ok(())
            })?;
        }
        let holding = RootValue::prepared_holding(bytes, edges)?;
        if matches!(value, Value::Tuple(_) | Value::Error(_)) {
            self.holdings.insert(handle.index(), holding.clone());
        }
        Ok(holding)
    }
    fn proof_body(
        &mut self,
        pages: &Access<'_>,
        body: &VirtualBody,
    ) -> Result<(Holding, Option<NativeMembers>), HeapFault> {
        use lash_kernel_doc::NativeResourceEntity as Kind;
        let mut values = Vec::new();
        let (kind, fields) = match body {
            VirtualBody::NativeList(members) => {
                values.extend_from_slice(members);
                (Kind::List, 0)
            }
            VirtualBody::NativeMap(entries) => {
                values.extend(entries.iter().flat_map(|(key, value)| [*key, *value]));
                (Kind::Map, 0)
            }
            VirtualBody::NativeSet(members) => {
                values.extend(members.iter().flat_map(|key| [*key, Value::Absent]));
                (Kind::Set, 0)
            }
            VirtualBody::NativeRecord(entries) => {
                let mut names = 0u64;
                for (key, value) in entries {
                    names = names
                        .checked_add(
                            keys::text(self, key)
                                .map_err(|error| proof_fault(&error.to_string()))?
                                .len() as u64,
                        )
                        .ok_or_else(|| proof_fault("record name overflow"))?;
                    values.extend([*key, *value]);
                }
                (
                    Kind::Record {
                        field_name_bytes: names,
                    },
                    entries.len() as u64,
                )
            }
            VirtualBody::NativeVariable(value) => {
                values.push(*value);
                (Kind::List, 0)
            }
            VirtualBody::Closure(captures) => {
                return Ok((
                    Holding::with_edges(
                        Cost::OBJECT
                            .checked_add(
                                (captures.len() as u64)
                                    .checked_mul(Cost::ENTITY_EDGE)
                                    .ok_or_else(|| proof_fault("capture cost overflow"))?,
                            )
                            .ok_or_else(|| proof_fault("closure cost overflow"))?,
                        captures.iter().copied().map(Identity::Object),
                        pages,
                    )?,
                    None,
                ));
            }
            _ => return Err(proof_fault("prepared body lost native provenance")),
        };
        let mut bytes = Cost::OBJECT;
        let mut edges = Vec::new();
        let mut members = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let holding = self.proof_holding(pages, value)?;
            bytes = bytes
                .checked_add(holding.bytes())
                .ok_or_else(|| proof_fault("prepared body cost overflow"))?;
            holding.visit_edges(pages, &mut |identity, count| {
                edges.push((identity, count));
                Ok(())
            })?;
            if fields == 0 || index % 2 != 0 {
                members.push(self.proof_occurrence(value, holding.bytes())?);
            }
        }
        bytes = bytes
            .checked_sub(
                fields
                    .checked_mul(Cost::REFERENCE)
                    .ok_or_else(|| proof_fault("record cost overflow"))?,
            )
            .ok_or_else(|| proof_fault("record cost underflow"))?;
        Ok((
            RootValue::prepared_holding(bytes, edges)?,
            if matches!(body, VirtualBody::NativeVariable(_)) {
                None
            } else {
                Some((kind, members))
            },
        ))
    }
    fn proof_literal(&self, value: &Value) -> Result<Option<lash_kernel_doc::Literal>, HeapFault> {
        use lash_kernel_doc::Literal as L;
        Ok(Some(match value {
            Value::Null => L::Null,
            Value::Absent => L::Absent,
            Value::Bool(value) => L::Bool(*value),
            Value::Float(value) => L::Float(*value),
            Value::Int(value) => L::Int((*value).into()),
            Value::Text(value) => L::Text(value.as_str().into()),
            Value::BigInt(handle) => match self
                .body(*handle)
                .map_err(|error| proof_fault(&error.to_string()))?
            {
                Body::BigInt(value) => L::Int(value.clone()),
                _ => return Err(proof_fault("literal integer has another body")),
            },
            Value::LongText(handle) => match self
                .body(*handle)
                .map_err(|error| proof_fault(&error.to_string()))?
            {
                Body::LongText(text) => match &*text.chunks {
                    [Chunk::Resident(text)] => L::Text(text.to_string()),
                    _ => return Err(proof_fault("literal text is cold")),
                },
                _ => return Err(proof_fault("literal text has another body")),
            },
            Value::Bytes(handle) => match self
                .body(*handle)
                .map_err(|error| proof_fault(&error.to_string()))?
            {
                Body::Bytes(bytes) => match &*bytes.chunks {
                    [Chunk::Resident(bytes)] => {
                        L::Bytes(lash_kernel_doc::Bytes::new(bytes.to_vec()))
                    }
                    [] => L::Bytes(lash_kernel_doc::Bytes::new(Vec::new())),
                    _ => return Err(proof_fault("literal bytes are cold")),
                },
                _ => return Err(proof_fault("literal bytes have another body")),
            },
            Value::Function(handle) => match self
                .body(*handle)
                .map_err(|error| proof_fault(&error.to_string()))?
            {
                Body::Function(callable) => L::Function(callable.function),
                _ => return Err(proof_fault("literal function has another body")),
            },
            _ => return Ok(None),
        }))
    }
}
fn naming(body: &Body, handle: ArenaHandle) -> Value {
    match body {
        Body::List(_) => Value::List(handle),
        Body::Map(_) => Value::Map(handle),
        Body::Set(_) => Value::Set(handle),
        Body::Record(_) => Value::Record(handle),
        Body::Closure { .. } => Value::Closure(handle),
        Body::Cell(_) => Value::ObjectRef(handle),
        Body::Tuple(_) => Value::Tuple(handle),
        Body::Error(_) => Value::Error(handle),
        Body::BigInt(_) => Value::BigInt(handle),
        Body::Timestamp(_) | Body::Cold(ColdImmutable::Timestamp(_)) => Value::BigTimestamp(handle),
        Body::Cold(ColdImmutable::BigInt(_)) => Value::BigInt(handle),
        Body::Cold(ColdImmutable::Error) => Value::Error(handle),
        Body::LongText(_) => Value::LongText(handle),
        Body::Bytes(_) => Value::Bytes(handle),
        Body::Function(_) => Value::Function(handle),
        Body::Handle(_) => Value::Handle(handle),
        Body::Free => unreachable!("a detached batch contains a free body"),
    }
}
impl PreparedBatch {
    pub fn graph_proof(&self) -> Option<&PreparedGraphProof> {
        self.proof.as_ref()
    }
    pub fn matches_proof(&self, proof: &PreparedGraphProof, arena: &Arena) -> bool {
        self.proof.as_ref().is_some_and(|actual| actual.same(proof)) && self.is_current(arena)
    }
    /// Journal preparation may grant identities, but may not change a body,
    /// release a generation, or install another allocation before this batch.
    pub fn is_current(&self, arena: &Arena) -> bool {
        arena.registration_active()
            && self
                .proof
                .as_ref()
                .is_none_or(|proof| proof.is_current(arena))
            && self.registration_epoch == arena.registration_epoch
            && std::rc::Rc::ptr_eq(&self.identity, &arena.identity)
            && self.tokens == arena.tokens
            && self.freed == arena.freed
            && self
                .empty_tuple
                .is_none_or(|(expected, _)| expected == arena.empty_tuple)
            && self.entries.iter().all(|entry| {
                entry
                    .facts
                    .provisional
                    .is_none_or(|id| id.ordinal < arena.next_provisional().ordinal)
            })
            && arena.entities.capacity() - arena.entities.len()
                >= self
                    .entries
                    .iter()
                    .filter(|entry| entry.facts.provisional.is_some())
                    .count()
            && self
                .dependencies
                .iter()
                .all(|version| arena.content_is(version))
            && arena.zero.capacity() - arena.zero.len() >= self.entries.len()
            && self
                .entries
                .iter()
                .all(|entry| (entry.handle.index() as usize) < arena.facts.capacity())
    }
    /// Called only by the matching accepted journal. No key lookup, summary
    /// rebuild, host allocation or recoverable failure remains after this check.
    pub fn publish(self, arena: &mut Arena) {
        assert!(arena.registration_active() && self.is_current(arena));
        self.publish_region(arena);
    }
    pub(crate) fn publish_region(self, arena: &mut Arena) {
        arena.allocations += self.allocations;
        for entry in &self.entries {
            arena.resident = arena.resident.saturating_add(body_values(&entry.body));
        }
        let start = arena.zero.len();
        for entry in self.entries {
            arena.objects.install(entry.handle, entry.body);
            arena.place(entry.handle, entry.facts);
        }
        if let Some((_, handle)) = self.empty_tuple {
            arena.empty_tuple = Some(handle);
        }
        // Every forward target exists before a member count is applied.
        for handle in &arena.zero[start..] {
            let Some(body) = arena.objects.get(*handle) else {
                unreachable!("the prepared batch lost an installed target");
            };
            each_handle(body, &mut |handle| {
                let facts = &mut arena.facts[handle.index() as usize];
                facts.count = facts.count.saturating_add(1);
            });
        }
        if let Some(proof) = self.proof {
            proof.0.published.set(true);
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
            data: data.value(),
        }),
    })
}

#[cfg(test)]
impl Arena {
    pub(crate) fn check_prepared_empty_tuple() {
        let mut arena = Arena::new();
        let before = arena.held();
        {
            let (value, storage) = arena.prepare_empty_tuple().unwrap();
            let (mut registration, _batch) = storage.into_parts();
            let arena = registration.arena();
            assert!(arena.body(value.handle().unwrap()).is_err());
            assert!(arena.resident_empty_tuple().is_none());
            assert_eq!(arena.held(), before);
        }
        assert!(arena.resident_empty_tuple().is_none());
        assert_eq!(arena.held(), before);
        let (value, storage) = arena.prepare_empty_tuple().unwrap();
        let first = value.handle().unwrap();
        storage.publish();
        assert!(
            matches!(arena.tuple(Vec::new(), 8).unwrap(), Value::Tuple(handle) if handle == first)
        );
        let held = arena.held();
        let (value, storage) = arena.prepare_empty_tuple().unwrap();
        assert_eq!(value.handle(), Some(first));
        let (mut registration, batch) = storage.into_parts();
        assert!(batch.is_current(registration.arena()));
        assert_eq!(registration.arena().held(), held);
        batch.publish(registration.arena());
        registration.commit();
        assert_eq!(arena.held(), held);
        arena.release(&mut |_| {});
        assert!(arena.body(first).is_err());
        let (value, storage) = arena.prepare_empty_tuple().unwrap();
        assert_ne!(value.handle(), Some(first));
        storage.publish();
        assert_eq!(
            arena.resident_empty_tuple().unwrap().handle(),
            value.handle()
        );
    }

    pub(crate) fn check_prepared_closure_edges() {
        // These fields are inert storage metadata here; no source admission or
        // function call is manufactured by this arena-only lifetime oracle.
        let identity = std::sync::Arc::new(lash_kernel_doc::arena::ClosureIdentity {
            capture_names: vec![lash_kernel_value::Name::new("captured")],
            callable: COLD_CLOSURE_SITE.callable,
            site: COLD_CLOSURE_SITE.site.clone(),
        });
        let code = lash_kernel_doc::arena::LocalCode {
            executable: 0,
            code: 0,
        };
        let mut arena = Arena::new();
        let cell = arena.new_cell(Value::Int(256));
        let (abandoned, storage) = arena
            .prepare_closure(identity.clone(), vec![cell], code)
            .unwrap();
        let (mut registration, batch) = storage.into_parts();
        assert!(registration.arena().body(abandoned).is_err());
        registration
            .arena()
            .cell_set(cell, Value::Int(257))
            .unwrap();
        assert!(
            !batch.is_current(registration.arena()),
            "a captured cell write invalidates its prepared closure"
        );
        drop(registration);
        let (closure, storage) = arena.prepare_closure(identity, vec![cell], code).unwrap();
        assert_eq!(closure, abandoned);
        storage.publish();
        arena.release(&mut |visit| visit(&Value::Closure(closure)));
        assert!(
            arena.cell(cell).unwrap().same(&Value::Int(257)),
            "a prepared closure retains its captured cell after the declaring root leaves"
        );
    }
}

mod region;
