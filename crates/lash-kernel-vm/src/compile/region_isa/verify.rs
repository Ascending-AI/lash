//! Verification reads admitted syntax and canonical resource/charge definitions.
//! It does not invoke the region emitter or execute a candidate semantic path.
use super::*;
mod types;
mod via;
use crate::compile::room::charges::region_isa as canonical;
use crate::compile::room::publication::Successor;
use crate::compile::room::{CallSource, Measure, Owner};
use lash_kernel_state::kernel_internals::logical::Owner as Root;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn register(draft: &RegionDraft, owner: &Owner) -> Result<Reg<LeaseValue>, Refused> {
    let slots = draft.source.owner_slots()?;
    if matches!(
        slots.root(owner, doc::TaskId::MAIN, 0),
        Some(Root::Incoming(_))
    ) {
        return Reg::new(draft.roles.len());
    }
    let Some(role) = slots
        .root(owner, doc::TaskId::MAIN, 0)
        .and_then(resources::root_role)
    else {
        return Err(Refused::Facts);
    };
    draft
        .roles
        .iter()
        .find(|(actual, _)| *actual == role)
        .map(|(_, reg)| *reg)
        .ok_or(Refused::Facts)
}
fn source_next(source: &Source, at: &Point) -> Result<Option<Point>, Refused> {
    let node = resources::node(source, &at.site)?;
    if at.site == source.site && at.phase == Phase::Leave {
        return Ok(None);
    }
    if at.phase == Phase::Enter
        && let doc::Node::Block(body) = node
    {
        return Ok(Some(if body.is_empty() {
            Point {
                site: at.site.clone(),
                phase: if at.site == source.site {
                    Phase::Fallthrough
                } else {
                    Phase::Leave
                },
            }
        } else {
            source.first_event(&at.site.child(0))?
        }));
    }
    if at.phase == Phase::Fallthrough {
        return Ok(Some(Point {
            site: at.site.clone(),
            phase: Phase::Leave,
        }));
    }
    // Finishing or failing ends the program: nothing follows its Commit.
    if at.phase == Phase::Commit
        && matches!(
            node,
            doc::Node::Stmt(doc::Stmt::Finish { .. } | doc::Stmt::Fail { .. })
        )
    {
        return Ok(None);
    }
    // A raise no try of the frame encloses leaves the frame from its Commit.
    if at.phase == Phase::Commit
        && matches!(node, doc::Node::Stmt(doc::Stmt::Throw { .. }))
        && super::departures::uncaught(source, at)?
    {
        return Ok(None);
    }
    // A protocol `for`'s Test starts the pass it binds.
    if at.phase == Phase::Test
        && matches!(node, doc::Node::Stmt(doc::Stmt::For { via: Some(_), .. }))
    {
        return source.first_event(&at.site.child(1)).map(Some);
    }
    // A compound statement's Commit continues after it in its block.
    if at.phase == Phase::Commit
        && matches!(
            node,
            doc::Node::Stmt(
                doc::Stmt::If { .. } | doc::Stmt::Try(_) | doc::Stmt::For { via: Some(_), .. }
            )
        )
    {
        let mut parent = at.site.clone();
        let index = parent.path.pop().ok_or(Refused::Path)? as usize;
        let doc::Node::Block(body) = resources::node(source, &parent)? else {
            return Err(Refused::Path);
        };
        return Ok(Some(if index + 1 < body.len() {
            source
                .first_event(&parent.child(u32::try_from(index + 1).map_err(|_| Refused::Path)?))?
        } else {
            Point {
                phase: if parent == source.site {
                    Phase::Fallthrough
                } else {
                    Phase::Leave
                },
                site: parent,
            }
        }));
    }
    if at.phase == Phase::Commit
        && matches!(
            node,
            doc::Node::Stmt(
                doc::Stmt::Return { .. }
                    | doc::Stmt::ReturnVia { .. }
                    | doc::Stmt::Throw { .. }
                    | doc::Stmt::Finish { .. }
                    | doc::Stmt::Fail { .. }
                    | doc::Stmt::Break
                    | doc::Stmt::Continue
            )
        )
    {
        return Ok(None);
    }
    if at.phase == Phase::Leave {
        let mut parent = at.site.clone();
        let index = parent.path.pop().ok_or(Refused::Path)?;
        match resources::node(source, &parent)? {
            doc::Node::Stmt(doc::Stmt::If { .. }) => {
                return Ok(Some(Point {
                    site: parent,
                    phase: Phase::Commit,
                }));
            }
            doc::Node::Stmt(doc::Stmt::For { .. }) if index == 1 => {
                return Ok(Some(Point {
                    site: parent,
                    phase: Phase::Test,
                }));
            }
            // A loop's `else` finishes at the loop's own Commit.
            doc::Node::Stmt(doc::Stmt::For { .. } | doc::Stmt::While { .. })
                if index == 2
                    && source.room.events.contains_key(&Point {
                        site: parent.clone(),
                        phase: Phase::Commit,
                    }) =>
            {
                return Ok(Some(Point {
                    site: parent,
                    phase: Phase::Commit,
                }));
            }
            _ => {}
        }
    }
    if matches!(at.phase, Phase::Commit | Phase::Restore) && at.site != source.site {
        let mut parent = at.site.clone();
        if parent.path.pop() == Some(0)
            && let doc::Node::Stmt(doc::Stmt::For { via, .. }) = resources::node(source, &parent)?
        {
            // A protocol `for` opens what its iterable gave.
            return Ok(Some(Point {
                site: parent,
                phase: if via.is_some() {
                    Phase::Open
                } else {
                    Phase::Enter
                },
            }));
        }
    }
    source.exact_successor(at, Successor::Normal).map(Some)
}

