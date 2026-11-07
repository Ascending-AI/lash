//! Action completion transfers its result and resumes the actual caller.
use super::*;

impl KernelMachine {
    /// Completes the statement the top frame waits in with `value`, what
    /// its action yielded, and runs the statement on from the action
    /// (`K-EVAL-003`). A statement's own action leaves the value in
    /// register 0 and resumes after its action op, where the statement
    /// binds, writes or drops it. A nested action's resume entry loads the
    /// saved roles into their registers under this layout and the value
    /// into the action's, and the statement goes on from the op after the
    /// action's, charging nothing again (`K-EVAL-008`). Gives the statement
    /// and how its range ended.
    pub(in crate::machine) fn finish_action(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
        origin: ResultOrigin,
    ) -> Eval<(StmtId, Ran)> {
        self.oracle_only()?;
        if matches!(origin, ResultOrigin::Returned)
            && let Some((id, pc)) = self.publish_source_call_resume(task, exe, value)?
        {
            let running = Running::new(task, exe, id);
            let ran = match self.register_action_suffix(host, &running, pc as usize)? {
                Some(ran) => ran,
                None => self.run(host, &running, pc as usize)?,
            };
            return Ok((id, ran));
        }
        let frame = self.frame(task)?;
        let Some(id) = frame.awaiting.take() else {
            return Err(fault("a value arrived at no statement").into());
        };
        let nested = frame.nested.take();
        let pending = frame.pending.take();
        if nested.is_none()
            && pending.is_none()
            && matches!(exe.stmt(id).form, Form::ReturnVia { .. })
        {
            if self.return_leaves_run(task)? {
                self.copy_out_returned(&value, &exe.stmt(id).site)?;
            }
            return Ok((id, Ran::Depart(Completion::Return(Box::new(value)))));
        }
        // A `for`'s protocol hook answered from its frame: the header
        // goes on (`K-ITER-005`).
        if nested.is_none() && pending.is_none() && self.in_header(task)? {
            let ran = match self.header_answered(task, host, exe, value)? {
                crate::machine::ops::Flow::Done => Ran::Done,
                crate::machine::ops::Flow::Step => Ran::Step,
            };
            self.logical_flush_controls(task, exe)?;
            {
                use lash_kernel_state::kernel_internals::logical::Owner;
                let store = self.logical.clone();
                let mut ledger = store.borrow_mut();
                let owner = Owner::Incoming(task);
                if ledger.holding(owner, self.heap.pages())?.is_some() {
                    ledger.transfer_source(
                        &mut self.arena.borrow_mut(),
                        &self.heap,
                        &[owner],
                        &[],
                    )?;
                }
            }
            return Ok((id, ran));
        }
        let running = Running::new(task, exe, id);
        let (Some(act), Some(pending)) = (nested, pending) else {
            if nested.is_some() {
                return Err(fault("a resuming nested action has no saved continuation").into());
            }
            // The value is in no variable until the statement binds it.
            self.put(0, value);
            if matches!(origin, ResultOrigin::Delivered) {
                self.pin(0)?;
            } else {
                self.borrow_value(self.regs[0]);
            }
            self.transfer_precise_result(
                &running,
                running.stmt.resume() as u32,
                lash_kernel_exec::Reg(0),
                &value,
            )?;
            if let Some(ran) = self.register_action_suffix(host, &running, running.stmt.resume())? {
                return Ok((id, ran));
            }
            return match self.run(host, &running, running.stmt.resume())? {
                Ran::Done => Ok((id, Ran::Done)),
                _ => Err(fault("an action's statement did not complete").into()),
            };
        };
        let entry = running
            .stream
            .nested
            .get(act.ix())
            .ok_or_else(|| fault("a pending action has no resume entry"))?;
        let mut saved = pending.roles;
        for (index, role) in entry.roles.iter().enumerate() {
            // Saved in the entry's order, unless a restore admitted them
            // in another layout's.
            let at = match saved.get(index) {
                Some(stood) if stood.site == role.suffix => index,
                _ => saved
                    .iter()
                    .position(|stood| stood.site == role.suffix)
                    .ok_or_else(|| fault("a saved continuation role is missing"))?,
            };
            // Back in the register it was evaluated to, as if the
            // statement had never stopped.
            self.put(role.reg.0 as usize, saved[at].value);
        }
        if saved.capacity() > 0 {
            saved.clear();
            self.storage.roles.push(saved);
        }
        // The value is in no variable until the statement uses it.
        self.put(entry.dst.0 as usize, value);
        if matches!(origin, ResultOrigin::Delivered) {
            self.pin(entry.dst.0 as usize)?;
        } else {
            self.borrow_value(self.regs[entry.dst.0 as usize]);
        }
        let pc = running.stmt.ops.start as usize + entry.resume as usize;
        self.transfer_precise_result(&running, pc as u32, entry.dst, &value)?;
        let ran = match self.register_action_suffix(host, &running, pc)? {
            Some(ran) => ran,
            None => self.run(host, &running, pc)?,
        };
        Ok((id, ran))
    }

    /// The statement a resume ran on has ended its range as `ran` says: in
    /// its block, its step edge is taken; a `while` condition's value goes
    /// to its loop; a departure is the frame's.
    pub(in crate::machine) fn resumed(
        &mut self,
        task: TaskId,
        exe: &Executable,
        (stmt, ran): (StmtId, Ran),
    ) -> Eval<Completion> {
        match ran {
            Ran::Done => {
                self.take_edge_of(task, exe, stmt)?;
                Ok(Completion::Normal)
            }
            Ran::Step => Ok(Completion::Normal),
            Ran::Depart(completion) => Ok(completion),
            Ran::Tested(value) => {
                self.tested(task, value)?;
                Ok(Completion::Normal)
            }
        }
    }
}
