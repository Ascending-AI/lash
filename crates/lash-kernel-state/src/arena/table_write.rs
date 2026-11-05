//! Version-bound member patches. Preparation reserves only incremental storage;
//! no prior value or insertion-sequence population is copied per insertion.
use super::*;
use crate::paged::logical::{Member, MemberEdit};
use lash_kernel_doc::heap::{HeapFault, HostResource};

#[derive(Clone, Copy)]
enum TableKind {
    Map,
    Set,
    Record,
}
struct Patch {
    address: Member,
    normalized: Key,
    key: Option<Value>,
    next: Option<Value>,
    old: Option<(Value, Value)>,
    place: Option<usize>,
    sequence: u64,
}
/// Storage and generation authority for an exact accepted table edit.
pub struct PreparedTableMember {
    version: ContentVersion,
    handle: ArenaHandle,
    kind: TableKind,
    first: Patch,
    rest: Vec<Patch>,
    growth: tables::PreparedIndexGrowth,
    spill: Option<Vec<Value>>,
    flat: Option<Option<sizes::FlatSize>>,
    next_sequence: Option<u64>,
}
fn bad(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
fn fault(error: NativeError) -> HeapFault {
    match error {
        NativeError::Host(error) => error,
        error => bad(&error.to_string()),
    }
}
fn reserve<T>(values: &mut Vec<T>, count: usize) -> Result<(), HeapFault> {
    values.try_reserve(count).map_err(|_| HeapFault::Exhausted {
        resource: HostResource::StagingBytes,
    })
}
impl Arena {
    pub fn prepare_table_member(
        &mut self,
        handle: ArenaHandle,
        key: Value,
        next: Option<Value>,
    ) -> Result<Option<PreparedTableMember>, HeapFault> {
        let Ok(normalized) = self.key(&key) else {
            return Ok(None);
        };
        self.prepare_table_patch(handle, Member::Table(key), Some(key), next, normalized)
    }
    pub(super) fn prepare_table_patch(
        &mut self,
        handle: ArenaHandle,
        address: Member,
        key: Option<Value>,
        next: Option<Value>,
        normalized: Key,
    ) -> Result<Option<PreparedTableMember>, HeapFault> {
        let Some((version, kind, patch)) =
            self.table_patch(handle, address, key, next, normalized)?
        else {
            return Ok(None);
        };
        let inserts = usize::from(patch.old.is_none() && patch.next.is_some());
        let removes = patch.old.is_some() && patch.next.is_none();
        let spill = self.reserve_table_patch_values(handle, inserts)?;
        let growth = self.facts[handle.index() as usize]
            .index
            .as_mut()
            .ok_or_else(|| bad("resident table has no index"))?
            .prepare_growth(inserts, removes, Some(&patch.normalized))?;
        let mut prepared = PreparedTableMember {
            version,
            handle,
            kind,
            first: patch,
            rest: Vec::new(),
            growth,
            spill,
            flat: None,
            next_sequence: None,
        };
        prepared.seal_flat(&self.objects, self.facts[handle.index() as usize].flat);
        prepared.reserve(self, 0)?;
        Ok(Some(prepared))
    }
    fn table_patch(
        &self,
        handle: ArenaHandle,
        address: Member,
        key: Option<Value>,
        next: Option<Value>,
        normalized: Key,
    ) -> Result<Option<(ContentVersion, TableKind, Patch)>, HeapFault> {
        let Some(version) = self.content_version(handle) else {
            return Ok(None);
        };
        if !self.facts(handle).is_ok_and(|facts| !facts.cold) {
            return Ok(None);
        }
        let kind = match self.body(handle) {
            Ok(Body::Map(_)) => TableKind::Map,
            Ok(Body::Set(_)) => TableKind::Set,
            Ok(Body::Record(_)) => TableKind::Record,
            _ => return Ok(None),
        };
        if matches!(kind, TableKind::Set)
            && next.is_some_and(|value| !matches!(value, Value::Absent))
        {
            return Ok(None);
        }
        if matches!(kind, TableKind::Record)
            && key.is_some_and(|key| !matches!(key, Value::Text(_) | Value::LongText(_)))
        {
            return Ok(None);
        }
        let Ok((table, index)) = self.table_view(handle) else {
            return Ok(None);
        };
        if tables::pairs(handle, table).is_err() {
            return Ok(None);
        }
        let place = tables::find(handle, table, index, &normalized).map_err(fault)?;
        if place.is_some_and(|place| place.chunk != 0) {
            return Ok(None);
        }
        let old = place
            .map(|place| tables::pair(table, place).map(|pair| (pair[0], pair[1])))
            .transpose()
            .map_err(fault)?;
        let sequence = match place {
            Some(place) => index
                .sequences(0)
                .get(place.at)
                .ok_or_else(|| bad("table entry lost its sequence"))?,
            None => index.next(),
        };
        if old.is_none() && next.is_some() {
            sequence
                .checked_add(1)
                .ok_or_else(|| bad("table insertion sequence overflow"))?;
            table
                .len
                .checked_add(1)
                .ok_or_else(|| bad("table length overflow"))?;
        }
        Ok(Some((
            version,
            kind,
            Patch {
                address,
                normalized,
                key,
                next,
                old,
                place: place.map(|place| place.at),
                sequence,
            },
        )))
    }
    fn reserve_table_patch_values(
        &mut self,
        handle: ArenaHandle,
        inserts: usize,
    ) -> Result<Option<Vec<Value>>, HeapFault> {
        let additional = inserts
            .checked_mul(2)
            .ok_or_else(|| bad("table value capacity overflow"))?;
        let Some(body) = self.objects.get_mut(handle) else {
            return Err(bad("stale table patch handle"));
        };
        let table = match body {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
            _ => return Err(bad("table patch changed kind")),
        };
        match &mut table.chunks {
            Chunks::Inline { len, values } if usize::from(*len) + additional > Chunks::INLINE => {
                let mut spill = Vec::new();
                reserve(&mut spill, usize::from(*len) + additional)?;
                spill.extend_from_slice(&values[..usize::from(*len)]);
                Ok(Some(spill))
            }
            Chunks::Inline { .. } => Ok(None),
            Chunks::Whole(values) => {
                reserve(values, additional)?;
                Ok(None)
            }
            Chunks::Split(chunks) => match chunks.as_mut_slice() {
                [Chunk::Resident(values)] => {
                    reserve(values, additional)?;
                    Ok(None)
                }
                _ => Err(bad("table patch needs a joined resident body")),
            },
        }
    }
    pub(super) fn prepare_region_table_keys(
        &mut self,
        edits: &[MemberEdit],
        keys: Vec<Key>,
    ) -> Result<Option<PreparedTableMember>, HeapFault> {
        let Some(first) = edits.first() else {
            return Ok(None);
        };
        if keys.len() != edits.len() {
            return Err(bad("region table keys do not cover its edits"));
        }
        let mut patches = Vec::new();
        reserve(&mut patches, edits.len())?;
        let mut version = None;
        let mut kind = None;
        let mut inserts = 0_usize;
        let mut removes = false;
        for (edit, normalized) in edits.iter().zip(keys) {
            if edit.handle != first.handle {
                return Err(bad("region table group changes identity"));
            }
            let key = match &edit.member {
                Member::Table(value) => Some(*value),
                Member::Field(name) => ShortText::new(name).map(Value::Text),
                _ => return Err(bad("region table has another address kind")),
            };
            let Some((current, current_kind, mut patch)) =
                self.table_patch(edit.handle, edit.member.clone(), key, edit.next, normalized)?
            else {
                return Ok(None);
            };
            if patch.old.is_none() && patch.next.is_some() {
                patch.sequence = patch
                    .sequence
                    .checked_add(inserts as u64)
                    .ok_or_else(|| bad("table sequence overflow"))?;
                inserts += 1;
            }
            removes |= patch.old.is_some() && patch.next.is_none();
            version = Some(current);
            kind = Some(current_kind);
            patches.push(patch);
        }
        let mut seen: std::collections::HashSet<&Key, foldhash::fast::RandomState> =
            Default::default();
        seen.try_reserve(patches.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: HostResource::StagingBytes,
            })?;
        for patch in &patches {
            if !seen.insert(&patch.normalized) {
                return Err(bad("region repeats a normalized table member"));
            }
        }
        drop(seen);
        let Some(version) = version else {
            return Ok(None);
        };
        let Some(kind) = kind else {
            return Ok(None);
        };
        let spill = self.reserve_table_patch_values(first.handle, inserts)?;
        let growth = self.facts[first.handle.index() as usize]
            .index
            .as_mut()
            .ok_or_else(|| bad("resident table has no index"))?
            .prepare_growth(
                inserts,
                removes,
                if edits.len() == 1 {
                    Some(&patches[0].normalized)
                } else {
                    None
                },
            )?;
        patches.sort_by_key(|patch| match (patch.old, patch.next) {
            (Some(_), Some(_)) => (0, 0),
            (Some(_), None) => (1, u64::MAX - patch.place.unwrap_or(0) as u64),
            (None, Some(_)) => (2, patch.sequence),
            (None, None) => (3, 0),
        });
        let first_patch = patches.remove(0);
        let mut prepared = PreparedTableMember {
            version,
            handle: first.handle,
            kind,
            first: first_patch,
            rest: patches,
            growth,
            spill,
            flat: None,
            next_sequence: None,
        };
        prepared.seal_flat(
            &self.objects,
            self.facts[first.handle.index() as usize].flat,
        );
        Ok(Some(prepared))
    }
}
impl PreparedTableMember {
    fn patches(&self) -> impl Iterator<Item = &Patch> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
    pub fn value(&self) -> Option<Value> {
        self.first.next
    }
    pub fn stage(&mut self, value: Value) -> bool {
        if !self.rest.is_empty()
            || self.first.next.is_none()
            || matches!(self.kind, TableKind::Set) && !matches!(value, Value::Absent)
        {
            return false;
        }
        self.first.next = Some(value);
        self.flat = None;
        true
    }
    pub fn edit(&self) -> MemberEdit {
        MemberEdit {
            handle: self.handle,
            member: self.first.address.clone(),
            next: self.first.next,
        }
    }
    pub(super) fn is_record(&self) -> bool {
        matches!(self.kind, TableKind::Record)
    }
    pub(super) fn old_key(&self) -> Option<Value> {
        self.first.old.map(|(key, _)| key)
    }
    pub(super) fn bind_field_key(&mut self, key: Option<Value>) {
        self.first.key = key;
        self.flat = None;
    }
    pub(super) fn seal_flat(&mut self, objects: &impl Bodies, mut flat: Option<sizes::FlatSize>) {
        for patch in self.patches() {
            let new = patch.next.and_then(|value| {
                patch
                    .old
                    .map(|(key, _)| key)
                    .or(patch.key)
                    .map(|key| (key, value))
            });
            flat = sizes::edit_table_flat(
                objects,
                flat,
                patch.old,
                new,
                matches!(self.kind, TableKind::Set),
                matches!(self.kind, TableKind::Record),
            );
        }
        self.flat = Some(flat);
    }
    pub(super) fn reserve(&self, arena: &mut Arena, batch: usize) -> Result<(), HeapFault> {
        let count = self.rest.len() + 1;
        reserve(&mut arena.zero, batch + 2 * count)?;
        reserve(&mut arena.dirty, batch + 1)?;
        reserve(&mut arena.facts[self.handle.index() as usize].cards, count)
    }
    pub fn publish(mut self, arena: &mut Arena, edit: MemberEdit) {
        assert!(arena.registration_active() && arena.content_is(&self.version));
        assert!(
            self.rest.is_empty()
                && edit.handle == self.handle
                && edit.member.same(&self.first.address)
                && match (edit.next, self.first.next) {
                    (Some(a), Some(b)) => a.same(&b),
                    (None, None) => true,
                    _ => false,
                }
        );
        if self.flat.is_none() {
            self.seal_flat(
                &arena.objects,
                arena.facts[self.handle.index() as usize].flat,
            );
        }
        self.publish_region(arena);
    }
    pub(crate) fn release_slots(&self) -> usize {
        self.patches()
            .map(|patch| match (patch.old, patch.next) {
                (Some(_), Some(_)) => 1,
                (Some(_), None) => 2,
                _ => 0,
            })
            .sum()
    }
    pub(crate) fn card_slots(&self) -> usize {
        self.rest.len() + 1
    }
    pub(crate) fn publish_region(self, arena: &mut Arena) {
        let Self {
            handle,
            kind: _,
            first,
            rest,
            growth,
            spill,
            flat,
            version: _,
            next_sequence,
        } = self;
        let shared_append = {
            let index = arena.facts[handle.index() as usize]
                .index
                .as_mut()
                .unwrap_or_else(|| unreachable!("prepared table index"));
            growth.install(index)
        };
        if let Some(spill) = spill {
            let Some(body) = arena.objects.get_mut(handle) else {
                unreachable!("prepared table body");
            };
            let table = match body {
                Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
                _ => unreachable!("prepared table kind"),
            };
            table.chunks = Chunks::Whole(spill);
        }
        for patch in std::iter::once(first).chain(rest) {
            {
                let Some(body) = arena.objects.get_mut(handle) else {
                    unreachable!("prepared table body");
                };
                let table = match body {
                    Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
                    _ => unreachable!("prepared table kind"),
                };
                match (patch.old, patch.next) {
                    (Some(_), Some(value)) => {
                        let at = patch
                            .place
                            .unwrap_or_else(|| unreachable!("prepared overwrite position"));
                        set_value(&mut table.chunks, at, value);
                    }
                    (Some(_), None) => {
                        let at = patch
                            .place
                            .unwrap_or_else(|| unreachable!("prepared removal position"));
                        remove_pair(&mut table.chunks, at);
                        table.len -= 1;
                        arena.facts[handle.index() as usize]
                            .index
                            .as_mut()
                            .unwrap_or_else(|| unreachable!("prepared table index"))
                            .remove_prepared(&patch.normalized, at);
                    }
                    (None, Some(value)) => {
                        let key = patch
                            .key
                            .unwrap_or_else(|| unreachable!("prepared insertion key"));
                        append_pair(&mut table.chunks, key, value);
                        table.len += 1;
                        if !shared_append {
                            arena.facts[handle.index() as usize]
                                .index
                                .as_mut()
                                .unwrap_or_else(|| unreachable!("prepared table index"))
                                .insert_prepared(patch.normalized, patch.sequence);
                        }
                    }
                    (None, None) => continue,
                }
            }
            match (patch.old, patch.next) {
                (None, Some(value)) => {
                    let key = patch
                        .key
                        .unwrap_or_else(|| unreachable!("prepared insertion key"));
                    arena.resident = arena.resident.saturating_add(2);
                    arena.hold(&key);
                    arena.hold(&value);
                }
                (Some((_, old)), Some(value)) => {
                    arena.hold(&value);
                    arena.unhold(&old);
                }
                (Some((key, old)), None) => {
                    arena.resident = arena.resident.saturating_sub(2);
                    arena.unhold(&key);
                    arena.unhold(&old);
                }
                (None, None) => unreachable!("no-op member was skipped"),
            }
            let structural = patch.old.is_none() || patch.next.is_none();
            let facts = &mut arena.facts[handle.index() as usize];
            if structural {
                facts.revision = facts.revision.saturating_add(1);
            }
            Arena::mark_resident(facts, &mut arena.dirty, handle, Some(patch.sequence / CARD));
        }
        if let Some(next) = next_sequence {
            let Some(index) = arena.facts[handle.index() as usize].index.as_mut() else {
                unreachable!("reserved table index")
            };
            index.set_region_next(next);
        }
        arena.facts[handle.index() as usize].flat = flat.unwrap_or(None);
    }
}
fn set_value(chunks: &mut Chunks, at: usize, value: Value) {
    match chunks {
        Chunks::Inline { values, .. } => values[at * 2 + 1] = value,
        Chunks::Whole(values) => values[at * 2 + 1] = value,
        Chunks::Split(chunks) => match &mut chunks[0] {
            Chunk::Resident(values) => values[at * 2 + 1] = value,
            _ => unreachable!("prepared resident table"),
        },
    }
}
fn append_pair(chunks: &mut Chunks, key: Value, value: Value) {
    match chunks {
        Chunks::Inline { len, values } => {
            values[usize::from(*len)] = key;
            values[usize::from(*len) + 1] = value;
            *len += 2;
        }
        Chunks::Whole(values) => values.extend([key, value]),
        Chunks::Split(chunks) => match &mut chunks[0] {
            Chunk::Resident(values) => values.extend([key, value]),
            _ => unreachable!("prepared resident table"),
        },
    }
}
fn remove_pair(chunks: &mut Chunks, at: usize) {
    match chunks {
        Chunks::Inline { len, values } => {
            let before = usize::from(*len);
            values.copy_within(at * 2 + 2..before, at * 2);
            *len -= 2;
            values[usize::from(*len)..before].fill(Value::Absent);
        }
        Chunks::Whole(values) => {
            values.drain(at * 2..at * 2 + 2);
        }
        Chunks::Split(chunks) => match &mut chunks[0] {
            Chunk::Resident(values) => {
                values.drain(at * 2..at * 2 + 2);
            }
            _ => unreachable!("prepared resident table"),
        },
    }
}

