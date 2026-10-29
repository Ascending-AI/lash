//! Residency changes consume an existing source fact, never a byte claim.
use super::*;
use crate::arena::Arena;
use lash_kernel_doc::arena::ArenaHandle;

pub struct WarmFact {
    stamp: Stamp,
    entity: EntityId,
    handle: ArenaHandle,
    balance: Balance,
    generation: crate::arena::ContentVersion,
    events: usize,
}
impl Ledger {
    pub fn authenticate_warm(
        &self,
        arena: &Arena,
        handle: ArenaHandle,
        access: &Access<'_>,
    ) -> Result<WarmFact, HeapFault> {
        let entity = arena
            .facts(handle)
            .map_err(|error| invalid(&error.to_string()))?
            .entity
            .ok_or_else(|| invalid("warming has no source identity"))?;
        if self.entity_holding(entity, access)?.is_none() {
            return Err(invalid("warming has no existing logical fact"));
        }
        Ok(WarmFact {
            events: arena
                .logical_member_frontier()
                .ok_or_else(|| invalid("warming requires canonical member observation"))?,
            stamp: self.stamp(),
            entity,
            handle,
            generation: arena
                .content_version(handle)
                .ok_or_else(|| invalid("warming fact has no generation"))?,
            balance: self.balance()?,
        })
    }
}
impl WarmFact {
    /// Materialization may change buffers, placeholders and residency views.
    /// Its identity and the committed source ledger must remain unchanged.
    pub fn finish(self, ledger: &Ledger, arena: &Arena) -> Result<Balance, HeapFault> {
        if !ledger.is_current(&self.stamp)
            || !arena.same_generation(&self.generation)
            || arena.logical_member_frontier() != Some(self.events)
            || arena
                .facts(self.handle)
                .map_err(|error| invalid(&error.to_string()))?
                .entity
                != Some(self.entity)
            || ledger.balance()? != self.balance
        {
            return Err(invalid("warming changed its source fact"));
        }
        Ok(self.balance)
    }
}
