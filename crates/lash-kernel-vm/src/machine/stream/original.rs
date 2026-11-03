//! Paid constructor prefixes return to the existing precise operation owner.
use super::*;
use crate::compile::room::charges::AcceptedOriginalPrefix;
use lash_kernel_exec::{Weight, charge::RegionEvent};
use lash_kernel_state::kernel_internals::logical::PublicationValue;

impl KernelMachine {
    /// This handoff is synchronous. The receipt and its physical placement are
    /// consumed before the statement can reach a durable scheduling boundary.
    pub(in crate::machine) fn run_original_prefix(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        prefix: AcceptedOriginalPrefix,
    ) -> Eval<Ran> {
        #[cfg(any(test, feature = "perf-witness"))]
        let _execution = execution::Scope::enter(
            &self.islands.census,
            execution::Owner::SourceEvaluator,
            self.islands.counting,
        );
        if self.region.is_some() {
            return Err(fault("original prefix entered an open source region").into());
        }
        let (code, invocation) = {
            let frame = self.frame(running.task)?;
            (frame.code, frame.invocation)
        };
        let source = &running.exe.code(code).source_room;
        let store = self.logical.ledger();
        let replay = prefix
            .into_replay(
                source,
                &store.borrow(),
                code,
                running.id,
                running.task,
                invocation,
                self.charged,
            )
            .map_err(|_| fault("original prefix has another source continuation"))?;
        let roots = self
            .frame(running.task)?
            .logical
            .as_ref()
            .ok_or_else(|| fault("original source handoff has no frame ownership"))?
            .bank();
        self.restore_original_bank(running, replay.next, invocation, roots, replay.roles)?;
        self.source_begin(running.task, running.stream, running.stmt, replay.next)?;
        let mut pc = replay.next;
        for (at, event) in replay.events {
            self.run_original_operations(running, &mut pc, at)?;
            self.charge_original_event(event)?;
        }
        self.run_original_operations(running, &mut pc, replay.stop)?;
        self.run_stream_entered(host, running, replay.stop as usize)
    }

    fn restore_original_bank(
        &mut self,
        running: &Running<'_>,
        pc: u32,
        invocation: u64,
        roots: crate::machine::logical_held::BankRoots,
        roles: Box<[(u32, lash_kernel_value::sites::Site, PublicationValue)]>,
    ) -> Eval<()> {
        use crate::machine::registers::{ResumeAt, ResumeValue};
        let mut values = Vec::with_capacity(roles.len());
        let store = self.logical.ledger();
        for (reg, site, value) in roles {
            let owner = roots.operand(&site)?;
            let holding = store
                .borrow()
                .holding(owner, self.heap.pages())?
                .ok_or_else(|| fault("original source role lost its accepted owner"))?;
            let value = match value {
                PublicationValue::Native(value) => value,
                PublicationValue::Page(value) => super::super::project::restore_source(
                    &mut self.arena.borrow_mut(),
                    self.heap.pages(),
                    &value,
                )?,
                _ => return Err(fault("original prefix has another value provenance").into()),
            };
            values.push(ResumeValue {
                reg: Reg(reg),
                site: Arc::new(site),
                held: crate::machine::logical_held::Held {
                    value,
                    holding,
                    owner: Some(owner),
                },
            });
        }
        let bank = self
            .regs
            .prepare_source_resume(
                ResumeAt {
                    task: running.task,
                    invocation,
                    statement: running.stmt.site.clone(),
                    pc,
                },
                roots,
                running.stmt.regs,
                values,
            )?
            .ok_or_else(|| fault("original source bank changed"))?;
        bank.publish();
        self.regs.bind_source_resume(&store.borrow());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn check_original_recovery_bank(
        &mut self,
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        invocation: &lash_kernel_state::kernel_internals::logical::Invocation,
        replay: crate::compile::room::charges::OriginalReplay,
    ) {
        let roots = crate::machine::logical_held::FrameRoots::prepared(
            self.logical.clone(),
            TaskId::MAIN,
            invocation.clone(),
            Arc::clone(&exe.code(code).source_room),
            Arc::clone(&exe.code(code).slots_by_at),
        )
        .unwrap()
        .bank();
        let running = Running::new(TaskId::MAIN, exe, statement);
        let store = self.logical.ledger();
        let stamp = store.borrow().stamp();
        let before = store.borrow().balance().unwrap();
        self.restore_original_bank(&running, replay.next, invocation.id(), roots, replay.roles)
            .unwrap();
        assert!(
            self.regs
                .begin_source_resume(
                    TaskId::MAIN,
                    invocation.id(),
                    &running.stmt.site,
                    replay.next,
                    &store.borrow()
                )
                .unwrap()
        );
        assert!(
            store.borrow().is_current(&stamp),
            "saved roles must be adopted without root publication"
        );
        assert_eq!(store.borrow().balance().unwrap(), before);
    }

    fn run_original_operations(
        &mut self,
        running: &Running<'_>,
        pc: &mut u32,
        stop: u32,
    ) -> Eval<()> {
        for at in *pc..stop {
            let op = running.stream.ops[at as usize];
            if !op.kind().in_region() || matches!(op, Op::Region { .. }) {
                return Err(fault("original suffix crossed an uncertified boundary").into());
            }
            if self.in_region(running, at)? {
                self.source_complete_joins(running, at)?;
            } else {
                self.source_complete(running, at)?;
            }
        }
        *pc = stop;
        Ok(())
    }

    fn charge_original_event(&mut self, event: RegionEvent) -> Eval<()> {
        if !self.charging {
            self.observe_region(&[event]);
            return Ok(());
        }
        let units = match event.weight {
            Weight::Units(units) | Weight::Atomic(units) => units,
        };
        let attempts = match event.weight {
            Weight::Units(_) => units,
            Weight::Atomic(_) => 1,
        };
        for unit in 0..attempts {
            self.charged = self.charged.saturating_add(match event.weight {
                Weight::Units(_) => 1,
                Weight::Atomic(_) => units,
            });
            if self.charged > self.bounds.charge_limit {
                let mut limit = self.bounds.charge_limit;
                if let Err(refused) = self.limiters.charge_reached(self.charged, &mut limit) {
                    self.observe_crossing(event, unit);
                    return Err(refused.into());
                }
                self.bounds.charge_limit = limit;
            }
        }
        self.observe_region(&[event]);
        Ok(())
    }
}
