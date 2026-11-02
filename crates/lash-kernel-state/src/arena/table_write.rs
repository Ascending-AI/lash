//! Table storage and insertion sequences are complete before source acceptance.
use super::*;
use crate::paged::logical::{Member, MemberEdit};
use lash_kernel_doc::heap::{HeapFault, HostResource};

/// The exact member address and all infallible installation storage.
pub struct PreparedTableMember {
    version: ContentVersion,
    handle: ArenaHandle,
    address: Member,
    key: Option<Value>,
    next: Option<Value>,
    edit: Option<TableEdit>,
}
#[derive(Clone, Copy)]
enum TableKind {
    Map,
    Set,
    Record,
}
struct TableEdit {
    body: Body,
    index: tables::Index,
    flat: Option<sizes::FlatSize>,
    old: Option<(Value, Value)>,
    structural: bool,
    sequence: u64,
    resident_before: u64,
    position: usize,
}
pub(super) struct TableSnapshot {
    version: ContentVersion,
    handle: ArenaHandle,
    kind: TableKind,
    values: Vec<Value>,
    sequences: Vec<u64>,
    next_sequence: u64,
    place: Option<usize>,
    old: Option<(Value, Value)>,
}
fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<(), HeapFault> {
    values
        .try_reserve(additional)
        .map_err(|_| HeapFault::Exhausted {
            resource: HostResource::StagingBytes,
        })
}
fn fault(error: NativeError) -> HeapFault {
    match error {
        NativeError::Host(error) => error,
        error => HeapFault::Malformed(error.to_string()),
    }
}
impl Arena {
    /// Capture the resident target, its original key spelling and sequence ids.
    pub(super) fn table_snapshot(
        &self,
        handle: ArenaHandle,
        key: &Key,
    ) -> Result<Option<TableSnapshot>, HeapFault> {
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
        let Ok((table, index)) = self.table_view(handle) else {
            return Ok(None);
        };
        let Ok(held) = tables::pairs(handle, table) else {
            return Ok(None);
        };
        let place = tables::find(handle, table, index, key).map_err(fault)?;
        let old = place
            .map(|place| tables::pair(table, place).map(|pair| (pair[0], pair[1])))
            .transpose()
            .map_err(fault)?;
        if place.is_some_and(|place| place.chunk != 0) {
            return Ok(None);
        }
        let mut values = Vec::new();
        reserve(&mut values, held.len() + 2)?;
        values.extend_from_slice(held);
        let mut sequences = Vec::new();
        reserve(&mut sequences, held.len() / 2 + 1)?;
        sequences.extend(index.sequences(0).iter());
        Ok(Some(TableSnapshot {
            version,
            handle,
            kind,
            values,
            sequences,
            next_sequence: index.next(),
            place: place.map(|place| place.at),
            old,
        }))
    }
    /// Prepare an evaluated index key with the same canonical table builder.
    pub fn prepare_table_member(
        &mut self,
        handle: ArenaHandle,
        key: Value,
        next: Option<Value>,
    ) -> Result<Option<PreparedTableMember>, HeapFault> {
        let Ok(resident) = self.key(&key) else {
            return Ok(None);
        };
        let Some(snapshot) = self.table_snapshot(handle, &resident)? else {
            return Ok(None);
        };
        if matches!(snapshot.kind, TableKind::Set)
            && next.is_some_and(|value| !matches!(value, Value::Absent))
        {
            return Ok(None);
        }
        if matches!(snapshot.kind, TableKind::Record)
            && !matches!(key, Value::Text(_) | Value::LongText(_))
        {
            return Ok(None);
        }
        let prepared = snapshot.build(
            &self.objects,
            &self.facts,
            &self.layouts,
            Member::Table(key),
            Some(key),
            next,
        )?;
        prepared.reserve(self, 0)?;
        Ok(Some(prepared))
    }
}
impl TableSnapshot {
    pub(super) fn is_record(&self) -> bool {
        matches!(self.kind, TableKind::Record)
    }
    pub(super) fn old_key(&self) -> Option<Value> {
        self.old.map(|(key, _)| key)
    }
    pub(super) fn build(
        mut self,
        objects: &impl Bodies,
        facts: &impl FactTable,
        layouts: &layouts::Layouts,
        address: Member,
        key: Option<Value>,
        next: Option<Value>,
    ) -> Result<PreparedTableMember, HeapFault> {
        if self.old.is_none() && next.is_none() {
            return Ok(PreparedTableMember {
                version: self.version,
                handle: self.handle,
                address,
                key,
                next,
                edit: None,
            });
        }
        let before = self.values.len() as u64;
        let position = self.place.unwrap_or(self.values.len() / 2);
        let sequence = if let Some(at) = self.place {
            let sequence = *self.sequences.get(at).ok_or_else(|| {
                HeapFault::Malformed("a table position has no insertion sequence".into())
            })?;
            match next {
                Some(value) => self.values[at * 2 + 1] = value,
                None => {
                    drop(self.values.drain(at * 2..at * 2 + 2));
                    self.sequences.remove(at);
                }
            }
            sequence
        } else {
            let key = key.ok_or_else(|| {
                HeapFault::Malformed("a table insertion has no prepared key".into())
            })?;
            let value =
                next.ok_or_else(|| HeapFault::Malformed("a table insertion has no value".into()))?;
            let sequence = self.next_sequence;
            self.next_sequence = sequence
                .checked_add(1)
                .ok_or_else(|| HeapFault::Malformed("table insertion sequence overflow".into()))?;
            self.values.extend([key, value]);
            self.sequences.push(sequence);
            sequence
        };
        let (table, mut index) = tables::build(
            objects,
            facts,
            layouts,
            self.values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, value]| (*key, *value)),
        )
        .map_err(fault)?;
        index
            .resequence(self.sequences, self.next_sequence)
            .map_err(fault)?;
        let body = match self.kind {
            TableKind::Map => Body::Map(table),
            TableKind::Set => Body::Set(table),
            TableKind::Record => Body::Record(table),
        };
        let flat = sizes::summarize_flat(objects, &body);
        Ok(PreparedTableMember {
            version: self.version,
            handle: self.handle,
            address,
            key,
            next,
            edit: Some(TableEdit {
                body,
                index,
                flat,
                old: self.old,
                structural: self.old.is_none() || next.is_none(),
                sequence,
                resident_before: before,
                position,
            }),
        })
    }
}
impl PreparedTableMember {
    /// The final value of the private member, before any arena installation.
    pub fn value(&self) -> Option<Value> {
        self.next
    }

