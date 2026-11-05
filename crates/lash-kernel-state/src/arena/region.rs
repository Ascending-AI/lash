//! Complete member storage for one authenticated region exit.
use super::*;
use crate::paged::logical::{Member, MemberEdit};
use lash_kernel_doc::heap::{HeapFault, HostResource};

pub(crate) struct RegionBodyMetadata {
    pub handle: ArenaHandle,
    pub content: u64,
    pub structural: u64,
    pub sequences: Option<(Vec<u64>, u64)>,
}
impl PreparedInstallation<'_> {
    pub(crate) fn prepare_lease_list(
        &self,
        handle: ArenaHandle,
        values: Vec<Value>,
        cards: Vec<u64>,
    ) -> Result<RegionMemberBody, HeapFault> {
        let arena = self.registration.read_arena();
        let before = arena.body(handle).map_err(fault)?;
        if !matches!(before, Body::List(_)) || !arena.facts(handle).is_ok_and(|facts| !facts.cold) {
            return Err(bad("lease list is cold or changed kind"));
        }
        let mut original = Vec::new();
        reserve(
            &mut original,
            usize::try_from(body_values(before)).map_err(|_| bad("list member count overflow"))?,
        )?;
        each_member(before, &mut |value| original.push(*value));
        let body = Body::List(Sequence::whole(values));
        let flat = sizes::summarize_flat(self, &body);
        Ok(RegionMemberBody {
            handle,
            body: Some(body),
            table: None,
            original,
            flat,
            cards,
            resident_before: body_values(before),
        })
    }
    pub(crate) fn region_metadata(
        &mut self,
        metadata: Vec<RegionBodyMetadata>,
    ) -> Result<(), HeapFault> {
        for version in metadata {
            let entry = self
                .batch
                .entries
                .iter_mut()
                .find(|entry| entry.handle == version.handle)
                .ok_or_else(|| bad("region metadata has no reserved header"))?;
            entry.facts.content_revision = version.content;
            entry.facts.revision = version.structural;
            if let Some((sequences, next)) = version.sequences {
                entry
                    .facts
                    .index
                    .as_mut()
                    .ok_or_else(|| bad("region table has no index"))?
                    .region_resequence(sequences, next)
                    .map_err(fault)?;
            }
        }
        Ok(())
    }
}

