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

use std::rc::Rc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::{ActIx, RegRange};
use lash_kernel_state::paged::PhaseRecord;

use super::super::frames::Pending;
use super::super::ops::Flow;
use super::super::stream::Running;
use super::super::{Eval, FRAME_BYTES, Halt, KernelMachine, SlotState, fault};
use super::{
    Exit, ISLAND_DEPTH, ISLAND_HOLD, Island, Level, MAX_ARGS, MAX_SLOTS, RunsOn, SLOT_BYTES,
    Stands, Stop, closure_island, held,
};
use crate::compile::{CodeId, Executable, NestedCallee, NestedKind};
use crate::interface::Host;

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
            self.charging,
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
        let ran = self.island(
            host.observes_sites(),
            running.task,
            running.exe,
            self.charging,
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
        self.island(
            !self.islands.quiet,
            task,
            exe,
            exe.code(code).charging,
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
    /// counting `occurrence` as the call's when given. `charging` is
    /// whether the callee runs charged, `sites` whether the host observes
    /// sites. `None`: the island does not run, and nothing changed.
    #[expect(
        clippy::too_many_arguments,
        reason = "each caller states where its callee runs and what it counts"
    )]
    fn island(
        &mut self,
        sites: bool,
        task: TaskId,
        exe: &Executable,
        charging: bool,
        target: Target<'_>,
        (first, given): (usize, usize),
        occurrence: Option<usize>,
    ) -> Eval<Option<Ran>> {
        // The callee's code has an island that takes these arguments,
        // before any guard is paid for.
        let plan = match target {
            Target::Declared(code) | Target::Code(code, _) => exe.island_of(code),
            Target::Closure(handle) => {
                closure_island(&self.arena.borrow(), exe, handle).map(|(plan, _)| plan)
            }
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
            || !charging
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
        // The memory headroom guard: the island's transient holdings and
        // its virtual frames with their variables fit, and nothing inside
        // it is admitted.
        if self.bounds.memory_meter != crate::MemoryMeter::Off
            && self
                .bounds
                .memory
                .saturating_sub(self.heap.certificate().upper.0)
                < ISLAND_HOLD
                    + u64::from(ISLAND_DEPTH) * (FRAME_BYTES + MAX_SLOTS as u64 * SLOT_BYTES)
        {
            return Ok(None);
        }
        if given > MAX_ARGS {
            return Ok(None);
        }
        let mut values = [Value::Absent; MAX_ARGS];
        for (index, value) in values.iter_mut().enumerate().take(given) {
            *value = self.regs[first + index];
        }
        let values = &values[..given];
        let frames = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .len();
        let base = (frames
            + self.plans as usize
            + usize::from(task != TaskId::MAIN || !self.session_cell)) as u64;
        let call_depth = u64::from(self.bounds.call_depth);
        let observed = self.observer.is_some();
        let mut scratch = std::mem::take(&mut self.islands);
        scratch.epoch = scratch.epoch.wrapping_add(1);
        scratch.captured.clear();
        let arena = Rc::clone(&self.arena);
        let ran = {
            let arena = arena.borrow();
            let counters = &mut self
                .tasks
                .get_mut(task.0 as usize)?
                .ok_or_else(|| fault("no task"))?
                .counters;
            let mut island = Island {
                exe,
                stream: exe.document_stream(),
                arena: &arena,
                tape: self
                    .observer
                    .as_deref()
                    .and_then(|observer| observer.charges.as_ref()),
                charged: self.charged,
                bound: self.bounds.charge_limit,
                slice_end: self.slice_end,
                counters,
                observed,
                limits: self.limits,
                memory: self.bounds.memory_meter != crate::MemoryMeter::Off,
                base,
                call_depth,
                depth: 0,
                scratch: &mut scratch,
            };
            let target = match target {
                Target::Declared(code) => island.declared(code),
                Target::Closure(handle) => island.resolve_cold(None, handle),
                Target::Code(code, cells) => island.with_cells(code, cells),
            };
            let plan = target.filter(|(plan, _)| {
                let plan = &exe.islands.plans[*plan as usize];
                given <= plan.params.len() && values.iter().all(held) && base <= call_depth
            });
            let ran = plan.map(|(plan, captured)| {
                occurrence
                    .map_or(Ok(()), |occurrence| island.count(occurrence))
                    .and_then(|()| island.enter(plan, captured, values))
            });
            (ran, island.charged)
        };
        let (ran, charged) = ran;
        self.charged = charged;
        let Some(ran) = ran else {
            self.islands = scratch;
            return Ok(None);
        };
        match ran {
            Ok(value) => {
                self.islands = scratch;
                Ok(Some(Ran::Returned(value)))
            }
            Err(Stop) => {
                let halt = scratch.exit.halt.take();
                let levels = std::mem::take(&mut scratch.exit.levels);
                scratch.exit = Exit::default();
                self.islands = scratch;
                if let Some(halt) = halt {
                    return Err(halt.into());
                }
                Ok(Some(Ran::Exited(levels)))
            }
        }
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
            Some(top) => Ok(self.run_on(host, task, exe, top)?),
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
        self.reserve_growth(FRAME_BYTES)?;
        let mut frame = self.fresh_frame(exe, level.code)?;
        frame.slots.fresh(level.slots.len());
        frame.control = level.control;
        for (at, slot) in level.slots.into_iter().enumerate() {
            match slot {
                SlotState::Empty => {}
                SlotState::Value(value) => {
                    self.reserve_growth(self.cost_of(&value))?;
                    frame.set_slot(at as u32, SlotState::Value(value));
                }
                cell @ SlotState::Cell(_) => frame.set_slot(at as u32, cell),
            }
        }
        let top = match level.stands {
            Stands::Boundary => None,
            Stands::Running { stmt, pc, regs } => Some((stmt, pc, regs)),
            Stands::Called { stmt, act, roles } => {
                for role in &roles {
                    self.reserve_growth(self.cost_of(&role.value))?;
                }
                let entry = &exe.document_stream().nested[act.ix()];
                frame.pending.set(Pending {
                    site: std::sync::Arc::clone(&entry.suffix),
                    phase: PhaseRecord::Called,
                    roles,
                });
                frame.nested = Some(act);
                frame.awaiting = Some(stmt);
                None
            }
        };
        self.task(task)?.frames.push(frame);
        self.observe(crate::observe::Event::Push {
            task,
            code: level.code,
        });
        Ok(top)
    }
}
