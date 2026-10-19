//! Island entry and exit (`docs/design/kernel-regions.md` §3.4, §3.5): the
//! guards an island call passes before anything is paid for, and the
//! machine frames its virtual frames become where it exits.
//!
//! An island runs a guest call nested in a statement's expression, the
//! root call action of a statement, or a callback a helper's plan applies
//! at one of its callback sites (FIG-6709). Where a callback's island
//! exits, its helper's plans stand as their bodies' frames, as they stand
//! at any callback site, and the island's frames stand above them; the
//! statement the top frame stands in runs on once the step that stood them
//! has settled, inside the same step, as a nested call's exit runs on in
//! its caller's statement.

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{KernelDataLimits, TaskId, Value};
use lash_kernel_exec::{ActIx, RegRange, Weight};
use lash_kernel_state::arena::Arena;
use lash_kernel_state::paged::PhaseRecord;

use super::super::ops::Flow;
use super::super::session::Session;
use super::super::stream::Running;
use super::super::tasks::Occurrences;
use super::super::{Eval, Halt, KernelMachine, SlotState, fault};
use super::{
    Exit, ISLAND_ADMITTED, Island, Level, MAX_ARGS, RunsOn, Scratch, Stands, Stop, found_code, held,
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
    /// A closure's code, with its cells as the call read them.
    Code(CodeId, &'c [ArenaHandle]),
}

/// What an island call came to.
pub(in crate::machine) enum Ran {
    /// Its callee returned this value.
    Returned(Value),
    /// It exited, leaving these virtual frames, innermost first.
    Exited(Vec<Level>),
}

impl KernelMachine {
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
            host.observes_sites(),
            running.task,
            running.exe,
            self.charging || self.paced,
            target,
            (first, given),
            Some(nested.occurrence),
        )?;
        self.island_ended(host, running, Caller::Nested(act), start, ran)
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
        if running.exe.island_of(code).is_none() {
            return Ok(None);
        }
        // The callee runs charged or paced as its code is, not as the
        // caller's: a helper's body is uncharged, its callbacks are not.
        let callee = running.exe.code(code);
        let ran = self.island(
            host.observes_sites(),
            running.task,
            running.exe,
            callee.charging || callee.paced,
            Target::Code(code, cells),
            (out, count),
            None,
        )?;
        self.island_ended(host, running, Caller::Action, out, ran)
    }

    /// Runs the callback of code `code`, closed over the cells `cells`,
    /// that a helper's plan of `task` applies at a callback site to the
    /// `count` arguments from `args`, as an island when the code's body is
    /// one. `None`: nothing changed, and the plan stands at its site while
    /// the reference runs the callback. The site is the helper's, and
    /// counts no occurrence.
    #[inline]
    pub(in crate::machine) fn callback_island(
        &mut self,
        task: TaskId,
        exe: &Executable,
        (code, cells): (CodeId, &[ArenaHandle]),
        args: usize,
        count: usize,
    ) -> Eval<Option<Ran>> {
        if exe.island_of(code).is_none() {
            return Ok(None);
        }
        let callee = exe.code(code);
        self.island(
            !self.islands.quiet,
            task,
            exe,
            callee.charging || callee.paced,
            Target::Code(code, cells),
            (args, count),
            None,
        )
    }

    /// Where a nested call or a root call action that `caller` stands at
    /// leaves its statement once the island call came to `ran`.
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
        sites: bool,
        task: TaskId,
        exe: &Executable,
        counted: bool,
        target: Target<'_>,
        (first, given): (usize, usize),
        occurrence: Option<usize>,
    ) -> Eval<Option<Ran>> {
        // The callee's code has an island that takes these arguments,
        // before any guard is paid for.
        let plan = match target {
            Target::Declared(code) | Target::Code(code, _) => exe.island_of(code),
            // A closure's first call enters its island too: it finds its
            // code as the reference's call does, and keeps it.
            Target::Closure(handle) => found_code(&mut self.arena.borrow_mut(), exe, handle)
                .and_then(|code| exe.island_of(code)),
        };
        if plan.is_none_or(|plan| {
            given > exe.islands.plans[plan as usize].params.len()
                || (first..first + given).any(|at| !held(&self.regs[at]))
        }) {
            return Ok(None);
        }
        // The environment guard: nothing observes the run but a charge
        // tape, which the island records, and no host observes sites.
        if self.islands.held
            || !counted
            || !self.ran.is_empty()
            || self.steps.verify
            || sites
            || self
                .observer
                .as_deref()
                .is_some_and(|observer| !observer.only_charges())
        {
            return Ok(None);
        }
        #[cfg(test)]
        if self.boundary.is_some() {
            return Ok(None);
        }
        // The call's push would end the step at the slice threshold.
        if self.charged >= self.slice_end {
            return Ok(None);
        }
        // The capacity headroom guard: the island's transient holdings and
        // its virtual frames with their variables fit, and nothing inside
        // it is counted (FIG-6791).
        if self.capacity_headroom() < ISLAND_ADMITTED {
            return Ok(None);
        }
        if given > MAX_ARGS {
            return Ok(None);
        }
        // A paced run has no charge to record and no bound: its pace only
        // meets the slice's end.
        let charging = self.bounds.charge == crate::Charge::Exact;
        let recorder = self
            .observer
            .as_deref()
            .and_then(|observer| observer.charges.as_ref());
        let below = self.plans as usize + usize::from(task != TaskId::MAIN || !self.session_cell);
        let mut arena = self.arena.borrow_mut();
        let running = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let ground = Ground {
            exe,
            arena: &mut arena,
            session: &self.session,
            tape: recorder.filter(|_| charging),
            recorder,
            charging,
            bound: if charging {
                self.bounds.charge_limit
            } else {
                u64::MAX
            },
            slice_end: self.slice_end,
            observed: self.observer.is_some(),
            limits: self.limits,
            memory: self.bounds.memory_meter != crate::MemoryMeter::Off,
            base: (running.frames.len() + below) as u64,
            call_depth: u64::from(self.bounds.call_depth),
        };
        let ran = enter(
            ground,
            &mut self.islands,
            &mut running.counters,
            &mut self.charged,
            (target, &self.regs[first..first + given]),
            occurrence,
            &[],
        );
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
            return Ok(self.run_on(host, task, exe, (call.stmt, call.resume, regs))?);
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

    /// Pushes the machine frames of the virtual frames a callback's island
    /// left, `levels` innermost first, above the frames its helper's plans
    /// stand in: the statement the top frame stands in runs on once the
    /// step settles ([`Self::island_run_on`]).
    #[cold]
    #[inline(never)]
    pub(in crate::machine) fn stand_island(
        &mut self,
        task: TaskId,
        exe: &Executable,
        levels: Vec<Level>,
    ) -> Eval<()> {
        let mut top = None;
        for level in levels.into_iter().rev() {
            top = self.convert_level(task, exe, level)?;
        }
        self.islands.run_on = top.map(|top| (task, top));
        Ok(())
    }

    /// Runs on the statement a callback island's top frame stands in, if
    /// any, once the step that stood its frames settled as `settled` says.
    #[cold]
    #[inline(never)]
    pub(in crate::machine) fn island_run_on(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        settled: Result<(), Halt>,
    ) -> Result<(), Halt> {
        let top = self.islands.run_on.take();
        settled?;
        let Some((stands, top)) = top else {
            return Ok(());
        };
        if stands != task {
            return Err(fault("an island's frames stand in another task"));
        }
        self.refresh_charging(task)?;
        self.run_on(host, task, exe, top)
    }

    /// The top frame an island left runs its statement on from the op the
    /// island stopped at, holding what the island held there, with islands
    /// held off until the step ends.
    fn run_on(
        &mut self,
        host: &mut dyn Host,
        task: TaskId,
        exe: &Executable,
        (stmt, pc, regs): RunsOn,
    ) -> Result<(), Halt> {
        for (reg, value) in regs {
            self.put(reg as usize, value);
        }
        self.islands.held = true;
        let running = Running::new(task, exe, stmt);
        let outcome = self
            .run(host, &running, pc as usize)
            .and_then(|ran| self.resumed(task, exe, (stmt, ran)));
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
        let mut frame = self.fresh_frame(exe, level.code)?;
        frame.slots.fresh(level.slots.len());
        frame.control = level.control;
        for (at, slot) in level.slots.into_iter().enumerate() {
            match slot {
                SlotState::Empty => {}
                SlotState::Value(value) => {
                    frame.set_slot(at as u32, SlotState::Value(value));
                }
                cell @ SlotState::Cell(_) => frame.set_slot(at as u32, cell),
            }
        }
        let top = match level.stands {
            Stands::Boundary => None,
            Stands::Running { stmt, pc, regs } => Some((stmt, pc, regs)),
            Stands::Called { stmt, act, roles } => {
                let entry = &exe.document_stream().nested[act.ix()];
                let pending =
                    self.continuation(&entry.suffix, PhaseRecord::Called, roles.len(), |_, at| {
                        roles[at].clone()
                    })?;
                frame.stand_at(act, pending);
                frame.awaiting = Some(stmt);
                None
            }
        };
        self.task(task)?.frames.push(frame);
        self.grown();
        self.observe(crate::observe::Event::Push {
            task,
            code: level.code,
        });
        Ok(top)
    }
}

