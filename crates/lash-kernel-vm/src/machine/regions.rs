//! Loop regions (`docs/design/kernel-regions.md` §2, §3): a `while`, or a
//! `for` over a list or a tuple, whose plan lowering formed runs whole
//! iterations inside one step, from the plan's compiled form, while each
//! fits the charge headroom and every guard holds. The body's `if` arms
//! and `continue`s run inside the region too. The reference interpreter
//! stays the oracle and the fallback: a region exits at the last statement
//! boundary it committed, with the exact control the reference would hold
//! there, and the reference runs on from it. A barrier, a `break` or a
//! `return` is such an exit: the reference runs it.
//!
//! An allocating plan (§5, R5) admits a nursery at its entry: room for
//! what its statements build and grow, which the census admits before the
//! region runs, so that no admission inside it refuses (§3.5). Each of its
//! statements is admitted against the bound and the slice on its own, as
//! its formulas measure what they are given. At each statement's end it
//! runs the statement's kill set: what the statement made is held by a
//! root or garbage. When the arena's collection is due, the region exits
//! at that boundary, where the reference's step collects. A write an
//! operator's heap paths make to a guest object is held until its `do`
//! is admitted and applied as it commits: a statement that misses leaves
//! nothing written, so the reference's run of it writes once.
//!
//! Session bindings are region slots between boundaries (§3.6): a region
//! takes each binding its plan names at its resident value on entry, reads
//! and writes it there, admitting a write's holding as the reference's
//! session write does, and publishes what it wrote to the cell's session
//! at its exit, in the order it wrote them, before any boundary.
//!
//! With charge Off, a region of charged code runs paced: it measures no
//! formula and records nothing, and counts the run's pace as the
//! reference does, by its statements' fixed units, which lowering under
//! Off leaves at their statement, action-atom and loop-test units. The
//! slice's end then stops it where it stops the reference.

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Weight;
use lash_kernel_state::paged::TableCursor;

use super::islands::{Level, RegionGuests};
use super::loops::{Cursor, Guard, Header, counted};
use super::ops::Flow;
use super::{Control, Eval, Frame, Halt, KernelMachine, SlotState, fault};
use crate::compile::{
    Accumulator, Arms, BARREN, Commit, CompiledBody, CompiledHead, CompiledLoop, CompiledStep, Cx,
    Event, Executable, GuestCall, Headroom, Hook, Kept, LoopPlan, Made, Meter, NESTING, Node, Ran,
    RegionSlot, Role, SessionSlot, Slots, Taken, Typed, UNPRODUCTIVE, scalar, store,
};
use crate::observe::ChargeTape;

/// The least capacity a region may grow by while it runs, admitted once at
/// its entry against the capacity meter's headroom (§3.5, FIG-6791): the
/// nursery an allocating plan's statements build and grow in. It is one
/// capacity chunk.
pub(super) const NURSERY: u64 = super::limits::CAPACITY_CHUNK;

/// The most capacity a region may grow by while it runs, where the bound
/// leaves it: what a region builds and drops is freed only once it stops,
/// so the room bounds the garbage it keeps, and a loop whose garbage fills
/// it stops that rarely.
const REGION_ROOM: u64 = 16 * NURSERY;

#[cfg(test)]
mod admission_laws;

/// The typed runs that may miss in one region entry before it leaves the
/// rest of its statements to run one by one: a run whose guards or kernels
/// keep missing costs its own work and the statements' both. Which code
/// runs the statements changes, never what they do, and nothing of it
/// outlives the entry.
const TYPED_MISSES: u32 = 8;

/// The session slots a region holds without allocating.
const SESSIONS_INLINE: usize = 8;

/// How a pass through a block of an iteration went.
#[derive(Clone, Copy)]
enum Went {
    /// It ran to the block's end.
    Through,
    /// A `continue` ended the iteration.
    Continued,
    /// It stopped before a statement, in the blocks the chain holds,
    /// outermost first.
    Stopped,
    /// It stopped inside a statement whose guest call ran, which the chain
    /// holds as started: the iteration's stack holds the call, and
    /// [`Cx::called`] what it came to.
    Called,
}

/// A region's exit inside a statement standing in its guest call `call`:
/// the registers it holds across the call, and what the call came to.
struct Called<'e> {
    call: &'e GuestCall,
    regs: Vec<(u32, Value)>,
    value: Option<Value>,
    levels: Vec<Level>,
}

/// An allocating plan's admission of each statement: the bound, the
/// slice's end, and the next test's unit an iteration still charges.
#[derive(Clone, Copy)]
struct Admit {
    bound: u64,
    slice_end: u64,
    loop_test: u64,
}

