//! A loop plan's verification against the lowered code
//! ([`LoopPlan::verify`]): the summaries, programs, arms, reads, writes,
//! session bindings and calls a plan states are its statements' own.

use lash_kernel_exec::{ActIx, Op, Weight};

use super::super::expr::arm_ends;
use super::super::{Atom, BindingName, BlockId, Callee, Form, NestedKind, StmtId, Var};
use super::{
    Arms, Body, End, Event, Gathered, Head, Kept, LibId, LoopPlan, Lowered, Refused, RegionSlot,
    Step, Summary, barrier, general, guest_call, guest_site, lean, library_invoke, root_call,
    scalar_site, writes, writes_last,
};

/// The plan's check against the lowered code: the summaries in the order
/// [`LoopPlan::verify`] numbers them, and the calls their ops make.
struct Check<'a, 'l> {
    lowered: &'a Lowered<'l>,
    loop_test: u64,
    statement: usize,
    calls: Vec<usize>,
    /// The session bindings the ops use, in the order they first use them.
    sessions: Gathered,
}

/// What a statement's ops show the verifier: the program and arms they
/// charge, the invokes they count when it commits and the guest call that
/// counts as it runs, the slots they read and write, whether they
/// allocate and whether their formulas are bounded.
struct Seen {
    expected: Vec<Event>,
    arms: Vec<Arms>,
    calls: Vec<usize>,
    guest: Option<SeenGuest>,
    reads: Vec<RegionSlot>,
    write: Option<RegionSlot>,
    allocates: bool,
    bounded: bool,
}

/// A guest call the ops make: its entry, its occurrence, where its
/// statement runs on after it and the register its value goes to, and
/// the registers its continuation's roles take.
struct SeenGuest {
    act: Option<ActIx>,
    occurrence: usize,
    resume: u32,
    reg: u32,
    roles: Vec<u32>,
}

