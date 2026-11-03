//! A prepared direct return closes its entered Invocation before physical unwind.
//! A suspended break or continue moves its record into the cleanup it enters;
//! otherwise its loop's Catch takes the record. A caught raise binds in its catch.
use super::*;
use crate::machine::{Completion, Control, Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use return_storage::{Carried, ReturnInput};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_departure(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<Option<Completion>, Halt> {
        let src = match running.stream.ops[pc as usize] {
            Op::Return { src } | Op::Throw { src } => Some(src),
            Op::Break | Op::Continue => None,
            _ => return Ok(None),
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
                        Control::Block { .. }
                            | Control::While { started: 1.., .. }
                            | Control::For {
                                started: 1..,
                                header: crate::machine::loops::Header::Body,
                                ..
                            }
                            | Control::Try { .. }
                    )
                })
                // A return inside cleanup supersedes a pending departure.
                || frame.control.iter().any(|control| {
                    matches!(
                        control,
                        Control::Try {
                            phase: crate::machine::TryPhase::Finally(_),
                            ..
                        }
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
        let Ok(interval) = source.compile_departure_suffix(&room::Point {
            site: running.stmt.site.clone(),
            phase: room::Phase::Commit,
        }) else {
            return Ok(None);
        };
        let departing = interval.exit();
        // A suspending suffix leaves the blocks inside its innermost cleanup.
        let suspend = interval
            .events()
            .last()
            .filter(|event| event.point.phase == room::Phase::Suspend)
            .map(|event| event.point.site.clone());
        // A raise its catch takes, or a break or continue its loop takes,
        // stays in its invocation.
        let caught = interval
            .events()
            .last()
            .filter(|event| event.point.phase == room::Phase::Catch)
            .map(|event| event.point.site.clone());
        #[cfg(test)]
        if (suspend.is_some()
            && crate::machine::islands::witness::take_suspended_departure_decline())
            || (caught.is_some()
                && crate::machine::islands::witness::take_caught_departure_decline())
        {
            return Ok(None);
        }
        let cursor = {
            let arena = std::rc::Rc::clone(&self.arena);
            let frame = self.frame(running.task)?;
            let cleanup = |control: &Control| match control {
                Control::Try { stmt, .. } => {
                    let site = Some(&running.exe.stmt(*stmt).site);
                    suspend.as_ref() == site || caught.as_ref() == site
                }
                Control::While { stmt, .. } | Control::For { stmt, .. } => {
                    caught.as_ref() == Some(&running.exe.stmt(*stmt).site)
                }
                Control::Block { .. } => false,
            };
            if (suspend.is_some() || caught.is_some()) && !frame.control.iter().any(cleanup) {
                return Err(malformed("precise departure has no entered target").into());
            }
            if !interval
                .events()
                .skip(1)
                .filter(|event| event.point.phase == room::Phase::Leave)
                .map(|event| &event.point.site)
                .eq(frame
                    .control
                    .iter()
                    .rev()
                    .take_while(|control| !cleanup(control))
                    .filter_map(|control| {
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
            // A for continue resumes where its actual cursor goes next.
            match frame.control.iter().rev().find(|control| cleanup(control)) {
                Some(Control::For {
                    stmt,
                    cursor,
                    guard: None,
                    ..
                }) if departing == room::ExitKind::Continue => {
                    let crate::compile::Form::For {
                        site, via: None, ..
                    } = &running.exe.stmt(*stmt).form
                    else {
                        return Ok(None);
                    };
                    let Some(Ok((_, next))) = cursor.resident_step(&arena.borrow()) else {
                        return Ok(None);
                    };
                    let Ok(head) = source.next_after_for_cursor(site, next.is_some()) else {
                        return Ok(None);
                    };
                    Some((head, cursor.walked().and_then(|value| value.handle())))
                }
                Some(Control::For { .. }) if departing == room::ExitKind::Continue => {
                    return Ok(None);
                }
                // A shared catch binding takes a cell the suffix does not create.
                Some(Control::Try { stmt, .. })
                    if matches!(&running.exe.stmt(*stmt).form,
                        crate::compile::Form::Try { catch: Some((binding, _)), .. } if binding.shared) =>
                {
                    return Ok(None);
                }
                _ => None,
            }
        };
        let roles = match src {
            Some(_) => match running.stmt.source_roles_at(running.stream, pc) {
                Some(roles) => Some(roles),
                None => return Ok(None),
            },
            None => None,
        };
        let rhs = match (src, &roles) {
            (Some(src), Some(roles)) => {
                let Some((_, rhs)) = roles.iter().find(|(reg, _)| *reg == src) else {
                    return Ok(None);
                };
                Some((src, rhs))
            }
            _ => None,
        };
        // A library body's formula bills its charged caller. The accepted
        // return pays it, so a refused fee leaves the invocation entered.
        // A suspended return stays in its invocation and pays nothing yet.
        let metered = self.bounds.charge == crate::Charge::Exact;
        let caller = self.task(running.task)?.frames.caller_charging()?;
        let fee = match &self.frame(running.task)?.library {
            Some(call) if suspend.is_none() && caught.is_none() && caller.unwrap_or(metered) => {
                Some((call.lib, call.args.clone()))
            }
            _ => None,
        };
        let fee = match fee {
            Some((lib, args)) => {
                // A failing formula leaves its refusal to the ordinary leave.
                let plan = &running.exe.lib(lib).charge;
                let (src, _) = rhs.ok_or_else(|| malformed("a returning fee has no operand"))?;
                let value = self.regs[src.0 as usize];
                let Ok(units) = self
                    .warm_values(plan, &args, Some(&value))
                    .and_then(|()| self.formula(plan, &args, Some(&value)))
                else {
                    return Ok(None);
                };
                let Some(debt) = room::charges::ReturnDebt::bind(
                    running.exe,
                    &source,
                    room::Point {
                        site: running.stmt.site.clone(),
                        phase: room::Phase::Commit,
                    },
                    lib,
                    &args,
                    value,
                    &self.arena.borrow(),
                ) else {
                    return Ok(None);
                };
                Some((lib, units, debt))
            }
            None => None,
        };
        self.logical_flush_controls(running.task, running.exe)?;
        self.logical_flush()?;
        let bank = match rhs {
            Some((src, rhs)) => {
                let Some(bank) = self.regs.prepare_source_return(src, rhs)? else {
                    return Ok(None);
                };
                Some((bank, rhs))
            }
            None => None,
        };
        let departure = match (&bank, departing) {
            (Some((bank, _)), room::ExitKind::Return) => Completion::Return(Box::new(bank.value())),
            (Some((bank, _)), room::ExitKind::Raise) => Completion::Throw(Box::new(bank.value())),
            (None, room::ExitKind::Break) => Completion::Break,
            (None, room::ExitKind::Continue) => Completion::Continue,
            _ => return Err(malformed("source departure differs from its statement").into()),
        };
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let mut arena = self.arena.borrow_mut();
        #[cfg(test)]
        let loop_ordinal = loops.last().map(|(_, ordinal)| *ordinal);
        let Some(mut recipe) = (ReturnInput {
            source: &source,
            invocation: &invocation,
            task: running.task,
            at: &running.stmt.site,
            carried: bank.as_ref().map(|(bank, rhs)| Carried {
                rhs,
                input: bank.owner(),
                value: bank.value(),
            }),
            safe_point: self.charged,
            loops,
            cursor,
        })
        .prepare(&ledger, &mut arena, &self.heap)?
        else {
            return Ok(None);
        };
        drop(ledger);
        recipe.allowance.units = fee.as_ref().map_or(0, |(_, units, _)| *units);
        recipe.debt = fee.as_ref().map(|(_, _, debt)| debt.clone());
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
        let library = &mut frame.library;
        let mut completion = None;
        let safe_point = self.charged;
        let published = Context {
            admission: Admission::Exact(MemoryLimit::of(
                self.bounds.memory_meter,
                &mut self.bounds.memory,
                &mut self.limiters,
            )),
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
                bound: if self.charging || fee.is_some() {
                    self.bounds.charge_limit
                } else {
                    u64::MAX
                },
                slice: self.slice_end,
                depth,
                limit: u64::from(self.bounds.call_depth),
                safe_point,
            },
            logical_memory: &self.logical_memory,
        }
        .publish_departure(
            &invocation,
            &mut frame.source_frontier,
            recipe,
            (bank, departure),
            |_, _, (bank, departure)| {
                if let Some((bank, _)) = bank {
                    bank.publish();
                }
                // The ordinary unwind then takes the physical blocks and
                // records the same pending holding the boundary transferred.
                if suspend.is_none() && caught.is_none() {
                    *logical = None;
                }
                if fee.is_some() {
                    *library = None;
                }
                completion = Some(departure);
            },
        );
        drop(arena);
        let published = match (published, &fee) {
            (Err(halt), Some((lib, units, _))) => {
                return Err(self.refused_at_body_return(running.exe, *lib, *units, halt));
            }
            (published, _) => published?,
        };
        if published && let Some((lib, units, _)) = fee {
            self.in_formula(lib, true, |_| ());
            self.observe_paid(units);
        }
        #[cfg(test)]
        if published && suspend.is_some() {
            crate::machine::islands::witness::precise_suspended_departed();
        }
        #[cfg(test)]
        if published && caught.is_some() {
            crate::machine::islands::witness::precise_caught_departed();
        }
        #[cfg(any(test, feature = "testing"))]
        if published && suspend.is_none() && caught.is_none() {
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

    /// Exact source admission is the only memory authority: an operation
    /// body whose return overflows the memory bound is refused at the
    /// operation's Commit after its formula is paid, as a native that
    /// answers is, and the refusal names the operation. A charge refusal
    /// while paying the formula comes first.
    #[cold]
    #[inline(never)]
    fn refused_at_body_return(
        &mut self,
        exe: &crate::compile::Executable,
        lib: crate::compile::LibId,
        units: u64,
        halt: Halt,
    ) -> Halt {
        let Halt::Bound(mut exceeded) = halt else {
            return halt;
        };
        if exceeded.bound != crate::interface::Bound::Memory {
            return Halt::Bound(exceeded);
        }
        self.in_formula(lib, true, |_| ());
        if let Err(charge) = self.charge_call(exe, lib, units) {
            return charge;
        }
        exceeded.function = Some(exe.lib(lib).header.name.clone());
        Halt::Bound(exceeded)
    }
}
