//! Admitted function execution: one flat register loop carries explicit
//! frames across statements and calls. The source stream owns semantic
//! coordinates, ordered events and rare operation handlers. Calls between islands go
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
pub(crate) mod cx;
mod entry;
mod execute;
mod heap;
mod native;
mod observing;
mod resident;
mod scheduling;
use observing::{Projection, StatementFact, Watching};

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::Reason;

use lash_kernel_doc::arena::{ArenaHandle, LocalCode};
use lash_kernel_doc::{PreparedAnswer, Value};

use lash_kernel_exec::{ActIx, Weight};
use lash_kernel_state::arena::Arena;
use lash_vocabulary::data_limits::KernelDataLimits;

use super::frames::Role;
use super::session::Session;
use super::sites::SiteCaches;
use super::tasks::Occurrences;
use super::{Control, Halt, SlotState, fault};
pub(crate) use compiled::Compiled;
use compiled::{Depth, Direct, HeapSite, NativeSite, OperatorSite, Read, UnitSite};

use crate::compile::expr::Close;
use crate::compile::islands::{HELD_BYTES, IslandPlan, MAX_ARGS};
use crate::compile::{
    Charge, CodeId, Executable, Fast, Fresh, Lib, LibRun, find_property, measured_formula, scalar,
};
use crate::compile::{StmtId, Stream};
use crate::observe::ChargeTape;

/// The most explicit frames an entry admits in its headroom reservation.
const ISLAND_DEPTH: u32 = 200;

/// What an island's statements may hold while it runs, admitted once at
/// its entry with its virtual frames (§3.5).
const ISLAND_HOLD: u64 = 1 << 16;

/// The most a variable of a virtual frame costs the census: a value it
/// may hold: an immediate or an arena handle.
const SLOT_BYTES: u64 = HELD_BYTES;

/// All simultaneously live windows share the original slot reservation.
/// Wider frames consume more of it, without increasing entry headroom.
const ISLAND_SLOTS: usize = ISLAND_DEPTH as usize * MAX_ARGS;

/// A [`Callee`]'s captures offset where its frame is resident: its own
/// slots hold its captured cells, as the source frame's do.
const RESIDENT: u32 = u32::MAX;

/// The headroom an entry admits: statement holdings, bounded frame count
/// and a separate total slot allowance (§3.5).
pub(super) const ISLAND_ADMITTED: u64 = ISLAND_HOLD + ISLAND_DEPTH as u64 * FRAME_ROOM;

/// What one virtual frame reserves of an entry's headroom: its frame and
/// its share of the slot allowance.
const FRAME_ROOM: u64 = super::FRAME_BYTES + MAX_ARGS as u64 * SLOT_BYTES;

/// What an entry in `headroom` admits: what its statements may hold and
/// the most virtual frames it runs. Under memory pressure half the
/// headroom goes to frames and the rest to holdings, so what an island
/// holds uncounted never passes the bound: every verdict the reference
/// makes inside it passes (FIG-6791), and an operation that would not fit
/// leaves the island at its instruction.
pub(super) fn admission(headroom: u64) -> (u64, u32) {
    if headroom >= ISLAND_ADMITTED {
        return (ISLAND_HOLD, ISLAND_DEPTH);
    }
    let frames = (headroom / 2 / FRAME_ROOM) as u32;
    (headroom - u64::from(frames) * FRAME_ROOM, frames)
}

/// A virtual frame's variables, at the plan's semantic slot coordinates.
type Slots = [SlotState];

/// An island stopped: what it stopped at is in its [`Exit`].
pub(super) struct Stop;

type Answer = Result<Value, Stop>;

/// Where an exited frame stands.
pub(super) enum Stands {
    /// A backedge before its header charges or cursor step commit.
    Iteration { stmt: StmtId },
    /// At a step end, as its control says.
    Boundary,
    /// In statement `stmt` at op `pc`, holding `regs`.
    Running {
        stmt: StmtId,
        pc: u32,
        regs: Vec<(u32, Value)>,
    },
    /// At a scheduler instruction, before any task/effect mutation.
    Departure(super::Completion),
    Scheduling {
        stmt: StmtId,
        pc: u32,
        regs: Vec<(u32, Value)>,
    },
    /// At a statement-root call whose callee has not returned.
    Action { stmt: StmtId },
    /// At the call `act` of statement `stmt`, its roles saved.
    Called {
        stmt: StmtId,
        act: ActIx,
        roles: Vec<Role>,
    },
}

