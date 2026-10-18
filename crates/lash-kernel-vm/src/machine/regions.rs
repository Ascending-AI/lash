//! Loop regions (`docs/design/kernel-regions.md` §2, §3): a `while`, or a
//! `for` over a list or a tuple, whose plan lowering formed runs whole
//! iterations inside one step, from the plan's compiled form, while each
//! fits the charge headroom and every guard holds. The body's `if` arms
//! and `continue`s run inside the region too. The reference interpreter
//! stays the oracle and the fallback: a region exits at the last statement
//! boundary it committed, with the exact control the reference would hold
//! there, and the reference runs on from it. A `break` or a `return` is
//! such an exit: the reference runs it.

use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Weight;
use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::PagedHeap;

use super::loops::{Cursor, Header};
use super::ops::Flow;
use super::{Control, Frame, Halt, KernelMachine, SlotState, fault};
use crate::compile::{
    Commit, CompiledBody, CompiledHead, CompiledStep, Event, Executable, Meter, NESTING, scalar,
};
use crate::observe::ChargeTape;

/// What a region's statements may hold while it runs, admitted once at its
/// entry (§3.5): their scalar pins, which net to zero at each statement's
/// end.
const REGION_HOLD: u64 = 1 << 16;

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

/// One region's run over its frame: what it charges, records and holds.
struct Run<'a, const INVOKES: bool> {
    frame: &'a mut Frame,
    counters: Option<&'a mut super::tasks::Occurrences>,
    arena: &'a Arena,
    heap: &'a mut PagedHeap,
    /// Whether variables' memory is held (`MemoryLimit` not off).
    memory: bool,
    charged: &'a mut u64,
    tape: Option<&'a ChargeTape>,
    meter: Meter,
    /// Where an iteration stopped: each block it stands in, outermost
    /// first, at the statement it runs next.
    chain: Vec<Control>,
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
        if exe.regions[region as usize].calls.is_empty() {
            self.run_loop_region::<false>(task, host, exe, region)
        } else {
            self.run_loop_region::<true>(task, host, exe, region)
        }
    }

    /// Specialize occurrence bookkeeping away for regions with no invokes.
    fn run_loop_region<const INVOKES: bool>(
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
        // The memory headroom guard: no admission inside refuses.
        if self
            .bounds
            .memory
            .saturating_sub(self.heap.certificate().upper.0)
            < REGION_HOLD
        {
            return Ok(None);
        }
        let memory = self.bounds.memory_limit != crate::MemoryLimit::Off;
        // A declaration holds its value's memory as the reference's does
        // in a fresh step, which has no statement credit to draw on.
        if plan.binds && memory && self.heap.statement_reserved() != 0 {
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
        let arena = self.arena.borrow();
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
        let mut run = Run::<INVOKES> {
            frame,
            counters: (INVOKES && durable).then_some(&mut state.counters),
            arena: &arena,
            heap: &mut self.heap,
            memory,
            charged: &mut self.charged,
            tape,
            meter: Meter::new(tape.is_some()),
            chain: Vec::new(),
        };
        let mut stack = Stack {
            bodies: [&compiled.body; NESTING + 1],
            nexts: [0; NESTING + 1],
        };
        let mut started = 0_u64;
        let mut stepped = 0_usize;
        let mut stopped = false;
        loop {
            // The charge headroom guard: no event of the iteration passes
            // the bound, and no step boundary inside reaches the slice's
            // end.
            match run.charged.checked_add(plan.iteration_max) {
                Some(end) if end <= bound && end < slice_end => {}
                _ => break,
            }
            run.meter.reset();
            match (&compiled.head, walk) {
                (CompiledHead::While { test, commit }, _) => {
                    if !run.ready(commit) {
                        break;
                    }
                    if !matches!(
                        test(run.frame.slot_states(), &mut run.meter),
                        Some(Value::Bool(true))
                    ) {
                        break;
                    }
                    run.commit(commit)?;
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
                    let Ok(Some(raw)) = run.arena.item(items, (position + stepped) as u64) else {
                        break;
                    };
                    let element = match hook {
                        Some(hook) => {
                            match walked.and_then(|walked| hook(walked, raw, &mut run.meter)) {
                                Some(element) => element,
                                None => break,
                            }
                        }
                        None => raw,
                    };
                    if !scalar(&element) || !run.hold(&element)? {
                        break;
                    }
                    run.commit(commit)?;
                    stepped += 1;
                    run.frame.set_slot(*binding, SlotState::Value(element));
                }
                (CompiledHead::For { .. }, None) => {
                    return Err(fault("a `for` region walks nothing"));
                }
            }
            started += 1;
            if let Went::Stopped = run.iterate(&mut stack)? {
                stopped = true;
                break;
            }
            // The next test's unit, which the reference charges first.
            *run.charged += plan.loop_test;
            if let Some(tape) = run.tape
                && plan.loop_test != 0
            {
                tape.push(Weight::Atomic(plan.loop_test));
            }
        }
        let Run { frame, chain, .. } = run;
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

impl<const INVOKES: bool> Run<'_, INVOKES> {
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
                self.end(body);
                if depth == 0 {
                    return Ok(Went::Through);
                }
                depth -= 1;
                continue;
            };
            nexts[depth] = index + 1;
            self.meter.reset();
            match step {
                CompiledStep::Write { value, at, commit } => {
                    if !self.ready(commit) {
                        return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                    }
                    let Some(value) = value(self.frame.slot_states(), &mut self.meter) else {
                        return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                    };
                    // A write that grows its slot is the reference's to
                    // admit.
                    match self.frame.slot(*at) {
                        SlotState::Value(old)
                            if self.arena.cost_of(&value) <= self.arena.cost_of(old) => {}
                        _ => return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index)),
                    }
                    self.frame.set_slot(*at, SlotState::Value(value));
                    self.commit(commit)?;
                }
                CompiledStep::Bind { value, at, commit } => {
                    if !self.ready(commit) {
                        return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                    }
                    let value = value(self.frame.slot_states(), &mut self.meter);
                    let Some(value) = value.filter(scalar) else {
                        return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                    };
                    if !self.hold(&value)? {
                        return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                    }
                    self.frame.set_slot(*at, SlotState::Value(value));
                    self.commit(commit)?;
                }
                CompiledStep::If {
                    condition,
                    commit,
                    then,
                    otherwise,
                } => {
                    if !self.ready(commit) {
                        return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                    }
                    let arm = match condition(self.frame.slot_states(), &mut self.meter) {
                        Some(Value::Bool(true)) => then,
                        Some(Value::Bool(false)) => otherwise,
                        _ => return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index)),
                    };
                    self.commit(commit)?;
                    // An empty arm ends as soon as it begins: the
                    // reference enters no block.
                    if !arm.steps.is_empty() {
                        depth += 1;
                        bodies[depth] = arm;
                        nexts[depth] = 0;
                    }
                }
                CompiledStep::Continue(commit) => {
                    self.commit(commit)?;
                    for body in bodies[..=depth].iter().rev() {
                        self.end(body);
                    }
                    return Ok(Went::Continued);
                }
                CompiledStep::Leave => {
                    return Ok(self.stop(&bodies[..=depth], &nexts[..depth], index));
                }
            }
        }
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
    /// the reference's `bind` does; `false` when the census would need
    /// precision first, having held nothing, so that the reference
    /// declares it.
    fn hold(&mut self, value: &Value) -> Result<bool, Halt> {
        if !self.memory {
            return Ok(true);
        }
        Ok(self
            .heap
            .hold_growth_metered(self.arena.cost_of_metered(value))?)
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
        *self.charged += commit.fixed + self.meter.units;
        if let Some(tape) = self.tape {
            record(tape, &commit.program, &self.meter.formulas);
        }
        Ok(())
    }

    fn ready(&self, commit: &Commit) -> bool {
        !INVOKES
            || self
                .counters
                .as_deref()
                .is_none_or(|counters| counters.can_advance(&commit.calls))
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
