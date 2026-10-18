//! A loop plan's verification against the lowered code
//! ([`LoopPlan::verify`]): the summaries, programs, arms, reads, writes,
//! session bindings and calls a plan states are its statements' own.

use lash_kernel_exec::{Op, Weight};

use super::super::expr::arm_ends;
use super::super::{Atom, BindingName, BlockId, Form, NestedKind, Var};
use super::{
    Arms, Body, End, Event, Gathered, Head, LibId, LoopPlan, Lowered, Refused, RegionSlot, Step,
    Summary, barrier, general, lean, library_invoke, scalar_site,
};

/// The plan's check against the lowered code: the summaries in the order
/// [`LoopPlan::verify`] numbers them, and the calls their ops make.
struct Check<'a, 'l> {
    lowered: &'a Lowered<'l>,
    statement: usize,
    calls: Vec<usize>,
    /// The session bindings the ops use, in the order they first use them.
    sessions: Gathered,
}

/// What a statement's ops show the verifier: the program and arms they
/// charge, the invokes they count, the slots they read and write, whether
/// they allocate and whether their formulas are bounded.
struct Seen {
    expected: Vec<Event>,
    arms: Vec<Arms>,
    calls: Vec<usize>,
    reads: Vec<RegionSlot>,
    write: Option<RegionSlot>,
    allocates: bool,
    bounded: bool,
}

impl Check<'_, '_> {
    /// Checks `summary`, the next statement, charged `head` before its ops
    /// and ending as `end`.
    fn summary(&mut self, summary: &Summary, head: Weight, end: End) -> Result<(), Refused> {
        let index = self.statement;
        self.statement += 1;
        let stream = self.lowered.stream;
        let ops = stream
            .ops
            .get(summary.ops.start as usize..summary.ops.end() as usize)
            .ok_or(Refused::Program { statement: index })?;
        if !end.ends(ops.last()) {
            return Err(Refused::Shape { statement: index });
        }
        let mut seen = Seen {
            expected: vec![Event::Fixed(head)],
            arms: Vec::new(),
            calls: Vec::new(),
            reads: Vec::new(),
            write: None,
            allocates: false,
            bounded: true,
        };
        self.ops(
            summary,
            summary.ops.start,
            summary.ops.end(),
            &mut seen,
            index,
        )?;
        let Seen {
            expected,
            arms,
            calls,
            reads,
            write,
            allocates,
            bounded,
        } = seen;
        if *summary.reads != *reads || summary.write != write {
            return Err(Refused::Slots { statement: index });
        }
        if summary.general != allocates || summary.formula_max.is_some() != bounded {
            return Err(Refused::Allocates { statement: index });
        }
        if *summary.calls != *calls {
            return Err(Refused::Calls);
        }
        if *summary.arms != *arms {
            return Err(Refused::Program { statement: index });
        }
        self.calls.extend(calls);
        self.program(summary, expected, index)
    }