/// One region's run over its frame: what it charges, records and holds.
struct Run<'a, 'c, const INVOKES: bool, const ALLOC: bool> {
    frame: &'a mut Frame,
    cx: Cx<'c>,
    tape: Option<&'a ChargeTape>,
    /// An allocating plan's admission (`ALLOC`); a scalar plan admits
    /// whole iterations.
    admit: Admit,
    /// The reference's step would end here: a statement spent the slice,
    /// or the arena's collection is due. The region stops at this
    /// boundary, as the reference's step ends at it.
    due: bool,
    /// Where an iteration stopped: each block it stands in, outermost
    /// first, at the statement it runs next.
    chain: Vec<Control>,
    /// The last statement the region tried was one it runs, not one it
    /// leaves: where it stopped there, a guard missed.
    missed: bool,
    /// The session writes the region made.
    writes: u64,
    /// Where it stopped, an operator left its operand kind to the
    /// reference ([`Cx::declined`]).
    declined: bool,
    /// Where it stopped, the iteration had committed a body statement.
    progressed: bool,
    /// A `for`'s step missed in its `item` or `check` hook, before its
    /// iteration's first body statement.
    head_missed: bool,
    /// Each accumulator slot whose long text its own statement made while
    /// the region ran, which no older version observes (FIG-6795).
    texts: Vec<(u32, ArenaHandle)>,
    /// The charge program a typed run holds until it completes.
    pending: Vec<Weight>,
    /// The typed runs that completed.
    blocks: u64,
    /// The typed runs that missed.
    misses: u32,
}

/// A typed run reads its frame's variables and writes back what it
/// assigned.
impl Slots for Frame {
    #[inline(always)]
    fn slots(&self) -> &[SlotState] {
        self.slot_states()
    }

    #[inline(always)]
    fn assign(&mut self, at: u32, value: Value) {
        self.set_slot(at, SlotState::Value(value));
    }
}

impl KernelMachine {
    /// Runs whole iterations of the innermost loop of `task` as its loop
    /// region `region`, entered where the reference has charged its test's
    /// unit and runs the rest of its test or steps its cursor. `None`: the
    /// loop stands there again, for the reference. `Some(Step)`: an
    /// iteration stopped before a body statement, which the reference runs
    /// from.
    pub(super) fn loop_region(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::Host,
        exe: &Executable,
        region: u32,
    ) -> Eval<Option<Flow>> {
        let plan = &exe.regions[region as usize];
        // A plan whose entries keep missing in this machine is the
        // reference's.
        if self
            .unproductive
            .get(region as usize)
            .is_some_and(|weight| *weight >= UNPRODUCTIVE)
        {
            return Ok(None);
        }
        let (flow, called) = match (plan.calls.is_empty(), plan.allocates) {
            (true, false) => self.run_loop_region::<false, false>(task, &*host, exe, region),
            (false, false) => self.run_loop_region::<true, false>(task, &*host, exe, region),
            (true, true) => self.run_loop_region::<false, true>(task, &*host, exe, region),
            (false, true) => self.run_loop_region::<true, true>(task, &*host, exe, region),
        }?;
        // An exit inside a statement standing in its guest call: the
        // reference runs the statement on, or the island's frames stand
        // above the loop's.
        if let Some(called) = called {
            self.guest_exit(
                host,
                (task, exe),
                called.call,
                called.regs,
                (called.value, called.levels),
            )?;
        }
        Ok(flow)
    }

