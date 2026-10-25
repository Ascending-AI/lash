//! Island entry and exit (`docs/design/kernel-regions.md` §3.4, §3.5): the
//! guards an island call passes before anything is paid for, and the
//! machine frames its virtual frames become where it exits.
//!
//! Helper bodies and guest callbacks use the same physical frames and source
//! maps. Exits materialize the innermost frame before continuing its original
//! operation, including its own charged or uncharged execution mode.

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::{self, Owner, Reason};

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{TaskId, Value};

use lash_kernel_exec::{ActIx, RegRange, Weight};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::paged::PhaseRecord;
use lash_vocabulary::data_limits::KernelDataLimits;

use super::super::ops::Flow;
use super::super::session::Session;
use super::super::sites::SiteCaches;
use super::super::stream::Running;
use super::super::tasks::Occurrences;
use super::super::{Eval, Halt, KernelMachine, SlotState, fault};
use super::{
    Exit, ISLAND_ADMITTED, ISLAND_DEPTH, Island, Level, RunsOn, Scratch, Stands, Stop, Watching,
    found_code,
};
use crate::compile::{
    BindingName, CodeId, Cx, Executable, GuestCall, Guests, NestedCallee, NestedKind,
};
use crate::interface::Host;
use crate::observe::ChargeTape;

/// Where the caller of an island call stands while its callee runs.
#[derive(Clone, Copy)]
enum Caller {
    /// At the call nested at this entry of its statement, the operands
    /// its statement evaluated saved as the continuation's roles.
    Nested(ActIx),
    /// At its statement's root call action, which resumes after it.
    Action,
}

/// The callee of an island call.
#[derive(Clone, Copy)]
enum Target<'c> {
    Declared(CodeId),
    Closure(ArenaHandle),
    /// The closure a region's guest call of this occurrence calls,
    /// resolved through the call's cache.
    Guest(ArenaHandle, usize),
    /// The island plan of a closure's code, with its cells as the call
    /// read them.
    Code(u32, &'c [ArenaHandle]),
}

/// Entry guards and the run's execution environment. Source continuations
/// hold admission until their current semantic step has settled.
#[derive(Clone, Copy)]
pub(in crate::machine) struct Gate {
    open: bool,
    charging: bool,
    slice_end: u64,
    observed: bool,
    pub(super) projecting: bool,
    sites: bool,
    limits: KernelDataLimits,
    memory: bool,
    base: u64,
    /// Only this resident frame can cross a public return boundary.
    root: bool,
    call_depth: u64,
}

/// What an island call came to.
pub(in crate::machine) enum Ran {
    /// Its callee returned this value.
    Returned(Value),
    /// It exited, leaving these virtual frames, innermost first.
    Exited(Vec<Level>),
}

impl KernelMachine {
    fn observation_coordinates(
        &mut self,
        task: TaskId,
        sites: bool,
        resident: bool,
    ) -> Result<Option<super::observing::Coordinates>, Halt> {
        if !sites && self.observer.as_deref().is_none_or(|o| o.path.is_none()) {
            return Ok(None);
        }
        let access = self.heap.pages().clone();
        let running = self.task(task)?;
        Ok(Some((
            running.identity.resolve(&access),
            if sites {
                running.frames.loop_coordinates().and_then(|mut loops| {
                    if resident && let Some(frame) = running.frames.last_mut()? {
                        let count = frame
                            .control
                            .iter()
                            .filter(|control| {
                                matches!(
                                    control,
                                    super::super::Control::For { .. }
                                        | super::super::Control::While { .. }
                                )
                            })
                            .count();
                        loops.truncate(loops.len().saturating_sub(count));
                    }
                    Ok(loops)
                })
            } else {
                Ok(Vec::new())
            },
        )))
    }

    /// Runs the call nested at `act` of `running`, its callee and arguments
    /// in `args`, as an island when its callee's body is one: `Done`, its
    /// value in `args.start`, when the island returned; `Step` when it
    /// exited, its frames converted and the step ended or run on by the
    /// reference. `None`: nothing changed, and the reference runs the call.
    pub(in crate::machine) fn call_island(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        act: ActIx,
        args: RegRange,
    ) -> Eval<Option<Flow>> {
        let nested = &running.stream.nested[act.ix()];
        let start = args.start.0 as usize;
        let (target, first) = match (&nested.kind, self.regs[start]) {
            (NestedKind::Call(NestedCallee::Declared(code)), _) => (Target::Declared(*code), start),
            (NestedKind::Call(NestedCallee::Read), Value::Closure(handle)) => {
                (Target::Closure(handle), start + 1)
            }
            _ => return Ok(None),
        };
        let given = start + args.len as usize - first;
        let ran = self.island(
            host,
            running.task,
            running.exe,
            self.charging || self.paced,
            target,
            (first, given),
            Some(nested.occurrence),
        )?;
        self.island_ended(host, running, Caller::Nested(act), start, ran)
    }