impl Check<'_, '_> {
    /// Checks `summary`, the next statement, charged `head` before its ops
    /// and ending as `end`: a guest call of it is body statement `id`'s.
    fn summary(
        &mut self,
        summary: &Summary,
        head: Weight,
        end: End,
        id: Option<StmtId>,
    ) -> Result<(), Refused> {
        let index = self.statement;
        self.statement += 1;
        let stream = self.lowered.stream;
        let ops = stream
            .ops
            .get(summary.ops.start as usize..summary.ops.end() as usize)
            .ok_or(Refused::Program { statement: index })?;
        // An operator that writes runs only as the last evaluation of a
        // `do`, whose commit applies its write.
        let root_writes = summary
            .action
            .and_then(|id| library_invoke(&(self.lowered.stmt)(id).form))
            .is_some_and(|(_, lib, _)| writes(self.lowered, lib));
        if !end.ends(ops.last()) || !writes_last(self.lowered, ops) || root_writes {
            return Err(Refused::Shape { statement: index });
        }
        let mut seen = Seen {
            expected: vec![Event::Fixed(head)],
            arms: Vec::new(),
            calls: Vec::new(),
            guest: None,
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
            guest,
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
        self.program(summary, expected, index)?;
        self.guest(summary, guest, id, index)?;
        self.calls
            .extend(summary.guest.as_ref().map(|guest| guest.occurrence));
        self.calls.extend(calls);
        Ok(())
    }

    /// Checks `summary`'s guest call against the one its ops make, `seen`,
    /// in body statement `id`: its entry, occurrence and resume, the fixed
    /// events before it, and the registers it holds across it, each read
    /// from a slot the statement reads.
    fn guest(
        &self,
        summary: &Summary,
        seen: Option<SeenGuest>,
        id: Option<StmtId>,
        index: usize,
    ) -> Result<(), Refused> {
        let shape = Refused::Shape { statement: index };
        let (stated, seen) = match (&summary.guest, seen) {
            (None, None) => return Ok(()),
            (Some(stated), Some(seen)) => (stated, seen),
            _ => return Err(Refused::Calls),
        };
        if Some(stated.stmt) != id
            || stated.act != seen.act
            || stated.occurrence != seen.occurrence
            || stated.resume != seen.resume
            || stated.reg != seen.reg
            || stated.holds.iter().map(|(reg, _)| *reg).ne(seen.roles)
        {
            return Err(shape);
        }
        let at = summary
            .program
            .iter()
            .position(|event| *event == Event::Call)
            .ok_or(Refused::Program { statement: index })?;
        let before = summary.program[..at].iter().map(|event| match event {
            Event::Fixed(weight) => Some(*weight),
            _ => None,
        });
        if stated.through != at + 1
            || before.ne(stated.before.iter().copied().map(Some))
            || stated.units
                != stated
                    .before
                    .iter()
                    .map(|weight| weight.units())
                    .sum::<u64>()
            || Some(stated.least) != summary.fixed.checked_add(self.loop_test)
        {
            return Err(Refused::Program { statement: index });
        }
        let read = |held: &Kept| match held {
            Kept::Slot(slot) => summary.reads.contains(slot),
            Kept::Constant => true,
        };
        if !stated.holds.iter().all(|(_, held)| read(held)) {
            return Err(Refused::Slots { statement: index });
        }
        Ok(())
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
                    let (action, callee, args) = root_call(&(self.lowered.stmt)(id).form)
                        .ok_or(Refused::Shape { statement: index })?;
                    // A guest call's callee is read before its arguments.
                    if let Callee::Value(Var::Local(local)) = callee {
                        seen.reads.push(RegionSlot::Frame(local.at));
                    }
                    if let Callee::Value(Var::Session(name)) = callee {
                        seen.reads.push(self.sessions.session(name));
                    }
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
                    let counted = action.guest;
                    match callee {
                        Callee::Import(_, None) => {
                            return Err(Refused::Calls);
                        }
                        Callee::Library(lib)
                        | Callee::CheckedHelper(lib)
                        | Callee::Import(_, Some(lib)) => {
                            if counted {
                                seen.calls.push(action.occurrence);
                            }
                            let scalar = scalar_site((self.lowered.lib)(*lib), args.len());
                            seen.bounded &= scalar;
                            seen.allocates |= !scalar;
                            seen.expected.push(Event::Formula);
                        }
                        Callee::Declared(_) | Callee::Value(_) => {
                            if !counted {
                                return Err(Refused::Calls);
                            }
                            seen.guest = Some(SeenGuest {
                                act: None,
                                occurrence: action.occurrence,
                                resume: pc + 1,
                                reg: 0,
                                roles: Vec::new(),
                            });
                            seen.bounded = false;
                            seen.allocates = true;
                            seen.expected.push(Event::Call);
                        }
                    }
                }
                Op::Nested { act, args } => {
                    let nested = stream
                        .nested
                        .get(act.ix())
                        .ok_or(Refused::Program { statement: index })?;
                    let guest = !nested.operator && guest_site(nested) && nested.guest;
                    if guest {
                        // A guest call is its statement's first counted op.
                        if seen.guest.is_some() || !seen.calls.is_empty() {
                            return Err(Refused::Calls);
                        }
                        seen.guest = Some(SeenGuest {
                            act: Some(act),
                            occurrence: nested.occurrence,
                            resume: pc + 1,
                            reg: args.start.0,
                            roles: nested.roles.iter().map(|role| role.reg.0).collect(),
                        });
                        seen.bounded = false;
                        seen.allocates = true;
                        seen.expected.push(Event::Call);
                    } else {
                        if matches!(nested.kind, NestedKind::Call(_)) && nested.guest {
                            seen.calls.push(nested.occurrence);
                        }
                        if let Some(lib) = nested.kind.operator_lib() {
                            seen.bounded &= scalar_site((self.lowered.lib)(lib), args.len as usize);
                        }
                        seen.expected.push(Event::Formula);
                    }
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
                                Event::Formula | Event::Arm(_) | Event::Call => 0,
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
                Event::Formula | Event::Arm(_) | Event::Call => 0,
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
                    self.summary(summary, unit, End::Write, Some(*id))?;
                }
                (Step::Bind(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Bind, Some(*id))?;
                }
                (Step::Continue(summary), Form::Plain) => {
                    self.summary(summary, unit, End::Continue, Some(*id))?;
                }
                (Step::Store(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Store, Some(*id))?;
                }
                (Step::Discard(summary), Form::Plain | Form::Action(_)) => {
                    self.summary(summary, unit, End::Discard, Some(*id))?;
                }
                // A region leaves to the reference only a barrier, and what
                // follows one.
                (Step::Leave, _)
                    if left
                        || barrier(self.lowered, statement)
                        || guest_call(self.lowered, statement) =>
                {
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
                    self.summary(test, unit, End::Branch, None)?;
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
            loop_test,
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
                check.summary(test, Weight::Atomic(*metered), End::Test, None)?;
                (*body, false)
            }
            (
                Head::For {
                    charge,
                    guarded,
                    binding,
                    item,
                    check: guard,
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
                    || *item
                        != via
                            .and_then(|via| via.item)
                            .and_then(crate::compile::LinkedHook::library)
                    || item.is_some_and(|item| writes(lowered, item))
                    || guard.is_some_and(|guard| writes(lowered, guard))
                    || *guard
                        != via
                            .and_then(|via| via.check)
                            .and_then(crate::compile::LinkedHook::library)
                {
                    return Err(head);
                }
                check.statement += 1;
                let charges = std::iter::once((charge, None)).chain(
                    guarded
                        .iter()
                        .map(|guarded| (&**guarded, guard.map(|guard| (guard, 4)))),
                );
                for (summary, guard) in charges {
                    if summary.write != Some(RegionSlot::Frame(bound.at))
                        || !summary.reads.is_empty()
                    {
                        return Err(head);
                    }
                    let hooks: Vec<(LibId, usize)> = guard
                        .into_iter()
                        .chain(item.map(|item| (item, 2)))
                        .collect();
                    let scalar = hooks
                        .iter()
                        .all(|(hook, count)| scalar_site((lowered.lib)(*hook), *count));
                    if summary.general != !scalar || summary.formula_max.is_none() == scalar {
                        return Err(Refused::Allocates { statement: 0 });
                    }
                    let mut expected = vec![Event::Fixed(Weight::Atomic(*metered))];
                    expected.extend(hooks.iter().map(|_| Event::Formula));
                    check.program(summary, expected, 0)?;
                }
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
        // Only an allocating plan walks a guarded table.
        if let Head::For {
            guarded: Some(_), ..
        } = &self.head
            && !allocates
        {
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