    /// Specialize occurrence bookkeeping away for regions with no invokes,
    /// and statement admission away for scalar plans.
    fn run_loop_region<'e, const INVOKES: bool, const ALLOC: bool>(
        &mut self,
        task: TaskId,
        host: &dyn crate::Host,
        exe: &'e Executable,
        region: u32,
    ) -> Result<(Option<Flow>, Option<Called<'e>>), Halt> {
        // The environment guard: the loop's code is charged or paced, and
        // nothing observes the run but a charge tape, which the region
        // records.
        if !(self.charging || self.paced)
            || host.observes_sites()
            || !self.ran.is_empty()
            || self.steps.verify
            || self
                .observer
                .as_deref()
                .is_some_and(|observer| !observer.only_charges())
        {
            return Ok((None, None));
        }
        #[cfg(test)]
        if self.boundary.is_some() {
            return Ok((None, None));
        }
        let plan = &exe.regions[region as usize];
        // The capacity headroom guard: the nursery fits the bound, so no
        // verdict inside refuses.
        let room = self.capacity_headroom();
        if room < NURSERY {
            return Ok((None, None));
        }
        let room = room.min(REGION_ROOM);
        let memory = self.bounds.memory_meter != crate::MemoryMeter::Off;
        // The depth guard: every site's depth check passes in this frame,
        // whose depth the region does not change.
        let frames = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .len();
        let depth = (frames
            + self.plans as usize
            + usize::from(task != TaskId::MAIN || !self.session_cell)) as u64;
        if let Some(need) = plan.depth
            && depth + u64::from(need) > u64::from(self.bounds.call_depth)
        {
            return Ok((None, None));
        }
        let Some(compiled) = plan.compiled(exe) else {
            return Ok((None, None));
        };
        // The session slots: each binding the plan names, at its resident
        // value (§3.6), held inline for a plan that names few.
        let mut inline = [SessionSlot::default(); SESSIONS_INLINE];
        let mut spilled = Vec::new();
        let sessions: &mut [SessionSlot] = match inline.get_mut(..plan.sessions.len()) {
            Some(inline) => inline,
            None => {
                spilled.resize(plan.sessions.len(), SessionSlot::default());
                &mut spilled
            }
        };
        for (slot, name) in sessions.iter_mut().zip(&plan.sessions) {
            slot.value = self.session.resident(&name.key);
        }
        // A paced run has no charge to record and no bound: its pace only
        // meets the slice's end.
        // The work it commits is recorded either way.
        let charging = self.charging;
        let recorder = self
            .observer
            .as_deref()
            .and_then(|observer| observer.charges.as_ref());
        let tape = recorder.filter(|_| charging);
        let bound = if charging {
            self.bounds.charge_limit
        } else {
            u64::MAX
        };
        let slice_end = self.slice_end;
        let mut guests = RegionGuests {
            exe,
            session: &mut self.session,
            names: &plan.sessions,
            scratch: &mut self.islands,
            tape,
            recorder,
            charging,
            bound,
            slice_end,
            observed: self.observer.is_some(),
            limits: self.limits,
            memory: memory.then_some(self.bounds.memory),
            floor: self.capacity_floor,
            base: depth,
            call_depth: u64::from(self.bounds.call_depth),
            levels: Vec::new(),
            halt: None,
        };
        let mut arena = self.arena.borrow_mut();
        let ceiling = if memory {
            arena.held().saturating_add(room)
        } else {
            u64::MAX
        };
        let state = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let frame = state.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        // What a `for` walks, from where; the cursor of a `while` is none.
        let mut walk = match (&compiled.head, frame.control.last()) {
            (CompiledHead::While { .. }, Some(Control::While { .. })) => None,
            (
                CompiledHead::For { .. },
                Some(Control::For {
                    cursor:
                        cursor @ (Cursor::List(items, position) | Cursor::Tuple(items, position)),
                    header: Header::Body,
                    guard: None,
                    ..
                }),
            ) => Some((
                cursor.walked(),
                Walking::Items {
                    items: *items,
                    position: *position,
                },
            )),
            // A guarded table needs the plan's guarded step.
            (
                CompiledHead::For {
                    guard: compiled, ..
                },
                Some(Control::For {
                    cursor: cursor @ (Cursor::Map(table, last) | Cursor::Set(table, last)),
                    header: Header::Body,
                    guard,
                    ..
                }),
            ) if guard.is_none() || compiled.is_some() => Some((
                cursor.walked(),
                Walking::Keys {
                    table: *table,
                    last: *last,
                    guard: guard.as_deref().copied(),
                },
            )),
            _ => return Ok((None, None)),
        };
        let mut run = Run::<INVOKES, ALLOC> {
            frame,
            cx: Cx {
                meter: Meter::new(tape.is_some()),
                charging,
                args: super::registers::Arguments::new(),
                exe,
                arena: &mut arena,
                sites: &mut self.sites,
                sessions: &mut *sessions,
                limits: self.limits,
                memory,
                ceiling,
                transient: 0,
                fault: None,
                lets: Vec::new(),
                declined: false,
                held: None,
                charged: self.charged,
                counters: INVOKES.then_some(&mut state.counters),
                guests: Some(&mut guests),
                called: None,
                accumulator: None,
            },
            tape,
            admit: Admit {
                bound,
                slice_end,
                loop_test: plan.loop_test,
            },
            due: false,
            chain: Vec::new(),
            missed: false,
            writes: 0,
            declined: false,
            progressed: false,
            head_missed: false,
            texts: Vec::new(),
            pending: Vec::new(),
            blocks: 0,
            misses: 0,
        };
        let batch = run.batch(plan, compiled, walk.as_mut(), bound, slice_end);
        // The region's charge is the machine's, however its batch ended.
        self.charged = run.cx.charged;
        let (started, stopped, called) = batch?;
        let Run {
            frame,
            chain,
            cx,
            missed,
            writes,
            declined,
            progressed,
            head_missed,
            blocks,
            ..
        } = run;
        let Cx { fault: refused, .. } = cx;
        let RegionGuests { halt, levels, .. } = guests;
        if let Some(halt) = halt {
            return Err(halt);
        }
        // An entry that missed a guard before an iteration ended was
        // unproductive; one that ended an iteration was not. One that
        // committed a body statement first, before a key the loop has not
        // yet written, for one, weighs less. A kind an operator leaves to
        // the reference stops the entry as a barrier does: unproductive
        // only where nothing ran before it. A `for` whose hook misses at a
        // step stops before the iteration's first body statement.
        if (stopped || head_missed)
            && started <= 1
            && (missed || head_missed || declined && !progressed)
        {
            let at = region as usize;
            if self.unproductive.len() <= at {
                self.unproductive.resize(exe.regions.len().max(at + 1), 0);
            }
            self.unproductive[at] += if progressed { 1 } else { BARREN };
        } else if started > 1
            && let Some(weight) = self.unproductive.get_mut(region as usize)
        {
            *weight = 0;
        }
        if let Some(refused) = refused {
            return Err(Halt::from(refused));
        }
        if let Some(recorder) = recorder {
            recorder.iterated(started - u64::from(stopped));
            recorder.typed(blocks);
        }
        // The exit publishes the session writes before any boundary, in
        // the order the region made them, as the reference's writes left
        // the bindings.
        if writes != 0 {
            let mut written: Vec<(u64, usize)> = sessions
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.written != 0)
                .map(|(at, slot)| (slot.written, at))
                .collect();
            written.sort_unstable();
            for (_, at) in written {
                let (name, value) = (&plan.sessions[at], sessions[at].value);
                if !value.is_some_and(|value| self.session.publish(&name.key, value)) {
                    return Err(fault(
                        "a region wrote a session binding the cell does not hold",
                    ));
                }
            }
        }
        // The exit materializes the control the reference holds there:
        // the loop's iterations started, a `for`'s cursor past the
        // elements it stepped, and the blocks standing at the statement the
        // region did not run.
        match (frame.control.last_mut(), walk) {
            (Some(Control::While { started: total, .. }), None) => *total += started,
            (
                Some(Control::For {
                    started: total,
                    cursor: Cursor::List(_, at) | Cursor::Tuple(_, at),
                    ..
                }),
                Some((_, Walking::Items { position, .. })),
            ) => {
                *total += started;
                *at = position;
            }
            (
                Some(Control::For {
                    started: total,
                    cursor: Cursor::Map(_, at) | Cursor::Set(_, at),
                    guard: held,
                    ..
                }),
                Some((_, Walking::Keys { last, guard, .. })),
            ) => {
                *total += started;
                *at = last;
                if let (Some(held), Some(guard)) = (held, guard) {
                    **held = guard;
                }
            }
            _ => return Err(fault("a loop region left its loop")),
        }
        if !stopped {
            return Ok((None, None));
        }
        frame.control.extend(chain);
        // A statement standing in its guest call holds the registers its
        // continuation reads, read again from where it read them.
        let called = match called {
            Some((call, value)) => {
                let mut regs = Vec::with_capacity(call.holds.len() + 1);
                for (reg, held) in &call.holds {
                    let held = match held {
                        Kept::Slot(RegionSlot::Frame(at)) => match frame.slot(*at) {
                            SlotState::Value(value) => Some(*value),
                            _ => None,
                        },
                        Kept::Slot(RegionSlot::Session(at)) => {
                            sessions.get(*at as usize).and_then(|slot| slot.value)
                        }
                        Kept::Value(value) => Some(*value),
                    };
                    regs.push((*reg, held.ok_or_else(|| fault("a held read is gone"))?));
                }
                Some(Called {
                    call,
                    regs,
                    value,
                    levels,
                })
            }
            None => None,
        };
        Ok((Some(Flow::Step), called))
    }
}