/// A virtual frame as the exit converts it.
pub(super) struct Level {
    /// The original library completion owner, including its ordered arguments.
    library: Option<super::LibraryCall>,
    link: lash_vocabulary::LinkId,
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
    scheduling: bool,
    /// The registers the frame being left holds.
    regs: Vec<(u32, Value)>,
    /// Its control, innermost first.
    control: Vec<Control>,
    stands: Option<Stands>,
    /// The call it stands at, when its callee stopped.
    suspended: Option<ActIx>,
    /// The frames left, innermost first.
    levels: Vec<Level>,
    #[cfg(any(test, feature = "perf-witness"))]
    reason: Option<Reason>,
}

/// A callee's island, resolved: its plan, its compiled form, and where
/// its captured values start.
#[derive(Clone, Copy)]
struct Callee<'e> {
    link: lash_vocabulary::LinkId,
    plan: &'e IslandPlan,
    stream: &'e Stream,
    compiled: &'e Compiled,
    captured: u32,
}

/// A closure call's cached callee, for one resolution scope
/// ([`Scratch::open`]).
#[derive(Clone, Copy)]
struct Cached {
    link: lash_vocabulary::LinkId,
    epoch: u32,
    handle: ArenaHandle,
    /// Index of the plan whose compiled form resolution admitted.
    compiled: u32,
    captured: u32,
}

/// The statement a converted top frame runs on, the op it runs on from and
/// the registers it holds there.
enum RunsOn {
    Instruction(StmtId, u32, Vec<(u32, Value)>),
    Iteration(StmtId),
    Scheduling(StmtId, u32, Vec<(u32, Value)>),
    Departure(super::Completion),
}

/// The machine's island state: buffers reused across entries, never saved.
#[derive(Default)]
pub(super) struct Scratch {
    /// Physical slot/register/control windows, emptied at their logical death.
    windows: Vec<execute::Window>,
    /// A force-cold law's run: every native call leaves the island for the
    /// call that puts its arguments back on their pages first.
    #[cfg(test)]
    pub(super) chilled: bool,
    /// Islands are held off until the step ends: a statement an island
    /// exited in runs on through the reference.
    held: bool,
    /// The resolution scope open: what its entries' callees resolved to.
    epoch: u32,
    /// The callee each island call site resolved, by its site.
    cached: Vec<Option<Cached>>,
    /// The captured values and their cells, of each cached callee.
    captured: Vec<(Value, ArenaHandle)>,
    /// Each session binding read, by its name in the stream, at its
    /// resident value: nothing writes a binding while an island runs.
    sessions: Vec<(u32, Value)>,
    /// The heap operations' operand stack, reused rather than rebuilt by
    /// each operation; empty between them.
    arguments: super::registers::Arguments,
    /// The last record key a membership test found absent, with the
    /// record's version then ([`cx::probe`]).
    absent: Option<cx::Absent>,
    exit: Exit,
    /// Diagnostic activation for this machine's census; never persisted.
    pub(super) counting: bool,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) census: crate::perf_witness::execution::Recorder,
    scheduled: Option<(super::TaskId, RunsOn)>,
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
}

