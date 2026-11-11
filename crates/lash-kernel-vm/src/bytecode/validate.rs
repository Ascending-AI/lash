//! The validator (cutover contract §1): one pass after emission checks a
//! [`Program`]'s structure, so the dispatcher reads its operands and
//! tables unchecked.
//!
//! It checks that every table an op names has the entry, that the blocks
//! partition each unit and that their charge and suffix tables sum
//! as the dispatcher bills them, that control enters blocks only at their
//! heads, and that the operand stack has one depth at every op and never
//! underflows. Semantic checks (types, bounds, guards) stay in the
//! dispatcher.

use std::fmt;

use super::op::Op;
use super::program::{ActionSite, Callee, Capture, Constant, Mark, Program, Unit, UnitId};

/// A structural defect: the compiler emitted a program the dispatcher
/// cannot run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed {
    pub unit: UnitId,
    /// The op, where the defect is one op's.
    pub pc: Option<u32>,
    pub problem: String,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "unit {} op {}: {}", self.unit, pc, self.problem),
            None => write!(f, "unit {}: {}", self.unit, self.problem),
        }
    }
}

type Checked = Result<(), Malformed>;

/// Checks `program`'s structure, giving each unit's operand height: the
/// most operands its frames hold above their locals, which the memory
/// schedule admits at frame entry (contract §2).
pub fn validate(program: &Program) -> Result<Vec<u32>, Malformed> {
    Ok(checked(program)?
        .into_iter()
        .map(|(height, _)| height)
        .collect())
}

/// A checked unit's operand height and each of its ops' operand depth.
pub(crate) type Depths = (u32, Vec<Option<u32>>);

/// Checks `program`'s own units, giving each one's operand height and
/// each of its ops' operand depth. The library's units it names were
/// checked when the library published them.
pub(crate) fn checked(program: &Program) -> Result<Vec<Depths>, Malformed> {
    let whole = |problem: String| Malformed {
        unit: program.main,
        pc: None,
        problem,
    };
    let unit = |id: UnitId| program.get_unit(id).is_some();
    let charge = |id: u32| program.get_charge(id).is_some();
    if program.tag == 0
        && program
            .own_index(program.main)
            .is_none_or(|at| at >= program.units.len())
    {
        return Err(whole(format!(
            "main is unit {} of {}",
            program.main,
            program.units.len()
        )));
    }
    for entry in &program.functions {
        if !unit(entry.unit) {
            return Err(whole(format!(
                "function {:?} names no unit",
                entry.callable
            )));
        }
    }
    for native in &program.natives {
        if !charge(native.charge)
            || native.guard.is_some_and(|guard| !charge(guard))
            || native.body.is_some_and(|body| !unit(body))
        {
            return Err(whole(format!(
                "native {} names a missing table entry",
                native.name
            )));
        }
    }
    captures(program)?;
    let mut checked = Vec::with_capacity(program.units.len());
    for (at, unit) in program.units.iter().enumerate() {
        checked.push(Unit::check(unit, program, program.own_unit(at))?);
    }
    for (at, unit) in program.units.iter().enumerate() {
        for n in 0..unit.inlines.len() {
            unit.check_region(program, program.own_unit(at), n, &checked)?;
        }
    }
    Ok(checked)
}

/// Every closure site captures as many cells as the unit it makes says.
fn captures(program: &Program) -> Result<(), Malformed> {
    for (maker, unit) in program.units.iter().enumerate() {
        for site in &unit.closures {
            let problem = |problem: String| Malformed {
                unit: program.own_unit(maker),
                pc: None,
                problem,
            };
            let Some(made) = program.get_unit(site.unit) else {
                return Err(problem(format!("a closure of missing unit {}", site.unit)));
            };
            if usize::from(made.captures) != site.captures.len() {
                return Err(problem(format!(
                    "a closure of unit {} captures {} variables of its {}",
                    site.unit,
                    site.captures.len(),
                    made.captures
                )));
            }
        }
    }
    Ok(())
}

