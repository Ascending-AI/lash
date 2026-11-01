//! A direct return transfers its real RHS, then leaves the same Invocation.
use super::*;
use room::{Phase as SourcePhase, Point};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_return_suffix(
        &mut self,
        frame: &mut Values<'_, '_>,
        value: Value,
    ) -> Result<ValuePublication, HeapFault> {
        if !self.source_return_frame(frame)
            || self.tape.is_some()
            || self.projection().is_some()
            || frame
                .state
                .source
                .as_ref()
                .and_then(|source| source.frontier.as_ref())
                .is_some_and(|frontier| frontier.position().unpaid.is_none())
        {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(returned) = frame.state.returned else {
            return Ok(ValuePublication::Unavailable);
        };
        if frame.state.statement != Some(returned.statement) {
            return Ok(ValuePublication::Unavailable);
        }
        let (stream, statement, _) = self.exe.statement(returned.statement);
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: statement.site.clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(_) = source.compile_return_suffix(&at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, returned.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| reg.0 == returned.reg) else {
            return Ok(ValuePublication::Unavailable);
        };
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("return has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("return source owners failed verification"))?;
        let root = |selector| {
            slots
                .root(&selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("return has no canonical source owner"))
        };
        let input_selector = room::Owner::Operand((**rhs).clone());
        let input = root(input_selector)?;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(input, self.heap.pages())?.is_none() {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(recipe) = (crate::machine::source_publication::return_storage::ReturnInput {
            source,
            invocation: &activation.invocation,
            task: self.task,
            at: &statement.site,
            rhs,
            input,
            value,
            safe_point: self.charged,
        })
        .prepare(&ledger, self.arena, self.heap)?
        else {
            return Ok(ValuePublication::Declined);
        };
        let crate::machine::source_publication::return_storage::ReturnRecipe { prefix, program } =
            recipe;
        drop(ledger);
        let published = self.publish_source(
            frame.state.callee,
            activation,
            prefix,
            || program.clone(),
            |_, edits| debug_assert!(edits.is_empty()),
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::return_published();
        }
        Ok(if published {
            ValuePublication::Published(value)
        } else {
            ValuePublication::Declined
        })
    }
}
