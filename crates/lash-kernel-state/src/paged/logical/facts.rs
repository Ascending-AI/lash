//! Present observations belong to one ledger view. Drafts start empty and
//! never share their memo with the committed view or with a sibling draft.
use super::*;
use crate::arena::{Arena, ContentVersion, each_member};
use lash_kernel_doc::{Value, arena::ArenaHandle};
use std::collections::HashMap;

const ROWS: usize = 64;
const PROJECTIONS: usize = 16;
const PROJECTION_DEPENDENCIES: usize = 64;

#[derive(Default)]
pub(super) struct Facts {
    revision: Option<u64>,
    rows: HashMap<Owner, Holding, foldhash::fast::RandomState>,
    projections: Vec<Projection>,
    #[cfg(test)]
    disabled: bool,
}
impl Facts {
    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.projections.clear();
        self.revision = None;
    }
    fn at(&mut self, revision: u64) {
        if self.revision != Some(revision) {
            self.rows.clear();
            self.projections.clear();
            self.revision = Some(revision);
        }
    }
    fn enabled(&self) -> bool {
        #[cfg(test)]
        if self.disabled {
            return false;
        }
        true
    }
}

struct Projection {
    value: Value,
    holding: Holding,
    dependencies: Vec<(ArenaHandle, ContentVersion, Option<EntityId>)>,
}
impl Projection {
    fn capture(value: Value, holding: Holding, arena: &Arena) -> Option<Self> {
        let mut dependencies = Vec::new();
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            let Some(handle) = value.handle() else {
                continue;
            };
            if dependencies.iter().any(|(seen, _, _)| *seen == handle) {
                continue;
            }
            let facts = arena.facts(handle).ok()?;
            let entity = facts.ledger_entity();
            if entity.is_some_and(|entity| entity.family == FamilyId::PROVISIONAL) {
                return None;
            }
            dependencies.push((handle, arena.content_version(handle)?, entity));
            if matches!(value, Value::Tuple(_) | Value::Error(_)) {
                let mut overflow = false;
                each_member(arena.body(handle).ok()?, &mut |member| {
                    if pending.len() + dependencies.len() < PROJECTION_DEPENDENCIES {
                        pending.push(*member);
                    } else {
                        overflow = true;
                    }
                });
                if overflow {
                    return None;
                }
            }
        }
        Some(Self {
            value,
            holding,
            dependencies,
        })
    }
    fn current(&self, arena: &Arena) -> bool {
        self.dependencies.iter().all(|(handle, version, entity)| {
            arena.content_is(version)
                && arena
                    .facts(*handle)
                    .is_ok_and(|facts| facts.ledger_entity() == *entity)
        })
    }
}

impl Ledger {
    #[cfg(test)]
    pub(crate) fn without_facts_for_law(mut self) -> Self {
        self.facts.get_mut().disabled = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn sibling_for_law(&self) -> Self {
        Self {
            state: self.state.clone(),
            identity: Rc::clone(&self.identity),
            revision: self.revision,
            facts: Default::default(),
        }
    }

    pub fn holding(&self, owner: Owner, access: &Access<'_>) -> Result<Option<Holding>, HeapFault> {
        {
            let mut facts = self.facts.borrow_mut();
            facts.at(self.revision);
            if facts.enabled()
                && let Some(value) = facts.rows.get(&owner)
            {
                return Ok(Some(value.clone()));
            }
        }
        let value = self
            .state
            .roots(owner)
            .get(&owner_key(owner), access)?
            .map(|row| row.value);
        if let Some(value) = &value {
            let mut facts = self.facts.borrow_mut();
            if facts.enabled() {
                if facts.rows.len() == ROWS {
                    facts.rows.clear();
                }
                facts.rows.insert(owner, value.clone());
            }
        }
        Ok(value)
    }

    pub(super) fn project_value(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        value: &Value,
    ) -> Result<Holding, HeapFault> {
        // Inline scalar projection has no index or identity walk to memoize.
        if value.handle().is_none() {
            return Holding::from_native(arena, access, value);
        }
        {
            let mut facts = self.facts.borrow_mut();
            facts.at(self.revision);
            if facts.enabled()
                && let Some(projection) = facts
                    .projections
                    .iter()
                    .find(|projection| projection.value.same(value) && projection.current(arena))
            {
                return Ok(projection.holding.clone());
            }
        }
        let holding = Holding::from_native(arena, access, value)?;
        if !holding.has_provisional_edges(access)?
            && let Some(projection) = Projection::capture(*value, holding.clone(), arena)
        {
            let mut facts = self.facts.borrow_mut();
            if facts.enabled() {
                if facts.projections.len() == PROJECTIONS {
                    facts.projections.clear();
                }
                facts.projections.push(projection);
            }
        }
        Ok(holding)
    }
}