impl Unit {
    fn check(&self, program: &Program, id: UnitId) -> Result<(u32, Vec<Option<u32>>), Malformed> {
        let at = |pc: usize, problem: String| Malformed {
            unit: id,
            pc: Some(pc as u32),
            problem,
        };
        let whole = |problem: String| Malformed {
            unit: id,
            pc: None,
            problem,
        };
        let len = self.ops.len();
        let parallel = [
            ("event", self.event.len()),
            ("step", self.step.len()),
            ("through", self.through.len()),
        ];
        for (table, size) in parallel {
            if size != len {
                return Err(whole(format!("{table} has {size} entries for {len} ops")));
            }
        }
        if self.action_marks.len() != self.actions.len() {
            return Err(whole("an action without its mark".into()));
        }
        if self.params > self.locals {
            return Err(whole(format!(
                "{} params in {} locals",
                self.params, self.locals
            )));
        }
        if !self.boxed.is_sorted_by(|a, b| a < b) || self.boxed.iter().any(|&n| n >= self.locals) {
            return Err(whole(
                "the boxed locals are not distinct locals in order".into(),
            ));
        }
        if !self.marked() && self.step.iter().any(|&step| step) {
            return Err(whole("unmarked code makes a step".into()));
        }
        self.check_blocks(id)?;
        self.check_inlines(program, id)?;
        let local = |n: u16| n < self.locals;
        let marked = |mark: &Mark| mark.loops.iter().all(|lp| local(lp.started));
        for site in &self.calls {
            let fits = match site.callee {
                Callee::Native { native, .. } => program.get_native(native).is_some(),
                Callee::Unit { unit, charge } => {
                    program.get_unit(unit).is_some()
                        && charge.is_none_or(|c| program.get_charge(c).is_some())
                }
                Callee::Value => true,
                Callee::Machine { charge, .. } => {
                    charge.is_none_or(|c| program.get_charge(c).is_some())
                }
            };
            if !fits || !marked(&site.mark) {
                return Err(whole(format!(
                    "call at {} names a missing entry",
                    site.site
                )));
            }
        }
        for (action, mark) in self.actions.iter().zip(&self.action_marks) {
            if let ActionSite::Spawn { unit, .. } = action
                && program.get_unit(*unit).is_none()
            {
                return Err(whole(format!("a spawn of missing unit {unit}")));
            }
            if !marked(mark) {
                return Err(whole(format!(
                    "action at {} counts no local",
                    action.site()
                )));
            }
        }
        let own = usize::from(self.captures);
        for site in &self.closures {
            let fits = site.captures.iter().all(|capture| match *capture {
                Capture::Local(n) => local(n),
                Capture::Capture(n) => usize::from(n) < own,
            });
            if !fits {
                return Err(whole(format!(
                    "closure of unit {} captures past its maker",
                    site.unit
                )));
            }
        }
        let text = |n: u32| {
            matches!(
                program.get_constant(n),
                Some(Constant::Text(_) | Constant::Immediate(lash_kernel_doc::Value::Text(_)))
            )
        };
        if self
            .records
            .iter()
            .flat_map(|shape| shape.iter())
            .any(|&n| !text(n))
        {
            return Err(whole("a record field is not a text constant".into()));
        }
        let head = |to: u32| matches!(self.ops.get(to as usize), Some(Op::Block(_)));
        // Control stays in the unit's own ops or in one region.
        let region = |pc: u32| {
            self.inlines
                .iter()
                .position(|site| (site.entry..site.end).contains(&pc))
        };
        let within = |from: usize, to: u32| head(to) && region(from as u32) == region(to);
        for site in &self.loops {
            let ends = [Some(site.exit), site.driver, site.join];
            if !local(site.binding) || !local(site.slot.saturating_add(1)) {
                return Err(whole(format!(
                    "loop at {} keeps state past the locals",
                    site.site
                )));
            }
            if ends.into_iter().flatten().any(|to| !head(to)) {
                return Err(whole(format!(
                    "loop at {} leaves into a block's middle",
                    site.site
                )));
            }
        }
        if !self.points.is_sorted_by_key(|(pc, _)| *pc)
            || self
                .points
                .last()
                .is_some_and(|(pc, _)| *pc as usize >= len)
        {
            return Err(whole("the points are not in op order".into()));
        }
        for (pc, op) in self.ops.iter().enumerate() {
            // A region's ops address its body's locals.
            let local = |n: u16| match region(pc as u32) {
                Some(k) => n < program.unit(self.inlines[k].callee).locals,
                None => local(n),
            };
            let fits = match *op {
                Op::Block(_) | Op::Nop | Op::Pop | Op::Index | Op::SetIndex => true,
                Op::SetIndexPlaceFirst | Op::RemoveIndex | Op::Return | Op::Fall => true,
                Op::Raise | Op::TryExit | Op::Print | Op::Finish | Op::Fail => true,
                Op::MakeTuple(_) | Op::MakeList(_) | Op::MakeMap(_) | Op::MakeSet(_) => true,
                Op::Const(n) => program.get_constant(n).is_some(),
                Op::Local(n) | Op::SetLocal(n) | Op::CellGet(n) | Op::CellSet(n) | Op::Box(n) => {
                    local(n)
                }
                Op::LoopStart(n) | Op::Pass(n) => local(n),
                Op::CaptureGet(n) | Op::CaptureSet(n) => usize::from(n) < own,
                Op::Global(n) | Op::SetGlobal(n) => usize::from(n) < program.globals.len(),
                Op::Jump(to) | Op::JumpIfFalse(to) | Op::WhileFalse(to) | Op::TryEnter(to) => {
                    within(pc, to)
                }
                Op::MakeRecord(n) => (n as usize) < self.records.len(),
                Op::MakeClosure(n) => (n as usize) < self.closures.len(),
                Op::Field(n)
                | Op::SetField(n)
                | Op::SetFieldPlaceFirst(n)
                | Op::RemoveField(n)
                | Op::Unavailable(n) => text(n),
                Op::Call(n) => (n as usize) < self.calls.len(),
                Op::ForOpen(n) | Op::ForNext(n) | Op::ProtoStep(n) | Op::ProtoMore(n) => {
                    (n as usize) < self.loops.len()
                }
                Op::ProtoOpen(n) | Op::ProtoGuard(n) | Op::ProtoChecked(n) | Op::ProtoPull(n) => {
                    // A protocol loop keeps four more hidden locals.
                    self.loops
                        .get(n as usize)
                        .is_some_and(|site| local(site.slot.saturating_add(5)))
                }
                Op::Action(n) => (n as usize) < self.actions.len(),
                // Fusion writes fused runs, and the trace pass traces,
                // after validation.
                Op::Fused(_) | Op::Trace(_) => false,
                Op::Inline(n) => self
                    .inlines
                    .get(n as usize)
                    .is_some_and(|site| site.at == pc as u32),
                Op::Leave(n) => region(pc as u32) == Some(n as usize),
                // Checked against the body with the region.
                Op::Deopt(_) => region(pc as u32).is_some(),
            };
            if !fits {
                return Err(at(pc, format!("{op:?} names a missing entry")));
            }
            // A transfer ends its block: the reservation is spent when
            // control leaves, and the next op the dispatcher runs is a
            // block's head.
            let transfers = matches!(
                op,
                Op::Jump(_)
                    | Op::JumpIfFalse(_)
                    | Op::WhileFalse(_)
                    | Op::ForNext(_)
                    | Op::ProtoStep(_)
                    | Op::ProtoGuard(_)
                    | Op::ProtoMore(_)
            );
            if transfers && pc + 1 < len && !head(pc as u32 + 1) {
                return Err(at(pc, format!("{op:?} does not end its block")));
            }
        }
        let (height, depth) = self.check_stack(id)?;
        self.check_sites(id, &depth)?;
        Ok((height, depth))
    }