/// Where a `for` region's own cursor stands in what it walks: past the
/// elements its committed steps gave.
enum Walking {
    /// A list's or a tuple's items, from `position`.
    Items { items: ArenaHandle, position: usize },
    /// A map's or a set's keys, after the entry `last` visited, with the
    /// protocol's guard when it checks (`K-ITER-005`).
    Keys {
        table: ArenaHandle,
        last: Option<TableCursor>,
        guard: Option<Guard>,
    },
}

/// The blocks an iteration stands in, the loop's body first: each at the
/// statement it runs next, an arm's enclosing block past its `if`.
struct Stack<'p> {
    bodies: [&'p CompiledBody; NESTING + 1],
    nexts: [usize; NESTING + 1],
    /// The guest call of the statement the iteration stopped in.
    call: Option<&'p GuestCall>,
}

impl<const INVOKES: bool, const ALLOC: bool> Run<'_, '_, INVOKES, ALLOC> {
    /// Runs whole iterations while each fits, a `for`'s cursor in `walk`
    /// stepped past the elements its iterations took: the iterations it
    /// started, whether one stopped before or in a body statement, and
    /// the guest call it stands in and what that came to, when in one.
    #[expect(
        clippy::type_complexity,
        reason = "a batch ends with its iterations and where it stopped"
    )]
    fn batch<'p>(
        &mut self,
        plan: &LoopPlan,
        compiled: &'p CompiledLoop,
        mut walk: Option<&mut (Option<Value>, Walking)>,
        bound: u64,
        slice_end: u64,
    ) -> Result<(u64, bool, Option<(&'p GuestCall, Option<Value>)>), Halt> {
        let mut stack = Stack {
            bodies: [&compiled.body; NESTING + 1],
            nexts: [0; NESTING + 1],
            call: None,
        };
        let mut started = 0_u64;
        // A whole loop's typed run repeats the iterations it completes;
        // the one it leaves runs on its own before it repeats again.
        let whole = compiled
            .iteration
            .as_ref()
            .filter(|iteration| iteration.whole);
        let mut repeated = false;
        loop {
            // The charge headroom guard of a scalar plan: no event of the
            // iteration passes the bound, and no step boundary inside
            // reaches the slice's end. An allocating plan admits each
            // statement against the bound, and stops where the reference's
            // step would end.
            if !ALLOC {
                match self.cx.charged.checked_add(plan.iteration_max) {
                    Some(end) if end <= bound && end < slice_end => {}
                    _ => break,
                }
                if let Some(whole) = whole
                    && !std::mem::replace(&mut repeated, true)
                {
                    started += self.repeat(whole, plan, bound, slice_end);
                    continue;
                }
                repeated = false;
            }
            self.cx.meter.reset();
            // The body statement the iteration runs from.
            let mut from = 0;
            match (&compiled.head, walk.as_deref_mut()) {
                (CompiledHead::While { test, commit }, _) => {
                    // A test and a typed run that complete are the
                    // iteration's first statements.
                    if !ALLOC
                        && let Some(iteration) = &compiled.iteration
                        && self.typed(iteration)
                    {
                        from = iteration.covers;
                    } else {
                        if !self.ahead(commit)
                            || !matches!(
                                test(self.frame.slot_states(), &mut self.cx),
                                Some(Value::Bool(true))
                            )
                            || !self.fits(commit)
                        {
                            break;
                        }
                        self.commit(commit)?;
                    }
                }
                (
                    CompiledHead::For {
                        hook,
                        guard,
                        commit,
                        binding,
                    },
                    Some((walked, walking)),
                ) => {
                    let walked = *walked;
                    match walking {
                        // The cursor's next item, which a cold chunk or the
                        // end leaves to the reference.
                        Walking::Items { items, position } => {
                            if !self.ahead(commit) {
                                break;
                            }
                            let Ok(Some(raw)) = self.cx.arena.item(*items, *position as u64) else {
                                break;
                            };
                            if !self.element(hook.as_ref(), walked, raw, commit, *binding)? {
                                break;
                            }
                            *position += 1;
                        }
                        // The table's next key; a guarded table's step runs
                        // its protocol's `check` first, which gives the items
                        // the loop may still give.
                        Walking::Keys {
                            table,
                            last,
                            guard: held,
                        } => {
                            let (check, commit) = match (&*held, guard) {
                                (Some(held), Some((check, commit))) => {
                                    (Some((check, *held)), commit)
                                }
                                (Some(_), None) => break,
                                (None, _) => (None, commit),
                            };
                            if !self.ahead(commit) {
                                break;
                            }
                            let Ok(Some((sequence, raw))) =
                                self.cx.arena.table_next(*table, last.map(|last| last.0))
                            else {
                                break;
                            };
                            let mut remaining = None;
                            if let Some((check, guard)) = check {
                                let args = [
                                    walked.unwrap_or(Value::Absent),
                                    counted(guard.size),
                                    counted(guard.remaining),
                                    Value::Bool(true),
                                ];
                                match check(args, &mut self.cx) {
                                    Some(Value::Int(word)) if word >= 0 => {
                                        remaining = u64::try_from(word).ok();
                                    }
                                    _ => {
                                        self.head_missed = true;
                                        break;
                                    }
                                }
                            }
                            if !self.element(hook.as_ref(), walked, raw, commit, *binding)? {
                                break;
                            }
                            *last = Some(TableCursor(sequence));
                            if let (Some(held), Some(remaining)) = (held, remaining) {
                                held.remaining = remaining;
                            }
                        }
                    }
                }
                (CompiledHead::For { .. }, None) => {
                    return Err(fault("a `for` region walks nothing"));
                }
            }
            self.ended();
            started += 1;
            match self.iterate(&mut stack, from)? {
                Went::Stopped => return Ok((started, true, None)),
                Went::Called => {
                    let called = stack.call.take().zip(self.cx.called.take());
                    if called.is_none() {
                        return Err(fault("a region stopped in a guest call it did not make"));
                    }
                    return Ok((started, true, called));
                }
                Went::Through | Went::Continued => {}
            }
            // The next test's unit, which the reference charges first.
            self.cx.charged += plan.loop_test;
            if let Some(tape) = self.tape
                && plan.loop_test != 0
            {
                tape.push(Weight::Atomic(plan.loop_test));
            }
        }
        if self.cx.fault.is_some() {
            return Err(fault("a region's fault ended its batch"));
        }
        Ok((started, false, None))
    }

    /// Runs one pass through the loop's body, `stack`'s first block, from
    /// its statement `from`, entering the `if` arms it takes.
    #[inline(always)]
    fn iterate(&mut self, stack: &mut Stack<'_>, from: usize) -> Result<Went, Halt> {
        let Stack {
            bodies,
            nexts,
            call,
        } = stack;
        nexts[0] = from;
        let mut depth = 0;
        loop {
            let body = bodies[depth];
            let index = nexts[depth];
            let Some(step) = body.steps.get(index) else {
                // The loop's body ends in a step of its own (`TO_END`): a
                // step that ended at its last statement leaves it standing
                // at its end. An arm's last statement leaves the arm.
                if ALLOC && depth == 0 && self.due {
                    return Ok(self.stop(&bodies[..1], &[], index));
                }
                self.end(body);
                if depth == 0 {
                    return Ok(Went::Through);
                }
                depth -= 1;
                continue;
            };
            nexts[depth] = index + 1;
            // A typed run that completes runs its statements; one that
            // misses leaves them to run one by one.
            let step = match step {
                CompiledStep::Typed(typed) if !ALLOC => {
                    let (run, step) = &**typed;
                    if self.typed(run) {
                        nexts[depth] = index + run.covers;
                        continue;
                    }
                    step
                }
                step => step,
            };
            self.cx.meter.reset();
            let tried = !(ALLOC && (self.due || !self.room(step)));
            let ran = if !tried {
                false
            } else {
                match step {
                    CompiledStep::Write {
                        value,
                        at,
                        commit,
                        accumulates: false,
                    } => {
                        let value = value(self.frame.slot_states(), &mut self.cx);
                        self.write(value, *at, commit)?
                    }
                    CompiledStep::Write {
                        value,
                        at,
                        commit,
                        accumulates: true,
                    } => self.accumulate(value, *at, commit)?,
                    CompiledStep::Bind { value, at, commit } => {
                        let value = value(self.frame.slot_states(), &mut self.cx);
                        self.bind(value, *at, commit)?
                    }
                    CompiledStep::Session { value, at, commit } => {
                        let value = value(self.frame.slot_states(), &mut self.cx);
                        self.write_session(value, *at, commit)?
                    }
                    CompiledStep::If {
                        condition,
                        commit,
                        then,
                        otherwise,
                    } => {
                        let arm = match condition(self.frame.slot_states(), &mut self.cx) {
                            Some(Value::Bool(true)) => Some(then),
                            Some(Value::Bool(false)) => Some(otherwise),
                            _ => None,
                        };
                        match arm {
                            Some(arm) if self.fits(commit) => {
                                self.commit(commit)?;
                                self.ended();
                                // An empty arm ends as soon as it begins:
                                // the reference enters no block.
                                if !arm.steps.is_empty() {
                                    depth += 1;
                                    bodies[depth] = arm;
                                    nexts[depth] = 0;
                                }
                                continue;
                            }
                            _ => false,
                        }
                    }
                    CompiledStep::Continue(commit) => {
                        if self.fits(commit) {
                            self.commit(commit)?;
                            self.ended();
                            for body in bodies[..=depth].iter().rev() {
                                self.end(body);
                            }
                            return Ok(Went::Continued);
                        }
                        false
                    }
                    CompiledStep::Store {
                        operands,
                        place,
                        commit,
                    } => {
                        let mut values = [Value::Absent; 3];
                        let mut evaluated = true;
                        for (role, node) in operands.iter() {
                            match node(self.frame.slot_states(), &mut self.cx) {
                                Some(value) => values[*role as usize] = value,
                                None => {
                                    evaluated = false;
                                    break;
                                }
                            }
                        }
                        let [target, index, value] = values;
                        evaluated
                            && self.fits(commit)
                            && store(&mut self.cx, place, target, index, value).is_some()
                            && {
                                self.commit(commit)?;
                                true
                            }
                    }
                    // A write its operator held is applied once the
                    // statement is admitted, as part of its commit.
                    CompiledStep::Discard { value, commit } => {
                        value(self.frame.slot_states(), &mut self.cx).is_some()
                            && self.fits(commit)
                            && self.cx.commit_write()
                            && {
                                self.commit(commit)?;
                                true
                            }
                    }
                    CompiledStep::Typed(_) | CompiledStep::Leave => false,
                }
            };
            if !ran {
                return self.stopped(
                    (&bodies[..=depth], &nexts[..depth], index),
                    step,
                    tried,
                    call,
                );
            }
            self.ended();
        }
    }

    /// Runs typed run `run` over the frame's variables: whether it
    /// completed, its charge committed and, with a charge tape attached,
    /// its statements' programs recorded. A scalar plan admitted the
    /// iteration whole, so nothing in the run passes the bound or the
    /// slice's end.
    #[inline(always)]
    fn typed(&mut self, run: &Typed) -> bool {
        if self.misses >= TYPED_MISSES {
            return false;
        }
        let ran = match self.tape {
            None => run.run::<false>(&mut *self.frame, &mut self.pending),
            Some(_) => run.run::<true>(&mut *self.frame, &mut self.pending),
        };
        let units = match ran {
            Ran::Done(units) => units,
            Ran::Ended => return false,
            Ran::Missed => {
                self.misses += 1;
                return false;
            }
        };
        self.cx.charged += units;
        if let Some(tape) = self.tape {
            for weight in self.pending.drain(..) {
                tape.push(weight);
            }
        }
        self.blocks += 1;
        true
    }

    /// Repeats whole loop `run`'s iterations while the bound and the
    /// slice's end admit each, as the batch admits them: the iterations it
    /// completed, their charge committed and, with a charge tape attached,
    /// their programs recorded.
    #[inline(always)]
    fn repeat(&mut self, run: &Typed, plan: &LoopPlan, bound: u64, slice_end: u64) -> u64 {
        if self.misses >= TYPED_MISSES {
            return 0;
        }
        let headroom = Headroom {
            charged: self.cx.charged,
            bound,
            slice_end,
            iteration_max: plan.iteration_max,
            loop_test: plan.loop_test,
        };
        let (iterations, units, ended) = match self.tape {
            None => run.repeat::<false>(&mut *self.frame, &mut self.pending, headroom),
            Some(_) => run.repeat::<true>(&mut *self.frame, &mut self.pending, headroom),
        };
        self.misses += u32::from(ended == Ran::Missed);
        self.cx.charged += units;
        if let Some(tape) = self.tape {
            for weight in self.pending.drain(..) {
                tape.push(weight);
            }
        }
        self.blocks += iterations;
        iterations
    }

    /// Assigns `value`, the statement's, to the slot at `at`, which the
    /// capacity meter counted with its frame.
    #[inline(always)]
    fn write(&mut self, value: Option<Value>, at: u32, commit: &Commit) -> Result<bool, Halt> {
        let Some(value) = value else {
            return Ok(false);
        };
        if !matches!(self.frame.slot(at), SlotState::Value(_)) || !self.fits(commit) {
            return Ok(false);
        }
        self.frame.set_slot(at, SlotState::Value(value));
        self.commit(commit)?;
        Ok(true)
    }

    /// Runs linear text accumulator `value`, a write of the slot at `at`
    /// (FIG-6795). Where the slot holds a long text this statement made
    /// that no member holds and no projection saw, its concatenation may
    /// take that text's storage. A statement that misses after it did
    /// gives the storage back, so the reference runs it over the text it
    /// read. One that writes the text its concatenation made leaves that
    /// text to the slot's next concatenation.
    fn accumulate(&mut self, value: &Node, at: u32, commit: &Commit) -> Result<bool, Halt> {
        let left = match self.frame.slot(at) {
            SlotState::Value(Value::LongText(text)) => Some(*text),
            _ => None,
        };
        let owned = left
            .is_some_and(|text| self.texts.contains(&(at, text)) && self.cx.arena.extensible(text));
        self.texts.retain(|(slot, _)| *slot != at);
        self.cx.accumulator = left.map(|left| Accumulator {
            left,
            owned,
            made: None,
        });
        let value = value(self.frame.slot_states(), &mut self.cx);
        let made = self
            .cx
            .accumulator
            .take()
            .and_then(|accumulator| accumulator.made);
        let fresh =
            matches!((value, made), (Some(Value::LongText(text)), Some(made)) if text == made.text);
        // Storage a concatenation took belongs to the value it made, or
        // goes back: the statement misses.
        let moved = made.and_then(|made| made.moved.map(|len| (made.text, len)));
        let value = value.filter(|_| fresh || moved.is_none());
        let ran = self.write(value, at, commit)?;
        match (made, moved, left) {
            (Some(Made { text, .. }), _, _) if ran && fresh => self.texts.push((at, text)),
            (_, Some((text, len)), Some(left)) if !ran => {
                self.cx.arena.unextend_text(left, text, len);
            }
            _ => {}
        }
        Ok(ran)
    }

    /// Assigns `value`, the statement's, to the session slot at `at`, which
    /// holds a resident binding.
    #[inline(always)]
    fn write_session(
        &mut self,
        value: Option<Value>,
        at: u32,
        commit: &Commit,
    ) -> Result<bool, Halt> {
        let (Some(value), Some(SessionSlot { value: Some(_), .. })) =
            (value, self.cx.sessions.get(at as usize).copied())
        else {
            return Ok(false);
        };
        if !self.fits(commit) {
            return Ok(false);
        }
        self.writes += 1;
        self.cx.sessions[at as usize] = SessionSlot {
            value: Some(value),
            written: self.writes,
        };
        self.commit(commit)?;
        Ok(true)
    }

    /// Declares the slot at `at` with `value`, the statement's, as the
    /// reference's `bind` does.
    #[inline(always)]
    fn bind(&mut self, value: Option<Value>, at: u32, commit: &Commit) -> Result<bool, Halt> {
        let Some(value) = value.filter(|value| ALLOC || scalar(value)) else {
            return Ok(false);
        };
        if !self.fits(commit) {
            return Ok(false);
        }
        self.frame.set_slot(at, SlotState::Value(value));
        self.commit(commit)?;
        Ok(true)
    }

    /// Binds `raw`, the element a `for`'s cursor gave from `walked`, as
    /// its `item` hook makes it, committing the step's `commit`; `false`,
    /// having committed nothing, where the step misses.
    #[inline(always)]
    fn element(
        &mut self,
        hook: Option<&Hook>,
        walked: Option<Value>,
        raw: Value,
        commit: &Commit,
        binding: u32,
    ) -> Result<bool, Halt> {
        let element = match hook {
            Some(hook) => match walked.and_then(|walked| hook(walked, raw, &mut self.cx)) {
                Some(element) => element,
                None => {
                    self.head_missed = true;
                    return Ok(false);
                }
            },
            None => raw,
        };
        if (!ALLOC && !scalar(&element)) || !self.fits(commit) {
            return Ok(false);
        }
        self.commit(commit)?;
        self.frame.set_slot(binding, SlotState::Value(element));
        Ok(true)
    }

    /// The statement `step` at `index` of the innermost of `bodies` did
    /// not run, having been `tried`: the iteration stops before it, or in
    /// it where its guest call ran.
    #[cold]
    #[inline(never)]
    fn stopped<'p>(
        &mut self,
        (bodies, nexts, index): (&[&CompiledBody], &[usize], usize),
        step: &'p CompiledStep,
        tried: bool,
        call: &mut Option<&'p GuestCall>,
    ) -> Result<Went, Halt> {
        // A statement whose guest call ran stands in it: started, as the
        // reference's frame holds it there.
        if self.cx.called.is_some() {
            let guest = commit_of(step)
                .and_then(|commit| commit.guest.as_deref())
                .ok_or_else(|| fault("a guest call ran in no statement of its"))?;
            // What the statement charged before its call is paid.
            self.cx.charged += guest.units;
            *call = Some(guest);
            self.stop(bodies, nexts, index + 1);
            return Ok(Went::Called);
        }
        // A statement before it, or the `if` whose arm it stands in,
        // committed; a decline ends the run, so it is this statement's.
        self.progressed = !nexts.is_empty() || index > 0;
        self.declined = self.cx.declined;
        self.missed = tried && !matches!(step, CompiledStep::Leave) && !self.declined;
        if self.cx.fault.is_some() {
            return Err(fault("a region's fault ended its iteration"));
        }
        Ok(self.stop(bodies, nexts, index))
    }

    /// The iteration stops before statement `index` of the innermost of
    /// `bodies`, each outer one standing at its statement in `nexts`.
    #[cold]
    #[inline(never)]
    fn stop(&mut self, bodies: &[&CompiledBody], nexts: &[usize], index: usize) -> Went {
        let blocks = bodies.iter().map(|body| body.block);
        let positions = nexts.iter().copied().chain([index]);
        self.chain.extend(
            blocks
                .zip(positions)
                .map(|(block, next)| Control::Block { block, next }),
        );
        Went::Stopped
    }

    /// `body`'s block is left: the variables it declared end.
    #[inline(always)]
    fn end(&mut self, body: &CompiledBody) {
        for at in &body.declares {
            self.frame.set_slot(*at, SlotState::Empty);
        }
    }

    /// Whether an allocating plan's statement `step` may run: one whose
    /// formulas are bounded runs only where its most fits, so that what it
    /// builds is not left for a charge it then cannot commit.
    #[inline(always)]
    fn room(&self, step: &CompiledStep) -> bool {
        commit_of(step).is_none_or(|commit| self.ahead(commit))
    }

    /// Whether a statement committing `commit` may run: always in a scalar
    /// plan; in an allocating one, unless its bounded most does not fit.
    #[inline(always)]
    fn ahead(&self, commit: &Commit) -> bool {
        if !self.ready(commit) {
            return false;
        }
        if !ALLOC {
            return true;
        }
        let admit = self.admit;
        commit.max.is_none_or(|max| {
            self.cx
                .charged
                .checked_add(max)
                .and_then(|total| total.checked_add(admit.loop_test))
                .is_some_and(|end| end <= admit.bound)
        })
    }

    /// Whether the statement that ran metered by the meter, committing
    /// `commit`, is admitted: always in a scalar plan, whose iteration was;
    /// in an allocating plan, when it does not pass the bound with the
    /// next test's unit still to charge. One that reaches the slice's end
    /// commits, and the region stops after it, where the reference yields.
    #[inline(always)]
    fn fits(&self, commit: &Commit) -> bool {
        if !ALLOC {
            return true;
        }
        let admit = self.admit;
        self.cx
            .charged
            .checked_add(commit.fixed)
            .and_then(|total| total.checked_add(self.cx.meter.units))
            .and_then(|total| total.checked_add(admit.loop_test))
            .is_some_and(|end| end <= admit.bound)
    }

    /// Commits a region statement that ran metered by the meter: its fixed
    /// units and formulas, and, with a charge tape attached, its static
    /// charge program as the reference charges it (§3.1).
    #[inline(always)]
    fn commit(&mut self, commit: &Commit) -> Result<(), Halt> {
        debug_assert!(
            self.cx.held.is_none(),
            "only a `do` holds a write, and applies it as it commits"
        );
        if INVOKES && let Some(counters) = self.cx.counters.as_deref_mut() {
            for &site in &commit.calls {
                counters.advance(site)?;
            }
        }
        // A statement whose guest call ran commits standing in no call.
        if ALLOC && commit.guest.is_some() {
            self.cx.called = None;
        }
        self.cx.charged += commit.fixed + self.cx.meter.units;
        if let Some(tape) = self.tape {
            // What a tape recorded before the statement's guest call is
            // recorded already.
            let recorded = std::mem::take(&mut self.cx.meter.recorded);
            record(
                tape,
                commit.program.get(recorded..).unwrap_or_default(),
                &commit.arms,
                &mut self.cx.meter.taken.iter(),
            );
        }
        if ALLOC {
            self.due |= self.cx.charged >= self.admit.slice_end;
        }
        Ok(())
    }

    /// Whether the occurrences `commit` advances can advance: a counter
    /// that would overflow is ordinary action dispatch's to refuse.
    #[inline(always)]
    fn ready(&self, commit: &Commit) -> bool {
        !INVOKES
            || self
                .cx
                .counters
                .as_deref()
                .is_none_or(|counters| counters.can_advance(&commit.calls))
    }

    /// An allocating plan's statement ended: its kill set runs, and a due
    /// collection stops the region at this boundary.
    #[inline(always)]
    fn ended(&mut self) {
        if ALLOC {
            self.cx.statement_ended();
            self.due |= self.cx.arena.release_due() || self.cx.arena.cycles_due();
        }
    }
}

