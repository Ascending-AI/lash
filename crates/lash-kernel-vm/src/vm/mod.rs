//! The core's dispatcher (cutover contract §1, §3, §4): one
//! `loop { match op }` over the running task's frames, billing the event
//! contract as it goes.
//!
//! A block head prepays its fixed cost when it fits before both the limit
//! and the slice's end; otherwise the block runs event by event, each op
//! billing its own event at its start. The per-op check is one flag,
//! `exact`, so a prepaid block's ops pay nothing more than the dispatch.
//! Under `Charge::Off` the same machinery runs over the same fixed
//! costs, which only place the slice's end: no limit, no dynamic fee and
//! no charge readout.

pub(crate) mod callable;
pub(crate) mod calls;
mod cells;
pub(crate) mod charge;
pub(crate) mod datum;
mod digests;
mod exceeded;
mod fits;
pub(crate) mod frames;
mod frozen;
mod fused;
pub(crate) mod inline;
mod library;
pub(crate) mod loops;
pub(crate) mod machine;
pub(crate) mod members;
pub(crate) mod plans;
mod raise;
pub(crate) mod tasks;
pub(crate) mod watch;

use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::native::{NativeError, NewObject, Retry};
use lash_kernel_doc::{Datum, Document, Measure, Value};
use lash_kernel_value::data_limits::KernelDataLimits;
use lash_kernel_value::names::CallableId;
use lash_kernel_value::sites::Site;

use self::digests::Digests;

#[cfg(test)]
thread_local! {
    /// The frames the call depth test has read on this thread (`K-BND-002`).
    static DEPTH_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one piece of charge work on this thread's witness: a formula
/// evaluated, a fee sized or billed, a charge-tape write or a fixed event
/// billed against the limit (FIG-6732). Compiled into test builds only.
#[cfg(test)]
pub(crate) fn charge_work() {
    crate::perf_witness::record(|work| work.charge_work += 1);
}

/// The frames the call depth test has read on this thread so far.
#[cfg(test)]
pub(crate) fn depth_reads() -> u64 {
    DEPTH_READS.with(std::cell::Cell::get)
}
use self::library::Library;
use crate::bytecode::charge_tables::RESULT;
use crate::bytecode::op::Op;
use crate::bytecode::program::{Callee, Constant, LIB, Program, Then, Unit, UnitId, UnitKind};
use crate::heap::{Heap, LoadError};
use crate::observe::Weight;
use crate::{Bound, ChargeLimiter, Host, LimitDecision};
use charge::{Meter, Refused, Rest};
use frames::{Bill, Context, Entry, Frame, Handler};

/// Why [`Vm::run`] returned.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The task's bottom frame returned this value, copied out.
    Returned(Datum),
    /// A raise no handler of the task caught.
    Uncaught(Value),
    /// `finish` ended the run with this copied-out value.
    Finish(Datum),
    /// `fail` ended the run with this copied-out value.
    Fail(Datum),
    /// No task is ready and some wait: the embedder holds the requests
    /// (`K-MACH-006`).
    Park(crate::interface::Park),
    /// No task is ready and nothing can wake one: the tasks still waiting
    /// (`K-TASK-024`).
    Deadlock(Vec<lash_kernel_value::sites::TaskLabel>),
    /// A read the host answered with a fault.
    Host(crate::HostFault),
    /// The embedder asked to cancel the run while it was idle.
    Cancelled,
    /// The slice ended at a step's end (`K-MACH-002`).
    Slice,
    /// A bound refused the run.
    Bound(Box<Exceeded>),
    /// Infrastructure: the heap or a native broke an invariant, or the
    /// program reached code the core could not compile.
    Fault(String),
}

impl Stop {
    pub(crate) fn bound(exceeded: Exceeded) -> Self {
        Self::Bound(Box::new(exceeded))
    }
}

/// A bound a run passed.
#[derive(Clone, Debug)]
pub(crate) struct Exceeded {
    pub bound: Bound,
    pub limit: u64,
    /// The library call that passed the bound, or the authored function
    /// containing a charge, memory or call-depth stop.
    pub function: Option<Arc<str>>,
    /// The nearest authored statement for a charge, memory or call-depth
    /// stop.
    pub site: Option<Site>,
}

impl Exceeded {
    /// A run-wide bound, passed by no function and at no site: a park's
    /// requests, the live tasks, a join's members.
    pub(crate) fn run(bound: Bound, limit: u64) -> Self {
        Self {
            bound,
            limit,
            function: None,
            site: None,
        }
    }
}

/// How an op leaves the straight line.
#[derive(Debug)]
pub(crate) enum Flow {
    /// A guest raise, not yet caught.
    Raise(Value),
    /// Boxed: a stop is rare, and the hot path's `Result` stays small.
    Stop(Box<Stop>),
    /// A read reached chunk `chunk` of `object` while it was cold. The op
    /// has committed nothing: the run loads the chunk and runs it again,
    /// or resumes the phase [`Vm::restart`] holds (contract §6, cold path).
    /// No guest sees it.
    Cold { object: ArenaHandle, chunk: u32 },
}

impl From<Stop> for Flow {
    fn from(stop: Stop) -> Self {
        Self::Stop(Box::new(stop))
    }
}

pub(crate) type Run<T> = Result<T, Flow>;

pub(crate) fn fault(what: impl Into<String>) -> Flow {
    Flow::from(Stop::Fault(what.into()))
}

/// What a read's native error is to the run: a cold chunk to load, or a
/// fault.
pub(crate) fn read(error: NativeError) -> Flow {
    match error {
        NativeError::Retry(Retry::Cold { object, chunk }) => Flow::Cold { object, chunk },
        other => fault(other.to_string()),
    }
}

/// A phase whose earlier work has committed, resumed after a cold load in
/// place of the op that began it: it never runs a native again, pops
/// another frame or bills a fee twice.
#[derive(Debug)]
pub(crate) enum Restart {
    /// The unwind of a raise of this value, at the running frame.
    Unwind(Value),
    /// A native call's completion: its result is built and installed, its
    /// arguments are at `from`, and its formula `charge` is yet to bill.
    Native {
        from: u32,
        native: u32,
        charge: Option<u32>,
        done: calls::Done,
    },
}

/// A leaving frame's completion, measured before the frame leaves: where
/// its call's billed arguments begin, and the fee its formula comes to.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Completion {
    from: Option<usize>,
    units: Option<u64>,
}

impl Restart {
    /// The values the phase holds, for the collector.
    fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        match self {
            Self::Unwind(value) => visit(value),
            Self::Native { done, .. } => done.roots(visit),
        }
    }
}

