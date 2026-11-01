//! A prepared direct return closes its entered Invocation before physical unwind.
use super::*;
use crate::machine::{Completion, Control, Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use return_storage::{ReturnInput, ReturnRecipe};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_return(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<Option<Completion>, Halt> {
        let Op::Return { src } = running.stream.ops[pc as usize] else {
            return Ok(None);
        };
        #[cfg(any(test, feature = "testing"))]
        let run_root =
            running.task == doc::TaskId::MAIN && self.task(running.task)?.frames.len() == 1;
        let (source, link, invocation, loops) = {
            let frame = self.frame(running.task)?;
            let code = running.exe.code(frame.code);
            if frame.public_result.is_some()
                || frame.convention.is_some()
                || frame.cleaning.is_some()
                || frame.awaiting.is_some()
                || frame.pending.peek().is_some()
                || !frame.control.iter().all(|control| {
                    matches!(
                        control,
                        Control::Block { .. } | Control::While { started: 1.., .. }
                    )
                })
                || frame
                    .source_frontier
                    .as_ref()
                    .is_some_and(|frontier| frontier.position().unpaid.is_none())
            {
                return Ok(None);
            }
            let Some(Control::Block { block, .. }) = frame.control.iter().next() else {
                return Ok(None);
            };
            if running.exe.block(*block).site != code.source_room.site {
                return Ok(None);
            }
            let Some(roots) = &frame.logical else {
                return Ok(None);
            };
            (
                Arc::clone(&code.source_room),
                frame.link,
                roots.invocation().clone(),
                source_loops(frame, running.exe)?
                    .ok_or_else(|| malformed("return has no actual loop occurrence"))?,
            )
        };
        let Ok(interval) = source.compile_return_suffix(&room::Point {
            site: running.stmt.site.clone(),
            phase: room::Phase::Commit,
        }) else {
            return Ok(None);
        };
        {
            let frame = self.frame(running.task)?;
            if !interval
                .events()
                .skip(1)
                .map(|event| &event.point.site)
                .eq(frame.control.iter().rev().filter_map(|control| {
                    let Control::Block { block, .. } = control else {
                        return None;
                    };
                    Some(&running.exe.block(*block).site)
                }))
            {
                return Err(
                    malformed("precise return differs from its entered source blocks").into(),
                );
            }
        }
        let Some(roles) = running.stmt.source_roles_at(running.stream, pc) else {
            return Ok(None);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| *reg == src) else {
            return Ok(None);
        };
        self.logical_flush_controls(running.task, running.exe)?;
        self.logical_flush()?;
        let Some(bank) = self.regs.prepare_source_return(src, rhs)? else {
            return Ok(None);
        };
        let value = bank.value();
        let result = Box::new(value);
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let mut arena = self.arena.borrow_mut();
        #[cfg(test)]
        let loop_ordinal = loops.last().map(|(_, ordinal)| *ordinal);
        let Some(ReturnRecipe { prefix, program }) = (ReturnInput {
            source: &source,
            invocation: &invocation,
            task: running.task,
            at: &running.stmt.site,
            rhs,
            input: bank.owner(),
            value,
            safe_point: self.charged,
            loops,
        })
        .prepare(&ledger, &mut arena, &self.heap)?
        else {
            return Ok(None);
        };
        drop(ledger);
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("return source has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("return source kernel is unsupported"))?;
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            + usize::from(running.task != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        if frame.invocation != invocation.id() {
            return Err(malformed("return preparation names another invocation").into());
        }
        #[cfg(test)]
        let scoped = frame.control.len() > 1;
        let logical = &mut frame.logical;
        let mut completion = None;
        let safe_point = self.charged;
        let published = Context {
            admission: Admission::Exact,
            source: &source,
            link,
            kernel,
            logical: &self.logical,
            heap: &self.heap,
            arena: &mut arena,
            session: &mut self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: &mut task.counters,
                bound: if self.charging {
                    self.bounds.charge_limit
                } else {
                    u64::MAX
                },
                slice: self.slice_end,
                depth,
                limit: u64::from(self.bounds.call_depth),
                safe_point,
            },
            memory_bound: self.bounds.memory,
            logical_memory: &self.logical_memory,
        }
        .publish(
            &invocation,
            &mut frame.source_frontier,
            prefix,
            || program.clone(),
            Reserve(move |_: &mut Arena| Ok(Some((bank, result)))),
            |_, _, edits, (bank, result)| {
                debug_assert!(edits.is_empty());
                bank.publish();
                *logical = None;
                completion = Some(Completion::Return(result));
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_return_published(run_root);
            #[cfg(test)]
            if let Some(ordinal) = loop_ordinal {
                crate::machine::islands::witness::loop_returned(true, ordinal);
            }

            #[cfg(test)]
            if scoped {
                crate::machine::islands::witness::precise_scoped_returned();
            }
        }
        debug_assert_eq!(published, completion.is_some());
        Ok(completion)
    }
}