fn check_edge(draft: &RegionDraft, index: usize) -> Result<(), Refused> {
    let step = &draft.steps[index];
    if step.stage.is_some() {
        return super::stages::check(draft, index);
    }
    if super::departures::contains(draft, index) {
        return super::departures::check_edge(draft, index);
    }
    let source = &draft.source;
    let target = |pc: Pc| {
        draft
            .steps
            .get(pc.offset())
            .and_then(|step| match step.stage {
                Some(stage) => draft.loops.get(stage.info as usize).map(|info| {
                    if stage.phase == LoopPhase::Enter {
                        &info.entry
                    } else {
                        &info.test
                    }
                }),
                None => Some(&step.at),
            })
            .ok_or(Refused::Path)
    };
    match &draft.ops[index] {
        RegionOp::Boundary { .. } => {}
        RegionOp::Call { site, next } => {
            if let Some(hook) = draft.calls.get(site.offset()).and_then(|call| call.hook) {
                return via::hook_edge(draft, step, hook, *next);
            }
            // A statement call's completion is taken at its Commit, a
            // nested call's at its distinct Restore.
            let expected = Point {
                site: step.at.site.clone(),
                phase: match resources::node(source, &step.at.site)? {
                    doc::Node::Action(_) => Phase::Commit,
                    _ => Phase::Restore,
                },
            };
            if target(*next)? != &expected || !source.room.events.contains_key(&expected) {
                return Err(Refused::Path);
            }
        }
        RegionOp::Branch { test, yes, no } => {
            if draft.registers.get(test.offset()) != Some(&RegisterKind::Bool) {
                return Err(Refused::Facts);
            }
            let Some(CheckedKernel {
                kind: CheckedKind::Truth { input, output },
            }) = &step.guard
            else {
                return Err(Refused::Facts);
            };
            let canonical = register(draft, &Owner::Operand(step.at.site.child(0)))?;
            if *input != canonical || *output != *test {
                return Err(Refused::Facts);
            }
            if step.at.phase != Phase::Test {
                return Err(Refused::Source);
            }
            for (taken, pc) in [(true, yes), (false, no)] {
                let expected = source
                    .next_after_while_test(&step.at.site, taken)
                    .or_else(|_| source.next_after_if_test(&step.at.site, taken))?;
                if target(*pc)? != &expected
                    || *pc != super::stages::target(draft, &step.at, &expected)?
                {
                    return Err(Refused::Path);
                }
            }
        }
        RegionOp::Checked { kernel, next, .. }
            if matches!(kernel.kind(), CheckedKind::Cursor { .. }) =>
        {
            let CheckedKind::Cursor { at, exhausted } = kernel.kind() else {
                return Err(Refused::Source);
            };
            if *at != step.at.site
                || step.at.phase != Phase::Test
                || !matches!(
                    resources::node(source, at)?,
                    doc::Node::Stmt(doc::Stmt::For { via: None, .. })
                )
            {
                return Err(Refused::Source);
            }
            if target(*next)? != &source.first_event(&at.child(1))?
                || target(*exhausted)? != &source.next_after_for_cursor(at, false)?
            {
                return Err(Refused::Path);
            }
        }
        _ => {
            let expected = source_next(source, &step.at)?;
            let successors = recovery::successors(draft, index);
            match (expected, successors.as_slice()) {
                (None, []) => {}
                (Some(expected), [pc])
                    if target(*pc)? == &expected
                        && *pc == super::stages::target(draft, &step.at, &expected)? => {}
                _ => return Err(Refused::Path),
            }
        }
    }
    Ok(())
}

fn word_inputs(
    draft: &RegionDraft,
    step: &Step,
    expected: &[Reg<LeaseValue>],
) -> Result<Vec<usize>, Refused> {
    if let Some(guard) = &step.guard {
        let CheckedKind::UnpackPair { inputs, outputs } = guard.kind() else {
            return Err(Refused::Facts);
        };
        if expected != inputs || outputs[0] == outputs[1] && inputs[0] != inputs[1] {
            return Err(Refused::Facts);
        }
        for (input, output) in inputs.iter().zip(outputs) {
            if draft.registers.get(output.offset()) != Some(&RegisterKind::Int)
                || output.offset() < draft.roles.len() + 1
                    && (input.offset() != output.offset()
                        || draft.registers[input.offset()] != RegisterKind::Int)
            {
                return Err(Refused::Facts);
            }
        }
        Ok(outputs.iter().map(|reg| reg.offset()).collect())
    } else {
        Ok(expected.iter().map(|reg| reg.offset()).collect())
    }
}

fn float_inputs(
    draft: &RegionDraft,
    step: &Step,
    expected: &[Reg<LeaseValue>],
) -> Result<Vec<usize>, Refused> {
    if let Some(guard) = &step.guard {
        let CheckedKind::UnpackFloats { inputs, outputs } = guard.kind() else {
            return Err(Refused::Facts);
        };
        if expected != inputs || outputs[0] == outputs[1] && inputs[0] != inputs[1] {
            return Err(Refused::Facts);
        }
        for (input, output) in inputs.iter().zip(outputs) {
            if draft.registers.get(output.offset()) != Some(&RegisterKind::Float)
                || output.offset() < draft.roles.len() + 1
                    && (input.offset() != output.offset()
                        || draft.registers[input.offset()] != RegisterKind::Float)
            {
                return Err(Refused::Facts);
            }
        }
        Ok(outputs.iter().map(|reg| reg.offset()).collect())
    } else {
        Ok(expected.iter().map(|reg| reg.offset()).collect())
    }
}

fn check_origin(
    exe: &Executable,
    draft: &RegionDraft,
    at: &Point,
    origin: KirOrigin,
) -> Result<(), Refused> {
    if origin.statement.0 as usize >= exe.stmt_count() {
        return Err(Refused::Source);
    }
    let (stream, stmt, _) = exe.statement(origin.statement);
    if exe.stmt_at(draft.code, &stmt.site) != Some(origin.statement)
        || stream.ops.get(origin.pc as usize) != Some(&origin.op)
        || stream
            .source_sites
            .get(origin.pc as usize)
            .and_then(Option::as_deref)
            != Some(&at.site)
    {
        return Err(Refused::Source);
    }
    Ok(())
}

/// A delivering action's Enter: the action its origin names is one the
/// machine performs, its atoms load the action's own atom slots from their
/// visible names or literals, its arguments are its atoms or its nested
/// operands, and it resumes at its own Commit.
fn check_action(
    exe: &Executable,
    draft: &RegionDraft,
    index: usize,
    node: doc::Node<'_>,
    spec: &BoundarySpec,
    action: &ActionBoundary,
) -> Result<(), Refused> {
    let step = &draft.steps[index];
    let origin = spec.origin.ok_or(Refused::Source)?;
    let (stream, statement, _) = exe.statement(origin.statement);
    let performs = match (node, origin.op) {
        (doc::Node::Expr(doc::Expr::Act(_)), Op::Nested { act, .. }) => {
            stream.nested.get(act.ix()).is_some_and(|entry| {
                !matches!(
                    entry.kind,
                    crate::compile::NestedKind::Call(_) | crate::compile::NestedKind::Body(_)
                )
            })
        }
        (doc::Node::Action(_), Op::Act) => matches!(
            &statement.form,
            crate::compile::Form::Action(action)
                if !matches!(action.kind, crate::compile::ActionKind::Call { .. })
        ),
        _ => false,
    };
    let commit = Point {
        site: step.at.site.clone(),
        phase: Phase::Commit,
    };
    if !performs
        || step.at.phase != Phase::Enter
        || !crate::compile::room::delivers(node)
        || step.event.output.is_some()
        || spec.next.is_none()
        || spec.next != super::stages::event_pc(draft, &commit).ok()
        || !step.event.entities.is_empty()
    {
        return Err(Refused::Source);
    }
    check_atoms(draft, index, node, &action.atoms)?;
    let arguments: Vec<_> = match node {
        doc::Node::Action(_) => action.atoms.iter().map(|load| load.dst).collect(),
        _ => draft
            .steps
            .get(spec.next.ok_or(Refused::Path)?.offset())
            .ok_or(Refused::Path)?
            .event
            .inputs
            .iter()
            .filter(|owner| !matches!(owner, Owner::Incoming(_)))
            .map(|owner| register(draft, owner))
            .collect::<Result<_, _>>()?,
    };
    if *action.arguments != *arguments {
        return Err(Refused::Facts);
    }
    Ok(())
}

