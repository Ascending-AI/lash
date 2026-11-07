//! Statement stepping for the independent source oracle.
use super::ops::Flow;
use super::stream::Running;
use super::*;

impl KernelMachine {
    pub(super) fn advance_source(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Eval<Completion> {
        self.refresh_charging(task)?;
        let frame = self.frame(task)?;
        let (block, next) = match frame.control.last() {
            Some(Control::Block { block, next }) => (*block, *next),
            Some(Control::While { stmt, .. } | Control::For { stmt, .. }) => {
                let stmt = *stmt;
                return self.iterate(task, host, exe, stmt);
            }
            _ => return self.block_ended(task, host, exe),
        };
        let Some(stmt) = exe.block(block).stmts.get(next).copied() else {
            if !self.publish_source_scope_leave(task, exe)? {
                let frame = self.frame(task)?;
                frame.control.pop();
                for at in &exe.block(block).declares {
                    self.set_source_slot(task, *at, SlotState::Empty)?;
                }
            }
            return self.block_ended(task, host, exe);
        };
        if self.charging {
            self.charge(1).map_err(|mut halt| {
                if let crate::machine::Halt::Bound(exceeded) = &mut halt {
                    exceeded.site = Some(Box::new(exe.stmt(stmt).site.clone()));
                }
                halt
            })?;
        } else if self.paced {
            self.charged += 1;
        }
        let Some(Control::Block { next, .. }) = self.frame(task)?.control.last_mut() else {
            return Err(fault("a source statement has no enclosing block").into());
        };
        *next += 1;
        let code = self.frame(task)?.code;
        self.observe(crate::observe::Event::Statement {
            task,
            code,
            stmt,
            charging: self.charging,
        });
        let running = Running::new(task, exe, stmt);
        let ran = self.run(host, &running, running.stmt.ops.start as usize)?;
        self.resumed(task, exe, (stmt, ran))
    }
}

impl KernelMachine {
    pub(super) fn iterate(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: crate::compile::StmtId,
    ) -> Eval<Completion> {
        let initial = matches!(
            self.frame(task)?.control.last(),
            Some(Control::While { started: 0, .. } | Control::For { started: 0, .. })
        );
        let next = self.next_iteration(task, host, exe);
        if initial
            && matches!(
                exe.stmt(stmt).form,
                crate::compile::Form::For { via: None, .. }
            )
        {
            // Source ForIn consumes its evaluated iterable when its first
            // header answers or refuses, even when no source bank was placed.
            self.publish_logical_meter()?;
            let frame = self.frame(task)?;
            let source = &exe.code(frame.code).source_room;
            let owner = source
                .owner_slots()
                .map_err(|_| fault("initial for ownership failed verification"))?
                .root(
                    &crate::compile::room::Owner::LoopCursor(exe.stmt(stmt).site.clone()),
                    task,
                    frame.invocation,
                )
                .ok_or_else(|| fault("initial for input has no canonical owner"))?;
            let store = self.logical.clone();
            let mut ledger = store.borrow_mut();
            if ledger.holding(owner, self.heap.pages())?.is_some() {
                ledger.transfer_source(&mut self.arena.borrow_mut(), &self.heap, &[owner], &[])?;
                self.logical_memory.set(ledger.balance()?.total()?);
            }
        }
        match next? {
            Flow::Done if !initial => Ok(Completion::Normal),
            Flow::Done => self.resumed(task, exe, (stmt, stream::Ran::Done)),
            Flow::Step => self.resumed(task, exe, (stmt, stream::Ran::Step)),
        }
    }
}
