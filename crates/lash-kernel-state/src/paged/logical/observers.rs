//! Observer exclusion follows the canonical root and member graph once at entry.
use super::*;
use std::collections::BTreeSet;

/// Affine evidence that the selected entities have no observer outside the
/// supplied activation roots. It is neither mutation nor source authority.
/// Any ledger change expires it, including a saved continuation or new alias.
pub struct ObserverExclusion {
    stamp: Stamp,
    entities: BTreeSet<EntityId>,
}
impl ObserverExclusion {
    pub fn permits(&self, ledger: &Ledger, entity: EntityId) -> bool {
        ledger.is_current(&self.stamp) && self.entities.contains(&entity)
    }
    pub(super) fn matches(&self, stamp: &Stamp) -> bool {
        Rc::ptr_eq(&self.stamp.identity, &stamp.identity) && self.stamp.revision == stamp.revision
    }
    pub(super) fn contains(&self, entity: EntityId) -> bool {
        self.entities.contains(&entity)
    }
}

impl Ledger {
    /// Check every semantic root, including zero-price captures, saved callers,
    /// departures, sessions and ended tasks. Immutable containers already expose
    /// their mutable identity edges in their canonical Holding.
    ///
    /// `allowed` is the source compiler's current activation footprint. Only
    /// binding, capture and evaluated operand roots of this invocation qualify;
    /// an external continuation cannot be relabeled as an internal observer.
    pub fn exclude_observers(
        &self,
        invocation: &Invocation,
        allowed: &[(Owner, Holding)],
        entities: &[EntityId],
        access: &Access<'_>,
    ) -> Result<Option<ObserverExclusion>, HeapFault> {
        let Owner::Frame {
            task,
            invocation: id,
        } = invocation.frame()
        else {
            return Ok(None);
        };
        if self.invocation(task, id, access).as_ref().ok() != Some(invocation)
            || allowed.is_empty()
            || entities.is_empty()
        {
            return Ok(None);
        }
        let roots: BTreeSet<_> = allowed.iter().map(|(owner, _)| *owner).collect();
        let targets: BTreeSet<_> = entities.iter().copied().collect();
        if roots.len() != allowed.len() || targets.len() != entities.len() {
            return Ok(None);
        }
        let mut private = VecDeque::new();
        for (owner, claimed) in allowed {
            if !matches!(owner, Owner::Value {
                task: actual_task, invocation: actual_id,
                slot: Slot::Binding(_) | Slot::Capture(_) | Slot::Operand(_),
            } if *actual_task == task && *actual_id == id)
            {
                return Ok(None);
            }
            let Some(value) = self.holding(*owner, access)? else {
                return Ok(None);
            };
            if !same_holding(&value, claimed, access)? {
                return Ok(None);
            }
            object_edges(&value, access, &mut private)?;
        }
        let private = self.observer_reach(private, access)?;
        if !targets.is_subset(&private) {
            return Ok(None);
        }
        let mut external = VecDeque::new();
        for tree in [&self.state.owners, &self.state.execution, &self.state.ended] {
            visit_tree(tree, access, &mut |root| {
                if !roots.contains(&root.owner) {
                    object_edges(&root.value, access, &mut external)?;
                }
                Ok(())
            })?;
        }
        let mut visited = BTreeSet::new();
        while let Some(entity) = external.pop_front() {
            if targets.contains(&entity) {
                return Ok(None);
            }
            if visited.insert(entity) {
                let Some(body) = self.entity_holding(entity, access)? else {
                    return Err(invalid("observer graph contains an absent entity"));
                };
                object_edges(&body, access, &mut external)?;
            }
        }
        Ok(Some(ObserverExclusion {
            stamp: self.stamp(),
            entities: targets,
        }))
    }

    fn observer_reach(
        &self,
        mut pending: VecDeque<EntityId>,
        access: &Access<'_>,
    ) -> Result<BTreeSet<EntityId>, HeapFault> {
        let mut reached = BTreeSet::new();
        while let Some(entity) = pending.pop_front() {
            if reached.insert(entity) {
                let Some(body) = self.entity_holding(entity, access)? else {
                    return Err(invalid("observer graph contains an absent entity"));
                };
                object_edges(&body, access, &mut pending)?;
            }
        }
        Ok(reached)
    }
}

fn object_edges(
    value: &Holding,
    access: &Access<'_>,
    pending: &mut VecDeque<EntityId>,
) -> Result<(), HeapFault> {
    value.each_edge(access, &mut |edge| {
        if let Identity::Object(entity) = edge.target {
            pending.push_back(entity);
        }
        Ok(())
    })
}

fn same_holding(a: &Holding, b: &Holding, access: &Access<'_>) -> Result<bool, HeapFault> {
    if a.bytes() != b.bytes() {
        return Ok(false);
    }
    let edges = |holding: &Holding| -> Result<BTreeMap<Identity, u64>, HeapFault> {
        let mut edges = BTreeMap::new();
        holding.each_edge(access, &mut |edge| {
            edges.insert(edge.target, edge.count);
            Ok(())
        })?;
        Ok(edges)
    };
    Ok(edges(a)? == edges(b)?)
}