/// A statement action's atom loads: each loads the action's own atom slot,
/// in order, from its visible name's role or its literal. A nested action
/// has none.
fn check_atoms(
    draft: &RegionDraft,
    index: usize,
    node: doc::Node<'_>,
    loads: &[AtomLoad],
) -> Result<(), Refused> {
    let step = &draft.steps[index];
    let atoms: &[doc::Atom] = match node {
        doc::Node::Action(action) => crate::compile::room::action_atoms(action),
        _ => &[],
    };
    if loads.len() != atoms.len() {
        return Err(Refused::Facts);
    }
    for (index, (load, atom)) in loads.iter().zip(atoms).enumerate() {
        let dst = register(
            draft,
            &Owner::Atom {
                at: step.at.site.clone(),
                index: u32::try_from(index).map_err(|_| Refused::Facts)?,
            },
        )?;
        let loads = match (&load.src, atom) {
            (AtomSource::Role { src, cell }, doc::Atom::Variable(name)) => {
                let role = resources::role(
                    &draft.source,
                    &Owner::Visible {
                        at: step.at.site.clone(),
                        name: name.clone(),
                    },
                )
                .ok_or(Refused::Facts)?;
                *cell == resources::cell(&draft.source, &step.at.site, name, role)
                    && draft
                        .roles
                        .iter()
                        .any(|(actual, reg)| *actual == role && reg == src)
            }
            (AtomSource::Constant(value), doc::Atom::Literal(literal)) => {
                super::lower::literal(literal).is_some_and(|expected| expected.same(value))
            }
            (AtomSource::Text(text), doc::Atom::Literal(doc::Literal::Text(expected))) => {
                doc::arena::ShortText::new(expected).is_none() && **text == **expected
            }
            _ => false,
        };
        if load.dst != dst || !loads {
            return Err(Refused::Facts);
        }
    }
    Ok(())
}

