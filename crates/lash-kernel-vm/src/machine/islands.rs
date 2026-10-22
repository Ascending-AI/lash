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

use lash_kernel_doc::arena::{ArenaHandle, LocalCode};
use lash_kernel_doc::heap::LogicalBytes;
use lash_kernel_doc::{Args, NativeCall, Nursery, PreparedAnswer, TaskId, Value, WorkCounter};

use lash_kernel_exec::{ActIx, Weight};
use lash_kernel_state::arena::Arena;
use lash_vocabulary::data_limits::KernelDataLimits;

use super::frames::Role;
use super::session::Session;
use super::sites::SiteCaches;
use super::tasks::Occurrences;
use super::{Control, Halt, SlotState, fault};
pub(crate) use compiled::Compiled;
use compiled::{CBlock, CKind, CStatement, CallSite, Depth, NativeSite, OperatorSite, UnitSite};

use crate::compile::expr::Close;
use crate::compile::islands::{HELD_BYTES, IslandPlan, MAX_SLOTS, held};
use crate::compile::{
    BlockId, Charge, CodeId, Executable, Fast, Fresh, Lib, LibRun, find_property, measured_formula,
    position, scalar,
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

/// A callee's island, resolved: its plan, its compiled form, and where
/// its captured values start.
#[derive(Clone, Copy)]
struct Callee<'e> {
    plan: &'e IslandPlan,
    compiled: &'e Compiled,
    captured: u32,
}

/// A closure call's cached callee, for one resolution scope
/// ([`Scratch::open`]).
#[derive(Clone, Copy)]
struct Cached {
    epoch: u32,
    handle: ArenaHandle,
    /// Index of the plan whose compiled form resolution admitted.
    compiled: u32,
    captured: u32,
}

/// The statement a converted top frame runs on, the op it runs on from and
/// the registers it holds there.
type RunsOn = (StmtId, u32, Vec<(u32, Value)>);

pub(super) use entry::{Gate, Ground, Ran, RegionGuests, SteadyIsland};

/// The machine's island state: buffers reused across entries, never saved.
#[derive(Default)]
pub(super) struct Scratch {
    /// Islands are held off until the step ends: a statement an island
    /// exited in runs on through the reference.
    held: bool,
    /// The resolution scope open: what its entries' callees resolved to.
    epoch: u32,
    /// The callee each island call site resolved, by its site.
    cached: Vec<Option<Cached>>,
    /// The callee each region guest call resolved, by its occurrence.
    guests: Vec<Option<Cached>>,
    /// The captured values and their cells, of each cached callee.
    captured: Vec<(Value, ArenaHandle)>,
    /// Each session binding read, by its name in the stream, at its
    /// resident value: nothing writes a binding while an island runs.
    sessions: Vec<(u32, Value)>,
    /// The session bindings a region wrote before a guest call, by when
    /// it wrote each, as they publish: reused from call to call.
    written: Vec<(u64, usize)>,
    exit: Exit,
    /// The running slice's host observes no sites: callbacks a helper's
    /// plan applies may run as islands.
    pub(super) quiet: bool,
    /// The statement the top frame a callback's island left runs on once
    /// the step settles, in its task.
    run_on: Option<(TaskId, RunsOn)>,
}

impl Scratch {
    /// Opens a resolution scope: no callee resolved before is found in
    /// it, and the captured values it reads start afresh. Its entries
    /// share what they resolve while no closure they resolved can be
    /// freed and no cell they read written: an entry the machine makes
    /// opens its own, and a region's guest calls share one while the
    /// arena frees nothing (FIG-6982).
    #[inline(always)]
    fn open(&mut self) -> u32 {
        self.epoch = self.epoch.wrapping_add(1);
        self.captured.clear();
        self.epoch
    }

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
    /// The run's arena: a closure whose code a call finds keeps it there.
    arena: &'a mut Arena,
    /// The machine's property caches, which a field read reads and keeps
    /// as the reference's does.
    sites: &'a mut SiteCaches,
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
    /// The most virtual frames it runs: [`ISLAND_DEPTH`], or fewer where
    /// the call depth leaves fewer above the frames below it, hoisted from
    /// each call to the entry, as neither changes while it runs.
    frames: u32,
    /// The running frame admits each of its plan's sites, as its depth
    /// and the observer stand for the whole frame
    /// ([`Compiled::admits`]): hoisted from each site to the frame's entry.
    admitted: bool,
    /// It charges and records no tape: a segment may be admitted whole.
    plain: bool,
    /// The running frame is admitted and plain.
    swift: bool,
    /// The running segment was admitted whole: its charges commit
    /// unchecked while its operators take their typed paths.
    covered: bool,
    scratch: &'a mut Scratch,
}