    /// Closed heap paths share the existing entry nursery and heap handler.
    /// A monotone upper bound admits completion before a staged write commits;
    /// the source still charges the actual formula after that write. Scheduled
    /// work retains its ordered source events and frame boundaries.
    pub(in crate::machine) fn prepared_heap(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: crate::compile::LibId,
        args: usize,
    ) -> Eval<Option<lash_kernel_doc::Value>> {
        if !exe.document_stream().layout.regions() {
            return Ok(None);
        }
        let function = exe.lib(lib);
        if function.shares_params {
            return Ok(None);
        }
        let Some((prepared, _)) = &function.prepared else {
            return Ok(None);
        };
        if matches!(prepared, crate::compile::Entry::Template { template, .. } if template.metered.is_some())
        {
            return Ok(None);
        }
        let Some((heap, callees, natives)) = prepared.heap(&|id| exe.library_lib(id)) else {
            return Ok(None);
        };
        let gate = self.gate(task, false)?;
        // Holding off program reentry leaves this source semantic handler
        // available; it enters no frame or instruction program.
        #[cfg(test)]
        if self.boundary.is_some() {
            return Ok(None);
        }
        if self.steps.verify
            || gate.projecting
            || self.charged >= gate.slice_end
            || self.capacity_headroom() < ISLAND_ADMITTED
            || gate.base.saturating_add(u64::from(
                prepared.deepest(&|id| exe.lib(id), &|id| exe.library_lib(id)),
            )) > gate.call_depth
        {
            return Ok(None);
        }
        let completion_room = self
            .charging
            .then(|| self.bounds.charge_limit.saturating_sub(self.charged));
        let mut arena = self.arena.borrow_mut();
        let running = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let ground = Ground {
            link: self.active_link,
            exe,
            tape: None,
            recorder: None,
            charging: false,
            slice_end: gate.slice_end,
            observed: false,
            limits: gate.limits,
            memory: gate.memory,
            base: gate.base,
            call_depth: gate.call_depth,
        };
        let entry = Entry {
            watching: None,
            ground: &ground,
            arena: &mut arena,
            sites: &mut self.sites,
            session: &self.session,
            // No charged work may be hidden before the source completion.
            bound: self.charged,
        };
        let mut island = open::<false>(
            entry,
            &mut self.islands,
            &mut running.counters,
            (self.charged, frames(gate.base, gate.call_depth)),
        );
        Ok(island
            .heap(0, true, |cx| {
                let (value, charges) = cx.heap_paths(
                    heap,
                    callees,
                    natives,
                    &self.regs[args..args + function.arity],
                )?;
                // Scheduled work retains its ordered source events and boundaries.
                (charges.events.is_empty()
                    && charges.pace == 0
                    && completion_room.is_none_or(|room| {
                        cx.completion_fits(
                            function,
                            &self.regs[args..args + function.arity],
                            &value,
                            room,
                        )
                    }))
                .then_some(value)
            })
            .map(|answer| answer.value))
    }

    /// Runs the root call action of `running` of code `code`, with the
    /// cells `cells` and its `count` arguments in the registers from `out`,
    /// as an island when the code's body is one: `Done`, its value in
    /// `out`, when the island returned; `Step` when it exited. `None`:
    /// nothing changed, and the reference pushes the call's frame. The
    /// action counted its occurrence.
    #[inline]
    pub(in crate::machine) fn action_island(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        (code, cells): (CodeId, &[ArenaHandle]),
        out: usize,
        count: usize,
    ) -> Eval<Option<Flow>> {
        let Some(plan) = running.exe.island_of(code) else {
            #[cfg(any(test, feature = "perf-witness"))]
            execution::slow_exit(
                &self.islands.census,
                Owner::RegisterProgram,
                Reason::NotAdmitted,
                self.islands.counting,
            );
            return Ok(None);
        };
        // The callee runs charged or paced as its code is, not as the
        // caller's: a helper's body is uncharged, its callbacks are not.
        let callee = running.exe.code(code);
        let ran = self.island(
            host,
            running.task,
            running.exe,
            callee.charging || callee.paced,
            Target::Code(plan, cells),
            (out, count),
            None,
        )?;
        self.island_ended(host, running, Caller::Action, out, ran)
    }

    fn island_ended(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        caller: Caller,
        out: usize,
        ran: Option<Ran>,
    ) -> Eval<Option<Flow>> {
        match ran {
            None => Ok(None),
            Some(Ran::Returned(value)) => {
                self.put(out, value);
                self.borrow_value(value);
                Ok(Some(Flow::Done))
            }
            Some(Ran::Exited(levels)) => {
                self.convert(host, running, caller, levels)?;
                Ok(Some(Flow::Step))
            }
        }
    }