/// One island entry: what it charges and counts, and its frames' bounds.
struct Island<'a, const OBSERVING: bool> {
    /// The already resident root returns through a host data boundary.
    public_return: bool,
    link: lash_vocabulary::LinkId,
    exe: &'a Executable,
    stream: &'a Stream,
    /// The run's arena: a closure whose code a call finds keeps it there.
    arena: &'a mut Arena,
    /// The machine's property caches, which a field read reads and keeps
    /// as the reference's does.
    sites: &'a mut SiteCaches,
    /// The session overlay its binding reads take resident values from and
    /// its binding writes write.
    session: &'a mut Session,
    tape: Option<&'a ChargeTape>,
    /// The tape the calls it enters are counted on, charged or paced.
    recorder: Option<&'a ChargeTape>,
    /// Whether the run is charged: a paced one measures no formula, and
    /// its prepared entries answer by their value-only guards.
    charging: bool,
    accounting: bool,
    /// The run's charge, or with charge Off its pace, the machine's once
    /// the island ends.
    charged: u64,
    bound: u64,
    /// Its admission is scaled to a headroom below a full one: a heap
    /// operation leaves the island for the reference's reservation, which
    /// collects before it builds.
    tight: bool,
    /// The slice end a resumption into a spent slice holds off until its
    /// first statement or loop header runs: a step does one step's work.
    deferred_end: Option<u64>,
    /// The run's limiters, lent to a resumed frame ([`Self::unit`]).
    limiters: Option<&'a mut super::Limiters>,
    /// The bound its limiter grew to.
    grown: Option<u64>,
    slice_end: u64,
    counters: &'a mut Occurrences,
    /// An observer is attached: helper calls answer as observed runs do.
    observed: bool,
    watching: Option<&'a dyn Projection>,
    /// The run's data limits, which natives answer under.
    limits: KernelDataLimits,
    /// Memory is accounted: heap operations stay within the entry nursery.
    memory: bool,
    /// Entry nursery and outstanding per-statement native/construction credit.
    ceiling: u64,
    transient: u64,
    /// Live physical register windows share the statement/nursery allowance.
    register_bytes: u64,
    /// The call depth the frames below the island hold (`K-BND-002`).
    base: u64,
    call_depth: u64,
    /// The virtual frames running.
    depth: u32,
    /// The most virtual frames it runs: [`ISLAND_DEPTH`], or fewer where
    /// the call depth leaves fewer above the frames below it, hoisted from
    /// each call to the entry, as neither changes while it runs.
    frames: u32,
    /// The variables all its live windows may hold: [`ISLAND_SLOTS`], or
    /// fewer where its admission admits fewer frames.
    slot_room: usize,
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
    /// The running accumulator statement's text (FIG-6795), which its heap
    /// operations' concatenation may extend.
    accumulator: Option<crate::compile::Accumulator>,
}