fn check_op(exe: &Executable, draft: &RegionDraft, index: usize) -> Result<(), Refused> {
    let step = &draft.steps[index];
    if step.stage.is_some() {
        return super::stages::check(draft, index);
    }
    let source = &draft.source;
    let node = resources::node(source, &step.at.site)?;
    let expected_inputs = step
        .event
        .inputs
        .iter()
        .map(|owner| register(draft, owner))
        .collect::<Result<Vec<_>, _>>();
    let expected_output = step
        .event
        .output
        .as_ref()
        .and_then(|owner| register(draft, owner).ok());
    let value_reg = |reg: Reg<LeaseValue>| {
        if reg.offset() < draft.roles.len() {
            Ok(())
        } else {
            Err(Refused::Facts)
        }
    };
    match &draft.ops[index] {
        RegionOp::Heap {
            kernel, args, dst, ..
        } => {
            kernel.verify(exe, draft, step, index)?;
            if expected_inputs.as_deref() != Ok(args.as_ref()) || *dst != expected_output {
                return Err(Refused::Facts);
            }
        }
        RegionOp::Constant { dst, value, .. } => {
            value_reg(*dst)?;
            let expected = match node {
                doc::Node::Expr(doc::Expr::Literal(literal)) => match literal {
                    doc::Literal::Null => Value::Null,
                    doc::Literal::Absent => Value::Absent,
                    doc::Literal::Bool(value) => Value::Bool(*value),
                    doc::Literal::Int(value) => Value::Int(value.word().ok_or(Refused::Facts)?),
                    doc::Literal::Float(value) => Value::Float(*value),
                    doc::Literal::Text(text) => {
                        Value::Text(doc::arena::ShortText::new(text).ok_or(Refused::Facts)?)
                    }
                    _ => return Err(Refused::Source),
                },
                doc::Node::Block(_) if step.at.phase == Phase::Fallthrough => Value::Null,
                _ => return Err(Refused::Source),
            };
            if Some(*dst) != expected_output || !value.same(&expected) {
                return Err(Refused::Source);
            }
        }
        // A raise origin row moves the raise, delivered as the task's
        // Incoming, into the frame's active departure and releases the
        // raising event's inputs.
        RegionOp::Move { dst, src, .. }
            if index >= source.room.events.len()
                && draft
                    .departures
                    .iter()
                    .any(|route| route.origin.offset() == index) =>
        {
            let sink = Reg::new(draft.roles.len())?;
            let inputs = expected_inputs?;
            if *src != sink
                || inputs.last() != Some(&sink)
                || expected_output != Some(*dst)
                || step.event.output != Some(Owner::ActiveDeparture(source.site.clone()))
                || !step.event.entities.is_empty()
            {
                return Err(Refused::Facts);
            }
            value_reg(*dst)?;
        }
        // A delivering action's Commit moves its delivered Incoming into its
        // operand and releases its arguments.
        RegionOp::Move { dst, src, .. }
            if step.at.phase == Phase::Commit && crate::compile::room::delivers(node) =>
        {
            let sink = Reg::new(draft.roles.len())?;
            let inputs = expected_inputs?;
            if *src != sink
                || inputs.last() != Some(&sink)
                || expected_output != Some(*dst)
                || !step.event.entities.is_empty()
            {
                return Err(Refused::Facts);
            }
            value_reg(*dst)?;
        }
        RegionOp::Move { dst, src, .. } => {
            if step.at.phase == Phase::Restore {
                if src.offset() != draft.roles.len() {
                    return Err(Refused::Facts);
                }
            } else {
                value_reg(*src)?;
            }
            value_reg(*dst)?;
            if expected_inputs.as_deref() != Ok(&[*src]) {
                return Err(Refused::Facts);
            }
            let output = match node {
                doc::Node::Stmt(doc::Stmt::Let { .. }) | doc::Node::Expr(doc::Expr::Let { .. })
                    if matches!(step.at.phase, Phase::Commit | Phase::Test) =>
                {
                    expected_output
                }
                doc::Node::Expr(doc::Expr::If { .. }) if step.at.phase == Phase::Commit => {
                    expected_output
                }
                // A raise moves its operand into the departure, and the
                // catching try's Catch moves the departure into its binding.
                doc::Node::Stmt(doc::Stmt::Throw { .. }) if step.at.phase == Phase::Commit => {
                    expected_output
                }
                doc::Node::Stmt(doc::Stmt::Try(_)) if step.at.phase == Phase::Catch => {
                    expected_output
                }
                doc::Node::Expr(doc::Expr::Call { .. } | doc::Expr::Act(_))
                    if step.at.phase == Phase::Restore =>
                {
                    expected_output
                }
                doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Variable(name),
                    ..
                }) => Some(register(
                    draft,
                    &Owner::Visible {
                        at: step.at.site.clone(),
                        name: name.clone(),
                    },
                )?),
                _ => return Err(Refused::Source),
            };
            if output != Some(*dst) || !step.event.entities.is_empty() {
                return Err(Refused::Source);
            }
        }
        // An uncaught raise moves its operand into the frame's departure.
        RegionOp::Raise { dst, src } => {
            value_reg(*src)?;
            value_reg(*dst)?;
            if step.at.phase != Phase::Commit
                || !matches!(node, doc::Node::Stmt(doc::Stmt::Throw { .. }))
                || index >= draft.source.room.events.len()
                || !super::departures::uncaught(&draft.source, &step.at)?
                || expected_inputs.as_deref() != Ok(&[*src])
                || expected_output != Some(*dst)
                || !step.event.entities.is_empty()
            {
                return Err(Refused::Source);
            }
        }
        // An uncaught ReturnVia's Commit leaves its operand held at its
        // role for the machine's policy exit; it writes nothing.
        RegionOp::ReturnVia { value } => {
            value_reg(*value)?;
            if step.at.phase != Phase::Commit
                || !matches!(node, doc::Node::Stmt(doc::Stmt::ReturnVia { .. }))
                || index >= draft.source.room.events.len()
                || !super::departures::uncaught(&draft.source, &step.at)?
                || expected_inputs.as_deref() != Ok(&[*value])
                || expected_output.is_some()
                || !step.event.entities.is_empty()
            {
                return Err(Refused::Source);
            }
        }
        RegionOp::IntBinary {
            kernel,
            lhs,
            rhs,
            dst,
            ..
        } => {
            for reg in [lhs, rhs, dst] {
                if draft.registers.get(reg.offset()) != Some(&RegisterKind::Int) {
                    return Err(Refused::Facts);
                }
            }
            let Some(function) = native_callee(node, source) else {
                return Err(Refused::Source);
            };
            let function = &function;
            if *function != kernel.function
                || exe.library_lib(function) != Some(kernel.lib)
                || !matches!(source.calls.get(function), Some(CallSource::Native(_)))
            {
                return Err(Refused::Source);
            }
            let lib = exe.lib(kernel.lib);
            if super::resource_pair(lib, true).and_then(|pair| pair.ints) != Some(kernel.op)
                || kernel.op.compares()
            {
                return Err(Refused::Source);
            }
            let table = lib
                .specialization
                .prepared
                .as_ref()
                .and_then(|(_, fast)| fast.scalar())
                .ok_or(Refused::Source)?;
            if (0..16).any(|mask| table.units(mask as u8) != Some(kernel.units[mask]))
                || kernel.depth != step.depth
            {
                return Err(Refused::Source);
            }
            if word_inputs(draft, step, &expected_inputs.clone()?)?
                != vec![lhs.offset(), rhs.offset()]
                || expected_output.map(|reg| reg.offset()) != Some(dst.offset())
            {
                return Err(Refused::Facts);
            }
        }
        RegionOp::Read {
            spec, base, dst, ..
        } => {
            value_reg(*base)?;
            value_reg(*dst)?;
            let doc::Node::Expr(doc::Expr::Member(member)) = node else {
                return Err(Refused::Source);
            };
            let inputs = expected_inputs?;
            if spec.at != step.at || inputs.first() != Some(base) || expected_output != Some(*dst) {
                return Err(Refused::Source);
            }
            let expected = match &**member {
                doc::Member::Field { field, .. } => Selector::Field(Arc::from(field.as_str())),
                doc::Member::Index { .. } => Selector::Index(*inputs.get(1).ok_or(Refused::Facts)?),
            };
            if expected != spec.selector {
                return Err(Refused::Source);
            }
            let retained = resources::retained_read(source, &step.at.site.child(0));
            if draft.registers.get(dst.offset()) != Some(&RegisterKind::Value) {
                return Err(Refused::Facts);
            }
            if retained != spec.retained {
                return Err(Refused::Retained);
            }
        }
        RegionOp::Checked {
            kernel, args, dst, ..
        } => match kernel.kind() {
            CheckedKind::Local { slot, cell } => {
                let name = match (node, step.at.phase) {
                    (doc::Node::Expr(doc::Expr::Variable(name)), Phase::Commit) => name,
                    (doc::Node::Expr(doc::Expr::Act(act)), Phase::Open) => match &act.kind {
                        doc::ActKind::Call(doc::Callee::Value(name))
                        | doc::ActKind::Spawn(doc::Callee::Value(name)) => name,
                        _ => return Err(Refused::Source),
                    },
                    _ => return Err(Refused::Source),
                };
                if Some(*slot)
                    != resources::role(
                        source,
                        &Owner::Visible {
                            at: step.at.site.clone(),
                            name: name.clone(),
                        },
                    )
                    || *cell != resources::cell(source, &step.at.site, name, *slot)
                    || Some(*dst) != expected_output
                    || !args.is_empty()
                {
                    return Err(Refused::Facts);
                }
            }
            CheckedKind::FloatBinary {
                function,
                lib,
                operation,
                lhs,
                rhs,
                units,
            } => {
                let Some(actual) = native_callee(node, source) else {
                    return Err(Refused::Source);
                };
                let actual = &actual;
                if function != actual
                    || exe.library_lib(function) != Some(*lib)
                    || super::resource_pair(exe.lib(*lib), true).and_then(|pair| pair.floats)
                        != Some(*operation)
                {
                    return Err(Refused::Source);
                }
                let table = exe
                    .lib(*lib)
                    .specialization
                    .prepared
                    .as_ref()
                    .and_then(|(_, fast)| fast.scalar())
                    .ok_or(Refused::Source)?;
                if table.units(0) != Some(*units) {
                    return Err(Refused::Source);
                }
                if float_inputs(draft, step, &expected_inputs.clone()?)?
                    != vec![lhs.offset(), rhs.offset()]
                    || Some(*dst) != expected_output
                    || expected_inputs.as_deref() != Ok(args.as_ref())
                    || draft.registers.get(dst.offset())
                        != Some(&if operation.compares() {
                            RegisterKind::Bool
                        } else {
                            RegisterKind::Float
                        })
                {
                    return Err(Refused::Facts);
                }
            }
            CheckedKind::IntTruth {
                function,
                lib,
                operation,
                lhs,
                rhs,
                units,
            } => {
                let Some(actual) = native_callee(node, source) else {
                    return Err(Refused::Source);
                };
                let actual = &actual;
                if function != actual
                    || exe.library_lib(function) != Some(*lib)
                    || super::resource_pair(exe.lib(*lib), true).and_then(|pair| pair.ints)
                        != Some(*operation)
                    || !operation.compares()
                {
                    return Err(Refused::Source);
                }
                let table = exe
                    .lib(*lib)
                    .specialization
                    .prepared
                    .as_ref()
                    .and_then(|(_, fast)| fast.scalar())
                    .ok_or(Refused::Source)?;
                if (0..16).any(|mask| table.units(mask as u8) != Some(units[mask])) {
                    return Err(Refused::Source);
                }
                if word_inputs(draft, step, &expected_inputs.clone()?)?
                    != vec![lhs.offset(), rhs.offset()]
                    || Some(*dst) != expected_output
                    || expected_inputs.as_deref() != Ok(args.as_ref())
                    || draft.registers.get(dst.offset()) != Some(&RegisterKind::Bool)
                {
                    return Err(Refused::Facts);
                }
            }
            CheckedKind::Native { function, lib } => {
                let Some(actual) = native_callee(node, source) else {
                    return Err(Refused::Source);
                };
                let actual = &actual;
                if function != actual
                    || exe.library_lib(function) != Some(*lib)
                    || !matches!(source.calls.get(function), Some(CallSource::Native(_)))
                {
                    return Err(Refused::Source);
                }
                if Some(*dst) != expected_output || expected_inputs.as_deref() != Ok(args.as_ref())
                {
                    return Err(Refused::Facts);
                }
            }
            CheckedKind::Cursor { at, .. } => {
                // It reads the loop's held iterable, the operand aliased to
                // its cursor role, and writes the bound item.
                let iterable =
                    super::lower::reg(&draft.roles, source, &Owner::Operand(at.child(0)))?;
                if Some(*dst) != expected_output || args.as_ref() != [iterable] {
                    return Err(Refused::Facts);
                }
            }
            CheckedKind::LoopItem { at } => via::check_item(
                source,
                step,
                at,
                Some(*dst) == expected_output && expected_inputs.as_deref() == Ok(args.as_ref()),
            )?,
            CheckedKind::LoopControl { .. } | CheckedKind::Header { .. } => {
                return Err(Refused::Source);
            }
            // A plain `for`'s Enter reads only what it walks.
            CheckedKind::Phase
                if step.at.phase == Phase::Enter
                    && matches!(node, doc::Node::Stmt(doc::Stmt::For { via: None, .. })) =>
            {
                let iterable = super::lower::reg(
                    &draft.roles,
                    source,
                    &Owner::Operand(step.at.site.child(0)),
                )?;
                if dst.offset() != draft.roles.len()
                    || args.as_ref() != [iterable]
                    || !step.event.entities.is_empty()
                {
                    return Err(Refused::Facts);
                }
            }
            CheckedKind::Phase => {
                if dst.offset() != draft.roles.len() {
                    return Err(Refused::Facts);
                }
                if !step.event.entities.is_empty()
                    && !super::lower::parameter_cells(source, &step.at, &step.event)
                    || !matches!(
                        node,
                        doc::Node::Block(_)
                            | doc::Node::Stmt(
                                doc::Stmt::Do { .. }
                                    | doc::Stmt::If { .. }
                                    | doc::Stmt::While { .. }
                                    | doc::Stmt::Break
                                    | doc::Stmt::Continue
                            )
                    ) && !(step.at.phase == Phase::Catch
                        && matches!(node, doc::Node::Stmt(doc::Stmt::For { .. })))
                        && !(step.at.phase == Phase::Commit
                            && matches!(
                                node,
                                doc::Node::Stmt(doc::Stmt::Try(_) | doc::Stmt::For { .. })
                            ))
                {
                    return Err(Refused::Source);
                }
            }
            _ => return Err(Refused::Source),
        },
        RegionOp::Branch { .. } => {}
        RegionOp::Call { site, .. } => {
            let call = draft.calls.get(site.offset()).ok_or(Refused::Source)?;
            check_origin(exe, draft, &step.at, call.origin)?;
            check_atoms(draft, index, node, &call.atoms)?;
            let inputs = expected_inputs?;
            // A statement call's arguments are its atoms, which it holds;
            // a nested call's are its operands, the callable aside.
            let atoms: Vec<_>;
            let arguments = match node {
                doc::Node::Action(_) if inputs.is_empty() => {
                    atoms = call.atoms.iter().map(|load| load.dst).collect();
                    &atoms[..]
                }
                doc::Node::Expr(doc::Expr::Act(act))
                    if matches!(act.kind, doc::ActKind::Call(doc::Callee::Value(_))) =>
                {
                    inputs.get(1..).ok_or(Refused::Facts)?
                }
                doc::Node::Stmt(doc::Stmt::For { via: Some(_), .. })
                    if matches!(call.hook, Some(Phase::Next | Phase::Check | Phase::Item))
                        && inputs.is_empty() =>
                {
                    atoms = via::hook_arguments(draft, step, call.hook.ok_or(Refused::Facts)?)?;
                    &atoms[..]
                }
                _ => &inputs,
            };
            if call.at != step.at
                || arguments != call.arguments.as_ref()
                || Some(call.result)
                    != expected_output.or_else(|| {
                        // An action, or a protocol hook whose header row
                        // takes what it returned, resumes into the sink.
                        (step.at.phase == Phase::Enter || call.hook.is_some())
                            .then(|| Reg::new(draft.roles.len()).ok())
                            .flatten()
                    })
            {
                return Err(Refused::Facts);
            }
        }
        RegionOp::Return { value } => {
            value_reg(*value)?;
            if !matches!(node, doc::Node::Stmt(doc::Stmt::Return { .. }))
                || step.at.phase != Phase::Commit
                || expected_inputs.as_deref() != Ok(&[*value])
            {
                return Err(Refused::Source);
            }
        }
        RegionOp::Boundary { spec } => {
            if spec.at != step.at {
                return Err(Refused::Source);
            }
            if let Some(origin) = spec.origin {
                check_origin(exe, draft, &step.at, origin)?;
            }
            if let Some(action) = spec.action() {
                check_action(exe, draft, index, node, spec, action)?;
            }
        }
        RegionOp::Effect {
            kind,
            value,
            departure,
            next,
        } => {
            value_reg(*value)?;
            if let Some(departure) = departure {
                value_reg(*departure)?;
            }
            let expected = match node {
                doc::Node::Stmt(doc::Stmt::Print { .. }) => EffectKind::Print,
                doc::Node::Stmt(doc::Stmt::Finish { .. }) => EffectKind::Finish,
                doc::Node::Stmt(doc::Stmt::Fail { .. }) => EffectKind::Fail,
                _ => return Err(Refused::Source),
            };
            if *kind != expected
                || step.at.phase != Phase::Commit
                || !step.event.entities.is_empty()
                || expected_output != *departure
                || (*kind == EffectKind::Print) != departure.is_none()
                || (*kind != EffectKind::Print && next.is_some())
                || expected_inputs.as_deref() != Ok(&[*value])
            {
                return Err(Refused::Facts);
            }
        }
        RegionOp::Exit { prefix } => {
            if step.at.site != source.site || step.at.phase != Phase::Leave || *prefix != step.after
            {
                return Err(Refused::Exit);
            }
        }
    }
    Ok(())
}

