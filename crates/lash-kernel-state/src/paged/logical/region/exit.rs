//! One affine translation of the current private source graph.
use super::storage::{
    Address, Entry, Kind, LeaseObject, LeaseValue, LocalBody, LocalStore, Repr, Spelling, capacity,
};
use super::*;
use crate::arena::{RegionBodyMetadata, RegionTablePatch};
use lash_kernel_doc::arena::{Body, Bytes, Chunk, LongText, Sequence, Table};
use std::sync::Arc;

struct Mapped {
    generation: u64,
    value: Option<Value>,
    proof: Option<usize>,
}
pub struct LeaseGraphExit {
    lease: u64,
    version: u64,
    token: Rc<()>,
    mapping: Vec<Mapped>,
    proof: Option<PreparedGraphProof>,
    fields: BTreeMap<std::sync::Arc<str>, Value>,
}
/// Read-only page projection of a frozen graph, including detached bodies.
/// It cannot identify, allocate, edit, install, or refresh a graph packet.
pub struct LeaseProjection<'a> {
    arena: &'a Arena,
    batch: &'a PreparedBatch,
    graph: &'a LeaseGraphExit,
}
impl LeaseProjection<'_> {
    pub fn body(&self, handle: ArenaHandle) -> Result<&Body, HeapFault> {
        match self.batch.region_body(handle) {
            Some(body) => Ok(body),
            None => self.arena.body(handle).map_err(arena_fault),
        }
    }
    pub fn page(
        &self,
        handle: ArenaHandle,
    ) -> Result<Option<lash_kernel_doc::heap::PageValue>, HeapFault> {
        if self.batch.region_body(handle).is_some() {
            return Ok(None);
        }
        Ok(self
            .arena
            .facts(handle)
            .map_err(arena_fault)?
            .page
            .get()
            .cloned())
    }
    pub fn ledger_key(&self, handle: ArenaHandle) -> Result<EntityId, HeapFault> {
        self.graph
            .proof
            .as_ref()
            .and_then(|proof| proof.region_entity(handle))
            .or_else(|| {
                self.arena
                    .facts(handle)
                    .ok()
                    .and_then(|facts| facts.ledger_entity())
            })
            .ok_or_else(|| invalid("frozen projection has no authenticated mutable identity"))
    }
}
impl LeaseGraphExit {
    fn mapped(&self, object: LeaseObject) -> Result<&Mapped, HeapFault> {
        if object.lease != self.lease {
            return Err(invalid("foreign lease graph object"));
        }
        self.mapping
            .get(object.slot as usize)
            .filter(|mapped| mapped.generation == object.generation && mapped.value.is_some())
            .ok_or_else(|| invalid("unbound or released lease graph object"))
    }
    pub fn value(&self, value: LeaseValue) -> Result<Value, HeapFault> {
        match value.repr {
            Repr::Scalar(value) => Ok(value),
            Repr::Object(object, native) => {
                let mapped = self
                    .mapped(object)?
                    .value
                    .ok_or_else(|| invalid("bound mapping omitted its value"))?;
                if let Some(native) = native {
                    if native.handle() != mapped.handle() {
                        return Err(invalid("lease mapping changed existing identity"));
                    }
                    Ok(native)
                } else {
                    Ok(mapped)
                }
            }
        }
    }
    pub fn object(&self, object: LeaseObject) -> Result<ArenaHandle, HeapFault> {
        self.mapped(object)?
            .value
            .and_then(|value| value.handle())
            .ok_or_else(|| invalid("lease graph object has no canonical header"))
    }
    pub fn publication(&self, value: LeaseValue) -> Result<PublicationValue, HeapFault> {
        Ok(match value.repr {
            Repr::Scalar(value) => PublicationValue::Native(value),
            Repr::Object(object, Some(_)) => {
                self.mapped(object)?;
                PublicationValue::Native(self.value(value)?)
            }
            Repr::Object(object, None) => {
                let index = self
                    .mapped(object)?
                    .proof
                    .ok_or_else(|| invalid("lease graph value has no detached proof"))?;
                PublicationValue::Prepared(
                    self.proof
                        .as_ref()
                        .ok_or_else(|| invalid("unsealed graph translation"))?
                        .value(index),
                )
            }
        })
    }
    fn spelling(&self, key: &Spelling) -> Result<Value, HeapFault> {
        match key {
            Spelling::Value(value) => self.value(*value),
            Spelling::Field(name) => lash_kernel_doc::arena::ShortText::new(name)
                .map(Value::Text)
                .or_else(|| self.fields.get(name).copied())
                .ok_or_else(|| invalid("lease omitted a retained field spelling")),
        }
    }
    fn entry(&self, entry: &Entry) -> Result<(Value, Value, u64), HeapFault> {
        Ok((
            self.spelling(&entry.key)?,
            self.value(entry.value)?,
            entry.sequence,
        ))
    }
    pub(super) fn authenticate(
        &self,
        local: &LocalStore,
        exclusion: &Exclusion,
    ) -> Result<(), HeapFault> {
        if !local.frozen
            || self.lease != local.id
            || self.version != local.version
            || !Rc::ptr_eq(&self.token, &exclusion.token)
        {
            return Err(invalid("foreign or stale lease graph packet"));
        }
        Ok(())
    }
}
impl StateLease<'_> {
    pub fn recovery_projection<'a>(
        &'a self,
        graph: &'a LeaseGraphExit,
    ) -> Result<LeaseProjection<'a>, HeapFault> {
        graph.authenticate(&self.local, &self.exclusion)?;
        let LeaseStorage::Installation {
            registration,
            batch,
        } = &self.storage
        else {
            return Err(invalid("frozen graph has no installation"));
        };
        Ok(LeaseProjection {
            arena: registration.read(),
            batch,
            graph,
        })
    }
    /// Translate a recovery role's existing canonical holding through the
    /// frozen packet. This neither adds a source occurrence nor publishes it.
    pub fn recovery_holding(
        &mut self,
        graph: &LeaseGraphExit,
        value: LeaseValue,
    ) -> Result<Holding, HeapFault> {
        graph.authenticate(&self.local, &self.exclusion)?;
        match graph.publication(value)? {
            PublicationValue::Prepared(proof) => proof.holding(self.arena()),
            PublicationValue::Native(value) => {
                let LeaseStorage::Installation { registration, .. } = &mut self.storage else {
                    return Err(invalid("recovery holding has no frozen installation"));
                };
                Holding::from_native(registration.arena(), self.heap.pages(), &value)
            }
            _ => Err(invalid("lease packet produced an invalid recovery value")),
        }
    }
    pub fn materialize_graph(&mut self) -> Result<LeaseGraphExit, HeapFault> {
        if self.local.frozen {
            return Err(invalid("lease graph was already frozen"));
        }
        {
            let ledger = self.heap.region_ledger()?;
            self.authenticate(&ledger)?;
        }
        // Freeze even a failed materialization. No failed header reservation
        // can be used to resume execution against a different identity mapping.
        self.local.frozen = true;
        let mut fresh = Vec::new();
        let mut field_set = std::collections::BTreeSet::new();
        for (index, slot) in self.local.slots.iter().enumerate() {
            if !slot.live {
                continue;
            }
            if slot.existing.is_none() {
                capacity(&mut fresh, 1)?;
                fresh.push(index);
            }
            let mut field = |entry: &Entry| {
                if let Spelling::Field(name) = &entry.key
                    && lash_kernel_doc::arena::ShortText::new(name).is_none()
                {
                    field_set.insert(Arc::clone(name));
                }
            };
            if let Some(existing) = &slot.existing {
                for overlay in existing.overlays.values() {
                    if let Some(entry) = &overlay.current {
                        field(entry);
                    }
                }
            }
            if let Some(LocalBody::Table(table)) = &slot.body {
                for entry in &table.entries {
                    field(entry);
                }
            }
        }
        let fields: Vec<_> = field_set.into_iter().collect();
        let count = fresh
            .len()
            .checked_add(fields.len())
            .ok_or_else(|| invalid("lease header count overflow"))?;
        let local = &self.local;
        let token = Rc::clone(&self.exclusion.token);
        let authority = match std::mem::replace(&mut self.storage, LeaseStorage::Vacant) {
            LeaseStorage::Arena(authority) => authority,
            _ => return Err(invalid("lease graph storage was already consumed")),
        };
        let (registration, batch, proof, (mapping, fields)) =
            match authority.stage_region(self.heap.pages(), count, local.ordinal, |handles| {
                let mut mapping = Vec::new();
                capacity(&mut mapping, local.slots.len())?;
                let mut values = Vec::new();
                capacity(&mut values, local.slots.len())?;
                for slot in &local.slots {
                    let value = if slot.live {
                        slot.existing.as_ref().and_then(|existing| existing.value)
                    } else {
                        None
                    };
                    mapping.push(Mapped {
                        generation: slot.generation,
                        value,
                        proof: None,
                    });
                }
                for (index, handle) in fresh.iter().zip(handles) {
                    mapping[*index].value = Some(local.slots[*index].kind.naming(*handle));
                }
                let field_values: BTreeMap<_, _> = fields
                    .iter()
                    .cloned()
                    .zip(handles[fresh.len()..].iter().copied().map(Value::LongText))
                    .collect();
                let mut packet = LeaseGraphExit {
                    lease: local.id,
                    version: local.version,
                    token: Rc::clone(&token),
                    mapping,
                    proof: None,
                    fields: field_values,
                };
                let mut bodies = Vec::new();
                capacity(&mut bodies, count)?;
                let mut ordinals = Vec::new();
                capacity(&mut ordinals, fresh.len())?;
                let mut metadata = Vec::new();
                capacity(&mut metadata, fresh.len())?;
                for index in &fresh {
                    let slot = &local.slots[*index];
                    let handle = packet.object(local.object(*index as u32))?;
                    if let Some(ordinal) = slot.ordinal {
                        ordinals.push((handle, ordinal));
                    }
                    let mut sequences = None;
                    let body = match slot
                        .body
                        .as_ref()
                        .ok_or_else(|| invalid("retained fresh object has no body"))?
                    {
                        LocalBody::Values(source) => {
                            let mut values = Vec::new();
                            capacity(&mut values, source.len())?;
                            for value in source {
                                values.push(packet.value(*value)?);
                            }
                            if slot.kind == Kind::Tuple {
                                Body::Tuple(Sequence::whole(values))
                            } else {
                                Body::List(Sequence::whole(values))
                            }
                        }
                        LocalBody::Cell(value) => Body::Cell(packet.value(*value)?),
                        LocalBody::Closure(closure) => {
                            let mut captures = Vec::new();
                            capacity(&mut captures, closure.captures.len())?;
                            for value in &closure.captures {
                                let Repr::Object(object, _) = value.repr else {
                                    return Err(invalid("closure capture is not an object"));
                                };
                                captures.push(packet.object(object)?);
                            }
                            Body::Closure {
                                identity: Arc::clone(&closure.identity),
                                captures: captures.into_boxed_slice(),
                                code: None,
                            }
                        }
                        LocalBody::Text(text) => Body::LongText(LongText {
                            summary: lash_kernel_doc::heap_values::text_summary(text),
                            chunks: Box::new([Chunk::Resident(text.clone())]),
                        }),
                        LocalBody::Bytes(bytes) => Body::Bytes(Bytes {
                            len: bytes.len() as u64,
                            chunks: Box::new([Chunk::Resident(bytes.clone().into_boxed_slice())]),
                        }),
                        LocalBody::Table(table) => {
                            let mut values = Vec::new();
                            capacity(
                                &mut values,
                                table
                                    .entries
                                    .len()
                                    .checked_mul(2)
                                    .ok_or_else(|| invalid("table value count overflow"))?,
                            )?;
                            let mut order = Vec::new();
                            capacity(&mut order, table.entries.len())?;
                            for entry in &table.entries {
                                let (key, value, sequence) = packet.entry(entry)?;
                                values.extend([key, value]);
                                order.push(sequence);
                            }
                            sequences = Some((order, table.next));
                            let table = Table::whole(values);
                            match slot.kind {
                                Kind::Map => Body::Map(table),
                                Kind::Set => Body::Set(table),
                                Kind::Record => Body::Record(table),
                                _ => unreachable!(),
                            }
                        }
                    };
                    metadata.push(RegionBodyMetadata {
                        handle,
                        content: slot.content,
                        structural: slot.structural,
                        sequences,
                    });
                    bodies.push(body);
                }
                for field in &fields {
                    bodies.push(Body::LongText(LongText {
                        summary: lash_kernel_doc::heap_values::text_summary(field),
                        chunks: Box::new([Chunk::Resident(field.to_string())]),
                    }));
                }
                for mapped in &mut packet.mapping {
                    if let Some(value) = mapped.value {
                        mapped.proof = Some(values.len());
                        values.push(value);
                    }
                }
                values.extend(packet.fields.values().copied());
                Ok(crate::arena::RegionBuild {
                    bodies,
                    values,
                    ordinals,
                    metadata,
                    data: (packet.mapping, packet.fields),
                })
            }) {
                Ok(staged) => staged,
                Err((authority, error)) => {
                    self.storage = LeaseStorage::Arena(authority);
                    return Err(error);
                }
            };
        self.storage = LeaseStorage::Installation {
            registration,
            batch,
        };
        Ok(LeaseGraphExit {
            lease: self.local.id,
            version: self.local.version,
            token,
            mapping,
            proof: Some(proof),
            fields,
        })
    }
}

