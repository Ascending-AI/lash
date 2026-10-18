//! Call islands (`docs/design/kernel-regions.md` §1, §3, §5 R4): a call of
//! a function whose body lowering formed as an island runs inside the
//! caller's step as virtual frames, each an array of variables on the
//! native stack, from the plan's compiled form. Calls between islands go
//! through a per-site cache from the closure handle (or the declared code)
//! to the callee's plan.
//!
//! An island charges each event as the reference does, in its order, and
//! counts each call's occurrence where the reference counts it, so nothing
//! it commits is ever taken back. With charge Off it runs paced: it
//! measures no formula, and its statements' units count the run's pace,
//! as `advance` counts them, so the slice's end stops it where it stops
//! the reference. It exits where the reference would have
//! to run on its own, and converts its virtual frames into the machine
//! frames the reference holds there (§3.4):
//!
//! - at a step end the slice threshold has reached, or before a statement
//!   whose unit would pass the bound: the frames stand at that step end,
//!   and the step ends;
//! - at an op it cannot run (a guard misses, a value would raise or grow, a
//!   charge would pass the bound, a callee is no island, the depth would
//!   pass a bound): the top frame stands in its statement at that op with
//!   the registers it holds there, and the reference runs the statement on
//!   from that op inside the same step, with islands held off until the
//!   step ends.
//!
//! Each frame below the top stands at its call with the operands its
//! statement evaluated as the continuation's roles, exactly as the
//! reference saves them, so a park or seal after the exit projects the same
//! bytes.

mod compiled;
mod entry;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::LogicalBytes;
use lash_kernel_doc::{
    Args, KernelDataLimits, NativeCall, Nursery, PreparedAnswer, TaskId, Value, WorkCounter,
};
use lash_kernel_exec::{ActIx, Weight};
use lash_kernel_state::arena::Arena;

use super::frames::Role;
use super::session::Session;
use super::tasks::Occurrences;
use super::{Control, Halt, SlotState, fault};
pub(crate) use compiled::Compiled;
use compiled::{CBlock, CKind, CStatement, CallSite, Depth, NativeSite, OperatorSite};

use crate::compile::expr::Close;
use crate::compile::islands::{HELD_BYTES, IslandPlan, MAX_SLOTS, held};
use crate::compile::{
    BlockId, Charge, CodeId, Executable, Fast, Lib, LibRun, measured_formula, position, scalar,
};
use crate::compile::{StmtId, Stream};
use crate::observe::ChargeTape;

/// The most virtual frames an island nests: the native stack it uses.
const ISLAND_DEPTH: u32 = 200;

/// What an island's statements may hold while it runs, admitted once at
/// its entry with its virtual frames (§3.5).
const ISLAND_HOLD: u64 = 1 << 16;

/// The most a variable of a virtual frame costs the census: a value it
/// may hold ([`held`]).
const SLOT_BYTES: u64 = HELD_BYTES;

/// The headroom an island's entry admits: its statements' holdings and its
/// deepest virtual stack with its variables (§3.5).
pub(super) const ISLAND_ADMITTED: u64 =
    ISLAND_HOLD + ISLAND_DEPTH as u64 * (super::FRAME_BYTES + MAX_SLOTS as u64 * SLOT_BYTES);

/// The most arguments an island call passes.
const MAX_ARGS: usize = MAX_SLOTS;

/// A virtual frame's variables.
type Slots = [Option<Value>; MAX_SLOTS];

/// An island stopped: what it stopped at is in its [`Exit`].
pub(super) struct Stop;

type Answer = Result<Value, Stop>;

/// How a statement left its block.
enum Next {
    /// It completed in its block, whose step edge leaves this many blocks,
    /// its own first.
    Done(u32),
    Returned(Value),
}

/// How a block ended.
enum Flow2 {
    /// Its last statement completed, and its step edge leaves this many
    /// blocks beyond it.
    Fell(u32),
    Returned(Value),
}

/// Where an exited frame stands.
pub(super) enum Stands {
    /// At a step end, as its control says.
    Boundary,
    /// In statement `stmt` at op `pc`, holding `regs`.
    Running {
        stmt: StmtId,
        pc: u32,
        regs: Vec<(u32, Value)>,
    },
    /// At the call `act` of statement `stmt`, its roles saved.
    Called {
        stmt: StmtId,
        act: ActIx,
        roles: Vec<Role>,
    },
}