    /// Each inline site names its call of its body and a region of its
    /// own past the unit's ops, which no step or point stands in.
    fn check_inlines(&self, program: &Program, id: UnitId) -> Checked {
        let whole = |problem: String| Malformed {
            unit: id,
            pc: None,
            problem,
        };
        let own = self
            .inlines
            .first()
            .map_or(self.ops.len(), |site| site.entry as usize);
        let mut next = own as u32;
        for (n, site) in self.inlines.iter().enumerate() {
            let calls = self.calls.get(site.call as usize).map(|call| call.callee);
            let named = matches!(
                calls,
                Some(Callee::Unit { unit, .. }) if unit == site.callee
            ) && program.get_unit(site.callee).is_some()
                && site.callee != id;
            if !named
                || (site.at as usize) >= own
                || self.ops[site.at as usize] != Op::Inline(n as u32)
            {
                return Err(whole(format!("inline site {n} names no call of its body")));
            }
            if site.entry != next || site.end <= site.entry || site.end as usize > self.ops.len() {
                return Err(whole(format!("inline site {n}'s region does not follow")));
            }
            let (entry, end) = (site.entry as usize, site.end as usize);
            if self.step[entry..end].iter().any(|&step| step)
                || self
                    .points
                    .iter()
                    .any(|(pc, _)| (entry..end).contains(&(*pc as usize)))
            {
                return Err(whole(format!("inline site {n}'s region steps or is seen")));
            }
            let steps = program.get_unit(site.callee).is_some_and(|body| body.steps);
            if site.steps != steps
                || (steps
                    && self.event[entry..end]
                        .iter()
                        .any(|event| event.total() != 0))
            {
                return Err(whole(format!(
                    "inline site {n}'s region bills where its body steps, or names its steps wrongly"
                )));
            }
            next = site.end;
        }
        if next as usize != self.ops.len() {
            return Err(whole("ops follow the last inline region".into()));
        }
        Ok(())
    }

