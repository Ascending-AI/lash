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
use lash_kernel_state::kernel_internals::logical::NativeBatch;
use lash_kernel_value::names::ImportSlot;
use lash_kernel_value::package::OperationMode;
use lash_kernel_value::{Type, sites::Site};
use std::sync::Arc;

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
    ) -> Eval<()> {
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
                        return Err(fault("a call is not a delivering action").into());
                    }
                };
                let spawned = match &entry.kind {
                    NestedKind::Spawn(callee) => Some(self.nested_target(&exe, callee, 0)?),
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
                    return Err(fault("a standing statement action has no action form").into());
                };
                let (service, spawned) = match &action.kind {
                    ActionKind::Spawn { callee, .. } => {
                        (Service::Spawn, Some((self.callee(task, &exe, callee)?, 0)))
                    }
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
                        return Err(fault("a call is not a delivering action").into());
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
    ) -> Eval<Option<Value>> {
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
                match self.spawn(task, host, exe, stmt, site, target, at, count)? {
                    Flow::Done => Some(self.take(at)),
                    _ => None,
                }
            }
            Service::Perform { slot, result } => {
                let effect = self.operation(task, slot, &site.unit)?;
                let mut copied = Vec::with_capacity(args.len());
                for (at, value) in args.iter().enumerate() {
                    copied.push(self.copy_out_tool_arg(&effect, at, value)?);
                }
                if self.effect_mode(&effect)? == OperationMode::Read {
                    Some(self.perform_read(task, host, exe, &effect, &copied, site, result)?)
                } else {
                    self.issue_effect(task, stmt, site, effect, copied)?;
                    None
                }
            }
            Service::Join => match self.join_one(task, argument(0)?)? {
                Ok(value) => Some(value),
                Err(joined) => {
                    self.wait_one(task, stmt, joined)?;
                    None
                }
            },
            Service::JoinMany(mode) => match self.join_list(task, mode, &argument(0)?)? {
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
                    return crate::machine::raise("type_error", "`cancel` takes a task handle");
                };
                if target == task {
                    return Err(crate::machine::actions::cancelled_here());
                }
                self.cancel(target)?;
                Some(Value::Null)
            }
            Service::CancelEffect => {
                let Value::Task(target) = argument(0)? else {
                    return crate::machine::raise(
                        "type_error",
                        "`cancel_effect` takes a task handle",
                    );
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
    /// kernel unable to answer in place. The native runs with the frame's
    /// argument values, charging nothing, into a nursery it gives back: what
    /// it built waits, detached, as one batch with the balances of the
    /// contract certifying the call, if one does, until the region takes
    /// that step and adopts it (Ruling 4). Nothing it built is held before
    /// then.
    pub(super) fn answer_native(
        &mut self,
        task: TaskId,
        at: crate::compile::room::Point,
        lib: crate::compile::LibId,
        slots: &[lash_kernel_state::kernel_internals::logical::Slot],
    ) -> Eval<()> {
        let mut operands = self.role_values(task, slots)?;
        let exe = Arc::clone(&self.exe);
        let function = exe.lib(lib);
        operands.resize(function.arity.max(operands.len()), Value::Absent);
        let used = operands.len().max(1);
        for (at, value) in operands.iter().enumerate() {
            self.put(at, *value);
        }
        let batch = self.native_batch(task, &exe, lib, function, &operands, &at.site.unit);
        self.regs.clear_range(0..used);
        self.served = Some(super::Served {
            task,
            at,
            answer: Some(batch?),
        });
        Ok(())
    }

    /// Checks the arguments of the checked helper call at `at` against the
    /// helper's signature, as the call's entry would: a misfit raises
    /// `function_argument`, and a fit lets the region take the call.
    pub(super) fn check_arguments(
        &mut self,
        task: TaskId,
        at: crate::compile::room::Point,
        lib: crate::compile::LibId,
        slots: &[lash_kernel_state::kernel_internals::logical::Slot],
    ) -> Eval<()> {
        let operands = self.role_values(task, slots)?;
        let exe = Arc::clone(&self.exe);
        let function = exe.lib(lib);
        for (at, value) in operands.iter().enumerate() {
            self.put(at, *value);
        }
        let checked = self.check_call_arguments(
            &exe,
            lash_kernel_value::CallableId {
                function: function.specialization.function,
                link: self.current_link(),
            },
            &function.header.signature,
            0,
            operands.len(),
            true,
        );
        self.regs.clear_range(0..operands.len().max(1));
        checked?;
        self.served = Some(super::Served {
            task,
            at,
            answer: None,
        });
        Ok(())
    }

    /// Runs `function` over registers `0..` holding `operands`, and binds
    /// what it built and answered to its resource contract.
    fn native_batch(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: crate::compile::LibId,
        function: &crate::compile::Lib,
        operands: &[Value],
        unit: &lash_kernel_value::sites::Unit,
    ) -> crate::machine::Eval<NativeBatch> {
        use crate::compile::LibRun;
        use crate::functions::MachineFunction;
        use lash_kernel_doc::{Args, NativeError, NativeResourceContract, NewObject};
        let operands_box: Box<[Value]> = operands.into();
        let malformed = |error: NativeError| -> Interrupt {
            match error {
                NativeError::Host(error) => Halt::from(error).into(),
                error => fault(&error.to_string()).into(),
            }
        };
        let (value, objects, contract, held) = match &function.run {
            LibRun::Native { native, .. } => {
                // The contract is a query of the inputs, made before the
                // call: what the call does cannot certify itself.
                let certified = loop {
                    let query = {
                        let arena = self.arena.borrow();
                        let semantics = arena.semantics().with_limits(self.limits);
                        native.resource_contract(Args::new(operands, arena.objects(), &semantics))
                    };
                    match query {
                        Err(NativeError::Retry(lash_kernel_doc::Retry::Cold { object, chunk })) => {
                            self.warm(object, chunk)?
                        }
                        Err(error) => return Err(malformed(error)),
                        Ok(certified) => break certified,
                    }
                };
                let limit = match &function.limit {
                    Some(limit) => {
                        self.warm_formula(limit, 0, None)?;
                        Some(self.in_formula(lib, false, |machine| {
                            machine.formula(limit, &machine.regs[0..function.arity], None)
                        })?)
                    }
                    None => None,
                };
                if self.observer.is_some() {
                    self.observe(crate::observe::Event::NativeEnter { task, lib });
                }
                let (answer, built, spent) =
                    self.run_native(exe, lib, function, native, 0, limit, true)?;
                self.observe(crate::observe::Event::GuardWork(spent));
                self.observe(crate::observe::Event::NativeExit);
                let value = match answer {
                    Ok(value) => value,
                    Err(NativeError::Raised(error)) => return Err(Interrupt::raised(error)),
                    Err(NativeError::Refused { kind, message }) => {
                        return Err(Interrupt::raised(self.error_value(kind, message)));
                    }
                    Err(NativeError::Guard(guard)) => {
                        return Err(crate::machine::bound(
                            crate::interface::Bound::Guard {
                                function: function.specialization.function,
                            },
                            guard.limit,
                        )
                        .into());
                    }
                    Err(NativeError::Decline) => {
                        return Err(fault(
                            "a declined native's kernel body is not integrated into the region",
                        )
                        .into());
                    }
                    Err(error) => return Err(malformed(error)),
                };
                let (held, objects) = built.unwrap_or_default();
                (value, objects, certified, held)
            }
            LibRun::Machine(MachineFunction::Deref) => {
                let value = match operands.first() {
                    Some(Value::TaskRef(task)) => Value::Task(*task),
                    Some(Value::ObjectRef(object)) => {
                        match self.in_arena(|arena| arena.object_value(*object))? {
                            Some(value) => value,
                            None => {
                                return Err(Interrupt::raised(
                                    self.error_value("type_error", "the ref names no object"),
                                ));
                            }
                        }
                    }
                    _ => {
                        return Err(Interrupt::raised(
                            self.error_value("type_error", "`deref` takes a ref"),
                        ));
                    }
                };
                (value, Vec::new(), None, 0)
            }
            LibRun::Machine(MachineFunction::TasksUnfinished) => {
                let mut unfinished = Vec::new();
                self.task_table()?.visit(&mut |id, other| {
                    if id != task
                        && !matches!(
                            other.state,
                            lash_kernel_state::paged::TaskStateRecord::Ended(_)
                        )
                    {
                        unfinished.push(Value::Task(id));
                    }
                    true
                })?;
                let (value, held, objects) = {
                    let arena = self.arena.borrow();
                    let mut nursery = lash_kernel_doc::Nursery::unaccounted(arena.objects());
                    let value = nursery.allocate(NewObject::List(unfinished.clone()));
                    (value, nursery.held().0, nursery.detach())
                };
                let contract = {
                    let arena = self.arena.borrow();
                    let semantics = arena.semantics().with_limits(self.limits);
                    NativeResourceContract::list(
                        Args::new(operands, arena.objects(), &semantics),
                        &unfinished,
                    )
                    .map_err(malformed)?
                };
                (value, objects, Some(contract), held)
            }
            // `brand.built()` answers the library body its call is written
            // in, and Absent outside one (`K-LIB-010`).
            LibRun::Machine(MachineFunction::BrandBuilt) => match unit {
                lash_kernel_value::sites::Unit::Library(body) => {
                    let arena = self.arena.borrow();
                    let mut nursery = lash_kernel_doc::Nursery::unaccounted(arena.objects());
                    let value = nursery.allocate(NewObject::Bytes(Arc::from(&body.as_bytes()[..])));
                    let held = nursery.held().0;
                    (value, nursery.detach(), None, held)
                }
                lash_kernel_value::sites::Unit::Main
                | lash_kernel_value::sites::Unit::Function(_) => {
                    (Value::Absent, Vec::new(), None, 0)
                }
            },
            LibRun::Body(_) | LibRun::Declaration => {
                return Err(fault("a served native call has no native answer").into());
            }
        };
        // An uncertified call is priced by what it built: its adoption.
        let batch = NativeBatch::new(objects, value, contract.as_ref(), held, operands_box)
            .map_err(|error| -> Interrupt { fault(&error.to_string()).into() })?;
        let depth = {
            let arena = self.arena.borrow();
            batch.depth(&|value| arena.summary_of(value).depth)
        };
        if depth as usize > self.limits.value_depth() {
            let message = format!(
                "a value exceeds the configured {}-level value depth",
                self.limits.value_depth()
            );
            return Err(Interrupt::raised(self.error_value("too_deep", message)));
        }
        Ok(batch)
    }

    /// Copies out the value of the effect `task`'s top frame stands before,
    /// held in `slot`: the region charges the copy when it takes the step.
    pub(super) fn serve_effect(
        &mut self,
        task: TaskId,
        at: crate::compile::room::Point,
        slot: lash_kernel_state::kernel_internals::logical::Slot,
    ) -> Eval<lash_kernel_doc::Datum> {
        let [value] = self.role_values(task, &[slot])?[..] else {
            return Err(fault("an effect has one operand").into());
        };
        let datum = self.copy_datum(&value)?;
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
    ) -> Eval<()> {
        let exe = Arc::clone(&self.exe);
        let value = self.resume(task, &exe, incoming)?;
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
