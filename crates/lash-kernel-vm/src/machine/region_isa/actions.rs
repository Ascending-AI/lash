//! A canonical frame standing at a delivering action: the machine performs
//! the action through its services, and delivers its result as the task's
//! Incoming, which the action's Commit takes.
//!
//! The region accepts the action's Enter, its atoms or operands live as
//! source roles, and stops. Performing it either answers at once or leaves
//! the task waiting; either way the result arrives as the task's Incoming,
//! held by the ledger as `Owner::Incoming(task)`. Delivery moves the frame to
//! the action's Commit with that value pending there.
use super::verified::Standing;
use crate::compile::{ActionKind, Executable, Form, NestedKind, StmtId};
use crate::interface::Host;
use crate::machine::ops::Flow;
use crate::machine::{Eval, Halt, Incoming, Interrupt, KernelMachine, fault};
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::{JoinMode, TaskId, Value};
use lash_kernel_value::names::ImportSlot;
use lash_kernel_value::package::OperationMode;
use lash_kernel_value::{Type, sites::Site};
use std::sync::Arc;

/// A raise out of an action is delivered once the region throws; until
/// then it is refused rather than dropped.
pub(super) fn answered<T>(eval: Eval<T>) -> Result<T, Halt> {
    eval.map_err(|interrupt| match interrupt {
        Interrupt::Halt(halt) => *halt,
        Interrupt::Raise(_) | Interrupt::Refuse(_) => {
            fault("a canonical action raised; region raise delivery is not integrated")
        }
    })
}

/// The service an action asks of the machine.
enum Service<'a> {
    Spawn,
    Perform { slot: ImportSlot, result: &'a Type },
    Join,
    JoinMany(JoinMode),
    Yield,
    Cancel,
    CancelEffect,
}

impl KernelMachine {
    /// Performs the action `standing` names for `task`'s top frame. An
    /// answer at once becomes the task's Incoming, delivered on its next
    /// step; otherwise the task waits, or runs on after its child.
    pub(super) fn perform_action(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        standing: Standing,
    ) -> Result<(), Halt> {
        let exe = Arc::clone(&self.exe);
        let stmt = standing.statement;
        let args = standing.arguments;
        for (at, value) in args.iter().enumerate() {
            self.put(at, *value);
        }
        // The service, its site and guest occurrence, and for a spawn its
        // target and the register its arguments start at.
        let (service, site, guest, spawned) = match standing.act {
            Some(act) => {
                let entry = exe
                    .statement(stmt)
                    .0
                    .nested
                    .get(act.ix())
                    .ok_or_else(|| fault("a standing action names no nested entry"))?;
                let service = match &entry.kind {
                    NestedKind::Spawn(_) => Service::Spawn,
                    NestedKind::Perform { slot, result, .. } => Service::Perform {
                        slot: *slot,
                        result,
                    },
                    NestedKind::Join => Service::Join,
                    NestedKind::JoinMany(mode) => Service::JoinMany(*mode),
                    NestedKind::Yield => Service::Yield,
                    NestedKind::Cancel => Service::Cancel,
                    NestedKind::CancelEffect => Service::CancelEffect,
                    NestedKind::Call(_) | NestedKind::Body(_) => {
                        return Err(fault("a call is not a delivering action"));
                    }
                };
                let spawned = match &entry.kind {
                    NestedKind::Spawn(callee) => {
                        Some(answered(self.nested_target(&exe, callee, 0))?)
                    }
                    _ => None,
                };
                self.frame(task)?.nested = Some(act);
                (
                    service,
                    &entry.site,
                    entry.guest.then_some(entry.occurrence),
                    spawned,
                )
            }
            None => {
                let Form::Action(action) = &exe.statement(stmt).1.form else {
                    return Err(fault("a standing statement action has no action form"));
                };
                let (service, spawned) = match &action.kind {
                    ActionKind::Spawn { callee, .. } => (
                        Service::Spawn,
                        Some((answered(self.callee(task, &exe, callee))?, 0)),
                    ),
                    ActionKind::Perform { slot, result, .. } => (
                        Service::Perform {
                            slot: *slot,
                            result,
                        },
                        None,
                    ),
                    ActionKind::Join(_) => (Service::Join, None),
                    ActionKind::JoinMany(mode, _) => (Service::JoinMany(*mode), None),
                    ActionKind::Yield => (Service::Yield, None),
                    ActionKind::Cancel(_) => (Service::Cancel, None),
                    ActionKind::CancelEffect(_) => (Service::CancelEffect, None),
                    ActionKind::Call { .. } => {
                        return Err(fault("a call is not a delivering action"));
                    }
                };
                (
                    service,
                    &action.site,
                    action.guest.then_some(action.occurrence),
                    spawned,
                )
            }
        };
        let answer = self.serve(task, host, &exe, stmt, site, guest, service, spawned, &args);
        self.regs.clear_range(0..args.len());
        if let Some(value) = answer? {
            let incoming = Incoming::Value(self.page_form(&value)?);
            self.logical_incoming(task, &incoming)?;
            self.task(task)?.wake(incoming, false);
            self.frame(task)?.awaiting = Some(stmt);
        }
        Ok(())
    }