    /// Region `n` runs from its entry over its body's locals: every op it
    /// reaches sees one operand depth, it leaves only by `Op::Leave`, a
    /// raise or a `Op::Deopt` whose depth is the body's at that op, and
    /// it holds no more operands than the body's frame admits.
    fn check_region(
        &self,
        program: &Program,
        id: UnitId,
        n: usize,
        checked: &[(u32, Vec<Option<u32>>)],
    ) -> Checked {
        let site = self.inlines[n];
        let body = program.unit(site.callee);
        let (admitted, body_depth): (u32, &[Option<u32>]) = match program.own_index(site.callee) {
            Some(at) => (checked[at].0, &checked[at].1),
            None => match program.lib.regions.get(&site.callee) {
                Some((height, depth)) => (*height, depth),
                None => {
                    return Err(Malformed {
                        unit: id,
                        pc: None,
                        problem: format!("inline site {n} expands a library unit it cannot"),
                    });
                }
            },
        };
        let (entry, end) = (site.entry as usize, site.end as usize);
        let mut depth: Vec<Option<u32>> = vec![None; end - entry];
        let mut work = vec![(entry, 0u32)];
        while let Some((pc, d)) = work.pop() {
            let at = |problem: String| Malformed {
                unit: id,
                pc: Some(pc as u32),
                problem,
            };
            if !(entry..end).contains(&pc) {
                return Err(at(format!("control leaves inline region {n}")));
            }
            match depth[pc - entry] {
                Some(known) if known == d => continue,
                Some(known) => {
                    return Err(at(format!("reached at depth {known} and at depth {d}")));
                }
                None => depth[pc - entry] = Some(d),
            }
            let op = self.ops[pc];
            let (pops, pushes) = self.effect(op);
            let Some(after) = d.checked_sub(pops) else {
                return Err(at(format!("{op:?} pops {pops} of {d} operands")));
            };
            let after = after + pushes;
            if d.max(after) > admitted {
                return Err(at(format!(
                    "region {n} holds {} operands where its body admits {admitted}",
                    d.max(after)
                )));
            }
            match op {
                Op::Leave(_) | Op::Raise => {}
                Op::Deopt(to) => {
                    let head = matches!(body.original(to as usize), Some(Op::Block(_)));
                    if !head || body_depth[to as usize] != Some(d) {
                        return Err(at(format!(
                            "region {n} leaves to its body's op {to} at depth {d}"
                        )));
                    }
                }
                Op::Jump(to) => work.push((to as usize, after)),
                Op::JumpIfFalse(to) => {
                    work.push((to as usize, after));
                    work.push((pc + 1, after));
                }
                Op::Call(_)
                | Op::Nop
                | Op::Block(_)
                | Op::Const(_)
                | Op::Local(_)
                | Op::SetLocal(_)
                | Op::Pop
                | Op::Index
                | Op::Field(_)
                | Op::MakeTuple(_)
                | Op::MakeList(_) => work.push((pc + 1, after)),
                _ => return Err(at(format!("{op:?} runs in inline region {n}"))),
            }
        }
        Ok(())
    }

