//! A conditional expression's Commit takes the operand of the arm it ran.
//! Its base row joins the then arm; one appended row joins the else arm.
//! Both rows move the arm operand into the expression operand and share
//! one successor. A continuation selects its row by the arm operand it holds.
use super::*;
use crate::compile::room::Owner;
use std::collections::BTreeMap;

/// Whether this source row is a conditional expression's Commit.
pub(super) fn joined(source: &Source, at: &Point) -> Result<bool, Refused> {
    Ok(at.phase == Phase::Commit
        && matches!(
            resources::node(source, &at.site)?,
            doc::Node::Expr(doc::Expr::If { .. })
        ))
}

/// The canonical event of the Commit joining `arm` (1 or 2).
pub(super) fn event(source: &Source, at: &Point, arm: u32) -> Result<Event, Refused> {
    let interval = source.compile_join(&at.site.child(arm), &at.site)?;
    let mut events = interval.events();
    let event = events.next().ok_or(Refused::Source)?.clone();
    if events.next().is_some() || event.point != *at {
        return Err(Refused::Source);
    }
    Ok(event)
}

/// The appended else-arm row for a conditional Commit, by source order.
fn rows(draft: &RegionDraft) -> Result<Vec<usize>, Refused> {
    let mut rows = Vec::new();
    for (index, at) in draft.source.room.events.keys().enumerate() {
        if joined(&draft.source, at)? {
            rows.push(index);
        }
    }
    Ok(rows)
}

/// The row that takes an edge from `from` into the conditional Commit `at`:
/// the else row for an edge leaving the else arm.
pub(super) fn target(draft: &RegionDraft, from: &Point, at: &Point) -> Result<Option<Pc>, Refused> {
    if !joined(&draft.source, at)? {
        return Ok(None);
    }
    let arm = at.site.child(2);
    if from.site.unit != arm.unit || !from.site.path.starts_with(&arm.path) {
        return Ok(None);
    }
    let base = draft.source.room.events.len();
    draft
        .steps
        .iter()
        .enumerate()
        .skip(base)
        .find(|(_, step)| step.stage.is_none() && step.at == *at)
        .map(|(index, _)| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
        .transpose()
}

fn redirect(op: &mut RegionOp, old: Pc, new: Pc) {
    let replace = |pc: &mut Pc| {
        if *pc == old {
            *pc = new;
        }
    };
    match op {
        RegionOp::Constant { next, .. }
        | RegionOp::Move { next, .. }
        | RegionOp::IntBinary { next, .. }
        | RegionOp::Read { next, .. }
        | RegionOp::Heap { next, .. }
        | RegionOp::Call { next, .. } => replace(next),
        RegionOp::Checked { kernel, next, .. } => {
            replace(next);
            match &mut kernel.kind {
                CheckedKind::Cursor { exhausted, .. } => replace(exhausted),
                CheckedKind::Header { driver, ended, .. } => {
                    if let Some(driver) = driver {
                        replace(driver);
                    }
                    if let Some(ended) = ended {
                        replace(ended);
                    }
                }
                _ => {}
            }
        }
        RegionOp::Branch { yes, no, .. } => {
            replace(yes);
            replace(no);
        }
        RegionOp::Boundary { spec } => {
            if let Some(next) = &mut spec.next {
                replace(next);
            }
        }
        RegionOp::Effect {
            next: Some(next), ..
        } => replace(next),
        _ => {}
    }
}

pub(super) fn append(draft: &mut RegionDraft) -> Result<(), Refused> {
    let mut ops = draft.ops.to_vec();
    let mut steps = draft.steps.to_vec();
    // An enclosing conditional precedes its arms in source order, so a
    // nested else row copies its already redirected successor.
    for index in rows(draft)? {
        let at = steps[index].at.clone();
        let RegionOp::Move { dst, next, .. } = ops[index] else {
            return Err(Refused::Source);
        };
        let row = steps.len();
        let pc = Pc(u32::try_from(row).map_err(|_| Refused::Facts)?);
        let before = PrefixId(
            u32::try_from(row.checked_mul(3).ok_or(Refused::Facts)?).map_err(|_| Refused::Facts)?,
        );
        let mut step = steps[index].clone();
        step.event = event(&draft.source, &at, 2)?;
        step.before = before;
        step.pending = PrefixId(before.0.checked_add(1).ok_or(Refused::Facts)?);
        step.after = PrefixId(before.0.checked_add(2).ok_or(Refused::Facts)?);
        let src = lower::reg(
            &draft.roles,
            &draft.source,
            &Owner::Operand(at.site.child(2)),
        )?;
        let arm = at.site.child(2);
        // Only the else arm's last event leaves it for this Commit.
        let old = Pc(u32::try_from(index).map_err(|_| Refused::Facts)?);
        for (from, op) in steps.iter().zip(ops.iter_mut()) {
            if from.at.site.unit == arm.unit && from.at.site.path.starts_with(&arm.path) {
                redirect(op, old, pc);
            }
        }
        ops.push(RegionOp::Move { dst, src, next });
        steps.push(step);
    }
    draft.ops = ops.into();
    draft.steps = steps.into();
    Ok(())
}

/// Check each else row against canonical syntax: its position, its joined
/// event and its fees, and that it moves the else operand.
pub(super) fn verify(
    draft: &RegionDraft,
    start: usize,
    charges: &BTreeMap<Point, Vec<CanonicalClose>>,
) -> Result<usize, Refused> {
    let rows = rows(draft)?;
    for (offset, index) in rows.iter().enumerate() {
        let cursor = start.checked_add(offset).ok_or(Refused::Facts)?;
        let base = draft.steps.get(*index).ok_or(Refused::Exit)?;
        let step = draft.steps.get(cursor).ok_or(Refused::Exit)?;
        let event = event(&draft.source, &base.at, 2)?;
        if step.at != base.at
            || step.event != event
            || step.stage.is_some()
            || step.guard.is_some()
            || step.depth != 0
            || !step.scratch.is_empty()
        {
            return Err(Refused::Source);
        }
        if step.before.offset() != cursor * 3
            || step.pending.offset() != cursor * 3 + 1
            || step.after.offset() != cursor * 3 + 2
        {
            return Err(Refused::Exit);
        }
        if step.peak
            != IndexedAmount::compile(
                &draft.source,
                &draft.roles,
                &event.peak,
                event.output.as_ref(),
            )?
        {
            return Err(Refused::Peak);
        }
        if step.delta
            != IndexedAmount::compile(
                &draft.source,
                &draft.roles,
                &event.delta,
                event.output.as_ref(),
            )?
        {
            return Err(Refused::Retained);
        }
        if step.charges.as_ref() != charges.get(&step.at).map_or(&[][..], Vec::as_slice) {
            return Err(Refused::Source);
        }
        if !matches!(draft.ops[cursor], RegionOp::Move { .. }) {
            return Err(Refused::Source);
        }
    }
    Ok(rows.len())
}