/// A virtual frame as the exit converts it.
pub(super) struct Level {
    code: CodeId,
    slots: Vec<SlotState>,
    /// Its control, outermost first.
    control: Vec<Control>,
    stands: Stands,
}

/// What an island stopped at, built as the stop leaves each frame.
#[derive(Default)]
pub(super) struct Exit {
    /// The op the reference resumes at; none at a step end.
    at: Option<u32>,
    halt: Option<Halt>,
    /// The registers the frame being left holds.
    regs: Vec<(u32, Value)>,
    /// Its control, innermost first.
    control: Vec<Control>,
    stands: Option<Stands>,
    /// The call it stands at, when its callee stopped.
    suspended: Option<ActIx>,
    /// The frames left, innermost first.
    levels: Vec<Level>,
}

/// A closure call site's cached callee, for one island entry.
#[derive(Clone, Copy)]
struct Cached {
    epoch: u32,
    handle: ArenaHandle,
    plan: u32,
    captured: u32,
}

/// The statement a converted top frame runs on, the op it runs on from and
/// the registers it holds there.
type RunsOn = (StmtId, u32, Vec<(u32, Value)>);

pub(super) use entry::{Ran, RegionGuests};

/// The machine's island state: buffers reused across entries, never saved.
#[derive(Default)]
pub(super) struct Scratch {
    /// Islands are held off until the step ends: a statement an island
    /// exited in runs on through the reference.
    held: bool,
    epoch: u32,
    cached: Vec<Option<Cached>>,
    /// The captured values and their cells, of each cached callee.
    captured: Vec<(Value, ArenaHandle)>,
    /// Each session binding read, by its name in the stream, at its
    /// resident value: nothing writes a binding while an island runs.
    sessions: Vec<(u32, Value)>,
    exit: Exit,
    /// The running slice's host observes no sites: callbacks a helper's
    /// plan applies may run as islands.
    pub(super) quiet: bool,
    /// The statement the top frame a callback's island left runs on once
    /// the step settles, in its task.
    run_on: Option<(TaskId, RunsOn)>,
}

impl Scratch {
    /// Whether a callback's island left a statement to run on.
    #[inline(always)]
    pub(super) fn runs_on(&self) -> bool {
        self.run_on.is_some()
    }
}

/// One island entry: what it charges and counts, and its frames' bounds.
struct Island<'a> {
    exe: &'a Executable,
    stream: &'a Stream,
    arena: &'a Arena,
    /// The session overlay its binding reads take resident values from.
    session: &'a Session,
    tape: Option<&'a ChargeTape>,
    /// The tape the calls it enters are counted on, charged or paced.
    recorder: Option<&'a ChargeTape>,
    /// Whether the run is charged: a paced one measures no formula, and
    /// its prepared entries answer by their value-only guards.
    charging: bool,
    /// The run's charge, or with charge Off its pace, the machine's once
    /// the island ends.
    charged: u64,
    bound: u64,
    slice_end: u64,
    counters: &'a mut Occurrences,
    /// An observer is attached: helper calls answer as observed runs do.
    observed: bool,
    /// The run's data limits, which natives answer under.
    limits: KernelDataLimits,
    /// Memory is accounted: a native answers only with nothing built.
    memory: bool,
    /// The call depth the frames below the island hold (`K-BND-002`).
    base: u64,
    call_depth: u64,
    /// The virtual frames running.
    depth: u32,
    scratch: &'a mut Scratch,
}

