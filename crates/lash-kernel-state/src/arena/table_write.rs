//! A complete resident table edit is prepared without changing its guest body.
use super::*;
use lash_kernel_doc::heap::{HeapFault, HostResource};

/// A complete indexed edit with all installation storage already reserved.
pub struct PreparedTableMember {
    version: ContentVersion,
    handle: ArenaHandle,
    key: Value,
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
    /// Reserve normalized table storage, its index and mutation bookkeeping.
    /// Key and cold-read errors are resolved by the ordinary source caller.
    pub fn prepare_table_member(
        &mut self,
        handle: ArenaHandle,
        key: Value,
        next: Option<Value>,
    ) -> Result<Option<PreparedTableMember>, HeapFault> {
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
        if matches!(kind, TableKind::Record) && !matches!(key, Value::Text(_) | Value::LongText(_))
        {
            return Ok(None);
        }
        let Ok(resident_key) = self.key(&key) else {
            return Ok(None);
        };
        let Ok((table, index)) = self.table_view(handle) else {
            return Ok(None);
        };
        let Ok(held) = tables::pairs(handle, table) else {
            return Ok(None);
        };
        let place = tables::find(handle, table, index, &resident_key).map_err(fault)?;
        let old = place
            .map(|place| tables::pair(table, place).map(|pair| (pair[0], pair[1])))
            .transpose()
            .map_err(fault)?;
        if old.is_none() && next.is_none() {
            return Ok(Some(PreparedTableMember {
                version,
                handle,
                key,
                next,
                edit: None,
            }));
        }
        let mut values = Vec::new();
        reserve(&mut values, held.len() + 2)?;
        values.extend_from_slice(held);
        let mut sequences = Vec::new();
        reserve(&mut sequences, held.len() / 2 + 1)?;
        sequences.extend(index.sequences(0).iter());
        let before = held.len() as u64;
        let mut next_sequence = index.next();
        let sequence = if let Some(place) = place {
            if place.chunk != 0 {
                return Ok(None);
            }
            let sequence = *sequences.get(place.at).ok_or_else(|| {
                HeapFault::Malformed("a table position has no insertion sequence".into())
            })?;
            match next {
                Some(value) => values[place.at * 2 + 1] = value,
                None => {
                    drop(values.drain(place.at * 2..place.at * 2 + 2));
                    sequences.remove(place.at);
                }
            }
            sequence
        } else {
            let Some(value) = next else {
                unreachable!("an absent removal was prepared above");
            };
            let sequence = next_sequence;
            next_sequence = next_sequence
                .checked_add(1)
                .ok_or_else(|| HeapFault::Malformed("table insertion sequence overflow".into()))?;
            values.extend([key, value]);
            sequences.push(sequence);
            sequence
        };
        let (table, mut index) = tables::build(
            &self.objects,
            &self.facts,
            &self.layouts,
            values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, value]| (*key, *value)),
        )
        .map_err(fault)?;
        index.resequence(sequences, next_sequence).map_err(fault)?;
        let body = match kind {
            TableKind::Map => Body::Map(table),
            TableKind::Set => Body::Set(table),
            TableKind::Record => Body::Record(table),
        };
        let flat = self.summarize_flat(&body);
        reserve(&mut self.zero, 2)?;
        reserve(&mut self.dirty, 1)?;
        reserve(&mut self.facts[handle.index() as usize].cards, 1)?;
        Ok(Some(PreparedTableMember {
            version,
            handle,
            key,
            next,
            edit: Some(TableEdit {
                body,
                index,
                flat,
                old,
                structural: old.is_none() || next.is_none(),
                sequence,
                resident_before: before,
            }),
        }))
    }
}
impl PreparedTableMember {
    /// Install the one authenticated member edit without another allocation.
    pub fn publish(self, arena: &mut Arena, edit: crate::paged::logical::MemberEdit) {
        assert!(arena.registration_active() && arena.content_is(&self.version));
        assert!(
            edit.handle == self.handle
                && matches!(edit.member, crate::paged::logical::Member::Table(key) if key.same(&self.key))
                && match (edit.next, self.next) {
                    (Some(a), Some(b)) => a.same(&b),
                    (None, None) => true,
                    _ => false,
                }
        );
        let Some(prepared) = self.edit else {
            return;
        };
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
                arena.hold(&self.key);
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