fn verify_layout(
    exe: &Executable,
    code: CodeId,
    draft: &RegionDraft,
    calls: &[CallPlan],
) -> Result<(), Refused> {
    if code.0 as usize >= exe.code_count() {
        return Err(Refused::Source);
    }
    let source = &exe.code(code).source_room;
    source.verify()?;
    if draft.code != code || draft.executable != exe.local() || !Arc::ptr_eq(source, &draft.source)
    {
        return Err(Refused::Source);
    }
    let canonical_program = canonical::program(exe, code)?;
    let source_events = source.room.events.len();
    let stages = super::stages::layout(&canonical_program.loops)?;
    let loop_steps = source_events
        .checked_add(stages.len())
        .ok_or(Refused::Facts)?;
    if draft.steps.len() < loop_steps || draft.ops.len() < loop_steps {
        return Err(Refused::Exit);
    }
    let departed = loop_steps
        .checked_add(super::departures::verify(draft, &canonical_program.events)?)
        .ok_or(Refused::Facts)?;
    let expected_steps = departed
        .checked_add(super::joins::verify(
            draft,
            departed,
            &canonical_program.events,
        )?)
        .ok_or(Refused::Facts)?;
    if draft.loops.as_ref() != canonical_program.loops.as_ref() {
        return Err(Refused::Source);
    }
    if draft.steps.len() != expected_steps
        || draft.ops.len() != expected_steps
        || draft.recovery.len() != expected_steps.checked_mul(3).ok_or(Refused::Facts)?
    {
        return Err(Refused::Exit);
    }
    if draft.roles.as_ref() != resources::roles(source)?.as_ref() {
        return Err(Refused::Facts);
    }
    if draft.steps.get(draft.entry.offset()).map(|step| &step.at)
        != Some(&Point {
            site: source.site.clone(),
            phase: Phase::Enter,
        })
    {
        return Err(Refused::Path);
    }
    let expected_charges = canonical_program.events;
    for (index, (at, canonical_event)) in source.room.events.iter().enumerate() {
        let step = &draft.steps[index];
        // A conditional Commit's base row joins its then arm.
        let joined;
        let canonical_event = if super::joins::joined(source, at)? {
            joined = super::joins::event(source, at, 1)?;
            &joined
        } else {
            canonical_event
        };
        if step.stage.is_some() || step.at != *at || step.event.point != *at {
            return Err(Refused::Source);
        }
        if step.event.peak != canonical_event.peak {
            return Err(Refused::Peak);
        }
        if step.event.delta != canonical_event.delta {
            return Err(Refused::Retained);
        }
        if step.event != *canonical_event {
            return Err(Refused::Source);
        }
        if step.peak
            != IndexedAmount::compile(
                source,
                &draft.roles,
                &canonical_event.peak,
                canonical_event.output.as_ref(),
            )?
        {
            return Err(Refused::Peak);
        }
        if step.delta
            != IndexedAmount::compile(
                source,
                &draft.roles,
                &canonical_event.delta,
                canonical_event.output.as_ref(),
            )?
        {
            return Err(Refused::Retained);
        }
        if step.charges.as_ref() != expected_charges.get(at).map_or(&[][..], Vec::as_slice) {
            return Err(Refused::Source);
        }
        let expected_scratch: Box<[_]> = source
            .room
            .scratch
            .iter()
            .filter(|scratch| scratch.at == *at)
            .map(|scratch| IndexedAmount::compile(source, &draft.roles, &scratch.amount, None))
            .collect::<Result<_, _>>()?;
        if step.scratch != expected_scratch {
            return Err(Refused::Scratch);
        }
        let expected_depth = match resources::node(source, &step.at.site)? {
            node if native_callee(node, source).is_some()
                && step.at.phase == native_phase(node) =>
            {
                let function = &native_callee(node, source).ok_or(Refused::Source)?;
                let lib = exe.lib(exe.library_lib(function).ok_or(Refused::Source)?);
                lib.prepared_depth(exe)
                    .checked_add(u32::from(lib.operation()))
                    .ok_or(Refused::Facts)?
            }
            _ => 0,
        };
        if step.depth != expected_depth {
            return Err(Refused::Facts);
        }
        if step.guard.is_some()
            && !matches!(
                draft.ops[index],
                RegionOp::Branch { .. }
                    | RegionOp::IntBinary { .. }
                    | RegionOp::Checked {
                        kernel: CheckedKernel {
                            kind: CheckedKind::IntTruth { .. } | CheckedKind::FloatBinary { .. }
                        },
                        ..
                    }
            )
        {
            return Err(Refused::Source);
        }
        if step.before.offset() != index * 3
            || step.pending.offset() != index * 3 + 1
            || step.after.offset() != index * 3 + 2
        {
            return Err(Refused::Exit);
        }
        for successor in recovery::successors(draft, index) {
            if successor.offset() >= draft.ops.len() {
                return Err(Refused::Path);
            }
        }
        if step.reachable {
            check_edge(draft, index)?;
        }
        check_op(exe, draft, index)?;
    }
    for index in source_events..loop_steps {
        if draft.steps[index].stage != Some(stages[index - source_events]) {
            return Err(Refused::Source);
        }
        let step = &draft.steps[index];
        if step.before.offset() != index * 3
            || step.pending.offset() != index * 3 + 1
            || step.after.offset() != index * 3 + 2
        {
            return Err(Refused::Exit);
        }
        super::stages::check(draft, index)?;
    }
    for index in loop_steps..expected_steps {
        check_op(exe, draft, index)?;
        check_edge(draft, index)?;
    }
    if draft.call_sources.len() != draft.calls.len()
        || draft
            .call_sources
            .iter()
            .zip(draft.calls.iter())
            .any(|(source, call)| *source != (call.origin.statement, call.origin.pc, call.hook))
    {
        return Err(Refused::Source);
    }
    if calls.len() != draft.calls.len() {
        return Err(Refused::Source);
    }
    for (index, call) in calls.iter().enumerate() {
        let site = &draft.calls[index];
        if call.point() != &site.at
            || call.arguments().len() != site.arguments.len()
            || call
                .arguments()
                .iter()
                .zip(site.arguments.iter())
                .any(|(slot, reg)| {
                    !draft.roles.iter().any(|(actual_slot, actual_reg)| {
                        *actual_slot == Role::Slot(slot.slot()) && actual_reg == reg
                    })
                })
        {
            return Err(Refused::Source);
        }
        call.verify_region(
            exe,
            draft,
            CallId(u32::try_from(index).map_err(|_| Refused::Facts)?),
        )?;
    }
    for (index, step) in draft.steps.iter().enumerate() {
        for (phase, prefix) in [step.before, step.pending, step.after]
            .into_iter()
            .enumerate()
        {
            let map = draft.recovery.get(prefix.offset()).ok_or(Refused::Exit)?;
            if map.continuation != step.at
                || !map.graph.check(
                    &step.at,
                    &map.roles,
                    map.pending,
                    phase as u32,
                    if (phase < 2
                        && step
                            .event
                            .inputs
                            .iter()
                            .any(|owner| matches!(owner, Owner::Incoming(_))))
                        || (phase > 0 && super::lower::delivers(&draft.ops[index]))
                    {
                        Some(Reg::new(draft.roles.len())?)
                    } else {
                        None
                    },
                )
            {
                return Err(Refused::Exit);
            }
        }
    }
    check_maps(draft)?;
    types::verify(exe, draft)?;
    Ok(())
}