    /// The blocks partition the ops, each headed by its own `Block` op,
    /// which bills nothing and starts no step, and their charge and
    /// suffix tables sum the ops' events.
    fn check_blocks(&self, id: UnitId) -> Checked {
        let whole = |problem: String| Malformed {
            unit: id,
            pc: None,
            problem,
        };
        let mut next = 0u32;
        for (k, block) in self.blocks.iter().enumerate() {
            let at = |problem: String| Malformed {
                unit: id,
                pc: Some(block.start),
                problem,
            };
            if block.start != next || block.end <= block.start {
                return Err(at(format!("block {k} does not follow the block before")));
            }
            let (start, end) = (block.start as usize, block.end as usize);
            if end > self.ops.len() || self.ops[start] != Op::Block(k as u32) {
                return Err(at(format!("block {k} is not headed by its own op")));
            }
            if !self.event[start].is_none() || self.through[start] != 0 || self.step[start] {
                return Err(at(format!("block {k}'s head bills or starts a step")));
            }
            let mut charge = Some(0u64);
            let mut sum = 0u64;
            for pc in start + 1..end {
                if matches!(self.ops[pc], Op::Block(_)) {
                    return Err(at(format!("block {k} holds another block's head")));
                }
                let cost = self.event[pc].total();
                charge = charge.and_then(|total| total.checked_add(cost));
                sum = sum.saturating_add(cost);
                if self.through[pc] != sum {
                    return Err(Malformed {
                        unit: id,
                        pc: Some(pc as u32),
                        problem: format!("block {k}'s suffix table does not sum its events"),
                    });
                }
            }
            if block.charge != charge {
                return Err(at(format!("block {k}'s cost is not its events' sum")));
            }
            next = block.end;
        }
        if next as usize != self.ops.len() {
            return Err(whole(format!(
                "the blocks cover {next} of {} ops",
                self.ops.len()
            )));
        }
        Ok(())
    }