impl Island<'_> {
    /// A stop at op `pc`, which the reference runs.
    #[cold]
    fn at(&mut self, pc: u32) -> Stop {
        self.scratch.exit.at = Some(pc);
        Stop
    }

    /// A stop at a step end.
    #[cold]
    fn boundary(&mut self, block: BlockId, next: usize) -> Stop {
        self.scratch.exit.at = None;
        self.scratch
            .exit
            .control
            .push(Control::Block { block, next });
        Stop
    }

    #[cold]
    fn halt(&mut self, halt: Halt) -> Stop {
        self.scratch.exit.halt = Some(halt);
        Stop
    }

    /// `stop`, the frame being left holding `value` in register `reg`.
    #[cold]
    fn held(&mut self, stop: Stop, reg: u32, value: Value) -> Stop {
        self.scratch.exit.regs.push((reg, value));
        stop
    }

    /// A stop at op `pc`, the frame holding `value` in register `reg`.
    #[cold]
    fn at_held(&mut self, pc: u32, reg: u32, value: Value) -> Stop {
        let stop = self.at(pc);
        self.held(stop, reg, value)
    }

    /// `stop` inside the operands of call `site`, which hold `callee` and
    /// the arguments `args` evaluated before it.
    #[cold]
    fn call_held(
        &mut self,
        mut stop: Stop,
        site: &CallSite,
        callee: Option<Value>,
        args: &[Value],
    ) -> Stop {
        let mut reg = site.reg;
        if let Some(callee) = callee {
            stop = self.held(stop, reg, callee);
            reg += 1;
        }
        for (index, value) in args.iter().enumerate() {
            stop = self.held(stop, reg + index as u32, *value);
        }
        stop
    }

    #[inline(always)]
    fn fits(&self, units: u64) -> bool {
        self.charged
            .checked_add(units)
            .is_some_and(|end| end <= self.bound)
    }

    /// Whether the slice threshold is reached: a step that ends here
    /// returns the slice.
    #[inline(always)]
    fn spent(&self) -> bool {
        self.charged >= self.slice_end
    }

    /// A statement's unit, as `advance` charges it.
    #[inline(always)]
    fn unit(&mut self) -> bool {
        if !self.fits(1) {
            return false;
        }
        self.charged += 1;
        if let Some(tape) = self.tape {
            tape.push(Weight::Atomic(1));
        }
        true
    }

    /// Closes charge region `close`, charged whole with its events, as
    /// `close_region` does; `false` when it would pass the bound.
    #[inline(always)]
    fn close(&mut self, close: &Close) -> bool {
        let total = u64::from(close.total);
        if !self.fits(total) {
            return false;
        }
        self.charged += total;
        if let Some(tape) = self.tape {
            record(tape, self.stream, close.region);
        }
        true
    }

    /// A formula's `units`, charged whole.
    #[inline(always)]
    fn formula(&mut self, units: u64) -> bool {
        if !self.fits(units) {
            return false;
        }
        self.charged += units;
        if units != 0
            && let Some(tape) = self.tape
        {
            tape.push(Weight::Atomic(units));
        }
        true
    }

    fn count(&mut self, occurrence: usize) -> Result<(), Stop> {
        if let Err(halt) = self.counters.advance(occurrence) {
            return Err(self.halt(halt));
        }
        Ok(())
    }

    /// Whether `extra` more frames than the running ones fit the depth.
    #[inline(always)]
    fn depth_fits(&self, depth: Depth) -> bool {
        let running = self.base + u64::from(self.depth);
        match depth {
            Depth::None => true,
            Depth::Operation(extra) => {
                running <= self.call_depth
                    && (extra <= 1 || u64::from(extra) <= self.call_depth - running)
            }
            Depth::Template(extra) => running + u64::from(extra) <= self.call_depth,
        }
    }

    /// The operator `site` on `args`: its prepared entry's value, charged
    /// its formula from its scalar table on scalars, measured otherwise, or
    /// a stop at its op.
    #[inline(always)]
    fn operate(&mut self, site: &OperatorSite, args: &[Value]) -> Answer {
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        // The prepared entry answers and declines as its charged guard
        // does, and on scalars the sizes the table indexes are the values'.
        let charge =
            if !(!site.body_when_observed || !self.observed) || !self.depth_fits(site.depth) {
                None
            } else if !self.charging {
                site.entry.call(args).filter(held).map(|value| (value, 0))
            } else if args.iter().all(scalar) {
                match site.entry.call(args) {
                    Some(result) if scalar(&result) => site
                        .table
                        .units(sizes(args, &result))
                        .map(|units| (result, units)),
                    Some(_) => self.measured(site, args),
                    None => None,
                }
            } else {
                self.measured(site, args)
            };
        let Some((result, units)) = charge else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        if !self.fits(units) {
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        if let Some(occurrence) = site.counted {
            self.count(occurrence)?;
        }
        self.formula(units);
        Ok(result)
    }

    /// The operator `site` on `args`, not all scalars, or answering no
    /// scalar: its prepared entry's answer, when the frame may hold it,
    /// charged as the reference charges a prepared answer, by its scalar
    /// table when the entry certifies the sizes and by its formula measured
    /// on the values otherwise. `None` where the reference runs it: the
    /// entry declines, the value is not [`held`], or a measured value is
    /// not where the arena can read it.
    #[inline(never)]
    fn measured(&self, site: &OperatorSite, args: &[Value]) -> Option<(Value, u64)> {
        let function = self.exe.lib(site.lib);
        let (_, fast) = function.prepared.as_ref()?;
        let answer = site.entry.answer(args, true)?;
        self.answered(function, fast, args, answer)
    }

    /// A prepared entry's `answer` of `function`'s call on `args`, when the
    /// frame may hold it, with what the reference charges it.
    fn answered(
        &self,
        function: &Lib,
        fast: &Fast,
        args: &[Value],
        answer: PreparedAnswer,
    ) -> Option<(Value, u64)> {
        let value = answer.value;
        if !held(&value) {
            return None;
        }
        if !self.charging {
            return Some((value, 0));
        }
        let units = match answer.scalar_sizes.and_then(|mask| fast.scalar_units(mask)) {
            Some(units) => units,
            None => match fast.charge(&function.charge, args, &value) {
                Charge::Units(units) => units,
                Charge::Plan(plan) => measured_formula(self.arena, plan, args, Some(&value))?,
            },
        };
        Some((value, units))
    }

    /// The general call `site` of a native on `args`: its prepared entry's
    /// answer or its native's, charged its formula, or a stop at its op.
    #[inline(always)]
    fn native(&mut self, site: &NativeSite, args: &[Value]) -> Answer {
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        let charge = self
            .native_answer(site, args)
            .filter(|(_, units)| self.fits(*units));
        let Some((value, units)) = charge else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        self.formula(units);
        Ok(value)
    }

    /// What the reference's general call answers `site` on `given`, its
    /// other parameters absent: its prepared entry's answer where its guard
    /// holds, its native's otherwise, with what it charges. `None` where
    /// the reference runs it: the native builds or holds anything, raises,
    /// declines or asks again, or answers what the frame may not hold.
    #[inline(never)]
    fn native_answer(&self, site: &NativeSite, given: &[Value]) -> Option<(Value, u64)> {
        let function = self.exe.lib(site.lib);
        let mut padded = [Value::Absent; MAX_SLOTS];
        padded[..given.len()].copy_from_slice(given);
        let args = &padded[..function.arity];
        if let Some((entry, fast)) = &function.prepared
            && self.depth_fits(site.depth)
            && let Some(answer) = entry.answer(args, self.charging)
        {
            return self.answered(function, fast, args, answer);
        }
        let LibRun::Native { native, .. } = &function.run else {
            return None;
        };
        let arena = self.arena;
        let limit = match &function.limit {
            Some(limit) => Some(measured_formula(arena, limit, args, None)?),
            None => None,
        };
        let mut counter = WorkCounter::new(limit);
        let semantics = arena.semantics().with_limits(self.limits);
        let mut nursery = if self.memory {
            Nursery::new(arena.objects(), LogicalBytes(0))
        } else {
            Nursery::unaccounted(arena.objects())
        };
        let answer = native.call(NativeCall {
            args: Args::new(args, arena.objects(), &semantics),
            new: &mut nursery,
            counter: &mut counter,
        });
        let built = !nursery.built().is_empty() || nursery.held().0 != 0;
        nursery.discard();
        let value = answer.ok().filter(|value| !built && held(value))?;
        if arena.summary_of(&value).depth as usize > self.limits.value_depth() {
            return None;
        }
        if !self.charging {
            return Some((value, 0));
        }
        let units = measured_formula(arena, &function.charge, args, Some(&value))?;
        Some((value, units))
    }

    /// The session binding named `name` in the stream, at its resident
    /// value when the frame may hold it; `None` where the reference reads
    /// it: no value is resident for it (`docs/design/kernel-regions.md`
    /// §3.6), or one the frame may not hold.
    #[inline(always)]
    fn session(&mut self, name: u32) -> Option<Value> {
        match self.scratch.sessions.iter().find(|(read, _)| *read == name) {
            Some((_, value)) => Some(*value),
            None => self.session_cold(name),
        }
    }

    #[inline(never)]
    fn session_cold(&mut self, name: u32) -> Option<Value> {
        let binding = self.stream.names.get(name as usize)?;
        let value = self.session.resident(&binding.key).filter(held)?;
        self.scratch.sessions.push((name, value));
        Some(value)
    }

    /// `target[at]` of a list or a tuple, when the frame may hold it;
    /// `None` where the reference reads it: another target, no item
    /// there, or one the arena cannot read where it is.
    #[inline(always)]
    fn item(&self, target: Value, at: Value) -> Option<Value> {
        let (Value::List(object) | Value::Tuple(object)) = target else {
            return None;
        };
        let value = self.arena.item(object, position(&at)?).ok()??;
        held(&value).then_some(value)
    }

    /// A stop at op `pc`, the frame holding operands `args` from register
    /// `reg`.
    #[cold]
    fn at_operands(&mut self, pc: u32, reg: u32, args: &[Value]) -> Stop {
        let mut stop = self.at(pc);
        for (index, value) in args.iter().enumerate() {
            stop = self.held(stop, reg + index as u32, *value);
        }
        stop
    }

    /// The call `site` of `callee`, or of its declared code, on `args`: the
    /// callee's island run as a virtual frame.
    fn call(&mut self, site: &CallSite, callee: Option<Value>, args: &[Value]) -> Answer {
        if let Some(close) = &site.close
            && !self.close(close)
        {
            let stop = self.at(close.head);
            return Err(self.call_held(stop, site, callee, args));
        }
        let target = match (site.callee, callee) {
            (Some(code), _) => self.declared(code),
            (None, Some(callee)) => self.resolve(site.site, callee),
            (None, None) => None,
        };
        let fits = |island: &Self, plan: u32| {
            let plan = &island.exe.islands.plans[plan as usize];
            args.len() <= plan.params.len()
                && args.iter().all(held)
                && island.base + u64::from(island.depth) <= island.call_depth
                && island.depth < ISLAND_DEPTH
        };
        let Some((plan, captured)) = target.filter(|(plan, _)| fits(self, *plan)) else {
            let stop = self.at(site.pc);
            return Err(self.call_held(stop, site, callee, args));
        };
        self.count(site.occurrence)?;
        match self.enter(plan, captured, args) {
            Ok(value) => Ok(value),
            Err(stop) => {
                if self.scratch.exit.halt.is_none() {
                    self.scratch.exit.suspended = Some(site.act);
                }
                Err(stop)
            }
        }
    }

    /// The island of declared code `code`.
    fn declared(&mut self, code: CodeId) -> Option<(u32, u32)> {
        let plan = self.exe.island_of(code)?;
        self.exe.islands.plans[plan as usize].compiled(self.exe)?;
        Some((plan, self.scratch.captured.len() as u32))
    }

    /// The island of the closure `callee`, through call site `site`'s
    /// cache: its plan and where its captured values start.
    #[inline(always)]
    fn resolve(&mut self, site: u32, callee: Value) -> Option<(u32, u32)> {
        let Value::Closure(handle) = callee else {
            return None;
        };
        if let Some(Some(cached)) = self.scratch.cached.get(site as usize)
            && cached.epoch == self.scratch.epoch
            && cached.handle == handle
        {
            return Some((cached.plan, cached.captured));
        }
        self.resolve_cold(Some(site), handle)
    }

    /// The island of the closure `handle`, cached for call site `site`
    /// when one is given.
    #[inline(never)]
    fn resolve_cold(&mut self, site: Option<u32>, handle: ArenaHandle) -> Option<(u32, u32)> {
        let (plan, cells) = closure_island(self.arena, self.exe, handle)?;
        let captured = self.captured(plan, cells)?;
        let Some(site) = site else {
            return Some((plan, captured));
        };
        let at = site as usize;
        if self.scratch.cached.len() <= at {
            self.scratch.cached.resize(at + 1, None);
        }
        self.scratch.cached[at] = Some(Cached {
            epoch: self.scratch.epoch,
            handle,
            plan,
            captured,
        });
        Some((plan, captured))
    }

    /// The island of code `code`, closed over the cells `cells`: its plan
    /// and where its captured values start.
    fn with_cells(&mut self, code: CodeId, cells: &[ArenaHandle]) -> Option<(u32, u32)> {
        let plan = self.exe.island_of(code)?;
        Some((plan, self.captured(plan, cells)?))
    }

    /// Reads the cells `cells` of a callee of plan `plan` into the captured
    /// values: where they start.
    fn captured(&mut self, plan: u32, cells: &[ArenaHandle]) -> Option<u32> {
        let island = &self.exe.islands.plans[plan as usize];
        island.compiled(self.exe)?;
        if cells.len() != island.captures.len() {
            return None;
        }
        let captured = self.scratch.captured.len() as u32;
        for cell in cells {
            let value = self.arena.cell(*cell).ok()?;
            self.scratch.captured.push((value, *cell));
        }
        Some(captured)
    }

    /// Runs plan `plan` on `args` as a new virtual frame, its captured
    /// values from `captured`: its value, or a stop with the frame left.
    fn enter(&mut self, plan: u32, captured: u32, args: &[Value]) -> Answer {
        let exe = self.exe;
        let island = &exe.islands.plans[plan as usize];
        let Some(compiled) = island.compiled(exe) else {
            return Err(self.halt(fault("an island entered uncompiled")));
        };
        let mut slots: Slots = [None; MAX_SLOTS];
        for (index, at) in island.params.iter().enumerate() {
            slots[*at as usize] = Some(args.get(index).copied().unwrap_or(Value::Absent));
        }
        for (index, at) in island.captures.iter().enumerate() {
            let (value, _) = self.scratch.captured[captured as usize + index];
            slots[*at as usize] = Some(value);
        }
        // The call's frame is pushed: its step ends.
        if self.spent() {
            let stop = self.boundary(compiled.body.id, 0);
            self.left(island, captured, &slots);
            return Err(stop);
        }
        if let Some(recorder) = self.recorder {
            recorder.entered();
        }
        self.depth += 1;
        let ran = run_block(self, &compiled.body, &mut slots);
        self.depth -= 1;
        match ran {
            Ok(Flow2::Returned(value)) => Ok(value),
            // The body ran its last statement: the frame returns null.
            Ok(Flow2::Fell(_)) => Ok(Value::Null),
            Err(stop) => {
                if self.scratch.exit.halt.is_none() {
                    self.left(island, captured, &slots);
                }
                Err(stop)
            }
        }
    }

    /// The frame of `plan` holding `slots` is left by a stop: its record.
    #[cold]
    fn left(&mut self, plan: &IslandPlan, captured: u32, slots: &Slots) {
        let exit = &mut self.scratch.exit;
        let mut slots: Vec<SlotState> = slots[..plan.slots]
            .iter()
            .map(|slot| slot.map_or(SlotState::Empty, SlotState::Value))
            .collect();
        for (index, at) in plan.captures.iter().enumerate() {
            let (_, cell) = self.scratch.captured[captured as usize + index];
            slots[*at as usize] = SlotState::Cell(cell);
        }
        let control = exit.control.drain(..).rev().collect();
        let stands = exit.stands.take().unwrap_or(Stands::Boundary);
        exit.regs.clear();
        exit.levels.push(Level {
            code: plan.code,
            slots,
            control,
            stands,
        });
    }

    /// `stop` inside statement `stmt` of the frame being left: where the
    /// frame stands.
    #[cold]
    fn stand(&mut self, stmt: StmtId, stop: Stop) -> Stop {
        let exit = &mut self.scratch.exit;
        if exit.halt.is_some() || exit.stands.is_some() {
            return stop;
        }
        let regs = std::mem::take(&mut exit.regs);
        let stands = if let Some(act) = exit.suspended.take() {
            let entry = &self.stream.nested[act.ix()];
            let mut roles = Vec::with_capacity(entry.roles.len());
            for role in &entry.roles {
                let Some((_, value)) = regs.iter().find(|(reg, _)| *reg == role.reg.0) else {
                    return self.halt(fault("an island call's role is held by no operand"));
                };
                roles.push(Role {
                    site: std::sync::Arc::clone(&role.suffix),
                    value: *value,
                });
            }
            Stands::Called { stmt, act, roles }
        } else if let Some(pc) = exit.at {
            Stands::Running { stmt, pc, regs }
        } else {
            Stands::Boundary
        };
        self.scratch.exit.stands = Some(stands);
        stop
    }
}