    /// Runs the call of `target` in `task` on the `given` arguments in the
    /// registers from `first` as an island when its code's body is one,
    /// counting `occurrence` as the call's when given. `counted` is
    /// whether the callee runs charged or, with charge Off, paced, `sites`
    /// whether the host observes sites. `None`: the island does not run,
    /// and nothing changed.
    #[expect(
        clippy::too_many_arguments,
        reason = "each caller states where its callee runs and what it counts"
    )]
    fn island(
        &mut self,
        host: &mut dyn Host,
        task: TaskId,
        exe: &Executable,
        counted: bool,
        target: Target<'_>,
        (first, given): (usize, usize),
        occurrence: Option<usize>,
    ) -> Eval<Option<Ran>> {
        // The callee's code has an island, before any guard is paid for.
        let plan = match target {
            Target::Declared(code) => exe.island_of(code),
            Target::Code(plan, _) => Some(plan),
            // A closure's first call enters its island too: it finds its
            // code as the reference's call does, and keeps it.
            Target::Closure(handle) | Target::Guest(handle, _) => {
                found_code(&mut self.arena.borrow_mut(), exe, handle)
                    .and_then(|code| exe.island_of(code))
            }
        };
        if plan.is_none() || !counted {
            #[cfg(any(test, feature = "perf-witness"))]
            execution::slow_exit(
                &self.islands.census,
                Owner::RegisterProgram,
                Reason::NotAdmitted,
                self.islands.counting,
            );
            return Ok(None);
        }
        let gate = self.gate(task, host.observes_sites())?;
        if !gate.open {
            #[cfg(any(test, feature = "perf-witness"))]
            execution::slow_exit(
                &self.islands.census,
                Owner::RegisterProgram,
                Reason::SourceContinuation,
                self.islands.counting,
            );
            return Ok(None);
        }
        if gate.projecting {
            self.enter_under::<true>(
                (&gate, Some(host)),
                task,
                exe,
                target,
                (first, given),
                occurrence,
            )
        } else {
            self.enter_under::<false>(
                (&gate, Some(host)),
                task,
                exe,
                target,
                (first, given),
                occurrence,
            )
        }
    }

    /// Whether this entry projects work, context, counts, paths or sites.
    #[inline(always)]
    fn projects(&self, sites: bool) -> bool {
        sites
            || !self.ran.is_empty()
            || self
                .observer
                .as_deref()
                .is_some_and(|observer| !observer.only_charges())
    }

    pub(super) fn gate(&mut self, task: TaskId, sites: bool) -> Eval<Gate> {
        // Observations project the common program's facts. Only a source
        // continuation or the source-only step verifier retains this step.
        let open = !(self.islands.held || self.steps.verify);
        #[cfg(test)]
        let open = open && self.boundary.is_none();
        // A paced run has no charge to record and no bound: its pace only
        // meets the slice's end.
        let charging = self.bounds.charge == crate::Charge::Exact;
        let below = usize::from(task != TaskId::MAIN || !self.session_cell);
        let frames = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .len();
        Ok(Gate {
            open,
            charging,
            slice_end: self.slice_end,
            observed: self.observer.is_some(),
            projecting: self.projects(sites),
            sites,
            limits: self.limits,
            memory: self.bounds.memory_meter != crate::MemoryMeter::Off,
            base: (frames + below) as u64,
            root: task == TaskId::MAIN && frames == 1,
            call_depth: u64::from(self.bounds.call_depth),
        })
    }

    /// Runs the call of `target` in `task` on the `given` arguments in the
    /// registers from `first` as an island under the open gate `gate`,
    /// counting `occurrence` as the call's when given, once the guards on
    /// what changes between entries hold. `None`: the island does not run,
    /// and nothing changed.
    #[inline]
    fn enter_under<'a, const OBSERVING: bool>(
        &'a mut self,
        (gate, host): (&Gate, Option<&'a mut dyn Host>),
        task: TaskId,
        exe: &Executable,
        target: Target<'_>,
        (first, given): (usize, usize),
        occurrence: Option<usize>,
    ) -> Eval<Option<Ran>> {
        // What changes from one entry to the next: the arguments, the
        // charge, which the call's push would end the step at once it
        // reaches the slice threshold, the charge bound, which a limiter
        // grows, and the capacity headroom, which must fit the island's
        // transient holdings and its virtual frames with their variables,
        // as nothing inside it is counted (FIG-6791).
        if self.charged >= gate.slice_end || self.capacity_headroom() < ISLAND_ADMITTED {
            #[cfg(any(test, feature = "perf-witness"))]
            execution::slow_exit(
                &self.islands.census,
                Owner::RegisterProgram,
                if self.charged >= gate.slice_end {
                    Reason::Slice
                } else {
                    Reason::Capacity
                },
                self.islands.counting,
            );
            return Ok(None);
        }
        let recorder = self
            .observer
            .as_deref()
            .and_then(|observer| observer.charges.as_ref())
            .cloned();
        let watching = if OBSERVING {
            let coordinates = self.observation_coordinates(task, gate.sites, false)?;
            Watching::new(
                self.observer.as_deref_mut(),
                &mut self.ran,
                task,
                coordinates,
                host,
                gate.sites,
            )
        } else {
            None
        };
        let mut arena = self.arena.borrow_mut();
        let running = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let ground = Ground {
            link: self.active_link,
            exe,
            tape: recorder.as_ref().filter(|_| gate.charging),
            recorder: recorder.as_ref(),
            charging: gate.charging,
            slice_end: gate.slice_end,
            observed: gate.observed,
            limits: gate.limits,
            memory: gate.memory,
            base: gate.base,
            call_depth: gate.call_depth,
        };
        let entry = Entry {
            watching: watching
                .as_deref()
                .map(|watching| watching as &dyn super::Projection),
            ground: &ground,
            arena: &mut arena,
            sites: &mut self.sites,
            session: &self.session,
            bound: if gate.charging {
                self.bounds.charge_limit
            } else {
                u64::MAX
            },
        };
        self.islands.open();
        let ran = enter::<OBSERVING>(
            entry,
            &mut self.islands,
            &mut running.counters,
            &mut self.charged,
            (target, &self.regs[first..first + given]),
            occurrence,
            &[],
        );
        drop(watching);
        Ok(ran?)
    }

    /// A loop region's guest call `call` ran in its statement, where the
    /// top frame of `task` now stands holding `regs` across the call. Its
    /// callee returned `value`: the statement runs on from after the call,
    /// as a nested call's exit runs on. Or its island stopped, leaving
    /// `levels`: they convert above the frame, standing at the call.
    pub(in crate::machine) fn guest_exit(
        &mut self,
        host: &mut dyn Host,
        (task, exe): (TaskId, &Executable),
        call: &GuestCall,
        mut regs: Vec<(u32, Value)>,
        (value, levels): (Option<Value>, Vec<Level>),
    ) -> Eval<()> {
        let running = Running::new(task, exe, call.stmt);
        if let Some(value) = value {
            regs.push((call.reg, value));
            self.borrow_value(value);
            return Ok(self.run_on(
                host,
                task,
                exe,
                RunsOn::Instruction(call.stmt, call.resume, regs),
            )?);
        }
        self.regs.statement(running.stmt.regs);
        for (reg, value) in regs {
            self.put(reg as usize, value);
        }
        let caller = match call.act {
            Some(act) => Caller::Nested(act),
            None => Caller::Action,
        };
        self.convert(host, &running, caller, levels)
    }

    /// Converts the virtual frames an island left, `levels` innermost
    /// first, into the machine frames the reference holds there, the
    /// caller standing at its call in `running` as `caller` says; then
    /// runs on a statement the top frame stands in.
    #[cold]
    #[inline(never)]
    fn convert(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        caller: Caller,
        levels: Vec<Level>,
    ) -> Eval<()> {
        let (task, exe) = (running.task, running.exe);
        if let Caller::Nested(act) = caller {
            let entry = &running.stream.nested[act.ix()];
            self.capture(task, entry, act, PhaseRecord::Called)?;
        }
        self.frame(task)?.awaiting = Some(running.id);
        let mut top = None;
        for level in levels.into_iter().rev() {
            top = self.convert_level(task, exe, level)?;
        }
        match top {
            Some(top) => {
                // The top frame's statement runs on charged as its code
                // is, not as the caller's: a helper's body is uncharged.
                self.refresh_charging(task)?;
                Ok(self.run_on(host, task, exe, top)?)
            }
            None => Ok(()),
        }
    }

    /// The top frame an island left runs its statement on from the op the
    /// island stopped at, holding what the island held there, with islands
    /// held off until the step ends.
    pub(super) fn run_on(
        &mut self,
        host: &mut dyn Host,
        task: TaskId,
        exe: &Executable,
        run: RunsOn,
    ) -> Result<(), Halt> {
        // A helper may have entered a charged callback before exiting.
        // The materialized top frame owns the source operation's meter.
        self.refresh_charging(task)?;
        self.active_link = self.frame(task)?.link;
        self.islands.held = true;
        let outcome = match run {
            RunsOn::Instruction(stmt, pc, regs) => {
                for (reg, value) in regs {
                    self.put(reg as usize, value);
                }
                let running = Running::new(task, exe, stmt);
                self.run(host, &running, pc as usize)
                    .and_then(|ran| self.resumed(task, exe, (stmt, ran)))
            }
            RunsOn::Departure(departure) => Ok(departure),
            RunsOn::Scheduling(stmt, pc, regs) => {
                return self.schedule_instruction(host, task, exe, stmt, pc, regs);
            }
            RunsOn::Iteration(stmt) => {
                let initial = matches!(
                    self.frame(task)?.control.last(),
                    Some(
                        super::super::Control::While { started: 0, .. }
                            | super::super::Control::For { started: 0, .. }
                    )
                );
                self.next_iteration(task, host, exe)
                    .and_then(|flow| match flow {
                        Flow::Done if !initial => Ok(super::super::Completion::Normal),
                        Flow::Done => {
                            self.resumed(task, exe, (stmt, super::super::stream::Ran::Done))
                        }
                        Flow::Step => {
                            self.resumed(task, exe, (stmt, super::super::stream::Ran::Step))
                        }
                    })
            }
        };
        let settled = self.settle(task, host, exe, outcome);
        self.islands.held = false;
        settled
    }

    /// Pushes the machine frame of virtual frame `level`, admitted as the
    /// reference admits a call's frame, its variables and its roles. Gives
    /// the statement, op and registers a running frame stands at.
    fn convert_level(
        &mut self,
        task: TaskId,
        exe: &Executable,
        level: Level,
    ) -> Eval<Option<RunsOn>> {
        self.install_level(task, exe, level, true)
    }

    fn install_level(
        &mut self,
        task: TaskId,
        exe: &Executable,
        level: Level,
        fresh: bool,
    ) -> Eval<Option<RunsOn>> {
        let mut pending = None;
        let mut action = None;
        let top = match level.stands {
            Stands::Action { stmt } => {
                action = Some(stmt);
                None
            }
            Stands::Boundary => None,
            Stands::Running { stmt, pc, regs } => Some(RunsOn::Instruction(stmt, pc, regs)),
            Stands::Iteration { stmt } => Some(RunsOn::Iteration(stmt)),
            Stands::Departure(departure) => Some(RunsOn::Departure(departure)),
            Stands::Scheduling { stmt, pc, regs } => Some(RunsOn::Scheduling(stmt, pc, regs)),
            Stands::Called { stmt, act, roles } => {
                let entry = &exe.statement(stmt).0.nested[act.ix()];
                pending = Some((
                    stmt,
                    act,
                    self.continuation(&entry.suffix, PhaseRecord::Called, roles.len(), |_, at| {
                        roles[at].clone()
                    })?,
                ));
                None
            }
        };
        let mut new = if fresh {
            Some(self.fresh_frame(exe, level.code)?)
        } else {
            None
        };
        if let Some(frame) = &mut new {
            frame.slots.fresh(level.slots.len());
        }
        let frame = match new.as_deref_mut() {
            Some(frame) => frame,
            None => self.frame(task)?,
        };
        if let Some(call) = level.library {
            frame.library = Some(call);
        }
        frame.link = level.link;
        frame.control = level.control;
        if let Some(stmt) = action {
            frame.awaiting = Some(stmt);
        }
        for (at, slot) in level.slots.into_iter().enumerate() {
            frame.set_slot(at as u32, slot);
        }
        if let Some((stmt, act, pending)) = pending {
            frame.stand_at(act, pending);
            frame.awaiting = Some(stmt);
        }
        if let Some(frame) = new {
            self.task(task)?.frames.push(frame);
        }
        self.grown();
        Ok(top)
    }

    pub(in crate::machine) fn advance_active(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        plan: u32,
    ) -> Eval<super::super::Completion> {
        match self.resume_function(task, host, exe, plan)? {
            Some(completion) => Ok(completion),
            None => self.advance(task, host, exe),
        }
    }

    /// A restored or already resident admitted function re-enters the same
    /// loop at its semantic block/statement boundary. Its existing frame
    /// remains the root owner until the loop returns or materializes it.
    pub(in crate::machine) fn resume_function(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        plan: u32,
    ) -> Eval<Option<super::super::Completion>> {
        let gate = self.gate(task, host.observes_sites())?;
        if gate.projecting {
            self.resume_projecting::<true>(gate, task, host, exe, plan, None)
        } else {
            self.resume_projecting::<false>(gate, task, host, exe, plan, None)
        }
    }

    pub(super) fn resume_projecting<const OBSERVING: bool>(
        &mut self,
        mut gate: Gate,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        plan: u32,
        suffix: Option<super::ActionSuffix>,
    ) -> Eval<Option<super::super::Completion>> {
        let Some(program) = exe.island_plan(plan) else {
            return Ok(None);
        };
        let Some(compiled) = program.compiled(exe) else {
            return Ok(None);
        };
        // A heap-operation exit has handed this activation to its
        // source loop owner, including any linear accumulator. Loops
        // outside the admitted register tree also retain that activation.
        // Verified scalar loops and scheduler loops belong to the register
        // tree; a delivered action resumes a scheduler loop's suffix.
        // Uncharged helper bodies own no source loop activation: they always
        // resume in the register program.
        if suffix.is_none()
            && exe.code(self.frame(task)?.code).charged
            && self
                .frame(task)?
                .control
                .iter()
                .any(|control| match control {
                    super::super::Control::While { stmt, .. }
                    | super::super::Control::For { stmt, .. } => !compiled.owns_loop(*stmt),
                    _ => false,
                })
        {
            return Ok(None);
        }
        // A generic re-entry into a spent slice can stop at its first
        // boundary before it charges anything; a delivered action's suffix
        // always completes its statement first.
        let spent = suffix.is_none() && self.charged >= gate.slice_end;
        if !gate.open
            || self.capacity_headroom() < ISLAND_ADMITTED
            || (gate.charging && self.charged >= self.bounds.charge_limit)
            || spent
        {
            #[cfg(any(test, feature = "perf-witness"))]
            execution::slow_exit(
                &self.islands.census,
                Owner::RegisterProgram,
                if !gate.open {
                    Reason::SourceContinuation
                } else if self.capacity_headroom() < ISLAND_ADMITTED {
                    Reason::Capacity
                } else if spent {
                    Reason::Slice
                } else {
                    Reason::Charge
                },
                self.islands.counting,
            );
            // The source charger owns the first unit that crosses the bound
            // or the slice. Re-entering without that unit would reproduce
            // this boundary.
            return Ok(None);
        }
        let frame = self.frame(task)?;
        if frame.pending.peek().is_some() || frame.awaiting.is_some() {
            return Ok(None);
        }
        let (charging, paced) = (frame.charging, frame.paced);
        let public_return = gate.root && self.return_leaves_run(task)?;
        // The loop's local meter and a later source exit belong to the same
        // active frame, including a charged callback of an uncharged helper.
        self.charging = charging;
        self.paced = paced;
        // The root is already in the semantic stack, so only the frames
        // below it contribute to this entry's base depth.
        gate.base = gate.base.saturating_sub(1);
        let recorder = self
            .observer
            .as_deref()
            .and_then(|observer| observer.charges.as_ref())
            .cloned();
        let watching = if OBSERVING {
            let coordinates = self.observation_coordinates(task, gate.sites, true)?;
            Watching::new(
                self.observer.as_deref_mut(),
                &mut self.ran,
                task,
                coordinates,
                Some(host),
                gate.sites,
            )
        } else {
            None
        };
        let mut arena = self.arena.borrow_mut();
        let task_state = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let frame = task_state
            .frames
            .last_mut()?
            .ok_or_else(|| fault("no frame"))?;
        let ground = Ground {
            link: frame.link,
            exe,
            tape: recorder.as_ref().filter(|_| gate.charging),
            recorder: recorder.as_ref(),
            charging: gate.charging,
            slice_end: gate.slice_end,
            observed: gate.observed,
            limits: gate.limits,
            memory: gate.memory,
            base: gate.base,
            call_depth: gate.call_depth,
        };
        self.islands.open();
        let entry = Entry {
            watching: watching
                .as_deref()
                .map(|watching| watching as &dyn super::Projection),
            ground: &ground,
            arena: &mut arena,
            sites: &mut self.sites,
            session: &self.session,
            bound: if gate.charging {
                self.bounds.charge_limit
            } else {
                u64::MAX
            },
        };
        let mut executor = open::<OBSERVING>(
            entry,
            &mut self.islands,
            &mut task_state.counters,
            (self.charged, frames(gate.base, gate.call_depth)),
        );
        executor.public_return = public_return;
        let mut cells = Vec::new();
        for at in &program.captures {
            let SlotState::Cell(cell) = frame.slot(*at) else {
                return Ok(None);
            };
            cells.push(*cell);
        }
        let Some(captured) =
            super::capture_values(executor.arena, &mut executor.scratch.captured, &cells)
        else {
            return Ok(None);
        };
        let callee = super::Callee {
            link: frame.link,
            plan: program,
            stream: program.stream(exe),
            compiled,
            captured,
        };
        let Some(mut active) = executor.resume(callee, frame, suffix.as_ref()) else {
            return Ok(None);
        };
        let defer = suffix.as_ref().is_some_and(|suffix| suffix.defer);
        if let Some(suffix) = suffix {
            executor.action_suffix(&mut active, &suffix, &self.regs);
        }
        let ran = executor.execute(&mut active);
        let handoff = executor.project_handoff(&mut active, frame, &mut self.regs);
        let returned_explicitly = active.returned_explicitly();
        self.charged = executor.charged;
        drop(watching);
        let ran = if handoff.is_some() {
            None
        } else {
            ended(&mut self.islands, ran)?
        };
        drop(arena);
        if let Some(top) = handoff {
            self.grown();
            if defer {
                self.islands.scheduled = Some((task, top));
            } else if let RunsOn::Scheduling(stmt, pc, regs) = top {
                self.schedule_instruction(host, task, exe, stmt, pc, regs)?;
            } else {
                unreachable!("a resident handoff names a scheduler instruction");
            }
            return Ok(Some(super::super::Completion::Normal));
        }
        match ran {
            Some(Ran::Returned(value)) => {
                if returned_explicitly && self.return_leaves_run(task)? {
                    self.copy_out(&value)?;
                }
                // The register loop carried the return through every
                // enclosing `try` and ran each `finally` once. The source
                // owner only leaves the frame; its stale positions would
                // run those cleanups again.
                self.frame(task)?.control.clear();
                Ok(Some(super::super::Completion::Return(Box::new(value))))
            }
            Some(Ran::Exited(mut levels)) => {
                let root = levels
                    .pop()
                    .ok_or_else(|| fault("an active root left no frame"))?;
                let mut top = self.install_level(task, exe, root, false)?;
                for level in levels.into_iter().rev() {
                    top = self.convert_level(task, exe, level)?;
                }
                if let Some(top) = top {
                    if defer {
                        self.islands.scheduled = Some((task, top));
                    } else {
                        self.run_on(host, task, exe, top)?;
                    }
                } else if matches!(
                    self.frame(task)?.control.last(),
                    Some(super::super::Control::Try { .. })
                ) {
                    // The body has finished in the register loop. Settle its
                    // catch take or deferred departure in this same step.
                    self.refresh_charging(task)?;
                    return self.block_ended(task, host, exe).map(Some);
                }
                Ok(Some(super::super::Completion::Normal))
            }
            None => Ok(None),
        }
    }
}