    /// Every op reached from the entry, or from a handler, sees one
    /// operand depth, and no op pops past the frame's locals. Gives the
    /// greatest depth any op reaches, and each op's depth.
    fn check_stack(&self, id: UnitId) -> Result<(u32, Vec<Option<u32>>), Malformed> {
        let len = self.ops.len();
        let mut height = 0u32;
        let mut depth: Vec<Option<u32>> = vec![None; len];
        let mut work = vec![(0usize, 0u32)];
        // A raise leaving the root body goes on at the cleanup's raise
        // path with the raised value.
        work.extend(self.cleanup.map(|cleanup| (cleanup.raised as usize, 1)));
        while let Some((pc, d)) = work.pop() {
            let at = |problem: String| Malformed {
                unit: id,
                pc: Some(pc as u32),
                problem,
            };
            let Some(seen) = depth.get_mut(pc) else {
                return Err(at("control runs off the unit's end".into()));
            };
            match *seen {
                Some(known) if known == d => continue,
                Some(known) => {
                    return Err(at(format!("reached at depth {known} and at depth {d}")));
                }
                None => *seen = Some(d),
            }
            let op = self.ops[pc];
            let (pops, pushes) = self.effect(op);
            let Some(after) = d.checked_sub(pops) else {
                return Err(at(format!("{op:?} pops {pops} of {d} operands")));
            };
            let after = after + pushes;
            height = height.max(d).max(after);
            let mut go = |to: usize, d: u32| work.push((to, d));
            match op {
                Op::Return | Op::Fall | Op::Raise | Op::Finish | Op::Fail | Op::Unavailable(_) => {}
                Op::Leave(_) | Op::Deopt(_) => {}
                Op::Jump(to) => go(to as usize, after),
                Op::JumpIfFalse(to) | Op::WhileFalse(to) => {
                    go(to as usize, after);
                    go(pc + 1, after);
                }
                // A raise truncates to the depth at entry and pushes the
                // raised value.
                Op::TryEnter(to) => {
                    go(to as usize, after + 1);
                    go(pc + 1, after);
                }
                Op::ForNext(n) | Op::ProtoMore(n) => {
                    go(self.loops[n as usize].exit as usize, after);
                    go(pc + 1, after);
                }
                Op::ProtoStep(n) => {
                    if let Some(driver) = self.loops[n as usize].driver {
                        go(driver as usize, after);
                    }
                    go(pc + 1, after);
                }
                Op::ProtoGuard(n) => {
                    if let Some(join) = self.loops[n as usize].join {
                        go(join as usize, d);
                    }
                    go(pc + 1, after);
                }
                _ => go(pc + 1, after),
            }
        }
        Ok((height, depth))
    }

    /// The resume table names a point at every call and action and at
    /// every step, and each point's stack is the depth its pc runs at
    /// (contract §5): a call's under its operands, an action's after it
    /// with its value pushed, and its roles within it.
    fn check_sites(&self, id: UnitId, depth: &[Option<u32>]) -> Checked {
        use super::site_tables::Phase;
        let refuse = |pc: u32, problem: String| {
            Err(Malformed {
                unit: id,
                pc: Some(pc),
                problem,
            })
        };
        for (pc, op) in self.ops.iter().enumerate() {
            let Some(d) = depth[pc] else {
                continue;
            };
            let pc = pc as u32;
            let mut wanted: Vec<(Phase, u32, u32)> = Vec::new();
            if self.stands(pc as usize) {
                let phase = match op {
                    Op::Jump(to) if *to < pc => Phase::Back,
                    _ => Phase::Statement,
                };
                wanted.push((phase, pc, d));
            }
            let (pops, pushes) = self.effect(*op);
            match op {
                Op::Call(_) | Op::Inline(_) => wanted.push((Phase::Call, pc + 1, d - pops)),
                Op::Action(n) => {
                    let kind = super::site_tables::ActionKind::of(&self.actions[*n as usize]);
                    wanted.push((Phase::Ready, pc, d));
                    if kind.waits() {
                        let kept = kind.kept_after_issue().len() as u32;
                        wanted.push((Phase::Issued, pc + 1, d - pops + kept));
                    }
                    wanted.push((Phase::Went, pc + 1, d - pops + pushes));
                }
                _ => {}
            }
            for (phase, at, height) in wanted {
                let Some(entry) = self.sites.at(at, phase) else {
                    return refuse(pc, format!("no resume point for {phase:?} at {at}"));
                };
                if entry.resume.stack.len() != height as usize
                    || usize::from(entry.held) > entry.resume.stack.len()
                {
                    return refuse(
                        pc,
                        format!(
                            "the {phase:?} point holds {} values at depth {height}",
                            entry.resume.stack.len()
                        ),
                    );
                }
                if self.sites.resume(&entry.site, phase, entry.copy) != Some(entry) {
                    return refuse(pc, format!("the {phase:?} point is not found by its site"));
                }
            }
        }
        // Every slot is named, and found again by its name.
        let locals = self.sites.locals();
        if locals.len() != usize::from(self.locals) {
            return Err(Malformed {
                unit: id,
                pc: None,
                problem: format!("{} of {} slots are named", locals.len(), self.locals),
            });
        }
        for local in locals {
            if self.sites.local(&local.site, &local.role, local.copy) != Some(local) {
                return Err(Malformed {
                    unit: id,
                    pc: None,
                    problem: format!("slot {} is not found by its site", local.slot),
                });
            }
        }
        Ok(())
    }