    /// Reads the ops `start..end` of `summary`'s statement into `seen`:
    /// a conditional's arms into an [`Event::Arm`] and their own programs.
    fn ops(
        &mut self,
        summary: &Summary,
        start: u32,
        end: u32,
        seen: &mut Seen,
        index: usize,
    ) -> Result<(), Refused> {
        let stream = self.lowered.stream;
        let mut pc = start;
        while pc < end {
            let op = *stream
                .ops
                .get(pc as usize)
                .ok_or(Refused::Program { statement: index })?;
            seen.allocates |= general(stream, self.lowered.lib, op);
            match op {
                Op::Region { region, total } => {
                    let stated = stream
                        .regions
                        .summary(region.0)
                        .map_err(|_| Refused::Region { region: region.0 })?;
                    if u64::from(total) != stated.total() {
                        return Err(Refused::Region { region: region.0 });
                    }
                    seen.expected.extend(
                        stream
                            .regions
                            .events(region.0)
                            .iter()
                            .map(|event| Event::Fixed(event.weight)),
                    );
                }
                Op::Operator { lib, args } => {
                    seen.bounded &=
                        scalar_site((self.lowered.lib)(LibId(lib.0)), args.len as usize);
                    seen.expected.push(Event::Formula);
                }
                Op::Call { .. } => {
                    seen.bounded = false;
                    seen.expected.push(Event::Formula);
                }
                Op::Act => {
                    let id = summary.action.ok_or(Refused::Shape { statement: index })?;
                    let (action, lib, args) = library_invoke(&(self.lowered.stmt)(id).form)
                        .ok_or(Refused::Shape { statement: index })?;
                    for arg in args {
                        match arg {
                            Atom::Var(Var::Local(local)) => {
                                seen.reads.push(RegionSlot::Frame(local.at));
                            }
                            Atom::Var(Var::Session(name)) => {
                                seen.reads.push(self.sessions.session(name));
                            }
                            _ => {}
                        }
                        seen.expected.push(Event::Fixed(Weight::Atomic(1)));
                    }
                    if !matches!(action.site.unit, lash_kernel_doc::Unit::Library(_)) {
                        seen.calls.push(action.occurrence);
                    }
                    let scalar = scalar_site((self.lowered.lib)(lib), args.len());
                    seen.bounded &= scalar;
                    seen.allocates |= !scalar;
                    seen.expected.push(Event::Formula);
                }
                Op::Nested { act, args } => {
                    if let Some(nested) = stream.nested.get(act.ix()) {
                        if matches!(nested.kind, NestedKind::Call(_))
                            && !matches!(nested.site.unit, lash_kernel_doc::Unit::Library(_))
                        {
                            seen.calls.push(nested.occurrence);
                        }
                        if let Some(lib) = nested.kind.operator_lib() {
                            seen.bounded &= scalar_site((self.lowered.lib)(lib), args.len as usize);
                        }
                    }
                    seen.expected.push(Event::Formula);
                }
                Op::Local { local, .. } => seen.reads.push(RegionSlot::Frame(local.at)),
                Op::Session { name, .. } => {
                    let name = stream
                        .names
                        .get(name.ix())
                        .ok_or(Refused::Slots { statement: index })?;
                    seen.reads.push(self.sessions.session(name));
                }
                Op::WriteLocal { local, .. } | Op::Bind { local, .. } => {
                    seen.write = Some(RegionSlot::Frame(local.at));
                }
                Op::WriteSession { name, .. } => {
                    let name = stream
                        .names
                        .get(name.ix())
                        .ok_or(Refused::Slots { statement: index })?;
                    seen.write = Some(self.sessions.session(name));
                }
                Op::JumpIfFalse { skip, .. } => {
                    let (then_end, else_end) =
                        arm_ends(stream, pc, skip).ok_or(Refused::Shape { statement: index })?;
                    let outer = std::mem::take(&mut seen.expected);
                    self.ops(summary, pc + 1, then_end - 1, seen, index)?;
                    let then = lean(std::mem::take(&mut seen.expected));
                    self.ops(summary, then_end, else_end, seen, index)?;
                    let otherwise = lean(std::mem::replace(&mut seen.expected, outer));
                    let fixed = |program: &[Event]| {
                        program
                            .iter()
                            .map(|event| match event {
                                Event::Fixed(weight) => weight.units(),
                                Event::Formula | Event::Arm(_) => 0,
                            })
                            .sum::<u64>()
                    };
                    let arm = u32::try_from(seen.arms.len())
                        .map_err(|_| Refused::Program { statement: index })?;
                    seen.expected.push(Event::Arm(arm));
                    seen.arms.push(Arms {
                        then_fixed: fixed(&then),
                        otherwise_fixed: fixed(&otherwise),
                        then,
                        otherwise,
                    });
                    pc = else_end;
                    continue;
                }
                _ => {}
            }
            pc += 1;
        }
        Ok(())
    }

    /// Checks that `summary`'s program is `expected` and that its fixed
    /// units are that program's.
    fn program(
        &self,
        summary: &Summary,
        expected: Vec<Event>,
        index: usize,
    ) -> Result<(), Refused> {
        if *summary.program != *lean(expected) {
            return Err(Refused::Program { statement: index });
        }
        let fixed = summary
            .program
            .iter()
            .map(|event| match event {
                Event::Fixed(weight) => weight.units(),
                Event::Formula | Event::Arm(_) => 0,
            })
            .sum::<u64>();
        if fixed != summary.fixed {
            return Err(Refused::Fixed { statement: index });
        }
        Ok(())
    }