/// Fields no call changes while a region batch or a callback walk runs.
/// The bound stays at the entry: a callback's limiter can grow it.
pub(in crate::machine) struct Ground<'a> {
    pub(in crate::machine) link: lash_vocabulary::LinkId,
    pub(in crate::machine) exe: &'a Executable,
    pub(in crate::machine) tape: Option<&'a ChargeTape>,
    pub(in crate::machine) recorder: Option<&'a ChargeTape>,
    pub(in crate::machine) charging: bool,
    pub(in crate::machine) slice_end: u64,
    pub(in crate::machine) observed: bool,
    pub(in crate::machine) limits: KernelDataLimits,
    pub(in crate::machine) memory: bool,
    pub(in crate::machine) base: u64,
    pub(in crate::machine) call_depth: u64,
}

/// The borrows and current bound of one entry under a held ground.
struct Entry<'a> {
    watching: Option<&'a dyn super::Projection>,
    ground: &'a Ground<'a>,
    arena: &'a mut Arena,
    sites: &'a mut SiteCaches,
    session: &'a Session,
    bound: u64,
}

/// Runs the call of `target` on `values` as an island in `scratch`, in
/// the resolution scope it holds open, counting `occurrence` in
/// `counters` when given, from the charge
/// `charged`, which it advances: first by the fixed events `before` its
/// caller's statement charges ahead of the call, once the callee is known
/// to have an island that takes these arguments. `None`: the island does
/// not run, and nothing changed.
#[inline(always)]
fn enter<const OBSERVING: bool>(
    entry: Entry<'_>,
    scratch: &mut Scratch,
    counters: &mut Occurrences,
    charged: &mut u64,
    (target, values): (Target<'_>, &[Value]),
    occurrence: Option<usize>,
    before: &[Weight],
) -> Result<Option<Ran>, Halt> {
    let (base, call_depth) = (entry.ground.base, entry.ground.call_depth);
    let mut island = open::<OBSERVING>(
        entry,
        scratch,
        counters,
        (*charged, frames(base, call_depth)),
    );
    let target = match target {
        Target::Declared(code) => island.declared(code),
        Target::Closure(handle) => island
            .resolve_cold(handle)
            .and_then(|cached| island.cached_callee(cached)),
        Target::Guest(handle, occurrence) => island.guest(occurrence, handle),
        Target::Code(plan, cells) => island.with_cells(plan, cells),
    };
    // Compiled already bounds one window by ISLAND_HOLD, which open just
    // claimed. Only fresh parameter cells can exceed that initial claim.
    let capacity = target.is_some_and(|target| {
        target.compiled.cell_parameters != 0 && !island.register_room(target.compiled)
    });
    let Some(target) = target.filter(|target| {
        values.len() <= target.compiled.layout.arity && base <= call_depth && !capacity
    }) else {
        #[cfg(any(test, feature = "perf-witness"))]
        execution::slow_exit(
            &island.scratch.census,
            Owner::RegisterProgram,
            if base > call_depth {
                Reason::Depth
            } else if capacity {
                Reason::Capacity
            } else {
                Reason::Call
            },
            island.scratch.counting,
        );
        return Ok(None);
    };
    for weight in before {
        island.charged += weight.units();
        if let Some(tape) = island.tape {
            tape.push(*weight);
        }
    }
    let ran = if let Some(occurrence) = occurrence
        && let Err(stop) = island.count(occurrence)
    {
        Err(stop)
    } else {
        island.enter(target, values)
    };
    *charged = island.charged;
    ended(scratch, ran)
}

/// One island entry in `scratch` under `ground`, in the resolution scope
/// it holds open, counting in `counters`, from the charge `charged`: no
/// callee resolved yet, and no frame.
#[inline(always)]
fn open<'a, const OBSERVING: bool>(
    entry: Entry<'a>,
    scratch: &'a mut Scratch,
    counters: &'a mut Occurrences,
    (charged, frames): (u64, u32),
) -> Island<'a, OBSERVING> {
    scratch.sessions.clear();
    let ground = entry.ground;
    let ceiling = if ground.memory {
        entry.arena.held().saturating_add(super::ISLAND_HOLD)
    } else {
        u64::MAX
    };
    Island {
        public_return: false,
        link: ground.link,
        exe: ground.exe,
        stream: ground.exe.document_stream(),
        arena: entry.arena,
        sites: entry.sites,
        session: entry.session,
        tape: ground.tape,
        recorder: ground.recorder,
        charging: ground.charging,
        accounting: true,
        charged,
        bound: entry.bound,
        slice_end: ground.slice_end,
        counters,
        observed: ground.observed,
        watching: entry.watching,
        limits: ground.limits,
        memory: ground.memory,
        ceiling,
        transient: 0,
        register_bytes: 0,
        base: ground.base,
        call_depth: ground.call_depth,
        depth: 0,
        frames,
        admitted: false,
        plain: ground.charging && ground.tape.is_none() && !OBSERVING,
        swift: false,
        covered: false,
        scratch,
    }
}