    /// Runs `service` with `args` in registers `0..`: `Some` is an answer
    /// at once, `None` leaves the task waiting or its child running.
    #[allow(clippy::too_many_arguments)]
    fn serve(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: StmtId,
        site: &Site,
        guest: Option<usize>,
        service: Service<'_>,
        spawned: Option<(crate::machine::actions::Target, usize)>,
        args: &[Value],
    ) -> Result<Option<Value>, Halt> {
        self.observe_action(exe, site);
        let argument = |at: usize| {
            args.get(at)
                .copied()
                .ok_or_else(|| fault("a standing action lacks an argument"))
        };
        // A guest action's entry is counted and told to a host that
        // observes sites; a spawn's and an effect's are their own.
        if let Some(occurrence) = guest
            && !matches!(service, Service::Spawn | Service::Perform { .. })
        {
            let occurrence = self.task(task)?.counters.advance(occurrence)?;
            self.observe_entry(task, host, site, occurrence)?;
        }
        Ok(match service {
            Service::Spawn => {
                let (target, at) =
                    spawned.ok_or_else(|| fault("a spawn has no resolved target"))?;
                let count = args
                    .len()
                    .checked_sub(at)
                    .ok_or_else(|| fault("a spawn lacks its callee"))?;
                match answered(self.spawn(task, host, exe, stmt, site, target, at, count))? {
                    Flow::Done => Some(self.take(at)),
                    _ => None,
                }
            }
            Service::Perform { slot, result } => {
                let effect = self.operation(task, slot, &site.unit)?;
                let mut copied = Vec::with_capacity(args.len());
                for (at, value) in args.iter().enumerate() {
                    copied.push(answered(self.copy_out_tool_arg(&effect, at, value))?);
                }
                if self.effect_mode(&effect)? == OperationMode::Read {
                    Some(answered(self.perform_read(
                        task, host, exe, &effect, &copied, site, result,
                    ))?)
                } else {
                    self.issue_effect(task, stmt, site, effect, copied)?;
                    None
                }
            }
            Service::Join => match answered(self.join_one(task, argument(0)?))? {
                Ok(value) => Some(value),
                Err(joined) => {
                    self.wait_one(task, stmt, joined)?;
                    None
                }
            },
            Service::JoinMany(mode) => match answered(self.join_list(task, mode, &argument(0)?))? {
                Ok(value) => Some(value),
                Err(members) => {
                    self.wait_list(task, stmt, mode, members)?;
                    None
                }
            },
            Service::Yield => {
                let incoming = Incoming::Value(PageValue::Null);
                self.logical_incoming(task, &incoming)?;
                self.task(task)?.wake(incoming, false);
                self.frame(task)?.awaiting = Some(stmt);
                self.ready.push_back(task)?;
                self.current = None;
                None
            }
            Service::Cancel => {
                let Value::Task(target) = argument(0)? else {
                    return answered(crate::machine::raise(
                        "type_error",
                        "`cancel` takes a task handle",
                    ));
                };
                if target == task {
                    return answered(Err(crate::machine::actions::cancelled_here()));
                }
                self.cancel(target)?;
                Some(Value::Null)
            }
            Service::CancelEffect => {
                let Value::Task(target) = argument(0)? else {
                    return answered(crate::machine::raise(
                        "type_error",
                        "`cancel_effect` takes a task handle",
                    ));
                };
                self.cancel_effect(target)?;
                Some(Value::Null)
            }
        })
    }