fn check_maps(draft: &RegionDraft) -> Result<(), Refused> {
    let source = &draft.source;
    let mut initial = BTreeSet::new();
    for name in &source.params {
        if let Some(slot) = resources::role(
            source,
            &Owner::Binding {
                declared: source.site.clone(),
                name: name.clone(),
            },
        )
        .and_then(Role::slot)
        {
            initial.insert(slot);
        }
    }
    initial.extend(
        draft
            .roles
            .iter()
            .filter_map(|(role, _)| role.slot().filter(|slot| matches!(slot, Slot::Capture(_)))),
    );
    let mut incoming = BTreeMap::from([(draft.entry.offset(), initial)]);
    let mut queue = VecDeque::from([draft.entry.offset()]);
    loop {
        while let Some(index) = queue.pop_front() {
            let before_owned = incoming.get(&index).ok_or(Refused::Exit)?.clone();
            let before = &before_owned;
            let step = &draft.steps[index];
            let origin = index >= source.room.events.len()
                && draft
                    .departures
                    .iter()
                    .any(|route| route.origin.offset() == index);
            let event = if origin || step.stage.is_some() || super::joins::joined(source, &step.at)?
            {
                &step.event
            } else {
                source.room.events.get(&step.at).ok_or(Refused::Source)?
            };
            let mut after = before.clone();
            for owner in &event.inputs {
                if let Some(slot) = resources::role(source, owner).and_then(Role::slot) {
                    after.remove(&slot);
                }
            }
            for measure in event.delta.measures() {
                if let Measure::Release(owners) = measure {
                    for owner in owners {
                        for root in resources::owner_roots(source, owner)? {
                            if let Root::Value { slot, .. } = root {
                                after.remove(&slot);
                            }
                        }
                    }
                }
            }
            // A statement action's Enter holds its atoms until its Commit.
            for (measure, coefficient) in canonical::terms(&event.delta) {
                if *coefficient > 0
                    && let Measure::Local(owner @ Owner::Atom { .. }) = measure
                    && let Some(slot) = resources::role(source, owner).and_then(Role::slot)
                {
                    after.insert(slot);
                }
            }
            if !matches!(draft.ops[index], RegionOp::Call { .. })
                && let Some(slot) = event
                    .output
                    .as_ref()
                    .and_then(|owner| resources::role(source, owner))
                    .and_then(Role::slot)
            {
                after.insert(slot);
            }
            if step.stage.is_none()
                && step.at.phase == Phase::Commit
                && let doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Variable(name),
                    ..
                }) = resources::node(source, &step.at.site)?
                && let Some(slot) = resources::role(
                    source,
                    &Owner::Visible {
                        at: step.at.site.clone(),
                        name: name.clone(),
                    },
                )
                .and_then(Role::slot)
            {
                after.insert(slot);
            }
            let sink = Reg::new(draft.roles.len())?;
            let incoming_before = event
                .inputs
                .iter()
                .any(|owner| matches!(owner, Owner::Incoming(_)))
                .then_some(sink);
            let incoming_after = super::lower::delivers(&draft.ops[index]).then_some(sink);
            let mut pending_roles = if incoming_after.is_some() {
                after.clone()
            } else {
                before.clone()
            };
            let pending = if super::lower::delivers(&draft.ops[index]) {
                Some(sink)
            } else if matches!(&draft.ops[index], RegionOp::Heap { kernel, .. } if kernel.atomic_move())
            {
                None
            } else if super::lower::pending_at_output(&draft.generic_ops[index]) {
                event
                    .output
                    .as_ref()
                    .map(|owner| {
                        let reg = register(draft, owner)?;
                        if let Role::Slot(slot) =
                            resources::role(source, owner).ok_or(Refused::Facts)?
                        {
                            pending_roles.insert(slot);
                        }
                        Ok::<_, Refused>(reg)
                    })
                    .transpose()?
            } else {
                event
                    .output
                    .as_ref()
                    .map(|_| Reg::new(draft.roles.len()))
                    .transpose()?
            };
            for (prefix, expected, pending, incoming) in [
                (step.before, before, incoming_before, incoming_before),
                (
                    step.pending,
                    &pending_roles,
                    pending,
                    incoming_after.or(incoming_before),
                ),
                (step.after, &after, incoming_after, incoming_after),
            ] {
                let map = draft.recovery.get(prefix.offset()).ok_or(Refused::Exit)?;
                let expected_roles: Vec<_> = draft
                    .roles
                    .iter()
                    .filter_map(|(role, reg)| {
                        role.slot()
                            .filter(|slot| expected.contains(slot))
                            .map(|slot| (slot, *reg))
                    })
                    .collect();
                if !map.graph.check(
                    &step.at,
                    &expected_roles,
                    map.pending,
                    (prefix.offset() % 3) as u32,
                    incoming,
                ) || map.roles.as_ref() != expected_roles.as_slice()
                    || map.continuation != step.at
                    || map.pending != pending
                {
                    return Err(Refused::Exit);
                }
                let operands: Vec<_> = expected_roles
                    .iter()
                    .filter(|(slot, _)| {
                        matches!(
                            slot,
                            Slot::Operand(_) | Slot::Continuation(_) | Slot::Argument(_)
                        )
                    })
                    .map(|(_, reg)| *reg)
                    .collect();
                if map.operands.as_ref() != operands.as_slice() {
                    return Err(Refused::Exit);
                }
            }
            if step.scalar_resources {
                if let RegionOp::Read { spec, .. } = &draft.ops[index] {
                    let retained = spec.retained.ok_or(Refused::Retained)?;
                    if !before.contains(&retained) || !after.contains(&retained) {
                        return Err(Refused::Retained);
                    }
                }

                if step
                    .peak
                    .terms
                    .iter()
                    .chain(step.delta.terms.iter())
                    .any(|(input, _)| {
                        matches!(
                            input,
                            ResourceInput::SourceFact(
                                Measure::Members(_)
                                    | Measure::MemberDelta(_)
                                    | Measure::MemberGrowth(_)
                                    | Measure::CaptureEdges(_)
                            )
                        )
                    })
                {
                    return Err(Refused::Retained);
                }
                if !event.entities.is_empty() {
                    return Err(Refused::Retained);
                }
                if matches!(
                    draft.ops[index],
                    RegionOp::Boundary { .. } | RegionOp::Call { .. }
                ) {
                    return Err(Refused::Retained);
                }
            }
            for successor in recovery::successors(draft, index) {
                let live = if recovery::unaccepted(&draft.ops[index], successor) {
                    before
                } else {
                    &after
                };
                match incoming.get(&successor.offset()) {
                    Some(prior) if prior != live => return Err(Refused::Exit),
                    Some(_) => {}
                    None => {
                        incoming.insert(successor.offset(), live.clone());
                        queue.push_back(successor.offset());
                    }
                }
            }
        }
        // A raising event's own origin row is entered where the event's
        // base row stands, holding what it holds.
        let events = source.room.events.len();
        for route in draft.departures.iter() {
            let origin = route.origin.offset();
            if origin < events || incoming.contains_key(&origin) {
                continue;
            }
            let at = &draft.steps[origin].at;
            let base = draft.steps[..events]
                .iter()
                .position(|step| step.at == *at)
                .ok_or(Refused::Exit)?;
            if let Some(mut live) = incoming.get(&base).cloned() {
                // A hook raises once its call is entered: what the call
                // moved into its callee is gone.
                if let RegionOp::Call { site, .. } = &draft.ops[base]
                    && draft
                        .calls
                        .get(site.offset())
                        .ok_or(Refused::Exit)?
                        .hook
                        .is_some()
                {
                    for owner in &draft.steps[base].event.inputs {
                        if let Some(slot) = resources::role(source, owner).and_then(Role::slot) {
                            live.remove(&slot);
                        }
                    }
                }
                incoming.insert(origin, live);
                queue.push_back(origin);
            }
        }
        if queue.is_empty() {
            break;
        }
    }
    if draft
        .steps
        .iter()
        .enumerate()
        .any(|(index, step)| step.reachable != incoming.contains_key(&index))
    {
        return Err(Refused::Exit);
    }
    Ok(())
}