/// The most virtual frames an island runs above `base` frames under the
/// call depth `call_depth`: [`ISLAND_DEPTH`], or fewer where the depth
/// leaves fewer.
#[inline(always)]
fn frames(base: u64, call_depth: u64) -> u32 {
    call_depth
        .saturating_add(1)
        .saturating_sub(base)
        .min(u64::from(ISLAND_DEPTH)) as u32
}

/// What an island entry in `scratch` came to, its call having answered
/// `ran`: its value, or the frames a stop left, or the halt it met.
#[inline(always)]
fn ended(scratch: &mut Scratch, ran: Result<Value, Stop>) -> Result<Option<Ran>, Halt> {
    match ran {
        Ok(value) => Ok(Some(Ran::Returned(value))),
        Err(Stop) => {
            let halt = scratch.exit.halt.take();
            let levels = std::mem::take(&mut scratch.exit.levels);
            scratch.exit = Exit::default();
            if let Some(halt) = halt {
                return Err(halt);
            }
            Ok(Some(Ran::Exited(levels)))
        }
    }
}

/// What runs a loop region's guest calls through their callees' islands
/// (`docs/design/kernel-regions.md` §5 R4): the machine's island state and
/// guards, lent to the region for its run. An island that stops leaves its
/// virtual frames here, for the region's exit to convert.
pub(in crate::machine) struct RegionGuests<'a> {
    pub(in crate::machine) ground: Ground<'a>,
    pub(in crate::machine) session: &'a mut Session,
    /// The plan's session bindings, which the region holds as slots.
    pub(in crate::machine) names: &'a [BindingName],
    pub(in crate::machine) scratch: &'a mut Scratch,
    /// The run's bound, `u64::MAX` when it is paced.
    pub(in crate::machine) bound: u64,
    /// The memory bound, when memory is accounted.
    pub(in crate::machine) memory: Option<u64>,
    /// The capacity the run's heap was sealed holding, until its first
    /// sweep: the least the capacity meter counts (FIG-6791).
    pub(in crate::machine) floor: u64,
    /// The frames an island that stopped left, innermost first.
    pub(in crate::machine) levels: Vec<Level>,
    pub(in crate::machine) halt: Option<Halt>,
    /// The resolution scope the guest calls share, once one entered:
    /// none before the first.
    pub(in crate::machine) scope: Option<Scope>,
}