    /// The values `task`'s top frame holds in `slots`, in order.
    fn role_values(
        &mut self,
        task: TaskId,
        slots: &[lash_kernel_state::kernel_internals::logical::Slot],
    ) -> Result<Vec<Value>, Halt> {
        let source = self
            .frame(task)?
            .pending
            .source
            .as_ref()
            .ok_or_else(|| fault("a served step has no source continuation"))?;
        slots
            .iter()
            .map(|slot| {
                source
                    .roles
                    .iter()
                    .find_map(|(saved, value)| (saved == slot).then_some(*value))
                    .ok_or_else(|| fault("a served step lacks a held operand"))
            })
            .collect()
    }

    /// Answers the native call `task`'s top frame stands before, its checked
    /// kernel unable to answer in place: the machine's general call runs it
    /// with the frame's argument values, charging nothing, and the answer
    /// waits for the region to take that call's step, which charges it.
    pub(super) fn answer_native(
        &mut self,
        task: TaskId,
        at: crate::compile::room::Point,
        lib: crate::compile::LibId,
        slots: &[lash_kernel_state::kernel_internals::logical::Slot],
    ) -> Result<(), Halt> {
        let arguments = self.role_values(task, slots)?;
        let exe = Arc::clone(&self.exe);
        let used = exe.lib(lib).arity.max(arguments.len()).max(1);
        for (at, value) in arguments.iter().enumerate() {
            self.put(at, *value);
        }
        let charging = std::mem::replace(&mut self.charging, false);
        let answer = self.answer(task, &exe, lib, 0, arguments.len(), 0);
        self.charging = charging;
        let answer = answered(answer);
        let value = match answer {
            Ok(crate::machine::ops::Answer::Done) => Ok(self.take(0)),
            Ok(crate::machine::ops::Answer::Body(_)) => Err(fault(
                "a declined native's kernel body is not integrated into the region",
            )),
            Err(halt) => Err(halt),
        };
        self.regs.clear_range(0..used);
        self.served = Some(super::Served {
            task,
            at,
            answer: Some(value?),
        });
        Ok(())
    }

    /// Copies out the value of the effect `task`'s top frame stands before,
    /// held in `slot`: the region charges the copy when it takes the step.
    pub(super) fn serve_effect(
        &mut self,
        task: TaskId,
        at: crate::compile::room::Point,
        slot: lash_kernel_state::kernel_internals::logical::Slot,
    ) -> Result<lash_kernel_doc::Datum, Halt> {
        let [value] = self.role_values(task, &[slot])?[..] else {
            return Err(fault("an effect has one operand"));
        };
        let datum = answered(self.copy_datum(&value).map_err(Into::into))?;
        self.served = Some(super::Served {
            task,
            at,
            answer: None,
        });
        Ok(datum)
    }

    /// Delivers `incoming` to the action `standing` names: the top frame
    /// moves to the action's Commit, its result pending there.
    pub(super) fn deliver_action(
        &mut self,
        task: TaskId,
        standing: Standing,
        incoming: Incoming,
    ) -> Result<(), Halt> {
        let exe = Arc::clone(&self.exe);
        let value = answered(self.resume(task, &exe, incoming))?;
        let charged = self.charged;
        let frame = self.frame(task)?;
        frame.awaiting = None;
        frame.nested = None;
        let source = frame
            .pending
            .source
            .as_mut()
            .ok_or_else(|| fault("a standing action has no source continuation"))?;
        source.at = standing.resume.clone();
        source.unpaid = standing.resume;
        source.progress = 0;
        source.pending = Some(value);
        // What the action charged outside the region is paid: the frame
        // resumes at the machine's charge.
        source.debt.paid = charged;
        Ok(())
    }
}
