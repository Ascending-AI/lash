//! Source root publication from values or existing authenticated holdings.
use super::*;
use crate::arena::Arena;
use lash_kernel_doc::{Value, arena::ArenaHandle};
use std::collections::BTreeSet;

/// Outputs name semantic owners. A copy uses the ledger's existing Holding;
/// a new value uses the canonical value projection, never a caller's byte delta.
#[derive(Clone, Copy)]
pub enum RootValue<'a> {
    Value(&'a Value),
    Existing(Owner),
    Frame,
    Capture(ArenaHandle),
}

impl Ledger {
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
            let holding = match value {
                RootValue::Frame if matches!(owner, Owner::Frame { .. }) => Holding::frame(),
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
            };
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
        mut pending: Vec<EntityId>,
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
            let handle = arena
                .logical_handle(entity)
                .ok_or_else(|| invalid("new source identity has no supplied body"))?;
            let body = Holding::native_body(arena, access, handle)?;
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
        for entity in bodies.keys() {
            changes.push(Change::Allocate {
                entity: *entity,
                body: Holding::scalar(Cost::OBJECT),
            });
        }
        for (entity, body) in bodies {
            changes.push(Change::Mutate { entity, body });
        }
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