/// A resolution scope a region's guest calls share: the scope, and the
/// objects the arena had freed when it opened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::machine) struct Scope {
    epoch: u32,
    freed: u64,
}

/// The captured-value threshold at which a resolution scope retires.
/// An individual callee still admits its complete capture set.
pub(super) const SCOPE_CAPTURED: usize = 1 << 10;

impl Guests for RegionGuests<'_> {
    fn call(
        &mut self,
        cx: &mut Cx<'_>,
        call: &GuestCall,
        callee: Option<Value>,
        args: &[Value],
    ) -> Option<Value> {
        // What the statement must fit before it pays anything: its least
        // with the next test's unit under the bound, its charge before the
        // call short of the slice's end, where the call's push would end
        // the step, and the island's headroom and depth.
        let charged = cx.charged;
        if self.scratch.held
            || charged.checked_add(call.least)? > self.bound
            || charged.checked_add(call.units)? >= self.ground.slice_end
            || self.ground.base > self.ground.call_depth
        {
            return None;
        }
        if let Some(memory) = self.memory
            && memory.saturating_sub(cx.arena.held().max(self.floor)) < ISLAND_ADMITTED
        {
            return None;
        }
        let target = match (call.callee, callee) {
            (Some(code), _) => Target::Declared(code),
            (None, Some(Value::Closure(handle))) => Target::Guest(handle, call.occurrence),
            _ => return None,
        };
        let counters = cx.counters.as_deref_mut()?;
        if !counters.can_advance(&[call.occurrence]) {
            return None;
        }
        // The island reads the bindings the region wrote as the region
        // left them: its writes so far are published first, in order.
        if cx.written != 0 {
            let written = &mut self.scratch.written;
            written.clear();
            written.extend(
                cx.sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, slot)| slot.written != 0)
                    .map(|(at, slot)| (slot.written, at)),
            );
            written.sort_unstable();
            for &(_, at) in &*written {
                let slot = &mut cx.sessions[at];
                if !slot
                    .value
                    .is_some_and(|value| self.session.publish(&self.names[at].key, value))
                {
                    self.halt = Some(fault(
                        "a region wrote a session binding the cell does not hold",
                    ));
                    return None;
                }
                slot.written = 0;
                cx.written -= 1;
            }
        }
        // The batch's guest calls share one resolution scope, as nothing
        // frees a closure or writes a cell while a region runs: a
        // collection ends the batch first. A scope is opened afresh when
        // the arena freed an object or another entry opened one since,
        // or its captured values grew past what it keeps.
        let freed = cx.arena.freed();
        if self.scope
            != Some(Scope {
                epoch: self.scratch.epoch,
                freed,
            })
        {
            #[cfg(test)]
            super::SCOPES.with(|scopes| scopes.set(scopes.get() + 1));
            self.scope = Some(Scope {
                epoch: self.scratch.open(),
                freed,
            });
        }
        let entry = Entry {
            watching: None,
            ground: &self.ground,
            arena: &mut *cx.arena,
            sites: &mut *cx.sites,
            session: self.session,
            bound: self.bound,
        };
        let ran = enter::<false>(
            entry,
            self.scratch,
            counters,
            &mut cx.charged,
            (target, args),
            Some(call.occurrence),
            &call.before,
        );
        // Reopen before the next call once this scope keeps too much.
        if self.scratch.captured.len() > SCOPE_CAPTURED {
            self.scope = None;
        }
        let value = match ran {
            Ok(None) => return None,
            Ok(Some(Ran::Returned(value))) => Some(value),
            Ok(Some(Ran::Exited(levels))) => {
                self.levels = levels;
                None
            }
            Err(halt) => {
                self.halt = Some(halt);
                None
            }
        };
        // What the statement charges before the call is paid, and its
        // commit pays it with the rest of its fixed units: the region's
        // charge holds the island's own until then, and an exit in the
        // call takes it as paid.
        cx.charged -= call.units;
        cx.meter.recorded = call.through;
        cx.called = Some(value);
        value
    }
}