/// What statement `step` commits; a statement the region leaves commits
/// nothing.
#[inline(always)]
fn commit_of(step: &CompiledStep) -> Option<&Commit> {
    match step {
        CompiledStep::Write { commit, .. }
        | CompiledStep::Bind { commit, .. }
        | CompiledStep::Session { commit, .. }
        | CompiledStep::If { commit, .. }
        | CompiledStep::Continue(commit)
        | CompiledStep::Store { commit, .. }
        | CompiledStep::Discard { commit, .. } => Some(commit),
        CompiledStep::Typed(typed) => commit_of(&typed.1),
        CompiledStep::Leave => None,
    }
}

/// Records `program` with what the statement's run took, in order: each
/// formula's units and each conditional's arm, whose own program `arms`
/// holds. This is what the reference's anchors record for the statement.
#[cold]
#[inline(never)]
fn record(
    tape: &ChargeTape,
    program: &[Event],
    arms: &[Arms],
    taken: &mut std::slice::Iter<'_, Taken>,
) {
    for event in program {
        let weight = match event {
            Event::Fixed(weight) => *weight,
            Event::Formula => match taken.next() {
                Some(Taken::Formula(units)) => Weight::Atomic(*units),
                _ => Weight::Atomic(0),
            },
            // A guest call's island recorded its own events as it ran.
            Event::Call => continue,
            Event::Arm(index) => {
                if let (Some(Taken::Arm(arm)), Some(conditional)) =
                    (taken.next(), arms.get(*index as usize))
                {
                    record(tape, conditional.arm(*arm).0, arms, taken);
                }
                continue;
            }
        };
        if weight != Weight::Atomic(0) {
            tape.push(weight);
        }
    }
}

/// A store's operands by role, as `Store` evaluates them.
const _: () = {
    assert!(Role::Target as usize == 0);
    assert!(Role::Index as usize == 1);
    assert!(Role::Value as usize == 2);
};