    /// What `op` pops and then pushes (`ProtoGuard` as when it guards).
    pub(super) fn effect(&self, op: Op) -> (u32, u32) {
        match op {
            Op::Block(_)
            | Op::Nop
            | Op::Box(_)
            | Op::Jump(_)
            | Op::TryEnter(_)
            | Op::TryExit
            | Op::ForNext(_)
            | Op::ProtoStep(_)
            | Op::ProtoMore(_)
            | Op::LoopStart(_)
            | Op::Pass(_)
            | Op::Fall
            | Op::Unavailable(_)
            | Op::Deopt(_)
            | Op::Fused(_)
            | Op::Trace(_) => (0, 0),
            Op::Const(_)
            | Op::Local(_)
            | Op::CellGet(_)
            | Op::CaptureGet(_)
            | Op::Global(_)
            | Op::MakeClosure(_) => (0, 1),
            Op::SetLocal(_)
            | Op::CellSet(_)
            | Op::CaptureSet(_)
            | Op::SetGlobal(_)
            | Op::Pop
            | Op::JumpIfFalse(_)
            | Op::WhileFalse(_)
            | Op::Return
            | Op::Raise
            | Op::Finish
            | Op::Fail
            | Op::Print
            | Op::ForOpen(_)
            | Op::ProtoOpen(_)
            | Op::ProtoChecked(_)
            | Op::ProtoPull(_)
            | Op::RemoveField(_)
            | Op::Leave(_) => (1, 0),
            Op::MakeTuple(n) | Op::MakeList(n) | Op::MakeSet(n) => (n, 1),
            Op::MakeMap(n) => (n.saturating_mul(2), 1),
            Op::MakeRecord(n) => (self.records[n as usize].len() as u32, 1),
            Op::Index => (2, 1),
            Op::Field(_) => (1, 1),
            Op::SetIndex | Op::SetIndexPlaceFirst => (3, 0),
            Op::SetField(_) | Op::SetFieldPlaceFirst(_) | Op::RemoveIndex => (2, 0),
            Op::Call(n) => {
                let site = &self.calls[n as usize];
                let callee = u32::from(matches!(site.callee, Callee::Value));
                (u32::from(site.argc) + callee, 1)
            }
            Op::Inline(n) => self.effect(Op::Call(self.inlines[n as usize].call)),
            Op::ProtoGuard(_) => (0, 4),
            Op::Action(n) => {
                let pops = match &self.actions[n as usize] {
                    ActionSite::Perform { argc, .. } | ActionSite::Spawn { argc, .. } => {
                        u32::from(*argc)
                    }
                    ActionSite::Yield { .. } => 0,
                    ActionSite::Join { .. }
                    | ActionSite::JoinMany { .. }
                    | ActionSite::Cancel { .. }
                    | ActionSite::CancelEffect { .. } => 1,
                };
                (pops, 1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash_kernel_doc::Value;
    use lash_kernel_value::sites::{Site, Unit as SiteUnit};

    use super::*;
    use crate::bytecode::program::{Block, Event, UnitKind};

    /// `if true { true; } return true`, by hand: block 0 tests and jumps
    /// past block 1 to block 2, each op billing one unit.
    fn program() -> Program {
        let ops = vec![
            Op::Block(0),
            Op::Const(0),
            Op::JumpIfFalse(6),
            Op::Block(1),
            Op::Const(0),
            Op::Pop,
            Op::Block(2),
            Op::Const(0),
            Op::Return,
        ];
        let heads = [0, 3, 6];
        let one = Event {
            units: 1,
            atomic: 0,
        };
        let event = (0..ops.len())
            .map(|pc| {
                if heads.contains(&pc) {
                    Event::default()
                } else {
                    one
                }
            })
            .collect();
        let through = vec![0, 1, 2, 0, 1, 2, 0, 1, 2];
        let block = |start, end| Block {
            start,
            end,
            charge: Some(2),
        };
        let mut unit = Unit {
            kind: UnitKind::Main,
            call: false,
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            covered: false,
            steps: true,
            params: 0,
            locals: 0,
            captures: 0,
            boxed: Vec::new(),
            step: vec![false; ops.len()],
            ops,
            event,
            through,
            blocks: vec![block(0, 3), block(3, 6), block(6, 9)],
            records: Vec::new(),
            closures: Vec::new(),
            calls: Vec::new(),
            loops: Vec::new(),
            actions: Vec::new(),
            action_marks: Vec::new(),
            authored: true,
            name: Arc::from("main"),
            owner: None,
            site: Site::new(SiteUnit::Main, Vec::new()),
            points: Vec::new(),
            fused: Vec::new(),
            traces: Vec::new(),
            micros: Vec::new(),
            inlines: Vec::new(),
            folds: Vec::new(),
            origin: Vec::new(),
            stated: Vec::new(),
            declared: Vec::new(),
            sites: Default::default(),
            cleanup: None,
        };
        unit.sites = super::super::site_tables::build(&unit);
        Program {
            units: vec![unit],
            constants: vec![Constant::Immediate(Value::Bool(true))],
            natives: Vec::new(),
            charges: Vec::new(),
            tag: 0,
            lib: Default::default(),
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            links: vec![lash_kernel_value::LinkId::from_bytes([1; 32])],
            functions: Vec::new(),
            declared: Default::default(),
            globals: Vec::new(),
            main: 0,
        }
    }

    /// Law: each structural defect the dispatcher would run unchecked is
    /// refused, at the op that has it.
    #[test]
    fn a_defect_the_dispatcher_would_trust_is_refused() {
        assert!(validate(&program()).is_ok());
        type Corrupt = fn(&mut Unit);
        let defects: [(&str, Option<u32>, Corrupt); 6] = [
            // Falling into block 2 leaves an operand the jump does not.
            ("two depths at a join", Some(6), |u| u.ops[5] = Op::Nop),
            ("a jump into a block's middle", Some(2), |u| {
                u.ops[2] = Op::JumpIfFalse(7)
            }),
            ("a pop past the locals", Some(7), |u| u.ops[7] = Op::Pop),
            ("a missing constant", Some(4), |u| u.ops[4] = Op::Const(1)),
            ("a block cost that is not its sum", Some(6), |u| {
                u.blocks[2].charge = Some(3)
            }),
            ("a suffix that is not its sum", Some(4), |u| {
                u.through[4] = 2
            }),
        ];
        for (defect, pc, corrupt) in defects {
            let mut program = program();
            corrupt(&mut program.units[0]);
            let refused = validate(&program).expect_err(defect);
            assert_eq!(refused.pc, pc, "{defect}: {refused}");
        }
    }
}