pub(super) fn verify(
    exe: &Executable,
    code: CodeId,
    draft: &RegionDraft,
    calls: &[CallPlan],
) -> Result<(), Refused> {
    if draft.registers.len() < draft.roles.len().checked_add(1).ok_or(Refused::Facts)?
        || draft.generic_ops.len() != draft.ops.len()
        || draft.generic_steps.len() != draft.steps.len()
        || draft.generic_registers.len() != draft.registers.len()
        || draft.packing.len() != draft.registers.len()
    {
        return Err(Refused::Facts);
    }
    for (index, (kind, pack)) in draft.registers.iter().zip(draft.packing.iter()).enumerate() {
        match (kind, pack.as_ref().map(CheckedKernel::kind)) {
            (RegisterKind::Int, Some(CheckedKind::PackInt { input }))
                if input.offset() == index => {}
            (RegisterKind::Float, Some(CheckedKind::PackFloat { input }))
                if input.offset() == index => {}
            (RegisterKind::Bool, Some(CheckedKind::PackBool { input }))
                if input.offset() == index => {}
            (RegisterKind::Value, None) => {}
            _ => return Err(Refused::Facts),
        }
    }
    verify_layout(exe, code, draft, calls)?;
    let mut generic = draft.clone();
    generic.ops = draft.generic_ops.clone();
    generic.steps = draft.generic_steps.clone();
    generic.registers = draft.generic_registers.clone();
    verify_layout(exe, code, &generic, calls)
}

/// The library function `node` calls: a call expression's, or a native call
/// act's, which answers at its Enter instead of its Commit.
fn native_callee(node: doc::Node<'_>, source: &Source) -> Option<FunctionId> {
    match node {
        doc::Node::Expr(doc::Expr::Call { function, .. }) => Some(*function),
        _ => crate::compile::room::native_act(node, &source.calls),
    }
}

/// The phase at which `node`'s library call answers.
fn native_phase(node: doc::Node<'_>) -> Phase {
    match node {
        doc::Node::Expr(doc::Expr::Act(_)) => Phase::Enter,
        _ => Phase::Commit,
    }
}
