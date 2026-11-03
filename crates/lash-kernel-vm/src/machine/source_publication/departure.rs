//! Departure transfer, resource settlement and semantic unwind share one commit.
use super::*;
use return_storage::ReturnRecipe;
impl Context<'_> {
    pub(in crate::machine) fn publish_departure<S>(
        self,
        invocation: &Invocation,
        frontier: &mut Option<Frontier>,
        recipe: ReturnRecipe,
        storage: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, S),
    ) -> Result<bool, Halt> {
        let ReturnRecipe {
            start,
            next,
            allowance,
            exit,
            input,
            dependencies,
            debt,
        } = recipe;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        if let Some(frontier) = frontier.as_mut()
            && frontier.departs(&start)
        {
            frontier.handoff(&mut store.borrow_mut(), start.clone())?;
        }
        let ledger = store.borrow();
        if frontier.is_none() {
            *frontier = Some(
                self.source
                    .frontier(
                        &ledger,
                        self.heap,
                        invocation,
                        self.link,
                        self.kernel,
                        start,
                    )
                    .map_err(|_| malformed("departure frontier failed authentication"))?,
            );
        }
        let frontier = frontier
            .as_mut()
            .ok_or_else(|| malformed("departure frontier disappeared"))?;
        if allowance.safe_point != *self.resources.charged {
            return Ok(false);
        }
        let Some(resources) = self.resources.facts(&allowance) else {
            return Ok(false);
        };
        let registration = self.arena.registration().map_err(native_fault)?;
        let Some(mut prepared) = exit.prepare(
            self.source,
            frontier,
            invocation,
            &ledger,
            registration,
            self.heap,
            input,
            &dependencies,
            next,
            allowance,
            debt.as_ref(),
            resources,
        )?
        else {
            return Ok(false);
        };
        let memory_bound = match self.admission {
            Admission::Envelope(bound) => bound,
            Admission::Exact(mut memory) => {
                if memory.exceeded(prepared.peak()) {
                    prepared = prepared.collect(&ledger, self.heap)?;
                }
                memory.admit(prepared.peak())?
            }
        };
        let Some(accepted) = prepared.admit(frontier, &ledger, self.resources, memory_bound)?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted.publish_departure(frontier, &mut store.borrow_mut(), |arena| {
            semantic(arena, self.session, storage)
        })?;
        self.logical_memory.set(balance.total()?);
        #[cfg(any(test, feature = "testing"))]
        crate::machine::islands::witness::source_loop_published(
            &frontier.position().occurrence.loops,
        );
        Ok(true)
    }
}
