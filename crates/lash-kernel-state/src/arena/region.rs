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
            structural: false,
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
    structural: bool,
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
fn copied(values: &[Value]) -> Result<Vec<Value>, HeapFault> {
    let mut result = Vec::new();
    reserve(&mut result, values.len())?;
    result.extend_from_slice(values);
    Ok(result)
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
    pub(crate) fn prepare_region_members(
        &mut self,
        edits: &[MemberEdit],
    ) -> Result<Vec<RegionMemberBody>, HeapFault> {
        let mut prepared = Vec::new();
        for edits in groups(edits) {
            if matches!(
                self.registration.read_arena().body(edits[0].handle),
                Ok(Body::Map(_) | Body::Set(_) | Body::Record(_))
            ) {
                let table = self
                    .prepare_region_table_members(&edits)?
                    .ok_or_else(|| bad("region table patch unavailable"))?;
                prepared.push(RegionMemberBody::table(edits[0].handle, table));
            } else {
                prepared.extend(prepare(self.registration.read_arena(), self, &edits)?);
            }
        }
        Ok(prepared)
    }
}
fn prepare(
    arena: &Arena,
    objects: &impl Bodies,
    edits: &[MemberEdit],
) -> Result<Vec<RegionMemberBody>, HeapFault> {
    let mut groups: Vec<(ArenaHandle, Vec<&MemberEdit>)> = Vec::new();
    for edit in edits {
        if let Some((_, group)) = groups.iter_mut().find(|(handle, _)| *handle == edit.handle) {
            group.push(edit);
        } else {
            groups.push((edit.handle, vec![edit]));
        }
    }
    let mut result = Vec::new();
    reserve(&mut result, groups.len())?;
    for (handle, edits) in groups {
        let before = arena.body(handle).map_err(fault)?;
        if !arena
            .facts(handle)
            .is_ok_and(|facts| !facts.cold && facts.content_revision != u64::MAX)
        {
            return Err(bad("region member target is cold, stale, or saturated"));
        }
        let mut original = Vec::new();
        each_member(before, &mut |value| original.push(*value));
        let mut cards = Vec::new();
        let mut structural = false;
        let body = match before {
            Body::Cell(old) => {
                if edits.len() != 1 || !matches!(edits[0].member, Member::Variable) {
                    return Err(bad("region repeats or changes a variable address"));
                }
                let _ = old;
                Body::Cell(
                    edits[0]
                        .next
                        .ok_or_else(|| bad("a variable cannot remove its member"))?,
                )
            }
            Body::List(_) => {
                let mut values = copied(arena.sequence(handle).map_err(fault)?)?;
                let initial = values.len();
                let mut seen = std::collections::BTreeSet::new();
                let edit_count = edits.len();
                for edit in edits {
                    let Member::List(at) = edit.member else {
                        return Err(bad("list member has another address kind"));
                    };
                    if !seen.insert(at) {
                        return Err(bad("region repeats a final list member"));
                    }
                    let at = usize::try_from(at).map_err(|_| bad("list index overflow"))?;
                    if at > initial || at > values.len() {
                        return Err(bad("region list member is absent"));
                    }
                    match (at == values.len(), edit.next) {
                        (true, Some(value)) => {
                            reserve(&mut values, 1)?;
                            values.push(value);
                        }
                        (false, Some(value)) => values[at] = value,
                        (false, None) if edit_count == 1 => {
                            values.remove(at);
                            cards.extend(at as u64 / CARD..=(initial as u64 - 1) / CARD);
                            structural = true;
                        }
                        _ => return Err(bad("region list removal needs its own boundary")),
                    }
                    cards.push(at as u64 / CARD);
                }
                Body::List(Sequence::whole(values))
            }
            _ => return Err(bad("region member target has no mutable body")),
        };
        cards.sort_unstable();
        cards.dedup();
        let flat = sizes::summarize_flat(objects, &body);
        result.push(RegionMemberBody {
            handle,
            body: Some(body),
            table: None,
            original,
            flat,
            cards,
            structural,
            resident_before: body_values(before),
        });
    }
    Ok(result)
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
            structural: false,
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
        if self.structural {
            facts.revision = facts.revision.saturating_add(1);
        }
        if self.cards.is_empty() {
            Arena::mark_resident(facts, &mut arena.dirty, self.handle, None);
        } else {
            for card in self.cards {
                Arena::mark_resident(facts, &mut arena.dirty, self.handle, Some(card));
            }
        }
    }
}

fn groups(edits: &[MemberEdit]) -> Vec<Vec<MemberEdit>> {
    let mut groups: Vec<Vec<MemberEdit>> = Vec::new();
    for edit in edits {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group[0].handle == edit.handle)
        {
            group.push(edit.clone());
        } else {
            groups.push(vec![edit.clone()]);
        }
    }
    groups
}

pub(crate) fn region_mutable_body(body: &Body) -> bool {
    is_mutable(body)
}
