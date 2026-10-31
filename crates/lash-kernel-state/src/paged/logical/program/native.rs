//! Native construction follows its bound event schedule before the atomic
//! output transfer. Entity bytes come from canonical bodies, never an envelope.
use super::*;
use lash_kernel_doc::{
    NativeResourceContract, NativeResourceEvent as Event, NativeResourceRelease as Release,
};

impl JournalEvent {
    /// Verify the exact native declaration against the canonical projected
    /// graph. Both source authorization and journal preparation use this check.
    pub fn matches_native(
        &self,
        contract: &NativeResourceContract,
        heap: &PagedHeap,
        arena: &Arena,
    ) -> Result<bool, HeapFault> {
        if self.native.as_ref().is_some_and(|bound| bound != contract) {
            return Ok(false);
        }
        let declarations: Vec<_> = contract
            .events
            .iter()
            .filter_map(|event| {
                if let Event::Entity {
                    owner,
                    kind,
                    members,
                    ..
                } = event
                {
                    Some((*owner, *kind, members))
                } else {
                    None
                }
            })
            .collect();
        if declarations.len() != self.entities.len()
            || self.outputs.len() != 1
            || self.member.is_some()
        {
            return Ok(false);
        }
        let owners: BTreeMap<_, _> = self
            .entities
            .iter()
            .zip(&declarations)
            .map(|(entity, (owner, _, _))| (entity.entity, *owner))
            .collect();
        if owners.len() != declarations.len() {
            return Ok(false);
        }
        for (entity, (_, kind, members)) in self.entities.iter().zip(&declarations) {
            let (actual_kind, actual_members) = entity.body.native_members(heap, arena, &owners)?;
            if &actual_kind != kind || &actual_members != *members {
                return Ok(false);
            }
        }
        Ok(self.outputs[0].1.native_holding(heap, arena, &owners)? == contract.output)
    }

    pub(super) fn native_construction_peak(
        &self,
        heap: &PagedHeap,
        arena: &mut Arena,
    ) -> Result<u64, HeapFault> {
        let Some(contract) = &self.native else {
            return Ok(0);
        };
        if contract.retained_bytes().is_none() || !self.matches_native(contract, heap, arena)? {
            return Err(invalid(
                "native construction does not match its bound graph",
            ));
        }
        let mut bodies = self.entities.iter();
        let mut events = Vec::new();
        for event in &contract.events {
            let (bytes, release) = match event {
                Event::Entity { release, .. } => {
                    let entity = bodies
                        .next()
                        .ok_or_else(|| invalid("native construction body is absent"))?;
                    (entity.body.holding(heap, arena)?.bytes(), *release)
                }
                Event::Scratch { bytes, release, .. } => (*bytes, Some(*release)),
            };
            let end = match release {
                None => contract.events.len() + 1,
                Some(Release::Exit) => contract.events.len(),
                Some(Release::Before(owner)) => contract
                    .events
                    .iter()
                    .position(|event| event.owner() == owner)
                    .ok_or_else(|| invalid("native release names another schedule"))?,
            };
            events.push((bytes, end));
        }
        let mut live = 0_u64;
        let mut peak = 0_u64;
        for (at, (bytes, end)) in events.iter().enumerate() {
            if *end <= at {
                return Err(invalid(
                    "native construction release precedes its acquisition",
                ));
            }
            for (bytes, end) in &events[..at] {
                if *end == at {
                    live = live
                        .checked_sub(*bytes)
                        .ok_or_else(|| invalid("native construction release underflow"))?;
                }
            }
            live = live
                .checked_add(*bytes)
                .ok_or_else(|| invalid("native construction holding overflow"))?;
            peak = peak.max(live);
        }
        Ok(peak)
    }
}
