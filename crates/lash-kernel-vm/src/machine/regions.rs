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
//! at that boundary, where the reference's step collects.

use std::sync::atomic::Ordering::Relaxed;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Weight;

use super::loops::{Cursor, Header};
use super::ops::Flow;
use super::{Control, Frame, Halt, KernelMachine, SlotState, fault};
use crate::compile::{
    Commit, CompiledBody, CompiledHead, CompiledLoop, CompiledStep, Cx, Event, Executable,
    LoopPlan, Meter, NESTING, Role, UNPRODUCTIVE, scalar, store,
};
use crate::observe::ChargeTape;

/// What a region may hold while it runs, admitted once at its entry
/// (§3.5): a scalar plan's scalar pins, which net to zero at each
/// statement's end, and the nursery an allocating plan's statements build
/// and grow in.
const NURSERY: u64 = 1 << 16;

/// How a pass through a block of an iteration went.
enum Went {
    /// It ran to the block's end.
    Through,
    /// A `continue` ended the iteration.
    Continued,
    /// It stopped before a statement, in the blocks the chain holds,
    /// outermost first.
    Stopped,
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
    /// The task's occurrence counters, which a durable run's invokes
    /// advance as they commit (`K-EFF-008`).
    counters: Option<&'a mut super::tasks::Occurrences>,
    cx: Cx<'c>,
    charged: &'a mut u64,
    tape: Option<&'a ChargeTape>,
    /// An allocating plan's admission (`ALLOC`); a scalar plan admits
    /// whole iterations.
    /// iterations.
    admit: Admit,
    /// The reference's step would end here: a statement spent the slice,
    /// or the arena's collection is due. The region stops at this
    /// boundary, as the reference's step ends at it.
    /// statement boundary.
    due: bool,
    /// Where an iteration stopped: each block it stands in, outermost
    /// first, at the statement it runs next.
    chain: Vec<Control>,
    /// The last statement the region tried was one it runs, not one it
    /// leaves: where it stopped there, a guard missed.
    missed: bool,
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
        host: &dyn crate::Host,
        exe: &Executable,
        region: u32,
    ) -> Result<Option<Flow>, Halt> {
        let plan = &exe.regions[region as usize];
        // A plan whose entries keep missing is the reference's.
        if plan.unproductive.load(Relaxed) >= UNPRODUCTIVE {
            return Ok(None);
        }
        match (plan.calls.is_empty(), plan.allocates) {
            (true, false) => self.run_loop_region::<false, false>(task, host, exe, region),
            (false, false) => self.run_loop_region::<true, false>(task, host, exe, region),
            (true, true) => self.run_loop_region::<false, true>(task, host, exe, region),
            (false, true) => self.run_loop_region::<true, true>(task, host, exe, region),
        }
    }

    /// Specialize occurrence bookkeeping away for regions with no invokes,
    /// and statement admission away for scalar plans.
    fn run_loop_region<const INVOKES: bool, const ALLOC: bool>(
        &mut self,
        task: TaskId,
        host: &dyn crate::Host,
        exe: &Executable,
        region: u32,
    ) -> Result<Option<Flow>, Halt> {
        // The environment guard: nothing observes the run but a charge
        // tape, which the region records.
        if !self.charging
            || host.observes_sites()
            || !self.ran.is_empty()
            || self.steps.verify
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
        let plan = &exe.regions[region as usize];
        // The memory headroom guard: the nursery is admitted, so no
        // admission inside refuses.
        if self
            .bounds
            .memory
            .saturating_sub(self.heap.certificate().upper.0)
            < NURSERY
        {
            return Ok(None);
        }
        let memory = self.bounds.memory_limit != crate::MemoryLimit::Off;
        // What the region holds is held as the reference's statements
        // hold it, each in a fresh step, which has no statement credit to
        // draw on.
        if (plan.binds || plan.allocates) && memory && self.heap.statement_reserved() != 0 {
            return Ok(None);
        }
        // The depth guard: every site's depth check passes in this frame,
        // whose depth the region does not change.
        if let Some(need) = plan.depth {
            let frames = self
                .tasks
                .get_mut(task.0 as usize)?
                .ok_or_else(|| fault("no task"))?
                .frames
                .len();
            let depth = frames
                + self.plans as usize
                + usize::from(task != TaskId::MAIN || !self.session_cell);
            if depth as u64 + u64::from(need) > u64::from(self.bounds.call_depth) {
                return Ok(None);
            }
        }
        let Some(compiled) = plan.compiled(exe) else {
            return Ok(None);
        };
        let tape = self
            .observer
            .as_deref()
            .and_then(|observer| observer.charges.as_ref());
        let (bound, slice_end) = (self.bounds.charge_limit, self.slice_end);
        let durable = self.durable();
        let mut arena = self.arena.borrow_mut();
        let state = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let frame = state.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        // What a `for` walks, from where; the cursor of a `while` is none.
        let walk = match (&compiled.head, frame.control.last()) {
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
            ) => Some((cursor.walked(), *items, *position)),
            _ => return Ok(None),
        };
        let mut run = Run::<INVOKES, ALLOC> {
            frame,
            counters: (INVOKES && durable).then_some(&mut state.counters),
            cx: Cx {
                meter: Meter::new(tape.is_some()),
                args: super::registers::Arguments::new(),
                args_reserved: false,
                exe,
                arena: &mut arena,
                heap: &mut self.heap,
                sites: &mut self.sites,
                limits: self.limits,
                memory,
                room: NURSERY,
                transient: 0,
                fault: None,
            },
            charged: &mut self.charged,
            tape,
            admit: Admit {
                bound,
                slice_end,
                loop_test: plan.loop_test,
            },
            due: false,
            chain: Vec::new(),
            missed: false,
        };
        let (started, stepped, stopped) = run.batch(plan, compiled, walk, bound, slice_end)?;
        let Run {
            frame,
            chain,
            cx,
            missed,
            ..
        } = run;
        // An entry that missed a guard before an iteration ended was
        // unproductive; one that ended an iteration was not.
        if stopped && missed && started <= 1 {
            plan.unproductive.fetch_add(1, Relaxed);
        } else if started > 1 {
            plan.unproductive.store(0, Relaxed);
        }
        if let Some(fault) = cx.fault {
            return Err(Halt::from(fault));
        }
        // The exit materializes the control the reference holds there:
        // the loop's iterations started, a `for`'s cursor past the
        // elements it stepped, and the blocks standing at the statement the
        // region did not run.
        match frame.control.last_mut() {
            Some(Control::While { started: total, .. }) => *total += started,
            Some(Control::For {
                started: total,
                cursor: Cursor::List(_, position) | Cursor::Tuple(_, position),
                ..
            }) => {
                *total += started;
                *position += stepped;
            }
            _ => return Err(fault("a loop region left its loop")),
        }
        if !stopped {
            return Ok(None);
        }
        frame.control.extend(chain);
        Ok(Some(Flow::Step))
    }
}