/// The machine state of one instance over a heap `H`: the program, the
/// heap, the meter and the running task's context. The scheduler keeps the
/// other tasks' contexts in [`Vm::tasks`] and swaps them in.
pub(crate) struct Vm<H: Heap> {
    /// The instance's own program, shared only so the dispatch loop can
    /// borrow it beside the machine's mutable state.
    pub program: Arc<Program>,
    pub heap: H,
    /// The program's constants, materialized in this instance's heap.
    pub(crate) constants: Vec<Value>,
    /// What each Link's frames of library code read, by the Link's index:
    /// the library's constants, each placed in this instance's heap
    /// before the first frame that reads it runs as the Link; and the
    /// constants a sealed run placed that this instance has not.
    library: Library,
    /// The Link each own unit runs as, by its index in the program's.
    links: Vec<u32>,
    /// Each native's implementation, by its tag, in this instance's registry,
    /// found when the run first calls it.
    implementations: [Vec<Option<Arc<dyn lash_kernel_doc::NativeFunction>>>; 2],
    /// The identity of each closure library code makes, by the Link it
    /// runs as, the making unit and its closure site.
    identities:
        std::collections::HashMap<(u32, UnitId, u32), Arc<lash_kernel_doc::arena::ClosureIdentity>>,
    /// The host bindings main's free names use, then the session's bindings it
    /// names nowhere, then its saved functions' objects ([`Self::session`]).
    pub globals: Vec<Value>,
    /// The program's globals no host or session binding gave a value:
    /// reading or assigning one raises (`K-FORM-003`). Empty, all bound,
    /// in a run that resumes.
    pub(crate) unbound: Box<[bool]>,
    /// What the globals past the program's are (`K-SES-001`, `K-SES-004`).
    pub(crate) session: crate::durable::session::Carried,
    /// `main`'s locals as its bottom frame left them: what its top-level
    /// bindings hand on (`K-SES-001`).
    pub(crate) top: Option<Vec<Value>>,
    /// The running task's frames and stack.
    pub cx: Context,
    /// Every task, and what waits between them.
    pub tasks: tasks::Tasks,
    /// The document the program runs, for decoding what crosses into it.
    pub document: Arc<Document>,
    /// Values the scheduler holds outside any context (results, operands
    /// of issued actions), for the collector.
    pub held: Vec<Value>,
    /// The direct-call guard ([`calls::Direct`]): the instance's own
    /// cache, never sealed.
    direct: Option<calls::Direct>,
    /// Capture reads the arena answered, by closure and capture: the
    /// instance's own cache, never sealed.
    captured: [std::cell::Cell<cells::Captured>; cells::KEPT],
    meter: Meter,
    /// The run is under `Charge::Off`: fixed costs place slices; no
    /// limit, fee or readout.
    off: bool,
    /// The current block runs event by event.
    exact: bool,
    /// The next op is the first of a run call: its step is not tested.
    fresh: bool,
    pub call_depth: u32,
    /// The deepest value the run builds or decodes (`K-VAL-034`): the
    /// deployment's data limit at a start, the standard one at an open.
    pub value_depth: usize,
    pub limiter: Option<Box<dyn ChargeLimiter>>,
    /// What decides where an allocation passes the memory bound.
    pub memory_limiter: Option<Box<dyn crate::interface::MemoryLimiter>>,
    /// The total a charge refusal reported, once one has.
    pub refused: Option<u64>,
    /// Each closure function's body unit, by its Link and function.
    closures: Digests<UnitId>,
    /// Each function value's trampoline, by its Link and function: every
    /// function each loaded Link declares.
    functions: Digests<UnitId>,
    /// What observes the run; an observed run takes the exact path.
    pub watch: Option<Box<watch::Watch>>,
    /// What a frame of each unit holds (contract §2): admitted when the
    /// frame is entered, returned when it leaves.
    pub(crate) frames: Vec<u64>,
    /// What a function crossing to a tool reads besides the heap.
    pub(crate) crossing: datum::Crossing,
    /// What the run is started with (its program, constants, inputs and
    /// first frame) did not fit the memory bound: the run starts, and its
    /// first step ends with the memory bound before it runs anything
    /// (`K-BND-001`).
    pub(crate) unadmitted: Option<Exceeded>,
    /// The committed phase a cold read interrupted, resumed once its chunk
    /// is loaded; none when the interrupted op runs again.
    pub(crate) restart: Option<Restart>,
    /// Where the last raise stood in document code, named by an uncaught
    /// end (`RunError::Uncaught`).
    pub(crate) raised_at: Option<Site>,
    /// A cold load has just ended: the running frame's op before its `pc`
    /// runs again, its fixed event already billed, its slice test, step
    /// and points already passed.
    pub(crate) retrying: bool,
    /// Whether the running slice's host observes sites (`Host::observes_sites`).
    pub(crate) sites: bool,
    /// The position proofs of folded short-text key reads (`inline`):
    /// process-local, each checked whole by the arena before it is used.
    pub(crate) lookups: inline::Lookups,
    /// The storage a native call's nursery lists what it built in, empty
    /// between calls: kept so a call allocates no list of its own.
    pub(crate) built: Vec<(
        lash_kernel_doc::arena::ArenaHandle,
        lash_kernel_doc::NewObject,
    )>,
    /// The laws' forced-cold runs: pages every object out at each
    /// statement boundary, the machine's values rooted.
    #[cfg(test)]
    pub(crate) chill: Option<fn(&mut H, &[Value])>,
    /// The laws' collected runs: a collection at each statement boundary.
    #[cfg(test)]
    pub(crate) collecting: bool,
}