    /// Checks `body` against block `block`'s statements and declarations.
    fn body(&mut self, body: &Body, block: BlockId) -> Result<(), Refused> {
        let lowered = (self.lowered.block)(block);
        let shape = Refused::Shape {
            statement: self.statement,
        };
        if body.block != block
            || *body.declares != *lowered.declares
            || body.steps.len() != lowered.stmts.len()
        {
            return Err(shape);
        }
        // Past the block's first statement the region leaves, it leaves
        // every statement.
        let mut left = false;
        for (step, id) in body.steps.iter().zip(&lowered.stmts) {
            let statement = (self.lowered.stmt)(*id);
            if left && !matches!(step, Step::Leave) {
                return Err(Refused::Shape {
                    statement: self.statement,
                });
            }
            // Each statement's summary covers exactly its own ops.
            let own = match step {
                Step::Write(summary)
                | Step::Bind(summary)
                | Step::Continue(summary)
                | Step::Store(summary)
                | Step::Discard(summary) => {
                    summary.ops == statement.ops
                        && summary.action
                            == matches!(statement.form, Form::Action(_)).then_some(*id)
                }
                Step::If { test, .. } => test.ops == statement.ops,
                Step::Leave => true,
            };
            if !own {
                return Err(Refused::Shape {
                    statement: self.statement,
                });
            }
            let unit = Weight::Atomic(1);
            match (step, &statement.form) {
                (Step::Write(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Write)?;
                }
                (Step::Bind(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Bind)?;
                }
                (Step::Continue(summary), Form::Plain) => {
                    self.summary(summary, unit, End::Continue)?;
                }
                (Step::Store(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Store)?;
                }
                (Step::Discard(summary), Form::Plain) => {
                    self.summary(summary, unit, End::Discard)?;
                }
                // A region leaves to the reference only a barrier, and what
                // follows one.
                (Step::Leave, _) if left || barrier(self.lowered, statement) => {
                    left = true;
                }
                (
                    Step::If {
                        test,
                        then,
                        otherwise,
                    },
                    Form::If {
                        then_block,
                        else_block,
                    },
                ) => {
                    self.summary(test, unit, End::Branch)?;
                    self.body(then, *then_block)?;
                    self.body(otherwise, *else_block)?;
                }
                _ => {
                    return Err(Refused::Shape {
                        statement: self.statement,
                    });
                }
            }
        }
        Ok(())
    }
}

impl LoopPlan {
    /// Checks the plan against `lowered` and the cost table's `loop_test`:
    /// its head is its loop's, its body's statements, arms and
    /// declarations are the blocks', it leaves only barriers to the
    /// reference, each summary's fixed units are its program's fixed
    /// events, every region's head states the total its `RegionSummary`
    /// gives, the programs are the stream's events in order, each summary
    /// allocates where its ops do, and the iteration maximum is the
    /// head's, the dearest path's and the loop-test unit's sum, or 0 for
    /// an allocating plan. Summaries are numbered in order: the head 0,
    /// then the body's statements depth first.
    pub(crate) fn verify(&self, lowered: &Lowered<'_>, loop_test: u64) -> Result<(), Refused> {
        if self.loop_test != loop_test {
            return Err(Refused::LoopTest);
        }
        let mut check = Check {
            lowered,
            statement: 0,
            calls: Vec::new(),
            sessions: Gathered::default(),
        };
        let statement = (lowered.stmt)(self.stmt);
        let head = Refused::Shape { statement: 0 };
        let (block, binds) = match (&self.head, &statement.form) {
            (
                Head::While(test),
                Form::While {
                    metered,
                    test: ops,
                    body,
                    ..
                },
            ) => {
                if test.ops != *ops {
                    return Err(head);
                }
                check.summary(test, Weight::Atomic(*metered), End::Test)?;
                (*body, false)
            }
            (
                Head::For {
                    charge,
                    binding,
                    item,
                },
                Form::For {
                    metered,
                    binding: bound,
                    via,
                    body,
                    ..
                },
            ) => {
                if *binding != bound.at
                    || bound.shared
                    || *item != via.and_then(|via| via.item)
                    || charge.write != Some(RegionSlot::Frame(bound.at))
                    || !charge.reads.is_empty()
                {
                    return Err(head);
                }
                let scalar = item.is_none_or(|item| scalar_site((lowered.lib)(item), 2));
                if charge.general != !scalar || charge.formula_max.is_none() == scalar {
                    return Err(Refused::Allocates { statement: 0 });
                }
                let mut expected = vec![Event::Fixed(Weight::Atomic(*metered))];
                expected.extend(item.map(|_| Event::Formula));
                check.statement += 1;
                check.program(charge, expected, 0)?;
                (*body, true)
            }
            _ => return Err(head),
        };
        check.body(&self.body, block)?;
        if self.binds != (binds || self.body.binds()) {
            return Err(Refused::Shape { statement: 0 });
        }
        if matches!(self.body.steps.first(), None | Some(Step::Leave)) {
            return Err(Refused::Shape { statement: 1 });
        }
        let allocates = self.head.summary().allocates() || self.body.allocates();
        if self.allocates != allocates {
            return Err(Refused::Allocates { statement: 0 });
        }
        let iteration = if allocates {
            Some(0)
        } else {
            self.head
                .summary()
                .max()
                .zip(self.body.max())
                .and_then(|(head, body)| head.checked_add(body)?.checked_add(loop_test))
        };
        if iteration != Some(self.iteration_max) {
            return Err(Refused::Iteration);
        }
        if *self.calls != *check.calls {
            return Err(Refused::Calls);
        }
        let named = |sessions: &[BindingName]| {
            sessions
                .iter()
                .map(|session| session.name.clone())
                .collect::<Vec<_>>()
        };
        if named(&self.sessions) != named(&check.sessions.sessions) {
            return Err(Refused::Slots { statement: 0 });
        }
        Ok(())
    }
}
