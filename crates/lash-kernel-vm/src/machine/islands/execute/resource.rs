//! A compiled scalar prefix publishes only surviving bindings. Its precise
//! source exit remains unpaid when the complete prefix cannot be admitted.
use super::super::compiled::resource::CertifiedStatement;
use super::*;
use crate::admission::SourceResources;
use crate::compile::room::publication::Occurrence;

#[cfg(test)]
thread_local! { static CERTIFIED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn take_certified_statements() -> u64 {
    CERTIFIED.with(|count| count.replace(0))
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn certified_statement(
        &mut self,
        frame: &mut Values<'_, '_>,
        certificate: &CertifiedStatement,
    ) -> Result<(), Stop> {
        let miss = |this: &mut Self| {
            this.scratch.exit.stands = Some(Stands::Statement);
            this.at(certificate.source_pc)
        };
        if OBSERVING || self.spent() || self.deferred_end.is_some() || self.arena.boundary_due() {
            return Err(miss(self));
        }
        let Some(Position::Block(block, next)) = frame.state.control.last() else {
            return Err(miss(self));
        };
        let source_block = &frame.state.callee.compiled.blocks[*block];
        if *next != certificate.index
            || frame.state.statement.is_some()
            || self.exe.block(source_block.id).stmts.get(*next) != Some(&certificate.id)
        {
            return Err(self.halt(fault(
                "compiled scalar packet is not at its source statement entry",
            )));
        }
        let Some(activation) = frame.state.source.as_ref() else {
            return Err(miss(self));
        };
        let invocation = activation.invocation.clone();
        // During construction this closes the preceding, unmigrated family.
        // The scalar prefix itself never records an interior replacement.
        self.logical
            .flush(self.arena, self.heap)
            .map_err(|error| self.halt(error.into()))?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let Some(evaluated) = certificate
            .evaluate(
                frame.state.callee.plan.code,
                frame.slots,
                &invocation,
                &ledger,
                self.heap,
            )
            .map_err(|error| self.halt(error.into()))?
        else {
            return Err(miss(self));
        };
        let Some(debt) = certificate.charges.bind(&[]) else {
            return Err(miss(self));
        };
        let Some(bound) = evaluated
            .bind(&invocation, &ledger, self.arena, self.heap)
            .map_err(|error| self.halt(error.into()))?
        else {
            return Err(miss(self));
        };
        let callee = frame.state.callee;
        let source = &self.exe.code(callee.plan.code).source_room;
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| self.halt(fault("compiled source has no defining document")))?;
        let kernel = lash_kernel_doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| self.halt(fault("compiled source kernel is unsupported")))?;
        let occurrence = Occurrence {
            task: self.task,
            invocation: invocation.id(),
            loops: self.source_loops(&frame.state.control),
            action: None,
        };
        let mut frontier = source
            .frontier(
                &ledger,
                self.heap,
                &invocation,
                callee.link,
                kernel,
                certificate.exit.start(occurrence),
            )
            .map_err(|_| self.halt(fault("compiled source entry failed authentication")))?;
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: (self.base + u64::from(self.depth)).saturating_sub(1),
            limit: self.call_depth,
            safe_point,
        };
        let registration = match self.arena.registration() {
            Ok(registration) => registration,
            Err(error) => {
                self.scratch.exit.halt =
                    Some(crate::machine::source_publication::native_fault(error).into());
                return Err(Stop);
            }
        };
        let accepted = match frontier.prepare_region(
            bound,
            &certificate.exit,
            &debt,
            resources,
            &ledger,
            registration,
            self.heap,
            self.memory_bound,
        ) {
            Ok(Some(accepted)) => accepted,
            Ok(None) => {
                self.scratch.exit.stands = Some(Stands::Statement);
                self.scratch.exit.at = Some(certificate.source_pc);
                return Err(Stop);
            }
            Err(error) => {
                self.scratch.exit.halt = Some(error.into());
                return Err(Stop);
            }
        };
        drop(ledger);
        let result = evaluated.result;
        let published = accepted.publish_region(&mut frontier, &mut store.borrow_mut(), || {
            frame.slots[certificate.target as usize] = SlotState::Value(result);
            let Some(Position::Block(_, next)) = frame.state.control.last_mut() else {
                unreachable!("the accepted source entry retains its enclosing block")
            };
            *next = certificate.index + 1;
        });
        let balance = match published {
            Ok(balance) => balance,
            Err(error) => {
                self.scratch.exit.halt = Some(error.into());
                return Err(Stop);
            }
        };
        frame
            .state
            .source
            .as_mut()
            .unwrap_or_else(|| unreachable!("accepted source invocation"))
            .frontier = Some(frontier);
        frame.state.statement = None;
        self.end_statement(frame);
        self.covered = false;
        self.logical_memory
            .set(balance.total().map_err(|error| self.halt(error.into()))?);
        if let Some(tape) = self.recorder {
            debt.record(tape);
        }
        #[cfg(test)]
        CERTIFIED.with(|count| count.set(count.get().saturating_add(1)));
        #[cfg(any(test, feature = "perf-witness"))]
        execution::statements(
            &self.scratch.census,
            Owner::RegisterProgram,
            1,
            self.scratch.counting,
        );
        Ok(())
    }
}