impl<'a> Island<'a> {
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

    #[inline(always)]
    fn count(&mut self, occurrence: usize) -> Result<(), Stop> {
        if let Err(halt) = self.counters.advance(occurrence) {
            return Err(self.halt(halt));
        }
        Ok(())
    }

    /// Whether the segment that starts here, charging at most `most`, is
    /// admitted whole.
    #[inline(always)]
    fn admit(&mut self, most: u64) -> bool {
        self.covered = self.swift && self.fits(most);
        self.covered
    }

    /// Closes `close`, unchecked where it is `covered` in an admitted
    /// segment, as [`Self::close`] does otherwise.
    #[inline(always)]
    fn close_in(&mut self, close: &Close, covered: bool) -> bool {
        if covered && self.covered {
            self.charged += u64::from(close.total);
            return true;
        }
        self.close(close)
    }

    /// Whether operator `site` may answer here rather than leave its call to
    /// the reference: no observed helper body, and its depth check holds.
    #[inline(always)]
    fn admits(&self, site: &OperatorSite) -> bool {
        self.admitted
            || (!(site.body_when_observed && self.observed) && self.depth_fits(site.depth))
    }

    /// Whether `extra` more frames than the running ones fit the depth.
    #[inline(always)]
    fn depth_fits(&self, depth: Depth) -> bool {
        if self.admitted {
            return true;
        }
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
    pub(super) fn operate(&mut self, site: &OperatorSite, args: &[Value]) -> Answer {
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        // The prepared entry answers and declines as its charged guard
        // does, and on scalars the sizes the table indexes are the values'.
        let charge = if !self.admits(site) {
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
        if !self.fits(units)
            || (site.imported
                && !crate::compile::import_result_fits(
                    &self.exe.lib(site.lib).header.signature.result,
                    result,
                ))
        {
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        if let Some(occurrence) = site.counted {
            self.count(occurrence)?;
        }
        self.formula(units);
        Ok(result)
    }

    /// The scalar answer `value` of the typed path of operator `site` on
    /// two floats or two word integers, committed as [`Self::operate`]
    /// commits the same answer: its region closed, its occurrence counted
    /// and its formula charged the `units` its scalar table gives for the
    /// call's sizes, in that order. `None`, with nothing committed, where
    /// [`Self::operate`] decides otherwise: an observed helper's body, a
    /// depth that does not fit, sizes its table does not certify, a charge
    /// past the bound, or a counter at its end.
    #[inline(always)]
    pub(super) fn typed(
        &mut self,
        site: &OperatorSite,
        value: Value,
        units: Option<u64>,
    ) -> Option<Answer> {
        if !self.admits(site)
            || (site.imported
                && !crate::compile::import_result_fits(
                    &self.exe.lib(site.lib).header.signature.result,
                    value,
                ))
        {
            return None;
        }
        let units = if self.charging { units? } else { 0 };
        if !self.fits(site.fixed.checked_add(units)?)
            || site
                .counted
                .is_some_and(|occurrence| !self.counters.can_advance(&[occurrence]))
        {
            return None;
        }
        // Both fit together, so each is charged without a second check.
        if let Some(close) = &site.close {
            self.charged += u64::from(close.total);
            if let Some(tape) = self.tape {
                record(tape, self.stream, close.region);
            }
        }
        if let Some(occurrence) = site.counted
            && let Err(stop) = self.count(occurrence)
        {
            return Some(Err(stop));
        }
        self.charged += units;
        if units != 0
            && let Some(tape) = self.tape
        {
            tape.push(Weight::Atomic(units));
        }
        Some(Ok(value))
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
        // As a loop region charges a prepared answer: the units its guard
        // certified, the formula over values that carry their measurements,
        // or the formula measured in the arena.
        let units = match answer
            .scalar_sizes
            .and_then(|mask| fast.scalar_units(mask))
            .or_else(|| fast.immediate(args, Fresh::default(), &value))
        {
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
    fn native(&mut self, site: &NativeSite, padded: &[Value; MAX_SLOTS], given: usize) -> Answer {
        let args = &padded[..given];
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        let length = match (site.length, padded[0]) {
            (Some(units), Value::List(list)) => self.arena.len(list).ok().map(|len| {
                (
                    Value::Int(len as i64),
                    if self.charging { units } else { 0 },
                )
            }),
            _ => None,
        };
        let charge = length
            .or_else(|| self.native_answer(site, padded))
            .filter(|(_, units)| self.fits(*units));
        let Some((value, units)) = charge else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        self.formula(units);
        Ok(value)
    }

    /// What the reference's general call answers `site` on the operands
    /// `padded` holds, its other parameters absent: its prepared entry's
    /// answer where its guard holds, its native's otherwise, with what it
    /// charges. `None` where the reference runs it: the native builds or
    /// holds anything, raises, declines or asks again, or answers what the
    /// frame may not hold.
    #[inline(never)]
    fn native_answer(
        &self,
        site: &NativeSite,
        padded: &[Value; MAX_SLOTS],
    ) -> Option<(Value, u64)> {
        let function = self.exe.lib(site.lib);
        let args = padded.get(..function.arity)?;
        if let Some((entry, fast)) = &function.prepared
            && self.depth_fits(site.depth)
            && let Some(answer) = entry.answer(args, self.charging)
        {
            return self.answered(function, fast, args, answer);
        }
        let LibRun::Native { native, .. } = &function.run else {
            return None;
        };
        let arena = &*self.arena;
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

    /// The field of `target` that the stream's field read `field` names,
    /// when the frame may hold it;
    /// `None` where the reference reads it: no record, a key the record
    /// has not resolved, or a value the arena cannot read where it is. A
    /// record's field is read through its site's property cache, as the
    /// reference reads it.
    #[inline(always)]
    fn field(&mut self, target: Value, field: usize) -> Option<Value> {
        let Value::Record(record) = target else {
            return None;
        };
        let field = self.stream.fields.get(field)?;
        if self.arena.unresolved_key(record, &field.key).ok()? {
            return None;
        }
        let slot = field.site.and_then(|site| self.sites.slot(site));
        let (value, found) = find_property(self.arena, record, slot, &field.key).ok()?;
        if let Some(site) = field.site
            && found != slot
        {
            self.sites.keep(site, found);
        }
        let value = value.unwrap_or(Value::Absent);
        held(&value).then_some(value)
    }

    /// The test `same(value, ())` of `site`: false for any value but a
    /// tuple, which `same` compares member by member, charged its region
    /// and its formula with the tuple measured as the empty construction
    /// it is; or a stop where the tuple's construction resumes, before
    /// anything is charged, where the value is a tuple, its formula
    /// measures what the arena holds, or the charge would pass the bound.
    #[inline(always)]
    fn unit_test(&mut self, site: &UnitSite, value: Value) -> Answer {
        let answer = Value::Bool(false);
        let units = if matches!(value, Value::Tuple(_)) {
            None
        } else if self.charging {
            self.exe
                .lib(site.lib)
                .prepared
                .as_ref()
                .and_then(|(_, fast)| {
                    fast.immediate(&[value, Value::Absent], Fresh::of(0b10), &answer)
                })
        } else {
            Some(0)
        };
        let fixed = site.close.map_or(0, |close| u64::from(close.total));
        let Some(units) = units.filter(|units| self.fits(fixed.saturating_add(*units))) else {
            return Err(self.at_held(site.miss, site.reg, value));
        };
        if let Some(close) = &site.close {
            self.close(close);
        }
        self.formula(units);
        Ok(answer)
    }

    /// `same(kind(x), literal)` of `x`: by its memo
    /// where it has answered `x`'s kind, as the two calls answer and
    /// charge it otherwise.
    #[inline(always)]
    fn kind_test(&mut self, site: &compiled::KindTest, x: Value) -> Answer {
        let memo = &site.memo;
        let total = |close: Option<Close>| close.map_or(0, |close| u64::from(close.total));
        let (kind_closed, same_closed) = (total(site.kind.close), total(site.same.close));
        if self.charging
            && let Some((answer, kind_units, same_units)) = memo.get(x.kind())
            && self.depth_fits(site.kind.depth)
            && self.depth_fits(site.same.depth)
            && self.fits(
                kind_closed
                    .saturating_add(kind_units)
                    .saturating_add(same_closed)
                    .saturating_add(same_units),
            )
        {
            if let Some(close) = &site.kind.close {
                self.close(close);
            }
            self.formula(kind_units);
            if let Some(close) = &site.same.close {
                self.close(close);
            }
            self.formula(same_units);
            return Ok(Value::Bool(answer));
        }
        let before = self.charged;
        let kind = self.operate(&site.kind, &[x])?;
        let between = self.charged;
        let same = self.operate(&site.same, &[kind, site.literal])?;
        if self.charging
            && let Value::Bool(answer) = same
            && matches!(kind, Value::Text(_))
        {
            memo.set(
                x.kind(),
                (
                    answer,
                    between - before - kind_closed,
                    self.charged - between - same_closed,
                ),
            );
        }
        Ok(same)
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

    /// The call `site` of `callee`, or of its declared code, on `args`,
    /// entered in one step: the site's region closed, the callee resolved
    /// through the site's cache, its frame admitted against the bounds
    /// this entry hoisted, the call's occurrence counted, and the callee's
    /// frame laid out and its body run in place.
    #[inline(always)]
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
        let Some(target) = target.filter(|target| {
            args.len() <= target.compiled.layout.arity
                && args.iter().all(held)
                && self.depth < self.frames
        }) else {
            let stop = self.at(site.pc);
            return Err(self.call_held(stop, site, callee, args));
        };
        self.count(site.occurrence)?;
        match self.enter(target, args) {
            Ok(value) => {
                if site.after != 0 {
                    self.admit(site.after);
                }
                Ok(value)
            }
            Err(stop) => {
                if self.scratch.exit.halt.is_none() {
                    self.scratch.exit.suspended = Some(site.act);
                }
                Err(stop)
            }
        }
    }

    /// The callee of plan `plan`, its captured values from `captured`.
    #[inline(always)]
    fn callee(&self, plan: u32, captured: u32) -> Option<Callee<'a>> {
        let exe = self.exe;
        let island = exe.islands.plans.get(plan as usize)?;
        Some(Callee {
            plan: island,
            compiled: island.compiled(exe)?,
            captured,
        })
    }

    /// A cache hit carries the compiled form's plan index: resolution
    /// has already compiled it, so neither code lookup nor lazy compilation
    /// belongs on this path.
    #[inline(always)]
    fn cached_callee(&self, cached: Cached) -> Option<Callee<'a>> {
        let plan = &self.exe.islands.plans[cached.compiled as usize];
        Some(Callee {
            plan,
            compiled: plan.resolved()?,
            captured: cached.captured,
        })
    }

    /// The island of declared code `code`.
    fn declared(&mut self, code: CodeId) -> Option<Callee<'a>> {
        let plan = self.exe.island_of(code)?;
        self.callee(plan, self.scratch.captured.len() as u32)
    }

    /// The island of the closure `callee`, through call site `site`'s
    /// cache.
    #[inline(always)]
    fn resolve(&mut self, site: u32, callee: Value) -> Option<Callee<'a>> {
        let Value::Closure(handle) = callee else {
            return None;
        };
        let cached = match hit(
            &self.scratch.cached,
            site as usize,
            self.scratch.epoch,
            handle,
        ) {
            Some(cached) => cached,
            None => {
                let cached = self.resolve_cold(handle)?;
                keep(&mut self.scratch.cached, site as usize, cached);
                cached
            }
        };
        self.cached_callee(cached)
    }

    /// The island of the closure `handle` a region's guest call of
    /// occurrence `occurrence` calls, through the call's cache.
    #[inline(always)]
    fn guest(&mut self, occurrence: usize, handle: ArenaHandle) -> Option<Callee<'a>> {
        let cached = match hit(&self.scratch.guests, occurrence, self.scratch.epoch, handle) {
            Some(cached) => cached,
            None => {
                #[cfg(test)]
                GUESTS.with(|guests| guests.set(guests.get() + 1));
                let cached = self.resolve_cold(handle)?;
                keep(&mut self.scratch.guests, occurrence, cached);
                cached
            }
        };
        self.cached_callee(cached)
    }

    /// The island of the closure `handle`, resolved in the open scope:
    /// its plan, and its captured values read from its cells.
    #[inline(never)]
    fn resolve_cold(&mut self, handle: ArenaHandle) -> Option<Cached> {
        #[cfg(test)]
        RESOLVED.with(|resolved| resolved.set(resolved.get() + 1));
        let plan = self
            .exe
            .island_of(found_code(self.arena, self.exe, handle)?)?;
        let (_, cells, _) = self.arena.closure(handle).ok()?;
        let captured = captured(
            self.exe,
            self.arena,
            &mut self.scratch.captured,
            plan,
            cells,
        )?;
        Some(Cached {
            epoch: self.scratch.epoch,
            handle,
            compiled: plan,
            captured,
        })
    }

    /// The island plan `plan`, closed over the cells `cells`.
    fn with_cells(&mut self, plan: u32, cells: &[ArenaHandle]) -> Option<Callee<'a>> {
        let captured = captured(
            self.exe,
            self.arena,
            &mut self.scratch.captured,
            plan,
            cells,
        )?;
        self.callee(plan, captured)
    }