/// The scalar sizes of a call of `args` that answered `result`, as its
/// scalar table indexes them: bit `i` for argument `i` and bit 3 for the
/// result, set for a nonzero word integer, whose size is 2.
#[inline(always)]
fn sizes(args: &[Value], result: &Value) -> u8 {
    let bit = |value: &Value| u8::from(matches!(value, Value::Int(word) if *word != 0));
    args.iter()
        .enumerate()
        .fold(bit(result) << 3, |mask, (index, value)| {
            mask | (bit(value) << index)
        })
}

/// The island of the closure `handle`'s code, with its captured cells:
/// only a closure this executable made, whose code the arena names.
fn closure_island<'a>(
    arena: &'a Arena,
    exe: &Executable,
    handle: ArenaHandle,
) -> Option<(u32, &'a [ArenaHandle])> {
    let (_, cells, code) = arena.closure(handle).ok()?;
    let code = code.filter(|code| code.executable == exe.local())?;
    Some((exe.island_of(CodeId(code.code))?, cells))
}

/// Records the events of charge region `region` of `stream`, as
/// `observe_region` does for a charging run.
#[cold]
#[inline(never)]
fn record(tape: &ChargeTape, stream: &Stream, region: u32) {
    for event in stream.regions.events(region) {
        tape.push(event.weight);
    }
}