impl<'a, const OBSERVING: bool> Island<'a, OBSERVING> {
    /// Projection mode is fixed at entry; both modes use the same program and
    /// instruction loop. Disabled taps disappear without another executor.
    #[inline(always)]
    fn projection(&self) -> Option<&'a dyn Projection> {
        if OBSERVING { self.watching } else { None }
    }

    /// Resolve pure operand addresses in source order. The instruction
    /// handler owns every operator, effect and control transition.
    #[inline(always)]
    fn inputs<const N: usize>(
        &mut self,
        slots: &Slots,
        reads: &[Read],
    ) -> Result<[Value; N], Stop> {
        let mut values = [Value::Absent; N];
        for (index, read) in reads.iter().enumerate() {
            let (value, miss) = match read {
                Read::Literal { value, .. } => (Some(*value), 0),
                Read::Local {
                    at, shared, miss, ..
                } => (
                    match slots[*at] {
                        SlotState::Cell(cell) if *shared => self.arena.cell(cell).ok(),
                        SlotState::Value(value) if !shared => Some(value),
                        _ => None,
                    },
                    *miss,
                ),
            };
            let Some(value) = value else {
                return Err(self.read_miss(&reads[..index], &values[..index], miss));
            };
            values[index] = value;
        }
        Ok(values)
    }

    /// The stop of a read that missed at op `miss`, keeping the operands
    /// `reads` had already read in `values`.
    #[cold]
    #[inline(never)]
    fn read_miss(&mut self, reads: &[Read], values: &[Value], miss: u32) -> Stop {
        #[cfg(any(test, feature = "perf-witness"))]
        {
            self.scratch.exit.reason = Some(Reason::ResidentRead);
        }
        for (read, value) in reads.iter().zip(values) {
            self.scratch.exit.regs.push((read.reg(), *value));
        }
        self.at(miss)
    }

    /// A stop at op `pc`, which the reference runs.
    #[cold]
    fn at(&mut self, pc: u32) -> Stop {
        self.scratch.exit.at = Some(pc);
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

    /// The step has done work: a deferred slice end stands again.
    #[cold]
    #[inline(never)]
    fn progressed(&mut self) {
        if let Some(end) = self.deferred_end.take() {
            self.slice_end = end;
        }
    }

    /// A statement's unit, charged before the statement runs.
    #[inline(always)]
    fn unit(&mut self) -> Result<(), Stop> {
        if !self.fits(1) {
            return self.reached();
        }
        self.charged += 1;
        if let Some(tape) = self.tape {
            tape.push(Weight::Atomic(1));
        }
        Ok(())
    }

    /// A resumed frame's statement unit passes the bound: the run's
    /// charge limiter grows it or the run halts there, as the source
    /// charger decides it. A frame the island called leaves that decision
    /// to its resumption, where the frames below it stand as the
    /// reference's do.
    #[cold]
    #[inline(never)]
    fn reached(&mut self) -> Result<(), Stop> {
        #[cfg(any(test, feature = "perf-witness"))]
        {
            self.scratch.exit.reason = Some(Reason::Charge);
        }
        if self.depth != 1 {
            return Err(Stop);
        }
        let Some(limiters) = self.limiters.as_deref_mut() else {
            return Err(Stop);
        };
        self.charged += 1;
        if let Some(tape) = self.tape {
            tape.push(Weight::Atomic(1));
        }
        let mut limit = self.bound;
        match limiters.charge_reached(self.charged, &mut limit) {
            Ok(()) => {
                self.bound = limit;
                self.grown = Some(limit);
                Ok(())
            }
            Err(halt) => Err(self.halt(halt)),
        }
    }

    /// Closes charge region `close`, charged whole with its events, as
    /// `close_region` does; `false` when it would pass the bound.
    #[inline(always)]
    fn close(&mut self, close: &Close) -> bool {
        if !self.accounting {
            if let Some(watching) = &self.projection() {
                watching.expressions(self.stream, close, false);
            }
            return true;
        }
        let total = u64::from(close.total);
        if !self.fits(total) {
            #[cfg(any(test, feature = "perf-witness"))]
            {
                self.scratch.exit.reason = Some(Reason::Charge);
            }
            return false;
        }
        self.charged += total;
        if let Some(watching) = &self.projection() {
            watching.expressions(self.stream, close, self.charging);
        }
        if let Some(tape) = self.tape {
            record(tape, self.stream, close.region);
        }
        true
    }

    /// A formula's `units`, charged whole.
    #[inline(always)]
    fn formula(&mut self, units: u64) -> bool {
        if !self.accounting {
            return true;
        }
        if !self.fits(units) {
            #[cfg(any(test, feature = "perf-witness"))]
            {
                self.scratch.exit.reason = Some(Reason::Charge);
            }
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

    /// Declared loop work is charged even inside an uncharged helper.
    #[inline(always)]
    fn scheduled(&mut self, units: u64) -> bool {
        if !self.fits(units) {
            #[cfg(any(test, feature = "perf-witness"))]
            {
                self.scratch.exit.reason = Some(Reason::Charge);
            }
            return false;
        }
        self.charged += units;
        if units != 0
            && let Some(tape) = self.recorder
        {
            tape.push(Weight::Atomic(units));
        }
        true
    }

    #[inline(always)]
    fn count(&mut self, occurrence: usize) -> Result<(), Stop> {
        match self.counters.advance(occurrence) {
            Err(halt) => return Err(self.halt(halt)),
            Ok(count) => {
                if let Some(watching) = &self.projection()
                    && let Err(halt) = watching.action(self.exe, occurrence, count)
                {
                    return Err(self.halt(halt));
                }
            }
        }
        Ok(())
    }

    /// Root actions keep their canonical occurrence and atomic argument
    /// charges in the source action, rather than inventing a nested site.
    #[inline(always)]
    fn root_prefix(&mut self, root: Option<StmtId>) -> Result<(u64, Option<usize>), Stop> {
        let Some(id) = root else {
            return Ok((0, None));
        };
        let crate::compile::Form::Action(action) = &self.exe.stmt(id).form else {
            return Err(self.halt(fault("a root call has no action")));
        };
        let crate::compile::ActionKind::Call { args, .. } = &action.kind else {
            return Err(self.halt(fault("a root call has no call action")));
        };
        // A projected authored entry must enter its host site before the
        // native runs. Its original source operation retains that order.
        if (self.accounting || action.guest) && self.projection().is_some() {
            return Err(self.at(self.exe.stmt(id).ops.start));
        }
        let before = if self.accounting {
            args.len() as u64
        } else {
            0
        };
        let occurrence = action.guest.then_some(action.occurrence);
        if !self.fits(before) || occurrence.is_some_and(|at| !self.counters.can_advance(&[at])) {
            return Err(self.at(self.exe.stmt(id).ops.start));
        }
        Ok((before, occurrence))
    }

    #[inline(always)]
    fn root_fits(&self, before: u64, units: u64, strict: bool) -> bool {
        before
            .checked_add(units)
            .and_then(|total| self.charged.checked_add(total))
            .is_some_and(|end| {
                end <= self.bound && (!strict || self.slice_end == u64::MAX || end < self.slice_end)
            })
    }

    #[inline(always)]
    fn root_arguments(&mut self, before: u64) {
        if before == 0 {
            return;
        }
        self.charged += before;
        if let Some(tape) = self.tape {
            for _ in 0..before {
                tape.push(Weight::Atomic(1));
            }
        }
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

    #[inline(always)]
    fn operator_value(&mut self, site: &OperatorSite, values: &[Value], covered: bool) -> Answer {
        let typed = if OBSERVING || (site.root.is_some() && self.accounting) {
            None
        } else {
            match values {
                [x] if self.accounting => compiled::unary(site, *x, self.exe),
                [x, y] => site
                    .pair
                    .and_then(|pair| compiled::typed(site, &pair, *x, *y, self.exe, self.charging)),
                _ => None,
            }
        };
        if !OBSERVING
            && !self.accounting
            && self.admitted
            && !site.checked
            && site.counted.is_none()
        {
            typed
                .map(|(value, _)| value)
                .or_else(|| site.entry.call(values))
                .map(Ok)
                .unwrap_or_else(|| self.operate(site, values))
        } else if OBSERVING || (site.root.is_some() && self.accounting) {
            self.covered = false;
            self.operate(site, values)
        } else {
            match typed {
                Some((value, Some(units))) if covered && self.covered => {
                    self.charged += site.fixed;
                    if let Some(occurrence) = site.counted {
                        self.count(occurrence)?;
                    }
                    self.charged += units;
                    Ok(value)
                }
                Some((value, units)) => match self.typed(site, value, units) {
                    Some(answer) => answer,
                    None => {
                        self.covered = false;
                        self.operate(site, values)
                    }
                },
                None => {
                    self.covered = false;
                    self.operate(site, values)
                }
            }
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
        self.observed_native(site.lib, |island| island.operator_answer(site, args))
    }

    #[inline(always)]
    fn operator_answer(&mut self, site: &OperatorSite, args: &[Value]) -> Answer {
        let (before, root_occurrence) = self.root_prefix(site.root)?;
        let counted = site.counted.or(root_occurrence);
        if site.act.is_some()
            && self
                .projection()
                .is_some_and(|watching| !watching.can_call())
        {
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        if site.checked
            && !self
                .exe
                .lib(site.lib)
                .header
                .signature
                .params
                .iter()
                .zip(args)
                .all(|(param, value)| {
                    (param.optional && matches!(value, Value::Absent))
                        || crate::compile::import_result_fits(&param.ty, *value)
                })
        {
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        // The prepared entry answers and declines as its charged guard
        // does, and on scalars the sizes the table indexes are the values'.
        let charge = if !self.admits(site) {
            None
        } else if self.projection().is_some() {
            let function = self.exe.lib(site.lib);
            function.prepared.as_ref().and_then(|(entry, fast)| {
                self.answered(function, fast, args, entry.answer(args, self.charging)?)
            })
        } else if let ([x, y], Some(pair)) = (args, site.pair)
            && let Some((value, Some(units))) =
                compiled::typed(site, &pair, *x, *y, self.exe, self.charging)
        {
            // A statement root's operator runs the typed kernel nested
            // operators run, charged its table's units as its entry is.
            Some((value, units))
        } else if !self.charging {
            site.entry.call(args).map(|value| (value, 0))
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
        if charge.is_some()
            && let Some(watching) = &self.projection()
        {
            watching.prepared();
        }
        if let Some((value, units)) = charge {
            self.admit_operator(site, args, value, units, (before, counted))?;
            self.formula(units);
            return Ok(value);
        }
        self.operator_heap(site, args, (before, counted))
    }

    /// Both scalar formulas and heap bodies pass the same result and action
    /// admission before publishing their charge facts. Only heap bodies own
    /// a pending event sequence; scalar answers stay a plain value and price.
    /// A statement-root call's argument prefix `before` and its occurrence
    /// commit here, ahead of the formula, as the source statement orders them.
    #[inline(always)]
    fn admit_operator(
        &mut self,
        site: &OperatorSite,
        args: &[Value],
        result: Value,
        units: u64,
        (before, counted): (u64, Option<usize>),
    ) -> Result<(), Stop> {
        // The reference answers a prepared or heap answer in its statement;
        // only a body's heap paths are held to the slice ([`Self::heap`]).
        if !self.root_fits(before, units, false)
            || (site.checked
                && !crate::compile::import_result_fits(
                    &self.exe.lib(site.lib).header.signature.result,
                    result,
                ))
        {
            #[cfg(any(test, feature = "perf-witness"))]
            if !self.fits(units) {
                self.scratch.exit.reason = Some(Reason::Charge);
            }
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        if let Some(occurrence) = counted {
            self.count(occurrence)?;
        } else if let Some(act) = site.act
            && let Some(watching) = &self.projection()
        {
            watching.action_start(self.exe, &self.stream.nested[act.ix()].site);
        }
        if let Some(root) = site.root
            && let Some(watching) = &self.projection()
        {
            watching.root_call(self.exe, root, args.len());
        }
        self.root_arguments(before);
        Ok(())
    }

    /// The scalar answer `value` of the typed path of operator `site` on
    /// two floats or two word integers, committed as [`Self::operate`]
    /// commits the same answer: its region closed, its occurrence counted
    /// and its formula charged the `units` its scalar table gives for the
    /// call's sizes, in that order. `None`, with nothing committed, where
    /// [`Self::operate`] decides otherwise: an observed helper's body, a
    /// depth that does not fit, sizes its table does not certify or a charge
    /// past the bound. The checked occurrence advance remains after the
    /// argument region closes, as in the source handler.
    #[inline(always)]
    pub(super) fn typed(
        &mut self,
        site: &OperatorSite,
        value: Value,
        units: Option<u64>,
    ) -> Option<Answer> {
        if !self.accounting && self.admitted && !site.checked && site.counted.is_none() {
            return Some(Ok(value));
        }
        // The kernel producer already validated checked arguments/result.
        if !self.admits(site) {
            return None;
        }
        let units = if self.charging { units? } else { 0 };
        let fixed = if self.accounting { site.fixed } else { 0 };
        if !self.fits(fixed.checked_add(units)?) {
            return None;
        }
        // Both fit together, so each is charged without a second check.
        if self.accounting
            && let Some(close) = &site.close
        {
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
    /// entry declines or a measured value is
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
        if !self.charging {
            return Some((value, 0));
        }
        // As a loop region charges a prepared answer: the units its guard
        // certified, the formula over values that carry their measurements,
        // or the formula measured in the arena.
        let units = match answer
            .scalar_sizes
            .and_then(|mask| fast.scalar_units(mask))
            .or_else(|| {
                self.projection()
                    .is_none()
                    .then(|| fast.immediate(args, Fresh::default(), &value))
                    .flatten()
            }) {
            Some(units) => units,
            None => match fast.charge(&function.charge, args, &value) {
                Charge::Units(units) => units,
                Charge::Plan(plan) => match &self.projection() {
                    Some(watching) => {
                        watching.formula_enter();
                        let answer = measured_formula(self.arena, plan, args, Some(&value));
                        watching.formula_leave();
                        answer
                    }?,
                    None => measured_formula(self.arena, plan, args, Some(&value))?,
                },
            },
        };
        Some((value, units))
    }

    #[inline(always)]
    fn admit_native(&mut self, units: u64) -> bool {
        let fits = self.fits(units);
        #[cfg(any(test, feature = "perf-witness"))]
        if !fits {
            self.scratch.exit.reason = Some(Reason::Charge);
        }
        fits
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

    /// Writes session binding `name`, a declaration when `bind`, as its
    /// source handler writes it. An assignment of a binding the cell does
    /// not hold resident leaves to that handler: `false`, nothing written.
    #[inline(never)]
    fn write_session(&mut self, name: u32, value: Value, bind: bool) -> bool {
        let Some(binding) = self.stream.names.get(name as usize) else {
            return false;
        };
        if !bind && self.session.resident(&binding.key).is_none() {
            return false;
        }
        self.session.write(binding, value);
        match self
            .scratch
            .sessions
            .iter_mut()
            .find(|(read, _)| *read == name)
        {
            Some((_, cached)) => *cached = value,
            None => self.scratch.sessions.push((name, value)),
        }
        true
    }

    #[inline(never)]
    fn session_cold(&mut self, name: u32) -> Option<Value> {
        let binding = self.stream.names.get(name as usize)?;
        let value = self.session.resident(&binding.key)?;
        self.scratch.sessions.push((name, value));
        Some(value)
    }

    /// `target[at]` of a list or a tuple, when the frame may hold it;
    /// `None` where the reference reads it: another target, no item
    /// there, or one the arena cannot read where it is.
    #[inline(always)]
    fn item(
        &mut self,
        site: Option<crate::compile::ReadSite>,
        target: Value,
        at: Value,
    ) -> Option<Value> {
        if let Value::List(object) | Value::Tuple(object) = target {
            return self
                .arena
                .item(object, crate::compile::position(&at)?)
                .ok()?;
        }
        self.heap(0, false, |cx| {
            crate::compile::read_index(cx, site, target, at)
        })
        .map(|answer| answer.value)
    }

    /// The field of `target` that the stream's field read `field` names,
    /// when the frame may hold it;
    /// `None` where the reference reads it: no record, a key the record
    /// has not resolved, or a value the arena cannot read where it is. A
    /// record's field is read through its site's property cache, as the
    /// reference reads it.
    #[inline(always)]
    fn field(&mut self, target: Value, field: usize) -> Option<Value> {
        let field = self.stream.fields.get(field)?;
        if let Value::Error(error) = target {
            let lash_kernel_doc::arena::Body::Error(parts) = self.arena.body(error).ok()? else {
                return None;
            };
            return Some(match field.name.as_str() {
                "kind" => parts.kind,
                "message" => parts.message,
                "data" => parts.data,
                _ => Value::Absent,
            });
        }
        let Value::Record(record) = target else {
            return None;
        };
        let slot = field.site.and_then(|site| self.sites.slot(site));
        let (value, found) = find_property(self.arena, record, slot, &field.key).ok()?;
        if let Some(site) = field.site
            && found != slot
        {
            self.sites.keep(site, found);
        }
        let value = value.unwrap_or(Value::Absent);
        Some(value)
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
        let unit = if self.projection().is_some() {
            Some(
                self.empty_tuple()
                    .ok_or_else(|| self.at_held(site.miss, site.reg, value))?,
            )
        } else {
            // The source builds the empty operand, which the arena holds
            // until its next release; a metered run holds it here too.
            if self.memory {
                self.empty_tuple()
                    .ok_or_else(|| self.at_held(site.miss, site.reg, value))?;
            }
            None
        };
        if let Some(close) = &site.close {
            self.close(close);
        }
        if let Some(unit) = unit {
            let native = NativeSite {
                lib: site.lib,
                close: None,
                pc: site.miss,
                reg: site.reg,
                depth: Depth::None,
                direct: None,
            };
            let mut args = [Value::Absent; MAX_ARGS];
            args[0] = value;
            args[1] = unit;
            return self.native(&native, &args, 2);
        }
        self.formula(units);
        Ok(answer)
    }

    /// `same(kind(x), literal)` of `x`: by its memo
    /// where it has answered `x`'s kind, as the two calls answer and
    /// charge it otherwise.
    #[inline(always)]
    fn kind_test(&mut self, site: &compiled::KindTest, x: Value) -> Answer {
        if self.projection().is_some() {
            let kind = self.operate(&site.kind, &[x])?;
            return self.operate(&site.same, &[kind, site.literal]);
        }
        let memo = &site.memo;
        let total = |close: Option<Close>| close.map_or(0, |close| u64::from(close.total));
        let (kind_closed, same_closed) = (total(site.kind.close), total(site.same.close));
        // Each compiled site belongs to a plan with one charge policy.
        // Unpaid helper sites can retain their general answer with zero fees;
        // charge-off guest sites must not seed a paid memo with those zeros.
        let memoized = self.charging || !self.accounting;
        if memoized
            && let Some((answer, kind_units, same_units)) = memo.get(x.kind())
            && self.depth_fits(site.kind.depth)
            && self.depth_fits(site.same.depth)
            && (!self.accounting
                || self.fits(
                    kind_closed
                        .saturating_add(kind_units)
                        .saturating_add(same_closed)
                        .saturating_add(same_units),
                ))
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
        if memoized
            && let Value::Bool(answer) = same
            && matches!(kind, Value::Text(_))
        {
            memo.set(
                x.kind(),
                (
                    answer,
                    if self.charging {
                        between - before - kind_closed
                    } else {
                        0
                    },
                    if self.charging {
                        self.charged - between - same_closed
                    } else {
                        0
                    },
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

    /// The callee of plan `plan`, its captured values from `captured`.
    #[inline(always)]
    fn callee(&self, plan: u32, captured: u32) -> Option<Callee<'a>> {
        let exe = self.exe;
        let island = exe.island_plan(plan)?;
        Some(Callee {
            link: self.link,
            plan: island,
            stream: island.stream(exe),
            compiled: island.compiled(exe)?,
            captured,
        })
    }

    /// A cache hit carries the compiled form's plan index: resolution
    /// has already compiled it, so neither code lookup nor lazy compilation
    /// belongs on this path.
    #[inline(always)]
    fn cached_callee(&self, cached: Cached) -> Option<Callee<'a>> {
        let plan = self.exe.island_plan(cached.compiled)?;
        Some(Callee {
            link: cached.link,
            plan,
            stream: plan.stream(self.exe),
            compiled: plan.resolved()?,
            captured: cached.captured,
        })
    }

    /// The island of declared code `code`.
    fn declared(&mut self, code: CodeId) -> Option<Callee<'a>> {
        // Free cells belong to a callable's captured environment. A bare
        // declared call has no such environment; the source reports it.
        if !self.exe.code(code).free.is_empty() {
            return None;
        }
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

    /// The island of the closure `handle`, resolved in the open scope:
    /// its plan, and its captured values read from its cells.
    #[inline(never)]
    fn resolve_cold(&mut self, handle: ArenaHandle) -> Option<Cached> {
        let code = found_code(self.arena, self.exe, handle)?;
        // A frozen environment is an immutable template. Its invocation
        // must instantiate fresh free cells through the source call owner.
        if !self.exe.code(code).free.is_empty() {
            return None;
        }
        let plan = self.exe.island_of(code)?;
        let (identity, cells, _) = self.arena.closure(handle).ok()?;
        let link = identity.callable.link;
        let captured = captured(
            self.exe,
            self.arena,
            &mut self.scratch.captured,
            plan,
            cells,
        )?;
        Some(Cached {
            link,
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

    /// The frame of `plan` holding `slots` is left by a stop: its record.
    #[cold]
    fn left(
        &mut self,
        plan: &IslandPlan,
        captured: u32,
        slots: &Slots,
        library: Option<super::LibraryCall>,
    ) {
        let exit = &mut self.scratch.exit;
        let mut slots = slots[..plan.slots].to_vec();
        if captured != RESIDENT {
            for (index, at) in plan.captures.iter().enumerate() {
                let (_, cell) = self.scratch.captured[captured as usize + index];
                slots[*at as usize] = SlotState::Cell(cell);
            }
        }
        let control = exit.control.drain(..).rev().collect();
        let stands = exit.stands.take().unwrap_or(Stands::Boundary);
        exit.regs.clear();
        exit.levels.push(Level {
            library,
            link: self.link,
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
            if std::mem::take(&mut exit.scheduling) {
                Stands::Scheduling { stmt, pc, regs }
            } else {
                Stands::Running { stmt, pc, regs }
            }
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
    /// Real iterations executed in unboxed typed registers.
    pub(crate) static TYPED_REPEATED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Collection operations answered without a heap context.
    pub(crate) static DIRECT_COLLECTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(crate) static DIRECT_ITEMS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(crate) static DIRECT_INDICES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(crate) static DIRECT_FIELDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Guarded member paths taken whole.
    pub(crate) static GUARDED_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
    let island = exe.island_plan(plan)?;
    island.compiled(exe)?;
    if cells.len() != island.captures.len() {
        return None;
    }
    capture_values(arena, values, cells)
}

/// Reads an already validated ordered capture set without resolving its plan again.
#[inline(always)]
fn capture_values(
    arena: &Arena,
    values: &mut Vec<(Value, ArenaHandle)>,
    cells: &[ArenaHandle],
) -> Option<u32> {
    // A loop can alternate closures at one call site indefinitely. End
    // the resolution scope through a precise exit; active frames still
    // own offsets into this buffer until they have materialized.
    if !cells.is_empty() && values.len() >= entry::SCOPE_CAPTURED {
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
            &identity.callable,
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

/// Validated source coordinates and evaluated operands of a paid action.
struct ActionSuffix {
    stmt: StmtId,
    after: usize,
    defer: bool,
}