    /// The island plan `plan` of a walk's steady callback, whose compiled
    /// form and captures the walk checked as it resolved it, closed over
    /// the cells `cells`, read afresh.
    #[inline(always)]
    fn steady(&mut self, plan: u32, cells: &[ArenaHandle]) -> Option<Callee<'a>> {
        let captured = self.scratch.captured.len() as u32;
        for cell in cells {
            let value = self.arena.cell(*cell).ok()?;
            self.scratch.captured.push((value, *cell));
        }
        self.callee(plan, captured)
    }

    /// Runs `callee` on `args` as a new virtual frame, laid out as its
    /// compiled form's layout says: its value, or a stop with the frame
    /// left.
    #[inline(always)]
    fn enter(&mut self, callee: Callee<'a>, args: &[Value]) -> Answer {
        let Callee {
            plan,
            compiled,
            captured,
        } = callee;
        let mut slots: Slots = [None; MAX_SLOTS];
        compiled.layout.fill(
            &mut slots,
            args,
            &self.scratch.captured[captured as usize..],
        );
        // The call's frame is pushed: its step ends.
        if self.spent() {
            let stop = self.boundary(compiled.body.id, 0);
            self.left(plan, captured, &slots);
            return Err(stop);
        }
        if let Some(recorder) = self.recorder {
            recorder.entered();
        }
        let outer = (self.admitted, self.swift);
        self.depth += 1;
        self.admitted = compiled.admits(
            self.base + u64::from(self.depth),
            self.call_depth,
            self.observed,
        );
        self.swift = self.admitted && self.plain;
        let ran = run_body(self, &compiled.body, &mut slots);
        self.depth -= 1;
        (self.admitted, self.swift) = outer;
        match ran {
            Ok(Flow2::Returned(value)) => Ok(value),
            // The body ran its last statement: the frame returns null.
            Ok(Flow2::Fell(_)) => Ok(Value::Null),
            Err(stop) => {
                if self.scratch.exit.halt.is_none() {
                    self.left(plan, captured, &slots);
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
pub(super) fn sizes(args: &[Value], result: &Value) -> u8 {
    let bit = |value: &Value| u8::from(matches!(value, Value::Int(word) if *word != 0));
    args.iter()
        .enumerate()
        .fold(bit(result) << 3, |mask, (index, value)| {
            mask | (bit(value) << index)
        })
}

#[cfg(test)]
thread_local! {
    /// How many closure callees islands resolved on this thread, rather
    /// than finding them cached.
    pub(crate) static RESOLVED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// How many resolution scopes regions' guest calls opened on this
    /// thread.
    pub(crate) static SCOPES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// How many callees regions' guest calls resolved on this thread,
    /// rather than finding them cached.
    pub(crate) static GUESTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The callee `cache` holds at `at`, when it was resolved in the scope
/// `epoch` for the closure `handle`.
#[inline(always)]
fn hit(cache: &[Option<Cached>], at: usize, epoch: u32, handle: ArenaHandle) -> Option<Cached> {
    match cache.get(at) {
        Some(Some(cached)) if cached.epoch == epoch && cached.handle == handle => Some(*cached),
        _ => None,
    }
}

/// Keeps `cached` in `cache` at `at`.
#[cold]
fn keep(cache: &mut Vec<Option<Cached>>, at: usize, cached: Cached) {
    if cache.len() <= at {
        cache.resize(at + 1, None);
    }
    cache[at] = Some(cached);
}

/// Reads the cells `cells` of a callee of plan `plan` into the captured
/// values `values`: where they start.
fn captured(
    exe: &Executable,
    arena: &Arena,
    values: &mut Vec<(Value, ArenaHandle)>,
    plan: u32,
    cells: &[ArenaHandle],
) -> Option<u32> {
    let island = &exe.islands.plans[plan as usize];
    island.compiled(exe)?;
    if cells.len() != island.captures.len() {
        return None;
    }
    let captured = values.len() as u32;
    for cell in cells {
        values.push((arena.cell(*cell).ok()?, *cell));
    }
    Some(captured)
}

/// The code the closure `handle` runs in `exe`: the code a call found for
/// it in this executable, else the code its site names here, as the
/// reference's call finds it, which the closure keeps for its next calls.
fn found_code(arena: &mut Arena, exe: &Executable, handle: ArenaHandle) -> Option<CodeId> {
    let executable = exe.local();
    let found = match arena.closure(handle).ok()? {
        (_, _, Some(code)) if code.executable == executable => return Some(CodeId(code.code)),
        (identity, ..) => super::project::closure_code(
            exe,
            &identity.callable.function,
            &identity.site.site,
            &identity.capture_names,
        )
        .ok()?,
    };
    arena
        .found_closure_code(
            handle,
            LocalCode {
                executable,
                code: found.0,
            },
        )
        .ok()?;
    Some(found)
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

/// Runs block `block` of the virtual frame holding `slots`: a block
/// nested in a body.
#[inline(never)]
fn run_block(island: &mut Island<'_>, block: &CBlock, slots: &mut Slots) -> Result<Flow2, Stop> {
    run_body(island, block, slots)
}

/// Runs block `block` of the virtual frame holding `slots` in place: a
/// body, run where its call enters it.
#[inline(always)]
fn run_body(island: &mut Island<'_>, block: &CBlock, slots: &mut Slots) -> Result<Flow2, Stop> {
    for (index, statement) in block.statements.iter().enumerate() {
        // A step: the statement's unit first, as `advance` charges it, in
        // its first segment.
        if island.admit(statement.ahead) {
            island.charged += 1;
        } else if !island.unit() {
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

#[inline(always)]
fn run_statement(
    island: &mut Island<'_>,
    statement: &CStatement,
    slots: &mut Slots,
) -> Result<Next, Stop> {
    let stmt = statement.id;
    match &statement.kind {
        CKind::Return {
            value,
            close,
            covered,
            reg,
        } => {
            let value = value(island, slots).map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close_in(close, *covered)
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
            covered,
            pc,
            reg,
            at,
            bind,
        } => {
            let value = value(island, slots).map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close_in(close, *covered)
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
            covered,
            pc,
            reg,
            then_,
            else_,
        } => {
            let value = cond(island, slots).map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close_in(close, *covered)
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
        CKind::Discard {
            value,
            close,
            covered,
            reg,
        } => {
            let value = value(island, slots).map_err(|stop| island.stand(stmt, stop))?;
            if let Some(close) = close
                && !island.close_in(close, *covered)
            {
                let stop = island.at(close.head);
                let stop = island.held(stop, *reg, value);
                return Err(island.stand(stmt, stop));
            }
            Ok(Next::Done(statement.leaves))
        }
        CKind::Cold { pc } => {
            let stop = island.at(*pc);
            Err(island.stand(stmt, stop))
        }
    }
}