/// The blocks an iteration stands in, the loop's body first: each at the
/// statement it runs next, an arm's enclosing block past its `if`.
struct Stack<'p> {
    bodies: [&'p CompiledBody; NESTING + 1],
    nexts: [usize; NESTING + 1],
}

impl<const INVOKES: bool, const ALLOC: bool> Run<'_, '_, INVOKES, ALLOC> {
    /// Runs whole iterations while each fits: the iterations it started,
    /// the elements a `for` stepped, and whether one stopped before a body
    /// statement.
    fn batch(
        &mut self,
        plan: &LoopPlan,
        compiled: &CompiledLoop,
        walk: Option<(Option<Value>, ArenaHandle, usize)>,
        bound: u64,
        slice_end: u64,
    ) -> Result<(u64, usize, bool), Halt> {
        let mut stack = Stack {
            bodies: [&compiled.body; NESTING + 1],
            nexts: [0; NESTING + 1],
        };
        let mut started = 0_u64;
        let mut stepped = 0_usize;
        loop {
            // The charge headroom guard of a scalar plan: no event of the
            // iteration passes the bound, and no step boundary inside
            // reaches the slice's end. An allocating plan admits each
            // statement against the bound, and stops where the reference's
            // step would end.
            if !ALLOC {
                match self.charged.checked_add(plan.iteration_max) {
                    Some(end) if end <= bound && end < slice_end => {}
                    _ => break,
                }
            }
            self.cx.meter.reset();
            match (&compiled.head, walk) {
                (CompiledHead::While { test, commit }, _) => {
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
                (
                    CompiledHead::For {
                        hook,
                        commit,
                        binding,
                    },
                    Some((walked, items, position)),
                ) => {
                    // The cursor's next element, which a cold chunk or the
                    // end leaves to the reference.
                    if !self.ahead(commit) {
                        break;
                    }
                    let Ok(Some(raw)) = self.cx.arena.item(items, (position + stepped) as u64)
                    else {
                        break;
                    };
                    let element = match hook {
                        Some(hook) => {
                            match walked.and_then(|walked| hook(walked, raw, &mut self.cx)) {
                                Some(element) => element,
                                None => break,
                            }
                        }
                        None => raw,
                    };
                    if (!ALLOC && !scalar(&element))
                        || !self.fits(commit)
                        || !self.hold(&element)?
                    {
                        break;
                    }
                    self.commit(commit)?;
                    stepped += 1;
                    self.frame.set_slot(*binding, SlotState::Value(element));
                }
                (CompiledHead::For { .. }, None) => {
                    return Err(fault("a `for` region walks nothing"));
                }
            }
            self.ended();
            started += 1;
            if let Went::Stopped = self.iterate(&mut stack)? {
                return Ok((started, stepped, true));
            }
            // The next test's unit, which the reference charges first.
            *self.charged += plan.loop_test;
            if let Some(tape) = self.tape
                && plan.loop_test != 0
            {
                tape.push(Weight::Atomic(plan.loop_test));
            }
        }
        if self.cx.fault.is_some() {
            return Err(fault("a region's fault ended its batch"));
        }
        Ok((started, stepped, false))
    }

    /// Runs one pass through the loop's body, `stack`'s first block,
    /// entering the `if` arms it takes.
    #[inline(always)]
    fn iterate(&mut self, stack: &mut Stack<'_>) -> Result<Went, Halt> {
        let Stack { bodies, nexts } = stack;
        nexts[0] = 0;
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
            self.cx.meter.reset();
            let tried = !(ALLOC && (self.due || !self.room(step)));
            let ran = if !tried {
                false
            } else {
                match step {
                    CompiledStep::Write { value, at, commit } => {
                        let value = value(self.frame.slot_states(), &mut self.cx);
                        self.write(value, *at, commit)?
                    }
                    CompiledStep::Bind { value, at, commit } => {
                        let value = value(self.frame.slot_states(), &mut self.cx);
                        self.bind(value, *at, commit)?
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
                    CompiledStep::Discard { value, commit } => {
                        value(self.frame.slot_states(), &mut self.cx).is_some()
                            && self.fits(commit)
                            && {
                                self.commit(commit)?;
                                true
                            }
                    }
                    CompiledStep::Leave => false,
                }
            };
            if !ran {
                self.missed = tried && !matches!(step, CompiledStep::Leave);
                if self.cx.fault.is_some() {
                    return Err(fault("a region's fault ended its iteration"));
                }
                return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
            }
            self.ended();
        }
    }

    /// Assigns `value`, the statement's, to the slot at `at`: a scalar
    /// plan's never grows its slot, which is the reference's to admit; an
    /// allocating plan's holds the growth as the reference's write does.
    #[inline(always)]
    fn write(&mut self, value: Option<Value>, at: u32, commit: &Commit) -> Result<bool, Halt> {
        let Some(value) = value else {
            return Ok(false);
        };
        let SlotState::Value(old) = self.frame.slot(at) else {
            return Ok(false);
        };
        if !ALLOC {
            // A scalar plan's write never grows its slot.
            if self.cx.arena.cost_of(&value) > self.cx.arena.cost_of(old) {
                return Ok(false);
            }
        } else if let (new, old) = (
            self.cx.arena.cost_of_metered(&value),
            self.cx.arena.cost_of_metered(old),
        ) && (!self.fits(commit)
            || !self.cx.fits(new.saturating_sub(old))
            || !self.cx.hold(new.saturating_sub(old)))
        {
            return Ok(false);
        }
        self.frame.set_slot(at, SlotState::Value(value));
        self.commit(commit)?;
        Ok(true)
    }

    /// Declares the slot at `at` with `value`, the statement's, holding its
    /// memory as the reference's `bind` does.
    #[inline(always)]
    fn bind(&mut self, value: Option<Value>, at: u32, commit: &Commit) -> Result<bool, Halt> {
        let Some(value) = value.filter(|value| ALLOC || scalar(value)) else {
            return Ok(false);
        };
        if !self.fits(commit) || !self.hold(&value)? {
            return Ok(false);
        }
        self.frame.set_slot(at, SlotState::Value(value));
        self.commit(commit)?;
        Ok(true)
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

    /// Holds the memory a declaration of `value` grows its variable by, as
    /// the reference's `bind` does; `false` when it does not fit, having
    /// held nothing, so that the reference declares it.
    fn hold(&mut self, value: &Value) -> Result<bool, Halt> {
        if !self.cx.memory {
            return Ok(true);
        }
        let bytes = self.cx.arena.cost_of_metered(value);
        if ALLOC {
            return Ok(self.cx.fits(bytes) && self.cx.hold(bytes));
        }
        Ok(self.cx.heap.hold_growth_metered(bytes)?)
    }

    /// Whether an allocating plan's statement `step` may run: one whose
    /// formulas are bounded runs only where its most fits, so that what it
    /// builds is not left for a charge it then cannot commit.
    #[inline(always)]
    fn room(&self, step: &CompiledStep) -> bool {
        let commit = match step {
            CompiledStep::Write { commit, .. }
            | CompiledStep::Bind { commit, .. }
            | CompiledStep::If { commit, .. }
            | CompiledStep::Continue(commit)
            | CompiledStep::Store { commit, .. }
            | CompiledStep::Discard { commit, .. } => commit,
            CompiledStep::Leave => return true,
        };
        self.ahead(commit)
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
            self.charged
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
        self.charged
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
        if INVOKES && let Some(counters) = self.counters.as_deref_mut() {
            for &site in &commit.calls {
                counters.advance(site)?;
            }
        }
        *self.charged += commit.fixed + self.cx.meter.units;
        if let Some(tape) = self.tape {
            record(tape, &commit.program, &self.cx.meter.formulas);
        }
        if ALLOC {
            self.due |= *self.charged >= self.admit.slice_end;
        }
        Ok(())
    }

    /// Whether the occurrences `commit` advances can advance: a counter
    /// that would overflow is ordinary action dispatch's to refuse.
    #[inline(always)]
    fn ready(&self, commit: &Commit) -> bool {
        !INVOKES
            || self
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

/// Records `program` with its formulas' units `formulas`, in order: what
/// the reference's anchors record for the same statement.
#[cold]
#[inline(never)]
fn record(tape: &ChargeTape, program: &[Event], formulas: &[u64]) {
    let mut formulas = formulas.iter();
    for event in program {
        let weight = match event {
            Event::Fixed(weight) => *weight,
            Event::Formula => Weight::Atomic(formulas.next().copied().unwrap_or(0)),
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