pub(crate) struct RegionMemberBody {
    handle: ArenaHandle,
    body: Option<Body>,
    table: Option<PreparedTableMember>,
    original: Vec<Value>,
    flat: Option<sizes::FlatSize>,
    cards: Vec<u64>,
    resident_before: u64,
}
fn fault(error: NativeError) -> HeapFault {
    match error {
        NativeError::Host(error) => error,
        error => HeapFault::Malformed(error.to_string()),
    }
}
fn bad(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
fn reserve<T>(values: &mut Vec<T>, count: usize) -> Result<(), HeapFault> {
    values.try_reserve(count).map_err(|_| HeapFault::Exhausted {
        resource: HostResource::StagingBytes,
    })
}

impl Arena {
    pub(crate) fn region_body_resident(&self, handle: ArenaHandle) -> Result<bool, NativeError> {
        Ok(!self.facts(handle)?.cold && !has_cold(self.body(handle)?))
    }

    pub(crate) fn region_key_tokens(
        &self,
        value: &Value,
        out: &mut lash_kernel_value::tokens::TokenWriter,
    ) -> Result<bool, NativeError> {
        keys::tokens(&self.objects, &self.facts, value, out)
    }

    pub(crate) fn prepare_region_revision(
        &self,
        handle: ArenaHandle,
        content: u64,
        structural: u64,
    ) -> Result<RegionBodyMetadata, HeapFault> {
        let facts = self.facts(handle).map_err(fault)?;
        Ok(RegionBodyMetadata {
            handle,
            content: facts
                .content_revision
                .checked_add(content)
                .ok_or_else(|| bad("region content revision overflow"))?,
            structural: facts
                .revision
                .checked_add(structural)
                .ok_or_else(|| bad("region structural revision overflow"))?,
            sequences: None,
        })
    }
    pub(crate) fn has_pending_source_changes(&self) -> bool {
        !self.rekeyed.is_empty() || !self.member_events.is_empty()
    }
    pub(crate) fn reserve_region_members(
        &mut self,
        prepared: &[RegionMemberBody],
        batch: usize,
        revisions: usize,
    ) -> Result<(), HeapFault> {
        let old = prepared.iter().try_fold(0usize, |count, body| {
            count
                .checked_add(
                    body.original.len()
                        + body
                            .table
                            .as_ref()
                            .map_or(0, PreparedTableMember::release_slots),
                )
                .ok_or_else(|| bad("member work queue overflow"))
        })?;
        reserve(
            &mut self.zero,
            old.checked_add(batch)
                .ok_or_else(|| bad("region work queue overflow"))?,
        )?;
        reserve(
            &mut self.dirty,
            prepared
                .len()
                .checked_add(batch)
                .and_then(|count| count.checked_add(revisions))
                .ok_or_else(|| bad("region work queue overflow"))?,
        )?;
        for body in prepared {
            reserve(
                &mut self.facts[body.handle.index() as usize].cards,
                body.cards.len()
                    + body
                        .table
                        .as_ref()
                        .map_or(0, PreparedTableMember::card_slots),
            )?;
        }
        Ok(())
    }
}
impl PreparedInstallation<'_> {
    pub(crate) fn prepare_region_cells(
        &self,
        edits: &[MemberEdit],
    ) -> Result<Vec<RegionMemberBody>, HeapFault> {
        let arena = self.registration.read_arena();
        let mut prepared = Vec::new();
        reserve(&mut prepared, edits.len())?;
        let mut seen = std::collections::BTreeSet::new();
        for edit in edits {
            if !matches!(edit.member, Member::Variable)
                || !seen.insert((edit.handle.index(), edit.handle.generation()))
            {
                return Err(bad("region cell patch repeats or changes its address"));
            }
            let Body::Cell(old) = arena.body(edit.handle).map_err(fault)? else {
                return Err(bad("region variable changed its body kind"));
            };
            if !arena
                .facts(edit.handle)
                .is_ok_and(|facts| !facts.cold && facts.content_revision != u64::MAX)
            {
                return Err(bad("region variable is cold or saturated"));
            }
            let body = Body::Cell(
                edit.next
                    .ok_or_else(|| bad("a variable cannot remove its member"))?,
            );
            let mut original = Vec::new();
            reserve(&mut original, 1)?;
            original.push(*old);
            prepared.push(RegionMemberBody {
                handle: edit.handle,
                flat: sizes::summarize_flat(self, &body),
                body: Some(body),
                table: None,
                original,
                cards: Vec::new(),
                resident_before: 1,
            });
        }
        Ok(prepared)
    }
}

impl RegionMemberBody {
    /// Only StateLease's fully prepared transaction calls this swap. Every
    /// address, value, generation and capacity was checked before it was built.
    pub(crate) fn table(handle: ArenaHandle, table: PreparedTableMember) -> Self {
        Self {
            handle,
            body: None,
            table: Some(table),
            original: Vec::new(),
            flat: None,
            cards: Vec::new(),
            resident_before: 0,
        }
    }
    pub(crate) fn publish(self, arena: &mut Arena) {
        if let Some(table) = self.table {
            table.publish_region(arena);
            return;
        }
        let body = self
            .body
            .unwrap_or_else(|| unreachable!("prepared region body"));
        arena.hold_members(&body);
        for value in &self.original {
            arena.unhold(value);
        }
        arena.resident = arena
            .resident
            .saturating_sub(self.resident_before)
            .saturating_add(body_values(&body));
        let Some(held) = arena.objects.get_mut(self.handle) else {
            unreachable!("exclusive prepared region member lost its body");
        };
        *held = body;
        let facts = &mut arena.facts[self.handle.index() as usize];
        facts.flat = self.flat;
        if self.cards.is_empty() {
            Arena::mark_resident(facts, &mut arena.dirty, self.handle, None);
        } else {
            for card in self.cards {
                Arena::mark_resident(facts, &mut arena.dirty, self.handle, Some(card));
            }
        }
    }
}

pub(crate) fn region_mutable_body(body: &Body) -> bool {
    is_mutable(body)
}