impl<H: Heap> Vm<H> {
    /// An instance of `program` over `heap`, its constants placed and
    /// admitted. `limit` is the charge limit under `Charge::Exact`;
    /// `off` selects `Charge::Off`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        document: Arc<Document>,
        program: Arc<Program>,
        heights: &[u32],
        mut heap: H,
        used: u64,
        limit: u64,
        off: bool,
        call_depth: u32,
        crossing: datum::Crossing,
        mut saved: library::Saved,
    ) -> Result<Self, Stop> {
        // The run holds its own document's code; the program's constants
        // are code, its own or the library's, and hold nothing (contract
        // §2, `K-MACH-001`, `K-BND-003`).
        let unadmitted = heap
            .admit(crate::heap::meter::code(&document))
            .err()
            .map(|refused| Exceeded::run(Bound::Memory, refused.bound));
        // A sealed run's constants are code, held at nothing, until a
        // collection finds no value referencing one it no longer places.
        for value in saved.values() {
            heap.adopt_constant(*value);
        }
        let constants =
            library::place_constants(&mut heap, &program, &mut saved, unadmitted.is_some())?;
        let frames = program
            .units
            .iter()
            .zip(heights)
            .map(|(unit, height)| crate::heap::meter::frame(unit, *height))
            .collect();
        let mut closures = Digests::default();
        let mut links = Vec::with_capacity(program.units.len());
        for (at, unit) in program.units.iter().enumerate() {
            links.push(program.link_index(&unit.link).unwrap_or(0));
            if let UnitKind::Closure(function) = unit.kind {
                let callable = CallableId {
                    link: unit.link,
                    function,
                };
                closures.insert(callable, program.own_unit(at));
            }
        }
        let functions = program
            .functions
            .iter()
            .map(|entry| (entry.callable, entry.unit))
            .collect();
        let globals = vec![Value::Absent; program.globals.len()];
        Ok(Self {
            program,
            heap,
            constants,
            library: Library::of(saved),
            links,
            implementations: [Vec::new(), Vec::new()],
            identities: Default::default(),
            globals,
            unbound: Box::default(),
            session: Default::default(),
            top: None,
            cx: Context::default(),
            tasks: tasks::Tasks::new(),
            document,
            held: Vec::new(),
            direct: None,
            captured: Default::default(),
            meter: Meter::new(used, if off { u64::MAX } else { limit }),
            off,
            exact: true,
            fresh: false,
            call_depth,
            value_depth: KernelDataLimits::standard().value_depth(),
            limiter: None,
            memory_limiter: None,
            refused: None,
            closures,
            functions,
            watch: None,
            frames,
            crossing,
            unadmitted,
            restart: None,
            raised_at: None,
            retrying: false,
            sites: false,
            lookups: Default::default(),
            built: Vec::new(),
            #[cfg(test)]
            chill: None,
            #[cfg(test)]
            collecting: false,
        })
    }

    /// What the run has paid (fixed costs only under `Charge::Off`).
    pub(crate) fn charged(&self) -> u64 {
        self.meter.total(self.through())
    }

    pub(crate) fn charge_limit(&self) -> u64 {
        self.meter.limit()
    }

    pub(crate) fn off(&self) -> bool {
        self.off
    }

    /// Starts unit `unit` as the running context's bottom frame, its
    /// `args` its parameters.
    pub(crate) fn start(
        &mut self,
        unit: UnitId,
        args: &[Value],
        closure: Option<ArenaHandle>,
    ) -> Run<()> {
        if self.unadmitted.is_none() {
            self.cx.stack.extend_from_slice(args);
            let entered = self.enter(unit, args.len(), closure, None, 0);
            self.admitted(entered, ())?;
        }
        // A run refused before it began stops at its first step: the
        // root's first statement, where an authored root has one.
        let root = self.program.unit(unit);
        if let Some(refused) = self.unadmitted.as_mut()
            && refused.site.is_none()
            && root.authored
        {
            refused.function = Some(Arc::clone(&root.name));
            refused.site = Some(
                root.step
                    .iter()
                    .position(|&step| step)
                    .and_then(|pc| root.statement(pc))
                    .unwrap_or_else(|| root.site.clone()),
            );
        }
        Ok(())
    }

    /// What starting the run built, or `refused` once something it is
    /// started with did not fit the memory bound: the refusal is held for
    /// the first step, as a refused program or constant is.
    pub(crate) fn admitted<T>(&mut self, built: Run<T>, refused: T) -> Run<T> {
        match built {
            Err(Flow::Stop(stop)) => match *stop {
                Stop::Bound(exceeded) if exceeded.bound == Bound::Memory => {
                    self.unadmitted.get_or_insert(*exceeded);
                    Ok(refused)
                }
                stop => Err(Flow::from(stop)),
            },
            built => built,
        }
    }

    /// Holds `units` of delivered outcomes raw until their consumption
    /// decodes them (contract §2). A holding past the memory bound, after
    /// a collection, is held for the next step, as a refused program or
    /// constant is.
    pub(crate) fn hold_raw(&mut self, units: u64) {
        let held = self.admit(units);
        if let Err(Flow::Stop(stop)) = held
            && let Stop::Bound(exceeded) = *stop
        {
            self.unadmitted.get_or_insert(*exceeded);
        }
    }

    /// Whether something the run is started with did not fit its memory
    /// bound.
    pub(crate) fn unadmitted(&self) -> bool {
        self.unadmitted.is_some()
    }

    // --- The meter.

    /// The fixed charge the running block has billed through the op last
    /// dispatched.
    fn through(&self) -> u64 {
        if !self.meter.prepaid() {
            return 0;
        }
        let Some(frame) = self.cx.frames.last() else {
            return 0;
        };
        let unit = self.program.unit(frame.unit);
        let at = (frame.pc as usize).saturating_sub(1);
        unit.through.get(at).copied().unwrap_or(0)
    }

    /// Leaves the running block early at the op last dispatched: its
    /// reservation is cancelled once.
    fn cancel(&mut self) {
        let through = self.through();
        self.meter.unbill(through);
        self.exact = true;
    }

    /// Asks the limiter about a refusal, retrying what it left unbilled
    /// while the limiter grows the limit.
    ///
    /// `past` when the refused charge is the running op's own, its frame
    /// standing past it; else it is the op's fixed event, at the op.
    fn refusal(&mut self, mut refused: Refused, past: bool) -> Run<()> {
        loop {
            let decision = match self.limiter.as_mut() {
                Some(limiter) => limiter.limit_reached(refused.total, self.meter.limit()),
                None => LimitDecision::Refuse,
            };
            match decision {
                LimitDecision::Grow(more) if more > 0 => {
                    self.meter.grow(more);
                    let retried = self.meter.retry(refused.rest);
                    // The tape holds a refused unit run's passing unit
                    // already; an atomic event, whole.
                    if let (Some(watch), Rest::Units(units)) = (&self.watch, refused.rest) {
                        let left = match &retried {
                            Ok(()) => 1,
                            Err(again) => match again.rest {
                                Rest::Units(left) => left,
                                Rest::Atomic(_) => units,
                            },
                        };
                        watch.charge(Weight::Units(units - left));
                    }
                    match retried {
                        Ok(()) => return Ok(()),
                        Err(again) => refused = again,
                    }
                }
                _ => {
                    self.refused = Some(refused.total);
                    return Err(Flow::from(Stop::bound(self.exceeded_at(
                        Bound::Charge,
                        self.meter.limit(),
                        past,
                    ))));
                }
            }
        }
    }

    /// Bills op `pc`'s fixed event on the exact path.
    #[inline]
    fn bill(&mut self, unit: &Unit, pc: usize) -> Run<()> {
        let event = unit.event[pc];
        if self.watch.is_some() || self.off {
            return self.bill_aside(event);
        }
        #[cfg(test)]
        charge_work();
        if event.units != 0
            && let Err(refused) = self.meter.units(event.units)
        {
            self.refusal(refused, false)?;
        }
        if event.atomic != 0
            && let Err(refused) = self.meter.atomic(event.atomic)
        {
            self.refusal(refused, false)?;
        }
        Ok(())
    }

    /// Bills a dynamic fee (a completion formula, a copy-out): on top of
    /// a prepaid block where the block still fits, otherwise on the exact
    /// path, the running block continuing event by event. Under
    /// `Charge::Off` it bills nothing.
    pub(crate) fn fee(&mut self, units: u64) -> Run<()> {
        if self.off {
            return Ok(());
        }
        #[cfg(test)]
        charge_work();
        if let Some(watch) = &self.watch {
            watch.charge(Weight::Atomic(units));
        }
        if self.meter.prepay(units) {
            return Ok(());
        }
        let through = self.through();
        self.exact = true;
        if let Err(refused) = self.meter.fee(units, through) {
            self.refusal(refused, true)?;
        }
        Ok(())
    }

    /// The bill of a call of formula `charge` over the top `argc`
    /// operands, which it keeps when the formula measures them.
    pub(super) fn owe(&mut self, charge: u32, argc: usize) -> Bill {
        let from = self.cx.billed.len() as u32;
        if self.program.charge(charge).reads_args() {
            let at = self.cx.stack.len() - argc;
            self.cx.billed.extend_from_slice(&self.cx.stack[at..]);
        }
        Bill { charge, from }
    }

    /// What a leaving frame's `bill` comes to over its call's arguments and
    /// its result, of size 0 when the call raised: every read the
    /// completion makes, before the frame leaves.
    fn completion(&self, bill: Option<Bill>, result: Option<&Value>) -> Run<Completion> {
        let Some(bill) = bill else {
            return Ok(Completion::default());
        };
        let from = bill.from as usize;
        let units = if self.off {
            None
        } else {
            Some(self.formula(bill.charge, &self.cx.billed[from..], result)?)
        };
        Ok(Completion {
            from: Some(from),
            units,
        })
    }

    /// Bills a completion measured before `callee`'s frame left, and drops
    /// the call's arguments. A refusal names the library call that made
    /// the bill, as a native's does.
    fn settle(&mut self, completion: Completion, callee: UnitId) -> Run<()> {
        if let Some(from) = completion.from {
            self.cx.billed.truncate(from);
        }
        match completion.units {
            Some(units) => self.fee(units).map_err(|flow| self.billed_in(flow, callee)),
            None => Ok(()),
        }
    }

    /// Evaluates formula `charge` over `args` and `result`.
    fn formula(&self, charge: u32, args: &[Value], result: Option<&Value>) -> Run<u64> {
        #[cfg(test)]
        charge_work();
        let formula = self.program.charge(charge);
        if let Some(units) = formula.constant() {
            return Ok(units);
        }
        // Two arguments and a result of size 1 or 2 read their fee from
        // the formula's table.
        if let ([first, second], Some(result)) = (args, result)
            && let Some(units) = formula.pair([first, second, result])
        {
            return Ok(units);
        }
        let arena = self.heap.arena();
        // A sum of sizes over values that hold no member is billed without
        // its tree, the same amount: such a value's deep size is its size
        // (`K-CHG-005`). A leaf the arena holds (a long text, bytes, a big
        // integer) is measured from its body; one it cannot read now takes
        // the tree, which meets the same read.
        if let Some(linear) = formula.linear()
            && let Some(units) = linear.units(|position| {
                let value = if position == RESULT {
                    result
                } else {
                    args.get(usize::from(position))
                };
                match value {
                    None => Some(0),
                    Some(value) => crate::data::immediate_size(value).or_else(|| {
                        crate::data::holds_nothing(value)
                            .then(|| crate::data::size(arena, value).ok())
                            .flatten()
                    }),
                }
            })
        {
            return Ok(units);
        }
        let mut failed = None;
        let units = formula.evaluate(|position, how| {
            let value = if position == RESULT {
                result
            } else {
                args.get(usize::from(position))
            };
            let Some(value) = value else {
                return 0;
            };
            if let Some(units) = crate::data::immediate_measure(value, how) {
                return units;
            }
            let measured = match how {
                Measure::Size => crate::data::size(arena, value),
                Measure::DeepSize => crate::data::deep_size(arena, value),
                Measure::NestedSize => crate::data::nested_size(arena, value),
                Measure::Magnitude => Ok(crate::data::magnitude(arena, value)),
            };
            measured.unwrap_or_else(|error| {
                failed = Some(error);
                0
            })
        });
        match failed {
            Some(error) => Err(read(error)),
            None => Ok(units),
        }
    }

    // --- The heap.

    /// Admits `units`, collecting once before it refuses (contract §2).
    #[inline]
    fn admit(&mut self, units: u64) -> Run<()> {
        if self.heap.admit(units).is_ok() {
            return Ok(());
        }
        self.admit_collected(units)
    }

    /// Admits `units` after a collection, or refuses.
    #[cold]
    #[inline(never)]
    fn admit_collected(&mut self, units: u64) -> Run<()> {
        self.collect();
        loop {
            let Err(refused) = self.heap.admit(units) else {
                return Ok(());
            };
            // The host decides once a collection leaves the bound passed
            // (FIG-6768), and is asked again while what it grew still
            // leaves it passed.
            let used = refused.held.saturating_add(refused.wanted);
            let decision = match self.memory_limiter.as_mut() {
                Some(limiter) => limiter.limit_reached(used, refused.bound),
                None => LimitDecision::Refuse,
            };
            match decision {
                LimitDecision::Grow(more) if more > 0 => self.heap.grow(more),
                _ => {
                    return Err(Flow::from(Stop::bound(
                        self.exceeded(Bound::Memory, refused.bound),
                    )));
                }
            }
        }
    }

    /// Places `object`, admitted first.
    fn alloc(&mut self, object: NewObject) -> Run<Value> {
        self.admit(crate::heap::slots::schedule(&object))?;
        self.heap.create(object).map_err(|error| self.native(error))
    }

    fn cell(&mut self, value: Value) -> Run<ArenaHandle> {
        self.admit(crate::heap::meter::CELL)?;
        Ok(self.heap.cell(value))
    }

    /// Mark-sweep from every value the machine holds.
    pub(crate) fn collect(&mut self) {
        self.retire_unreached();
        let Self {
            heap,
            constants,
            library,
            globals,
            top,
            cx,
            tasks,
            held,
            restart,
            ..
        } = self;
        heap.collect(&mut |visit| {
            constants.iter().for_each(&mut *visit);
            library.values().for_each(&mut *visit);
            globals.iter().for_each(&mut *visit);
            top.iter().flatten().for_each(&mut *visit);
            held.iter().for_each(&mut *visit);
            cx.roots(&mut *visit);
            tasks.roots(&mut *visit);
            if let Some(restart) = restart {
                restart.roots(&mut *visit);
            }
        });
    }

    /// At a statement boundary, collects under the laws' collected runs and
    /// pages out every object the heap can under their forced-cold runs.
    #[cfg(test)]
    fn chill(&mut self) {
        if self.collecting {
            self.collect();
        }
        let Some(chill) = self.chill else { return };
        self.tasks.paged = self.tasks.all.len();
        let mut roots = Vec::new();
        let mut visit = |value: &Value| roots.push(*value);
        self.constants.iter().for_each(&mut visit);
        self.library.values().for_each(&mut visit);
        self.globals.iter().for_each(&mut visit);
        self.top.iter().flatten().for_each(&mut visit);
        self.held.iter().for_each(&mut visit);
        self.cx.roots(&mut visit);
        self.tasks.roots(&mut visit);
        if let Some(restart) = &self.restart {
            restart.roots(&mut visit);
        }
        chill(&mut self.heap, &roots);
    }

    /// Loads chunk `chunk` of `object`, with no view of the heap live and
    /// every value the machine holds rooted: the interrupted op's operands
    /// on its stack, and what a held phase holds. Cancellation stays
    /// latched for the next slice or park; a load polls nothing.
    #[cold]
    #[inline(never)]
    pub(crate) fn reload(&mut self, object: ArenaHandle, chunk: u32) -> Result<(), Stop> {
        let Self {
            heap,
            constants,
            library,
            globals,
            top,
            cx,
            tasks,
            held,
            restart,
            ..
        } = self;
        let loaded = heap.load(object, chunk, &mut |visit| {
            constants.iter().for_each(&mut *visit);
            library.values().for_each(&mut *visit);
            globals.iter().for_each(&mut *visit);
            top.iter().flatten().for_each(&mut *visit);
            held.iter().for_each(&mut *visit);
            cx.roots(&mut *visit);
            tasks.roots(&mut *visit);
            if let Some(restart) = restart {
                restart.roots(&mut *visit);
            }
        });
        match loaded {
            Ok(()) => Ok(()),
            // The faulting op's reservation is unbilled once, as at any
            // stop, and the run refuses at the memory bound.
            Err(LoadError::Memory(refused)) => {
                Err(Stop::bound(self.exceeded(Bound::Memory, refused.bound)))
            }
            // The store failed: infrastructure, never a guest raise or a
            // retry; the last committed root stands.
            Err(LoadError::Store(error)) => {
                Err(Stop::Host(crate::HostFault::new(error.to_string())))
            }
        }
    }

    /// Goes on after a cold load: the held phase resumes, or the op the
    /// load interrupted runs again.
    #[cold]
    #[inline(never)]
    fn resume(&mut self) -> Run<Option<Stop>> {
        match self.restart.take() {
            None => {
                self.retrying = true;
                Ok(None)
            }
            Some(Restart::Unwind(value)) => self.unwind(value).map(|()| None),
            Some(Restart::Native {
                from,
                native,
                charge,
                done,
            }) => self
                .complete_native(from as usize, native, charge, done)
                .map(|()| None),
        }
    }

    /// The raise or stop a native error is.
    fn native(&mut self, error: NativeError) -> Flow {
        match error {
            NativeError::Retry(Retry::Cold { object, chunk }) => Flow::Cold { object, chunk },
            NativeError::Raised(value) => Flow::Raise(value),
            NativeError::Refused { kind, message } => self.error(kind, &message),
            NativeError::Guard(guard) => Flow::from(Stop::bound(Exceeded {
                bound: Bound::Charge,
                limit: guard.limit,
                function: None,
                site: None,
            })),
            other => fault(other.to_string()),
        }
    }

    /// The raise of a new error of `kind` and `message` (`K-ERR-002`).
    pub(crate) fn error(&mut self, kind: &str, message: &str) -> Flow {
        match self.new_error(kind, message, Value::Null) {
            Ok(value) => Flow::Raise(value),
            Err(flow) => flow,
        }
    }

    /// A new error value of `kind`, `message` and `data`, admitted for
    /// everything it holds before it is built (`K-ERR-001`). `data` must
    /// be held by the caller across the admission's collection.
    pub(crate) fn new_error(&mut self, kind: &str, message: &str, data: Value) -> Run<Value> {
        let units = self
            .heap
            .error_units(kind, message, data)
            .map_err(|error| fault(error.to_string()))?;
        self.admit(units)?;
        self.heap
            .error(kind, message, data)
            .map_err(|error| fault(error.to_string()))
    }

    /// A text value: inline when short, else a long text admitted first.
    pub(crate) fn text(&mut self, text: &str) -> Run<Value> {
        match lash_kernel_doc::arena::ShortText::new(text) {
            Some(short) => Ok(Value::Text(short)),
            None => self.alloc(NewObject::long_text(text.into())),
        }
    }

    // --- Frames.

    /// Whether the running task's active calls fill the call depth bound
    /// (`K-BND-002`): the top frame's [`Frame::depth`], read once.
    #[inline(always)]
    pub(crate) fn depth_full(&self) -> bool {
        #[cfg(test)]
        DEPTH_READS.with(|reads| reads.set(reads.get() + 1));
        self.cx.depth() >= self.call_depth
    }

    /// Pushes a frame of `unit` whose `argc` arguments are the top of the
    /// stack: missing parameters are absent (`K-FN-004`), and each local a
    /// closure shares is boxed into a cell (`K-CLO-001`). Library code
    /// runs as Link `caller`; an own unit as its own Link.
    pub(crate) fn enter(
        &mut self,
        unit: UnitId,
        argc: usize,
        closure: Option<ArenaHandle>,
        bill: Option<Bill>,
        caller: u32,
    ) -> Run<()> {
        let entry = self.entry(unit, caller);
        self.push_frame(entry, argc, closure, bill)
    }

    /// What entering a frame of `unit` called from Link `caller` reads of
    /// the program.
    #[inline(always)]
    pub(crate) fn entry(&self, unit: UnitId, caller: u32) -> Entry {
        let code = self.program.unit(unit);
        Entry {
            unit,
            link: self.link_of(unit, caller),
            locals: code.locals,
            call: code.call,
            boxes: !code.boxed.is_empty(),
            cost: self.frame_cost(unit),
        }
    }

    /// Pushes a frame of `entry` whose `argc` arguments are the top of
    /// the stack ([`Self::enter`]).
    #[inline(always)]
    pub(crate) fn push_frame(
        &mut self,
        entry: Entry,
        argc: usize,
        closure: Option<ArenaHandle>,
        bill: Option<Bill>,
    ) -> Run<()> {
        // A frame that is no call takes no unit; the call it makes is
        // checked where it is made.
        if self.depth_full() && entry.call {
            return Err(Flow::from(Stop::bound(
                self.exceeded(Bound::CallDepth, u64::from(self.call_depth)),
            )));
        }
        self.admit(entry.cost)?;
        self.prepare(entry.unit, entry.link)?;
        let base = self.cx.stack.len() - argc;
        let height = base + usize::from(entry.locals.max(argc as u16));
        if height > self.cx.stack.len() {
            self.cx.stack.resize(height, Value::Absent);
        }
        if entry.boxes {
            self.box_locals(entry.unit, base)?;
        }
        let depth = self.cx.depth() + u32::from(entry.call);
        self.cx.frames.push(Frame {
            unit: entry.unit,
            pc: 0,
            base: base as u32,
            closure,
            bill,
            link: entry.link,
            depth,
        });
        Ok(())
    }

    /// Boxes each local of a frame of `unit` at `base` that a closure
    /// shares into a cell (`K-CLO-001`).
    fn box_locals(&mut self, unit: UnitId, base: usize) -> Run<()> {
        let program = Arc::clone(&self.program);
        for &slot in &program.unit(unit).boxed {
            let at = base + usize::from(slot);
            let value = self.cx.stack[at];
            let cell = self.cell(value)?;
            self.cx.stack[at] = Value::Handle(cell);
        }
        Ok(())
    }

    /// Where the running locals start: the top frame's, or those of the
    /// body whose inline region it runs.
    #[inline]
    fn base(&self) -> usize {
        match self.cx.inline {
            Some(activation) => activation.base as usize,
            None => self.cx.frames.last().map_or(0, |frame| frame.base as usize),
        }
    }

    #[inline]
    fn pop(&mut self) -> Value {
        self.cx.stack.pop().unwrap_or(Value::Absent)
    }

    #[inline]
    fn push(&mut self, value: Value) {
        self.cx.stack.push(value);
    }

    // --- The dispatch loop.

    /// Runs the tasks until the run stops: it parks, the slice of `slice`
    /// units ends, or the run ends.
    pub(crate) fn run(&mut self, host: &mut dyn Host, slice: u64) -> Stop {
        if let Some(refused) = &self.unadmitted {
            return Stop::bound(refused.clone());
        }
        self.meter.slice(slice);
        self.sites = host.observes_sites();
        self.fresh = true;
        let mut next = if self.tasks.installed {
            Ok(None)
        } else {
            self.schedule(host)
        };
        loop {
            next = match next {
                Ok(Some(stop)) => return stop,
                Ok(None) => self.dispatch(host).map(Some),
                Err(Flow::Raise(value)) => self.raise(value).map(|()| None),
                Err(Flow::Cold { object, chunk }) => match self.reload(object, chunk) {
                    Ok(()) => self.resume(),
                    Err(stop) => {
                        self.restart = None;
                        Err(Flow::from(stop))
                    }
                },
                Err(Flow::Stop(stop)) => {
                    self.cancel();
                    match *stop {
                        // A task other than `main` ends with its raise
                        // (`K-TASK-008`).
                        Stop::Uncaught(value) if self.tasks.running != tasks::MAIN => {
                            self.end_task(host, Err(value))
                        }
                        stop => return stop,
                    }
                }
            };
        }
    }

    /// Runs the running context's frames. The running frame's unit, ops,
    /// base and `pc` are held in locals while the ops that keep the frame
    /// run here: the stack and local ops, jumps, inline scalar calls and
    /// fused runs. Every other op runs out of line in [`Self::operate`],
    /// with `pc` written back to the frame first and the frame read again
    /// after. The loop is kept small so that its state stays in registers.
    fn dispatch(&mut self, host: &mut dyn Host) -> Run<Stop> {
        let program = Arc::clone(&self.program);
        // The run's first op ends no step: the slice was just started.
        let mut fresh = std::mem::take(&mut self.fresh);
        // After a cold load the op it interrupted runs again, its start
        // already passed: no event, slice test or point a second time.
        let mut retrying = std::mem::take(&mut self.retrying);
        'frames: loop {
            let Some(frame) = self.cx.frames.last() else {
                return Err(fault("a context with no frame ran"));
            };
            let top = self.cx.frames.len() - 1;
            let (id, mut pc) = (frame.unit, frame.pc as usize);
            // An inline region runs over its body's locals.
            let mut base = match self.cx.inline {
                Some(activation) => activation.base as usize,
                None => frame.base as usize,
            };
            if retrying {
                pc -= 1;
            }
            let unit = program.unit(id);
            let ops = &unit.ops[..];
            // Writes the local `pc` back to the running frame.
            macro_rules! sync {
                ($pc:expr) => {
                    self.cx.frames[top].pc = $pc as u32
                };
            }
            // Returns `$e`'s error with the frame at the local `pc`.
            macro_rules! check {
                ($e:expr) => {
                    match $e {
                        Ok(value) => value,
                        Err(error) => {
                            sync!(pc);
                            return Err(error);
                        }
                    }
                };
            }
            // Counts authored call `$call`'s entry (`K-EFF-008`), the frame at `$at`
            // for an observing host, unless a cold load reruns its op (`$rerun`).
            macro_rules! enter {
                ($call:expr, $at:expr, $rerun:expr) => {
                    if let Some(entry) = $call.entry
                        && !$rerun
                    {
                        if self.sites {
                            sync!($at);
                        }
                        check!(self.enter_call(host, entry, $call));
                    }
                };
            }
            // The op a fused run's answer goes to takes `$value`.
            macro_rules! take {
                ($then:expr, $value:expr) => {
                    match $then {
                        Then::Push => self.push($value),
                        Then::SetLocal(n) => {
                            pc += 1;
                            self.cx.stack[base + usize::from(n)] = $value;
                        }
                        Then::Pop => pc += 1,
                        Then::Branch { to, looping } => match $value {
                            Value::Bool(true) => pc += 1,
                            Value::Bool(false) => pc = to as usize,
                            _ => {
                                sync!(pc + 1);
                                return Err(self.condition_error(looping));
                            }
                        },
                    }
                };
            }
            loop {
                let mut op = ops[pc];
                #[cfg(test)]
                if !retrying && unit.steps && unit.step[pc] {
                    crate::perf_witness::record(|work| work.vm_steps += 1);
                    self.chill();
                }
                // A block head bills nothing and starts no step
                // (validated): unobserved, it needs no exact step.
                if self.exact && (self.watch.is_some() || !matches!(op, Op::Block(_))) {
                    // A refusal, a limiter and a slice read the frame at
                    // the op.
                    sync!(pc);
                    if !retrying && self.exact_step(id, unit, pc, fresh)? {
                        return Ok(Stop::Slice);
                    }
                    // A fused run is a prepaid path's: here its first op
                    // runs alone.
                    if let Op::Fused(n) = op {
                        op = unit.fused[n as usize].first;
                    }
                }
                fresh = false;
                // A call op run again after a cold load has counted its entry.
                let rerun = std::mem::replace(&mut retrying, false);
                pc += 1;
                match op {
                    Op::Block(block) => {
                        // The block before ended: what it reserved is
                        // spent. A head bills nothing and starts no step
                        // (validated), so it needs no exact step of its
                        // own.
                        self.meter.leave();
                        let block = &unit.blocks[block as usize];
                        self.exact = self.watch.is_some() || !self.meter.enter(block.charge);
                    }
                    Op::Nop => {}
                    Op::Const(n) => self.push(self.konst(n)),
                    Op::Local(n) => self.push(self.cx.stack[base + usize::from(n)]),
                    Op::SetLocal(n) => {
                        let value = self.pop();
                        self.cx.stack[base + usize::from(n)] = value;
                    }
                    Op::Global(n) => {
                        let value = self.globals[usize::from(n)];
                        if let Value::Absent = value
                            && self.unbound(n)
                        {
                            sync!(pc);
                            return Err(self.unbound_variable(n));
                        }
                        self.push(value);
                    }
                    Op::SetGlobal(n) => {
                        if let Value::Absent = self.globals[usize::from(n)]
                            && self.unbound(n)
                        {
                            sync!(pc);
                            return Err(self.unbound_variable(n));
                        }
                        let value = self.pop();
                        self.globals[usize::from(n)] = value;
                    }
                    Op::Pop => {
                        self.pop();
                    }
                    Op::Jump(to) => pc = to as usize,
                    Op::JumpIfFalse(to) => match self.pop() {
                        Value::Bool(true) => {}
                        Value::Bool(false) => pc = to as usize,
                        _ => {
                            sync!(pc);
                            return Err(
                                self.error("type_error", "an `if` condition must be a bool")
                            );
                        }
                    },
                    Op::WhileFalse(to) => match self.pop() {
                        Value::Bool(true) => {}
                        Value::Bool(false) => pc = to as usize,
                        _ => {
                            sync!(pc);
                            return Err(
                                self.error("type_error", "a `while` condition must be a bool")
                            );
                        }
                    },
                    Op::LoopStart(n) => self.cx.stack[base + usize::from(n)] = Value::Int(0),
                    Op::Pass(n) => {
                        let at = base + usize::from(n);
                        if let Value::Int(count) = self.cx.stack[at] {
                            self.cx.stack[at] = Value::Int(count.saturating_add(1));
                        }
                    }
                    Op::Call(site) => {
                        let call = &unit.calls[site as usize];
                        enter!(call, pc, rerun);
                        if let Callee::Native { native, charged } = call.callee
                            && call.argc == 2
                            && call.checked.is_none()
                        {
                            let len = self.cx.stack.len();
                            let args = [self.cx.stack[len - 2], self.cx.stack[len - 1]];
                            if let Some(answer) = self.answer(&program, native, charged, &args) {
                                let (value, fee) = check!(answer);
                                self.cx.stack.truncate(len - 2);
                                self.push(value);
                                if let Some(units) = fee {
                                    sync!(pc);
                                    self.fee(units)?;
                                }
                                continue;
                            }
                        }
                        sync!(pc);
                        self.call(&program, unit, site)?;
                        // A native that answered leaves the frame running.
                        if matches!(call.callee, Callee::Native { .. })
                            && self.cx.frames.len() == top + 1
                        {
                            continue;
                        }
                        continue 'frames;
                    }
                    Op::Inline(n) => {
                        sync!(pc);
                        let site = &unit.inlines[n as usize];
                        let call = &unit.calls[site.call as usize];
                        enter!(call, pc, rerun);
                        if let Some(checked) = &call.checked {
                            self.check_arguments(checked, usize::from(call.argc))?;
                        }
                        if site.steps && self.meter.spent() {
                            self.enter_stepped(unit, n)?;
                            continue 'frames;
                        }
                        // A fold that holds completes the activation at
                        // the region's leave, and the caller runs on after
                        // the call; otherwise the region runs.
                        let (start, end) = site.folds;
                        let entered = if start < end {
                            let folds = &unit.folds[start as usize..end as usize];
                            self.inline_folded(unit, n, folds)?
                        } else {
                            Some(self.inline(unit, n)?)
                        };
                        if let Some(at) = entered {
                            base = at;
                            pc = site.entry as usize;
                        }
                    }
                    Op::Leave(n) => {
                        sync!(pc);
                        self.leave(unit, n)?;
                        base = self.cx.frames[top].base as usize;
                        pc = unit.inlines[n as usize].at as usize + 1;
                    }
                    Op::Deopt(to) => {
                        sync!(pc);
                        self.deopt(unit, to)?;
                        continue 'frames;
                    }
                    Op::Fused(n) => {
                        let fused = &unit.fused[n as usize];
                        if let Some(entry) = fused.entry
                            && !rerun
                        {
                            if self.sites {
                                sync!(fused.call as usize + 1);
                            }
                            check!(self.enter_call(host, entry, &unit.calls[fused.site as usize]));
                        }
                        let len = self.cx.stack.len();
                        let args = self.fused_args(fused, base, len);
                        pc = fused.call as usize + 1;
                        // Two word ints whose fee the block's reservation
                        // covers: the answer, as the native's ordinary
                        // answer gives it, billed and taken in place. An
                        // answer of `ints` is an int or a bool, of size
                        // two exactly when it is a nonzero int.
                        if let (Some(form), [Value::Int(a), Value::Int(b)]) = (&fused.words, args)
                            && let Some(value) = lash_kernel_doc::scalar::ints(form.operation, a, b)
                            && match form.fee {
                                Some(table) if !self.off => {
                                    let large = matches!(value, Value::Int(word) if word != 0);
                                    self.meter.prepay(
                                        table[usize::from(a != 0)
                                            | usize::from(b != 0) << 1
                                            | usize::from(large) << 2],
                                    )
                                }
                                _ => true,
                            }
                        {
                            self.cx.stack.truncate(len + usize::from(fused.pushes) - 2);
                            take!(fused.then, value);
                            continue;
                        }
                        // Another inline answer, its fee prepaid: taken in
                        // place as well.
                        let Callee::Native { native, charged } =
                            unit.calls[fused.site as usize].callee
                        else {
                            sync!(pc);
                            return Err(fault("a fused call of no native"));
                        };
                        if let Some(answer) = self.answer(&program, native, charged, &args) {
                            let (value, fee) = match answer {
                                Err(cold @ Flow::Cold { .. }) => {
                                    sync!(fused::rerun_from(fused));
                                    return Err(cold);
                                }
                                answer => check!(answer),
                            };
                            if fee.is_none_or(|units| self.meter.prepay(units)) {
                                self.cx.stack.truncate(len + usize::from(fused.pushes) - 2);
                                take!(fused.then, value);
                                continue;
                            }
                        }
                        match self.fused_slow(&program, unit, n, base, args, len, top)? {
                            Some(next) => pc = next,
                            None => continue 'frames,
                        }
                    }
                    Op::Return => {
                        sync!(pc);
                        let [value] = self.operands();
                        if let Some(stop) = self.ret(host, value, true)? {
                            return Ok(stop);
                        }
                        continue 'frames;
                    }
                    Op::CellGet(_) | Op::CellSet(_) | Op::CaptureGet(_) | Op::CaptureSet(_) => {
                        sync!(pc);
                        self.cell_op(op)?;
                    }
                    _ => {
                        sync!(pc);
                        match self.operate(host, &program, unit, op, rerun)? {
                            Some(stop) => return Ok(stop),
                            None => continue 'frames,
                        }
                    }
                }
            }
        }
    }

    /// An op's start on the exact path: whether the slice ends before it,
    /// and otherwise its fixed event billed and its points seen. The slice
    /// is tested after each step of any frame (`K-MACH-002`): at the next
    /// step's start, and where a stepped frame's last statement ends by
    /// falling off its body. A `return` statement's step runs on into its
    /// caller's continuation; dispatching it is no step.
    #[inline]
    fn exact_step(&mut self, id: UnitId, unit: &Unit, pc: usize, fresh: bool) -> Run<bool> {
        if unit.slices(pc) && !fresh && self.meter.spent() {
            return Ok(true);
        }
        self.bill(unit, pc)?;
        if self.watch.is_some() && !unit.points.is_empty() {
            self.see(id, unit, pc);
        }
        Ok(false)
    }

    /// Runs an op [`Self::dispatch`] does not, the running frame's `pc`
    /// past it. The frame may change. Gives the stop when the run stops.
    #[inline(never)]
    fn operate(
        &mut self,
        host: &mut dyn Host,
        program: &Program,
        unit: &Unit,
        op: Op,
        rerun: bool,
    ) -> Run<Option<Stop>> {
        match op {
            Op::Box(n) => {
                let at = self.base() + usize::from(n);
                let value = self.cx.stack[at];
                let cell = self.cell(value)?;
                self.cx.stack[at] = Value::Handle(cell);
            }
            Op::MakeTuple(n) => self.make_tuple(n as usize)?,
            // A built value's members stay on the stack, rooted, until it
            // is placed; a key is read before anything is placed.
            Op::MakeList(n) => {
                let values = self.top(n as usize).to_vec();
                self.place(n as usize, NewObject::List(values))?;
            }
            Op::MakeSet(n) => {
                let from = self.cx.stack.len() - n as usize;
                for at in from..self.cx.stack.len() {
                    let key = self.cx.stack[at];
                    self.key(&key)?;
                }
                let values = self.top(n as usize).to_vec();
                self.place(n as usize, NewObject::Set(values))?;
            }
            Op::MakeMap(n) => {
                let from = self.cx.stack.len() - 2 * n as usize;
                for at in (from..self.cx.stack.len()).step_by(2) {
                    let key = self.cx.stack[at];
                    self.key(&key)?;
                }
                let pairs: Vec<(Value, Value)> = self
                    .top(2 * n as usize)
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[key, value]| (key, value))
                    .collect();
                self.place(2 * n as usize, NewObject::Map(pairs))?;
            }
            Op::MakeRecord(shape) => {
                let shape = Arc::clone(&unit.records[shape as usize]);
                let pairs = shape
                    .iter()
                    .map(|field| self.konst(*field))
                    .zip(self.top(shape.len()).iter().copied())
                    .collect();
                self.place(shape.len(), NewObject::Record(pairs))?;
            }
            Op::MakeClosure(n) => {
                let site = &unit.closures[n as usize];
                let mut captures = Vec::with_capacity(site.captures.len());
                for capture in &site.captures {
                    captures.push(match capture {
                        crate::bytecode::program::Capture::Local(slot) => self.cell_at(*slot)?,
                        crate::bytecode::program::Capture::Capture(k) => self.capture(*k)?,
                    });
                }
                self.admit(crate::heap::meter::closure(captures.len()))?;
                let identity = match self.cx.frames.last() {
                    Some(frame) if frame.unit & LIB != 0 => {
                        let (made, link) = (frame.unit, frame.link);
                        self.identity(made, n, link)
                    }
                    _ => Arc::clone(&site.identity),
                };
                let closure = self.heap.closure(identity, captures);
                self.push(Value::Closure(closure));
            }
            // A member op reads its operands where they stand and pops
            // them once it is done: a cold read runs it again whole.
            Op::Index => {
                let [target, index] = self.operands();
                let value = self.index(target, index)?;
                self.popped(2);
                self.push(value);
            }
            Op::Field(field) => {
                let [target] = self.operands();
                let name = self.konst(field);
                let value = self.field(target, name)?;
                self.popped(1);
                self.push(value);
            }
            Op::SetIndex => {
                let [value, target, index] = self.operands();
                self.set_index(target, index, value)?;
                self.popped(3);
            }
            Op::SetIndexPlaceFirst => {
                let [target, index, value] = self.operands();
                self.set_index(target, index, value)?;
                self.popped(3);
            }
            Op::SetField(field) => {
                let [value, target] = self.operands();
                self.set_field(target, self.konst(field), value)?;
                self.popped(2);
            }
            Op::SetFieldPlaceFirst(field) => {
                let [target, value] = self.operands();
                self.set_field(target, self.konst(field), value)?;
                self.popped(2);
            }
            Op::RemoveIndex => {
                let [target, index] = self.operands();
                self.remove_index(target, index)?;
                self.popped(2);
            }
            Op::RemoveField(field) => {
                let [target] = self.operands();
                self.remove_field(target, self.konst(field))?;
                self.popped(1);
            }
            Op::Call(site) => self.call(program, unit, site)?,
            Op::Fall => {
                if let Some(stop) = self.ret(host, Value::Null, false)? {
                    return Ok(Some(stop));
                }
            }
            Op::Raise => {
                let value = self.pop();
                return Err(Flow::Raise(value));
            }
            Op::TryEnter(to) => {
                let frame = (self.cx.frames.len() - 1) as u32;
                let height = self.cx.stack.len() as u32;
                self.cx.handlers.push(Handler {
                    frame,
                    pc: to,
                    height,
                });
            }
            Op::TryExit => {
                self.cx.handlers.pop();
            }
            Op::ForOpen(n) => self.for_open(unit, n)?,
            Op::ForNext(n) => self.for_next(unit, n)?,
            Op::ProtoOpen(n) => self.proto_open(unit, n)?,
            Op::ProtoStep(n) => self.proto_step(unit, n)?,
            Op::ProtoGuard(n) => self.proto_guard(unit, n)?,
            Op::ProtoChecked(n) => self.proto_checked(unit, n)?,
            Op::ProtoPull(n) => self.proto_pull(unit, n)?,
            Op::ProtoMore(n) => self.proto_more(unit, n)?,
            Op::Action(n) => {
                self.meter.leave();
                self.exact = true;
                if let Some(stop) = self.act(host, unit, n, rerun)? {
                    return Ok(Some(stop));
                }
            }
            Op::Print => {
                let [value] = self.operands();
                let datum = self.copy_out(&value)?;
                self.popped(1);
                host.print(&datum);
            }
            Op::Finish => {
                let [value] = self.operands();
                let datum = self.copy_out(&value)?;
                self.popped(1);
                return Ok(Some(Stop::Finish(datum)));
            }
            Op::Fail => {
                let [value] = self.operands();
                let datum = self.copy_out(&value)?;
                self.popped(1);
                return Ok(Some(Stop::Fail(datum)));
            }
            Op::Unavailable(reason) => {
                let reason = match program.constant(reason) {
                    Constant::Text(text) => text.to_string(),
                    Constant::Immediate(Value::Text(text)) => text.as_str().to_owned(),
                    _ => String::from("a body the core cannot run"),
                };
                return Err(fault(reason));
            }
            Op::Block(_)
            | Op::Nop
            | Op::Const(_)
            | Op::Local(_)
            | Op::SetLocal(_)
            | Op::Global(_)
            | Op::SetGlobal(_)
            | Op::Pop
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::WhileFalse(_)
            | Op::LoopStart(_)
            | Op::Pass(_)
            | Op::Inline(_)
            | Op::Leave(_)
            | Op::Deopt(_)
            | Op::CellGet(_)
            | Op::CellSet(_)
            | Op::CaptureGet(_)
            | Op::CaptureSet(_)
            | Op::Return
            | Op::Fused(_) => return Err(fault("a dispatched op ran out of line")),
        }
        Ok(None)
    }

    /// Returns `value` from the running frame, billing the call's
    /// completion formula. Gives the stop when `main`'s bottom frame
    /// returned; another task's bottom frame ends the task.
    fn ret(&mut self, host: &mut dyn Host, value: Value, explicit: bool) -> Run<Option<Stop>> {
        self.meter.leave();
        self.exact = true;
        let bottom = self.cx.frames.len() == 1;
        // A frame above another that owes no completion copies nothing
        // out and bills nothing: it checks its result, leaves and hands
        // the value to its caller.
        if !bottom && let Some(Frame { bill: None, .. }) = self.cx.frames.last() {
            let unfit = self.unfit_result(value)?;
            let Some(frame) = self.cx.frames.pop() else {
                return Err(fault("a return with no frame"));
            };
            self.heap.release(self.frame_cost(frame.unit));
            self.drop_handlers();
            self.cx.stack.truncate(frame.base as usize);
            if let Some(unfit) = unfit {
                return Err(self.typed_error("entry_type_mismatch", &unfit));
            }
            self.push(value);
            return Ok(None);
        }
        let main = self.tasks.running == tasks::MAIN;
        // A return that leaves the run copies its value out where it
        // stands, so a raise of the copy is caught by the frame's own
        // handlers; falling off the end copies nothing (`K-FORM-015`).
        // Every read comes before anything commits: a cold read runs the
        // return again whole, its frame still in place.
        let copied = match (bottom && main, explicit) {
            (true, true) => Some(self.copied(&value)?),
            _ => None,
        };
        let unfit = match bottom {
            true => None,
            false => self.unfit_result(value)?,
        };
        let Some(bill) = self.cx.frames.last().map(|frame| frame.bill) else {
            return Err(fault("a return with no frame"));
        };
        let completion = self.completion(bill, Some(&value))?;
        let leaving = match (bottom && main, copied) {
            (true, Some(copied)) => Some(self.bill_copy(copied)?),
            (true, None) => Some(Datum::Null),
            (false, _) => None,
        };
        let Some(frame) = self.cx.frames.pop() else {
            return Err(fault("a return with no frame"));
        };
        if bottom && main {
            let base = frame.base as usize;
            let locals = usize::from(self.program.unit(frame.unit).locals);
            self.top = self
                .cx
                .stack
                .get(base..base + locals)
                .map(<[Value]>::to_vec);
        }
        self.heap.release(self.frame_cost(frame.unit));
        self.drop_handlers();
        self.settle(completion, frame.unit)?;
        self.cx.stack.truncate(frame.base as usize);
        if let Some(datum) = leaving {
            return Ok(Some(Stop::Returned(datum)));
        }
        if bottom {
            return self.end_task(host, Ok(value));
        }
        if let Some(unfit) = unfit {
            return Err(self.typed_error("entry_type_mismatch", &unfit));
        }
        self.push(value);
        Ok(None)
    }

    #[inline]
    fn jump(&mut self, to: u32) {
        if let Some(frame) = self.cx.frames.last_mut() {
            frame.pc = to;
        }
    }

    /// The top `n` operands, in order.
    fn take(&mut self, n: usize) -> Vec<Value> {
        let from = self.cx.stack.len() - n;
        self.cx.stack.split_off(from)
    }

    /// The top `n` operands, in order, left in place.
    #[inline]
    fn top(&self, n: usize) -> &[Value] {
        &self.cx.stack[self.cx.stack.len() - n..]
    }

    /// The top `N` operands, in order, left in place until the op that
    /// reads them has done every read.
    #[inline]
    fn operands<const N: usize>(&self) -> [Value; N] {
        let mut operands = [Value::Absent; N];
        operands.copy_from_slice(self.top(N));
        operands
    }

    /// Pops the `n` operands an op has finished with.
    #[inline]
    fn popped(&mut self, n: usize) {
        let len = self.cx.stack.len();
        self.cx.stack.truncate(len - n);
    }

    /// Places `object`, built of the top `n` operands, in their place: they
    /// stay rooted across the admission's collection.
    fn place(&mut self, n: usize, object: NewObject) -> Run<()> {
        let value = self.alloc(object)?;
        self.popped(n);
        self.push(value);
        Ok(())
    }

    /// Refuses a value that is no legal key (`K-KEY-002`).
    fn key(&mut self, value: &Value) -> Run<()> {
        match self.heap.arena().key(value) {
            Ok(_) => Ok(()),
            Err(error) => Err(self.native(error)),
        }
    }

    /// A tuple of the top `n` operands, refused past the value depth
    /// (`K-VAL-034`).
    fn make_tuple(&mut self, n: usize) -> Run<()> {
        let values = self.top(n).to_vec();
        let arena = self.heap.arena();
        let depth = values
            .iter()
            .map(|value| arena.summary_of(value).depth)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let limit = self.value_depth;
        if depth as usize > limit {
            return Err(self.error(
                "too_deep",
                &format!("a value exceeds the configured {limit}-level value depth"),
            ));
        }
        self.place(n, NewObject::Tuple(values))
    }
}