fn original_value(value: LeaseValue) -> Result<Value, HeapFault> {
    match value.repr {
        Repr::Scalar(value) => Ok(value),
        Repr::Object(_, Some(value)) => Ok(value),
        _ => Err(invalid("entry overlay contains a fresh original")),
    }
}
fn original_entry(entry: &Entry) -> Result<(Value, Value, u64), HeapFault> {
    let key = match &entry.key {
        Spelling::Value(value) => original_value(*value)?,
        Spelling::Field(_) => return Err(invalid("entry overlay has no canonical key spelling")),
    };
    Ok((key, original_value(entry.value)?, entry.sequence))
}
fn occurrence(
    arena: &mut Arena,
    pages: &Access<'_>,
    value: Value,
    proof: &PreparedGraphProof,
) -> Result<Holding, HeapFault> {
    if let Some(holding) = proof.holding_for(&value) {
        return Ok(holding);
    }
    if value
        .handle()
        .is_some_and(|handle| proof.contains_handle(handle))
    {
        return Err(invalid("unproved local graph occurrence"));
    }
    Holding::from_native(arena, pages, &value)
}
fn entry_holding(
    arena: &mut Arena,
    pages: &Access<'_>,
    entry: (Value, Value, u64),
    table: bool,
    record: bool,
    proof: &PreparedGraphProof,
) -> Result<Holding, HeapFault> {
    let value = occurrence(arena, pages, entry.1, proof)?;
    if !table {
        return Ok(value);
    }
    let mut key = occurrence(arena, pages, entry.0, proof)?;
    if record {
        key.bytes = key
            .bytes
            .checked_sub(Cost::REFERENCE)
            .ok_or_else(|| invalid("record field holding underflow"))?;
    }
    key.bytes = key
        .bytes
        .checked_add(value.bytes)
        .ok_or_else(|| invalid("table entry holding overflow"))?;
    value.each_edge(pages, &mut |edge| {
        key.add_edge(edge.target, edge.count, pages)
    })?;
    Ok(key)
}
pub(super) struct ProjectedExit {
    pub(super) prepared: Prepared,
    pub(super) edits: Vec<MemberEdit>,
    pub(super) tables: Vec<(ArenaHandle, Vec<RegionTablePatch>, u64)>,
    pub(super) lists: Vec<(ArenaHandle, Vec<Value>, Vec<u64>)>,
    pub(super) revisions: Vec<RegionBodyMetadata>,
}
impl LeaseGraphExit {
    /// Independently reconstruct the incoming equation from the original
    /// bodies, final bodies and actual root replacements. Local counters are
    /// checked against this equation before any canonical tree is installed.
    pub(super) fn project(
        &self,
        local: &LocalStore,
        ledger: &Ledger,
        arena: &mut Arena,
        pages: &Access<'_>,
        exit: &StateExit,
    ) -> Result<ProjectedExit, HeapFault> {
        let proof = self
            .proof
            .as_ref()
            .ok_or_else(|| invalid("unsealed lease graph"))?;
        if !proof.is_current(arena) {
            return Err(invalid("stale detached lease graph"));
        }
        for (_, value) in &exit.outputs {
            if let PublicationValue::Prepared(value) = value
                && !proof.same(value.graph())
            {
                return Err(invalid("output belongs to another graph packet"));
            }
        }
        let outputs: Vec<_> = exit
            .outputs
            .iter()
            .map(|(owner, value)| (*owner, value.root()))
            .collect();
        let roots = ledger.ownership_changes_with_graph(
            arena,
            pages,
            &exit.inputs,
            &outputs,
            &mut Vec::new(),
            Some(proof),
        )?;
        let mut expected = BTreeMap::<EntityId, i128>::new();
        let mut final_bodies = BTreeMap::<EntityId, Holding>::new();
        let mut edits = Vec::new();
        let mut tables = Vec::new();
        let mut lists = Vec::new();
        let mut revisions = Vec::new();
        let mut add_edges = |holding: &Holding, sign: i128| -> Result<(), HeapFault> {
            holding.each_edge(pages, &mut |edge| {
                if let Identity::Object(entity) = edge.target {
                    let count = expected.entry(entity).or_default();
                    *count = count
                        .checked_add(i128::from(edge.count) * sign)
                        .ok_or_else(|| invalid("exit edge equation overflow"))?;
                }
                Ok(())
            })
        };
        for (index, slot) in local.slots.iter().enumerate() {
            let Some(existing) = &slot.existing else {
                continue;
            };
            let Some(entity) = existing.entity else {
                continue;
            };
            let original = ledger
                .state
                .entities
                .get(&entity_key(entity), pages)?
                .ok_or_else(|| invalid("entry entity disappeared"))?;
            add_edges(&original.body, -1)?;
            if !slot.live {
                continue;
            }
            let mut body = original.body;
            let mut table_patches = Vec::new();
            for (address, overlay) in existing.overlays.iter() {
                let table = matches!(address, Address::Table(_));
                let remove = overlay
                    .original
                    .as_ref()
                    .map(original_entry)
                    .transpose()?
                    .map(|entry| {
                        entry_holding(arena, pages, entry, table, slot.kind == Kind::Record, proof)
                    })
                    .transpose()?
                    .unwrap_or_else(|| Holding::scalar(0));
                let add = overlay
                    .current
                    .as_ref()
                    .map(|entry| self.entry(entry))
                    .transpose()?
                    .map(|entry| {
                        entry_holding(arena, pages, entry, table, slot.kind == Kind::Record, proof)
                    })
                    .transpose()?
                    .unwrap_or_else(|| Holding::scalar(0));
                body.replace_members(&remove, &add, pages)?;
                let handle = existing
                    .value
                    .and_then(|value| value.handle())
                    .ok_or_else(|| invalid("edited entry body has no resident handle"))?;
                match address {
                    Address::Table(_) => table_patches.push(RegionTablePatch {
                        original: overlay.original.as_ref().map(original_entry).transpose()?,
                        current: overlay
                            .current
                            .as_ref()
                            .map(|entry| self.entry(entry))
                            .transpose()?,
                    }),
                    Address::List(_) => {}
                    Address::Variable => edits.push(MemberEdit {
                        handle,
                        member: Member::Variable,
                        next: overlay
                            .current
                            .as_ref()
                            .map(|entry| self.value(entry.value))
                            .transpose()?,
                    }),
                }
            }
            if body.bytes != slot.bytes {
                return Err(invalid(
                    "lease body and canonical member projection disagree",
                ));
            }
            if slot.content != 0 {
                let handle = self.object(local.object(index as u32))?;
                revisions.push(arena.prepare_region_revision(
                    handle,
                    slot.content,
                    slot.structural,
                )?);
                if matches!(slot.kind, Kind::Map | Kind::Set | Kind::Record) {
                    tables.push((handle, table_patches, existing.next));
                }
                if slot.kind == Kind::List {
                    let mut values = Vec::new();
                    capacity(
                        &mut values,
                        usize::try_from(existing.len)
                            .map_err(|_| invalid("exit list length overflow"))?,
                    )?;
                    for at in 0..existing.len {
                        let value = if let Some(overlay) = existing.overlays.get(&Address::List(at))
                        {
                            self.value(
                                overlay
                                    .current
                                    .as_ref()
                                    .ok_or_else(|| invalid("exit list has a missing member"))?
                                    .value,
                            )?
                        } else {
                            arena
                                .item(handle, at)
                                .map_err(arena_fault)?
                                .ok_or_else(|| invalid("exit list member is absent"))?
                        };
                        values.push(value);
                    }
                    let mut cards: Vec<_> = existing
                        .overlays
                        .keys()
                        .filter_map(|address| match address {
                            Address::List(at) => Some(*at / crate::paged::stored::CARD),
                            _ => None,
                        })
                        .collect();
                    cards.sort_unstable();
                    cards.dedup();
                    lists.push((handle, values, cards));
                }
            }
            add_edges(&body, 1)?;
            final_bodies.insert(entity, body);
        }
        for (index, entity) in proof.entities().iter().enumerate() {
            let body = proof.body(index).clone();
            add_edges(&body, 1)?;
            final_bodies.insert(entity.entity, body);
        }
        for (owner, next) in &roots {
            if let Some(old) = ledger.holding(*owner, pages)? {
                add_edges(&old, -1)?;
            }
            if let Some(next) = next {
                add_edges(next, 1)?;
            }
        }
        let mut next = ledger.state.clone();
        let mut bound = std::collections::BTreeSet::new();
        for (index, slot) in local.slots.iter().enumerate() {
            let entity = if let Some(existing) = &slot.existing {
                existing.entity
            } else if slot.live && slot.ordinal.is_some() {
                let handle = self.object(local.object(index as u32))?;
                proof.region_entity(handle)
            } else {
                None
            };
            let Some(entity) = entity else { continue };
            bound.insert(entity);
            let initial = ledger
                .state
                .entities
                .get(&entity_key(entity), pages)?
                .map_or(0, |record| record.incoming);
            let incoming = i128::from(initial) + expected.get(&entity).copied().unwrap_or(0);
            if incoming != i128::from(slot.incoming) || (!slot.live && incoming != 0) {
                return Err(invalid(
                    "lease source incoming multiplicity disagrees with final roots and members",
                ));
            }
            if slot.live {
                let body = final_bodies
                    .remove(&entity)
                    .ok_or_else(|| invalid("retained lease entity has no final body"))?;
                if body.bytes != slot.bytes {
                    return Err(invalid("retained lease body cost mismatch"));
                }
                next.entities.write(
                    entity_key(entity),
                    EntityHolding {
                        body,
                        incoming: slot.incoming,
                    },
                    pages,
                )?;
            } else {
                next.entities.remove(&entity_key(entity), pages)?;
            }
        }
        if !final_bodies.is_empty() || expected.keys().any(|entity| !bound.contains(entity)) {
            return Err(invalid("exit graph contains an unbound source identity"));
        }
        for (owner, value) in roots {
            let tree = next.roots_mut(owner);
            if let Some(value) = value {
                let root = RootHolding { owner, value };
                codec::validate_root(&root)?;
                tree.write(owner_key(owner), root, pages)?;
            } else {
                tree.remove(&owner_key(owner), pages)?;
            }
        }
        let before = ledger.balance()?.total()?;
        let after = next.balance()?.total()?;
        let required = i128::from(before)
            .checked_add(local.peak)
            .and_then(|peak| u64::try_from(peak).ok())
            .ok_or_else(|| invalid("lease peak overflow"))?;
        Ok(ProjectedExit {
            prepared: Prepared {
                identity: Rc::clone(&ledger.identity),
                revision: ledger.revision,
                next,
                peak: required.max(before).max(after),
                before,
                projected: bound.into_iter().collect(),
            },
            edits,
            tables,
            lists,
            revisions,
        })
    }
}

#[cfg(any(test, feature = "perf-witness"))]
pub(super) fn fixture() -> LeaseGraphExit {
    LeaseGraphExit {
        lease: 0,
        version: 0,
        token: Rc::new(()),
        mapping: Vec::new(),
        proof: None,
        fields: BTreeMap::new(),
    }
}