impl PreparedInstallation<'_> {
    pub(crate) fn prepare_region_table_members(
        &mut self,
        edits: &[MemberEdit],
    ) -> Result<Option<PreparedTableMember>, HeapFault> {
        let Some(first) = edits.first() else {
            return Ok(None);
        };
        let flat = self.registration.read_arena().facts[first.handle.index() as usize].flat;
        let keys = edits
            .iter()
            .map(|edit| match &edit.member {
                Member::Table(value) => keys::key_of(self, self, value)
                    .map_err(fault)?
                    .ok_or_else(|| bad("invalid detached table key")),
                Member::Field(name) => Ok(Key::of_text(name)),
                _ => Err(bad("region table has another address kind")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut prepared = self
            .registration
            .arena()
            .prepare_region_table_keys(edits, keys)?;
        if let Some(prepared) = &mut prepared {
            prepared.seal_flat(self, flat);
        }
        Ok(prepared)
    }
}

/// Final contents of a distinct changed member, including its current source
/// sequence. Reinsertions are a removal plus a new spelling, never an overwrite.
pub(crate) struct RegionTablePatch {
    pub original: Option<(Value, Value, u64)>,
    pub current: Option<(Value, Value, u64)>,
}
impl PreparedInstallation<'_> {
    pub(crate) fn prepare_lease_table(
        &mut self,
        handle: ArenaHandle,
        edits: Vec<RegionTablePatch>,
        next: u64,
    ) -> Result<RegionMemberBody, HeapFault> {
        let mut patches = Vec::new();
        reserve(&mut patches, edits.len().saturating_mul(2))?;
        let mut version = None;
        let mut kind = None;
        let mut inserts = 0;
        let mut removes = false;
        for edit in edits {
            let key = edit.original.or(edit.current).map(|entry| entry.0);
            let Some(key) = key else { continue };
            let normalized = keys::key_of(self, self, &key)
                .map_err(fault)?
                .ok_or_else(|| bad("lease table key is invalid"))?;
            let Some((current_version, current_kind, mut patch)) =
                self.registration.arena().table_patch(
                    handle,
                    Member::Table(key),
                    Some(key),
                    edit.current.map(|entry| entry.1),
                    normalized,
                )?
            else {
                return Err(bad("lease table lost its entry authority"));
            };
            if !match (patch.old, edit.original) {
                (Some((key, value)), Some((old_key, old_value, sequence))) => {
                    key.same(&old_key) && value.same(&old_value) && patch.sequence == sequence
                }
                (None, None) => true,
                _ => false,
            } {
                return Err(bad("lease table original changed"));
            }
            version = Some(current_version);
            kind = Some(current_kind);
            match (edit.original, edit.current) {
                (Some(old), Some(new)) if old.2 != new.2 => {
                    removes = true;
                    inserts += 1;
                    let insertion = Patch {
                        address: Member::Table(new.0),
                        normalized: patch.normalized.clone(),
                        key: Some(new.0),
                        next: Some(new.1),
                        old: None,
                        place: None,
                        sequence: new.2,
                    };
                    patch.next = None;
                    patches.push(patch);
                    patches.push(insertion);
                }
                (None, Some(new)) => {
                    inserts += 1;
                    patch.key = Some(new.0);
                    patch.sequence = new.2;
                    patches.push(patch);
                }
                (Some(_), None) => {
                    removes = true;
                    patches.push(patch);
                }
                (Some(_), Some(_)) => patches.push(patch),
                (None, None) => {}
            }
        }
        // A net-empty overlay can still have consumed insertion sequences.
        let arena = self.registration.arena();
        let version = version
            .or_else(|| arena.content_version(handle))
            .ok_or_else(|| bad("lease table has no version"))?;
        let kind = kind.unwrap_or(match arena.body(handle).map_err(fault)? {
            Body::Map(_) => TableKind::Map,
            Body::Set(_) => TableKind::Set,
            Body::Record(_) => TableKind::Record,
            _ => return Err(bad("lease table changed kind")),
        });
        let initial_next = arena.region_table_next(handle).map_err(fault)?;
        let spill = arena.reserve_table_patch_values(handle, inserts)?;
        let growth = arena.facts[handle.index() as usize]
            .index
            .as_mut()
            .ok_or_else(|| bad("lease table has no index"))?
            .prepare_growth(
                inserts,
                removes || next != initial_next + inserts as u64,
                None,
            )?;
        patches.sort_by_key(|patch| match (patch.old, patch.next) {
            (Some(_), Some(_)) => (0, 0),
            (Some(_), None) => (1, u64::MAX - patch.place.unwrap_or(0) as u64),
            (None, Some(_)) => (2, patch.sequence),
            _ => (3, 0),
        });
        let first = if patches.is_empty() {
            Patch {
                address: Member::Field(std::sync::Arc::from("")),
                normalized: Key::of_text(""),
                key: None,
                next: None,
                old: None,
                place: None,
                sequence: next,
            }
        } else {
            patches.remove(0)
        };
        let mut prepared = PreparedTableMember {
            version,
            handle,
            kind,
            first,
            rest: patches,
            growth,
            spill,
            flat: None,
            next_sequence: Some(next),
        };
        let flat = arena.facts[handle.index() as usize].flat;
        prepared.seal_flat(self, flat);
        Ok(RegionMemberBody::table(handle, prepared))
    }
}