/// What an island entry runs under, borrowed from the machine or from the
/// loop region whose statement makes the call.
struct Ground<'a> {
    exe: &'a Executable,
    arena: &'a mut Arena,
    session: &'a Session,
    tape: Option<&'a ChargeTape>,
    recorder: Option<&'a ChargeTape>,
    charging: bool,
    bound: u64,
    slice_end: u64,
    observed: bool,
    limits: KernelDataLimits,
    memory: bool,
    base: u64,
    call_depth: u64,
}

/// Runs the call of `target` on `values` as an island in `scratch`,
/// counting `occurrence` in `counters` when given, from the charge
/// `charged`, which it advances: first by the fixed events `before` its
/// caller's statement charges ahead of the call, once the callee is known
/// to have an island that takes these arguments. `None`: the island does
/// not run, and nothing changed.
#[inline(always)]
fn enter(
    ground: Ground<'_>,
    scratch: &mut Scratch,
    counters: &mut Occurrences,
    charged: &mut u64,
    (target, values): (Target<'_>, &[Value]),
    occurrence: Option<usize>,
    before: &[Weight],
) -> Result<Option<Ran>, Halt> {
    let exe = ground.exe;
    scratch.epoch = scratch.epoch.wrapping_add(1);
    scratch.captured.clear();
    scratch.sessions.clear();
    let mut island = Island {
        exe,
        stream: exe.document_stream(),
        arena: ground.arena,
        session: ground.session,
        tape: ground.tape,
        recorder: ground.recorder,
        charging: ground.charging,
        charged: *charged,
        bound: ground.bound,
        slice_end: ground.slice_end,
        counters,
        observed: ground.observed,
        limits: ground.limits,
        memory: ground.memory,
        base: ground.base,
        call_depth: ground.call_depth,
        depth: 0,
        scratch,
    };
    let target = match target {
        Target::Declared(code) => island.declared(code),
        Target::Closure(handle) => island.resolve_cold(None, handle),
        Target::Code(code, cells) => island.with_cells(code, cells),
    };
    let Some((plan, captured)) = target.filter(|(plan, _)| {
        let plan = &exe.islands.plans[*plan as usize];
        values.len() <= plan.params.len()
            && values.iter().all(held)
            && ground.base <= ground.call_depth
    }) else {
        return Ok(None);
    };
    for weight in before {
        island.charged += weight.units();
        if let Some(tape) = island.tape {
            tape.push(*weight);
        }
    }
    let ran = occurrence
        .map_or(Ok(()), |occurrence| island.count(occurrence))
        .and_then(|()| island.enter(plan, captured, values));
    *charged = island.charged;
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
    pub(in crate::machine) exe: &'a Executable,
    pub(in crate::machine) session: &'a mut Session,
    /// The plan's session bindings, which the region holds as slots.
    pub(in crate::machine) names: &'a [BindingName],
    pub(in crate::machine) scratch: &'a mut Scratch,
    /// The tape a charged run records on, and the one its calls are
    /// counted on, charged or paced.
    pub(in crate::machine) tape: Option<&'a ChargeTape>,
    pub(in crate::machine) recorder: Option<&'a ChargeTape>,
    /// Whether the run is charged, or paced with charge Off.
    pub(in crate::machine) charging: bool,
    /// The run's bound, `u64::MAX` when it is paced.
    pub(in crate::machine) bound: u64,
    pub(in crate::machine) slice_end: u64,
    pub(in crate::machine) observed: bool,
    pub(in crate::machine) limits: KernelDataLimits,
    /// The memory bound, when memory is accounted.
    pub(in crate::machine) memory: Option<u64>,
    /// The capacity the run's heap was sealed holding, until its first
    /// sweep: the least the capacity meter counts (FIG-6791).
    pub(in crate::machine) floor: u64,
    pub(in crate::machine) base: u64,
    pub(in crate::machine) call_depth: u64,
    /// The frames an island that stopped left, innermost first.
    pub(in crate::machine) levels: Vec<Level>,
    pub(in crate::machine) halt: Option<Halt>,
}

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
            || charged.checked_add(call.units)? >= self.slice_end
            || self.base > self.call_depth
            || args.len() > MAX_ARGS
            || !args.iter().all(held)
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
            (None, Some(Value::Closure(handle))) => Target::Closure(handle),
            _ => return None,
        };
        let counters = cx.counters.as_deref_mut()?;
        if !counters.can_advance(&[call.occurrence]) {
            return None;
        }
        // The island reads the bindings the region wrote as the region
        // left them: its writes so far are published first, in order.
        let mut written: Vec<(u64, usize)> = cx
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.written != 0)
            .map(|(at, slot)| (slot.written, at))
            .collect();
        written.sort_unstable();
        for (_, at) in written {
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
        }
        let ground = Ground {
            exe: self.exe,
            arena: &mut *cx.arena,
            session: self.session,
            tape: self.tape,
            recorder: self.recorder,
            charging: self.charging,
            bound: self.bound,
            slice_end: self.slice_end,
            observed: self.observed,
            limits: self.limits,
            memory: self.memory.is_some(),
            base: self.base,
            call_depth: self.call_depth,
        };
        let ran = enter(
            ground,
            self.scratch,
            counters,
            &mut cx.charged,
            (target, args),
            Some(call.occurrence),
            &call.before,
        );
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