/// Runs block `block` of the virtual frame holding `slots`.
fn run_block(island: &mut Island<'_>, block: &CBlock, slots: &mut Slots) -> Result<Flow2, Stop> {
    for (index, statement) in block.statements.iter().enumerate() {
        // A step: the statement's unit first, as `advance` charges it.
        if !island.unit() {
            return Err(island.boundary(block.id, index));
        }
        match run_statement(island, statement, slots) {
            Ok(Next::Done(0)) => {
                if island.spent() {
                    return Err(island.boundary(block.id, index + 1));
                }
            }
            Ok(Next::Done(leaves)) => {
                for at in &block.declares {
                    slots[*at as usize] = None;
                }
                return Ok(Flow2::Fell(leaves - 1));
            }
            Ok(Next::Returned(value)) => return Ok(Flow2::Returned(value)),
            Err(stop) => {
                island.scratch.exit.control.push(Control::Block {
                    block: block.id,
                    next: index + 1,
                });
                return Err(stop);
            }
        }
    }
    Ok(Flow2::Fell(0))
}

fn run_statement(
    island: &mut Island<'_>,
    statement: &CStatement,
    slots: &mut Slots,
) -> Result<Next, Stop> {
    let stmt = statement.id;
    match &statement.kind {
        CKind::Return { value, close, reg } => {
            let value = value
                .eval(island, slots)
                .map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close(close)
            {
                let stop = island.at(close.head);
                let stop = island.held(stop, *reg, value);
                return Err(island.stand(stmt, stop));
            }
            Ok(Next::Returned(value))
        }
        CKind::Write {
            value,
            close,
            pc,
            reg,
            at,
            bind,
        } => {
            let value = value
                .eval(island, slots)
                .map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close(close)
            {
                let stop = island.at(close.head);
                let stop = island.held(stop, *reg, value);
                return Err(island.stand(stmt, stop));
            }
            // A held value is within the headroom admitted at entry; any
            // other value, or a write to a cell, is the reference's to admit.
            let at = *at as usize;
            let fits = held(&value) && (*bind || slots[at].is_some());
            if !fits {
                let stop = island.at(*pc);
                let stop = island.held(stop, *reg, value);
                return Err(island.stand(stmt, stop));
            }
            slots[at] = Some(value);
            Ok(Next::Done(statement.leaves))
        }
        CKind::If {
            cond,
            close,
            pc,
            reg,
            then_,
            else_,
        } => {
            let value = cond
                .eval(island, slots)
                .map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close(close)
            {
                let stop = island.at(close.head);
                let stop = island.held(stop, *reg, value);
                return Err(island.stand(stmt, stop));
            }
            let taken = match value {
                Value::Bool(true) => then_,
                Value::Bool(false) => else_,
                _ => {
                    let stop = island.at(*pc);
                    let stop = island.held(stop, *reg, value);
                    return Err(island.stand(stmt, stop));
                }
            };
            // An empty block ends as soon as it begins: the statement
            // completes in its own.
            if taken.statements.is_empty() {
                return Ok(Next::Done(statement.leaves));
            }
            // The block is pushed: the step ends.
            if island.spent() {
                return Err(island.boundary(taken.id, 0));
            }
            match run_block(island, taken, slots)? {
                Flow2::Returned(value) => Ok(Next::Returned(value)),
                Flow2::Fell(leaves) => Ok(Next::Done(leaves)),
            }
        }
        CKind::Discard { value, close, reg } => {
            let value = value
                .eval(island, slots)
                .map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close(close)
            {
                let stop = island.at(close.head);
                let stop = island.held(stop, *reg, value);
                return Err(island.stand(stmt, stop));
            }
            Ok(Next::Done(statement.leaves))
        }
    }
}
