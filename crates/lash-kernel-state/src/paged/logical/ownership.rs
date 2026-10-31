//! Source root publication from values or existing authenticated holdings.
use super::*;
use crate::arena::Arena;
use crate::paged::heap_impl::PagedHeap;
use lash_kernel_doc::{Value, arena::ArenaHandle};
use std::collections::{BTreeSet, HashMap};

/// Outputs name semantic owners. A copy uses the ledger's existing Holding;
/// a new value uses the canonical value projection, never a caller's byte delta.
#[derive(Clone, Copy)]
pub enum RootValue<'a> {
    Value(&'a Value),
    Page(&'a lash_kernel_doc::heap::PageValue),
    Frozen(&'a crate::paged::FrozenRef),
    Existing(Owner),
    Frame,
    Capture(ArenaHandle),
}

impl Ledger {
    /// Runtime ownership uses resident bodies when present and canonical heap
    /// descriptors otherwise. Reading cold edges never warms arena buffers.
    pub fn prepare_source_ownership(
        &self,
        arena: &mut Arena,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Prepared, HeapFault> {
        self.prepare_source_event(arena, heap, inputs, outputs, &[])
    }
    pub(super) fn prepare_saved_ownership(
        &self,
        heap: &PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
        environments: &[EntityId],
    ) -> Result<Prepared, HeapFault> {
        let access = heap.pages();
        let mut arena = Arena::new();
        let mut pending = Vec::new();
        let roots = self.ownership_changes(&mut arena, access, inputs, outputs, &mut pending)?;
        let mut mutations = Vec::new();
        for entity in environments.iter().copied().collect::<BTreeSet<_>>() {
            if self.entity_holding(entity, access)?.is_some() {
                let body = Holding::heap_body(heap, entity)?;
                body.each_edge(access, &mut |edge| {
                    if let Identity::Object(child) = edge.target {
                        pending.push(child);
                    }
                    Ok(())
                })?;
                mutations.push(Change::Mutate { entity, body });
            }
        }
        let mut changes = self.allocate_membership_from(&mut arena, access, pending, Some(heap))?;
        changes.extend(mutations);
        changes.push(Change::Roots(roots));
        self.prepare_event(changes, access)
    }
    pub(super) fn prepare_source_event(
        &self,
        arena: &mut Arena,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
        members: &[crate::arena::MemberEvent],
    ) -> Result<Prepared, HeapFault> {
        let access = heap.pages();
        let mut pending = Vec::new();
        let roots = self.ownership_changes(arena, access, inputs, outputs, &mut pending)?;
        let mut mutations = Vec::new();
        let mut membership = HashMap::<EntityId, bool, foldhash::fast::RandomState>::default();
        for event in members {
            let present = if members.len() >= 32 {
                if let Some(present) = membership.get(&event.entity) {
                    *present
                } else {
                    let present = self.entity_holding(event.entity, access)?.is_some();
                    membership.insert(event.entity, present);
                    present
                }
            } else {
                self.entity_holding(event.entity, access)?.is_some()
            };
            if !present {
                // Include fresh mutated identities even when their final
                // external owner was elided. Self-cycles remain source-retained.
                pending.push(event.entity);
                continue;
            }
            let remove = members::occurrence(arena, access, event.remove)?;
            let add = members::occurrence(arena, access, event.add)?;
            add.each_edge(access, &mut |edge| {
                if let Identity::Object(entity) = edge.target {
                    pending.push(entity);
                }
                Ok(())
            })?;
            mutations.push(Change::Members {
                entity: event.entity,
                remove,
                add: Box::new(add),
            });
        }
        let mut changes = self.allocate_membership_from(arena, access, pending, Some(heap))?;
        changes.extend(mutations);
        changes.push(Change::Roots(roots));
        self.prepare_event(changes, access)
    }
    pub fn prepare_ownership_at(
        &self,
        stamp: &Stamp,
        arena: &mut Arena,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Prepared, HeapFault> {
        if !self.is_current(stamp) {
            return Err(invalid("stale source facts"));
        }
        self.prepare_ownership(arena, access, inputs, outputs)
    }
    /// Stage one source ownership frontier and newly reached entity membership.
    /// Inputs must exist. Replacing a destination must consume its old owner.
    /// This does not refresh existing mutable bodies from resident storage:
    /// mutations remain explicit checked Change::Mutate events.
    pub fn prepare_ownership(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Prepared, HeapFault> {
        let mut pending = Vec::new();
        let roots = self.ownership_changes(arena, access, inputs, outputs, &mut pending)?;
        let mut changes = self.allocate_membership(arena, access, pending)?;
        changes.push(Change::Roots(roots));
        self.prepare_event(changes, access)
    }
    /// The source release fact is evaluated with the result's existing graph
    /// protected. An alias or a fresh wrapper must not retire the body that
    /// the result is about to own. This borrows reach, with no new occurrence,
    /// and changes only a cloned graph. Identity projection belongs to the
    /// caller's Registration until its eventual publication.
    pub fn release_cost_preserving(
        &self,
        arena: &mut Arena,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<u64, HeapFault> {
        let access = heap.pages();
        let mut pending = Vec::new();
        for (owner, value) in outputs {
            self.project_root(arena, access, owner, value)?
                .each_edge(access, &mut |edge| {
                    if let Identity::Object(entity) = edge.target {
                        pending.push(entity);
                    }
                    Ok(())
                })?;
        }
        self.release_cost_protecting(arena, heap, inputs, pending)
    }
    /// A detached wrapper preserves the exact child graph of its prospective
    /// canonical body without introducing synthetic owner occurrences.
    pub fn release_cost_preserving_body(
        &self,
        arena: &mut Arena,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        body: &VirtualBody,
    ) -> Result<u64, HeapFault> {
        let access = heap.pages();
        let mut pending = Vec::new();
        body.holding(heap, arena)?.each_edge(access, &mut |edge| {
            if let Identity::Object(entity) = edge.target {
                pending.push(entity);
            }
            Ok(())
        })?;
        self.release_cost_protecting(arena, heap, inputs, pending)
    }
    fn release_cost_protecting(
        &self,
        arena: &mut Arena,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        mut pending: Vec<EntityId>,
    ) -> Result<u64, HeapFault> {
        let access = heap.pages();
        let mut protected = BTreeSet::new();
        let mut fresh = BTreeSet::new();
        while let Some(entity) = pending.pop() {
            if self.entity_holding(entity, access)?.is_some() {
                protected.insert(entity);
            } else if fresh.insert(entity) {
                let body = if arena.holds_resident(entity) {
                    let handle = arena
                        .logical_handle(entity)
                        .ok_or_else(|| invalid("source result body is absent"))?;
                    Holding::native_body(arena, access, handle)?
                } else {
                    Holding::heap_body(heap, entity)?
                };
                body.each_edge(access, &mut |edge| {
                    if let Identity::Object(child) = edge.target {
                        pending.push(child);
                    }
                    Ok(())
                })?;
            }
        }
        let protection =
            Holding::with_edges(0, protected.into_iter().map(Identity::Object), access)?;
        let mut edit = Edit {
            state: self.state.clone(),
            access,
            zero: VecDeque::new(),
            fresh: Vec::new(),
        };
        edit.retain(&protection)?;
        let before = edit.state.balance()?.total()?;
        edit.roots(inputs.iter().map(|owner| (*owner, None)).collect())?;
        before
            .checked_sub(edit.state.balance()?.total()?)
            .ok_or_else(|| invalid("source release grew its balance"))
    }
    fn project_root(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        owner: &Owner,
        value: &RootValue<'_>,
    ) -> Result<Holding, HeapFault> {
        Ok(match value {
            RootValue::Frame if matches!(owner, Owner::Frame { .. }) => Holding::frame(),
            RootValue::Frozen(value) if matches!(owner, Owner::Incoming(_)) => Holding::scalar(
                super::super::family_impl::frozen_holding_cost(access, value)?,
            ),
            RootValue::Page(value) if !matches!(owner, Owner::Frame { .. }) => {
                Holding::from_page(access, value)?
            }
            RootValue::Value(value) if !matches!(owner, Owner::Frame { .. }) => {
                Holding::from_native(arena, access, value)?
            }
            RootValue::Existing(from) => self
                .holding(*from, access)?
                .ok_or_else(|| invalid("source copy owner is absent"))?,
            RootValue::Capture(handle)
                if matches!(
                    owner,
                    Owner::Value {
                        slot: Slot::Capture(_),
                        ..
                    }
                ) =>
            {
                Holding::borrowed_cell(arena, access, *handle)?
            }
            _ => return Err(invalid("source output has another owner kind")),
        })
    }
    pub(super) fn ownership_changes(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
        pending: &mut Vec<EntityId>,
    ) -> Result<Vec<(Owner, Option<Holding>)>, HeapFault> {
        let mut consumed = BTreeSet::new();
        let mut roots = BTreeMap::new();
        for owner in inputs {
            if !consumed.insert(*owner) {
                return Err(invalid("source input occurs twice"));
            }
            if self.holding(*owner, access)?.is_none() {
                return Err(invalid("source input owner is absent"));
            }
            roots.insert(*owner, None);
        }
        let mut produced = BTreeSet::new();
        for (owner, value) in outputs {
            if !produced.insert(*owner) {
                return Err(invalid("source output occurs twice"));
            }
            if !consumed.contains(owner) && self.holding(*owner, access)?.is_some() {
                return Err(invalid("source destination was not consumed"));
            }
            let holding = self.project_root(arena, access, owner, value)?;
            holding.each_edge(access, &mut |edge| {
                if let Identity::Object(entity) = edge.target {
                    pending.push(entity);
                }
                Ok(())
            })?;
            roots.insert(*owner, Some(holding));
        }
        Ok(roots.into_iter().collect())
    }
    pub(super) fn allocate_membership(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        pending: Vec<EntityId>,
    ) -> Result<Vec<Change>, HeapFault> {
        self.allocate_membership_from(arena, access, pending, None)
    }
    pub(super) fn allocate_membership_from(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        mut pending: Vec<EntityId>,
        heap: Option<&super::super::heap_impl::PagedHeap>,
    ) -> Result<Vec<Change>, HeapFault> {
        let mut bodies = BTreeMap::new();
        while let Some(entity) = pending.pop() {
            if bodies.contains_key(&entity)
                || self
                    .state
                    .entities
                    .get(&entity_key(entity), access)?
                    .is_some()
            {
                continue;
            }
            let body = if arena.holds_resident(entity) {
                let handle = arena
                    .logical_handle(entity)
                    .ok_or_else(|| invalid("resident source identity has no body"))?;
                Holding::native_body(arena, access, handle)?
            } else if let Some(heap) = heap {
                Holding::heap_body(heap, entity)?
            } else {
                return Err(invalid("new source identity has no supplied body"));
            };
            body.each_edge(access, &mut |edge| {
                if let Identity::Object(child) = edge.target {
                    pending.push(child);
                }
                Ok(())
            })?;
            bodies.insert(entity, body);
        }
        // Declare all headers before their member edges. This permits cycles
        // without recursively allocating a body or losing a forward edge.
        let mut changes = Vec::new();
        let mut members = Vec::new();
        for (entity, body) in bodies {
            if body.links.is_empty() {
                changes.push(Change::Allocate { entity, body });
            } else {
                changes.push(Change::Allocate {
                    entity,
                    body: Holding::scalar(Cost::OBJECT),
                });
                members.push(Change::Mutate { entity, body });
            }
        }
        changes.extend(members);
        Ok(changes)
    }
}

impl Prepared {
    /// Extend an unpublished source journal. Each event sees its predecessor's
    /// itemized graph, while the accepted frontier stays the original ledger's.
    pub fn then_ownership(
        self,
        arena: &mut Arena,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Self, HeapFault> {
        let staged = Ledger {
            state: self.next,
            identity: self.identity,
            revision: self.revision,
        };
        let mut next = staged.prepare_ownership(arena, access, inputs, outputs)?;
        next.projected.extend(self.projected);
        Ok(Self {
            before: self.before,
            peak: self.peak.max(next.peak),
            ..next
        })
    }
}