    /// Fold an overwrite of the same private member into its detached body.
    /// Insertion/removal order and key spelling remain those of the original
    /// reservation. A structural successor needs its own canonical boundary.
    /// The certificate, not this storage object, proves observer exclusion.
    pub fn stage(&mut self, value: Value) -> bool {
        if self.next.is_none() {
            return false;
        }
        let Some(prepared) = &mut self.edit else {
            return false;
        };
        if matches!(prepared.body, Body::Set(_)) && !matches!(value, Value::Absent) {
            return false;
        }
        let table = match &mut prepared.body {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table,
            _ => unreachable!("a prepared table has a table body"),
        };
        let Some(Chunk::Resident(values)) = table.chunks.get_mut(0) else {
            unreachable!("a prepared table has one resident chunk");
        };
        values[prepared.position * 2 + 1] = value;
        // Rebuild the derived summary once at publication, not per private
        // overwrite. No guest state or logical authority changes here.
        prepared.flat = None;
        self.next = Some(value);
        true
    }

    /// Exact source address and final value checked by the boundary ledger.
    pub fn edit(&self) -> MemberEdit {
        MemberEdit {
            handle: self.handle,
            member: self.address.clone(),
            next: self.next,
        }
    }

    pub(super) fn reserve(&self, arena: &mut Arena, batch: usize) -> Result<(), HeapFault> {
        reserve(&mut arena.zero, batch + 2)?;
        reserve(&mut arena.dirty, batch + 1)?;
        reserve(&mut arena.facts[self.handle.index() as usize].cards, 1)
    }
    /// Install the exact authenticated edit without a lookup or allocation.
    pub fn publish(self, arena: &mut Arena, edit: MemberEdit) {
        assert!(arena.registration_active() && arena.content_is(&self.version));
        assert!(
            edit.handle == self.handle
                && edit.member.same(&self.address)
                && match (edit.next, self.next) {
                    (Some(a), Some(b)) => a.same(&b),
                    (None, None) => true,
                    _ => false,
                }
        );
        let Some(mut prepared) = self.edit else {
            return;
        };
        prepared.flat = prepared
            .flat
            .or_else(|| sizes::summarize_flat(&arena.objects, &prepared.body));
        arena.resident = arena
            .resident
            .saturating_sub(prepared.resident_before)
            .saturating_add(body_values(&prepared.body));
        let Some(body) = arena.objects.get_mut(self.handle) else {
            unreachable!("an authenticated table edit lost its body");
        };
        *body = prepared.body;
        if prepared.structural {
            arena.facts[self.handle.index() as usize].revision = arena.facts
                [self.handle.index() as usize]
                .revision
                .saturating_add(1);
        }
        arena.facts[self.handle.index() as usize].index = Some(prepared.index);
        arena.facts[self.handle.index() as usize].flat = prepared.flat;
        match (prepared.old, self.next) {
            (None, Some(value)) => {
                let Some(key) = self.key else {
                    unreachable!("a prepared insertion has its key");
                };
                arena.hold(&key);
                arena.hold(&value);
            }
            (Some((_, old)), Some(value)) => {
                arena.hold(&value);
                arena.unhold(&old);
            }
            (Some((key, old)), None) => {
                arena.unhold(&key);
                arena.unhold(&old);
            }
            (None, None) => unreachable!("an absent removal changes no body"),
        }
        Arena::mark_resident(
            &mut arena.facts[self.handle.index() as usize],
            &mut arena.dirty,
            self.handle,
            Some(prepared.sequence / CARD),
        );
    }
}
