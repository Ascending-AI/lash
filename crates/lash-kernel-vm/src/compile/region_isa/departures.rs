//! A Return, Break, Continue or Throw transfers its operand (if any) before
//! leaving entered scopes. Each departure owns its Leave rows because normal
//! and departing successors differ. A Return ends at the frame Leave; a loop
//! departure ends at its target loop's Catch, and a raise at its catching
//! try's Catch, which moves it into the catch binding. Both resume where
//! source does.
use super::*;
use crate::compile::room::ExitKind;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct Departure {
    pub(super) origin: Pc,
    pub(super) leaves: Box<[Pc]>,
}

pub(super) fn successor(draft: &RegionDraft, pc: Pc) -> Option<Pc> {
    draft
        .departures
        .iter()
        .find(|route| route.origin == pc)?
        .leaves
        .first()
        .copied()
}
pub(super) fn contains(draft: &RegionDraft, index: usize) -> bool {
    draft.departures.iter().any(|route| {
        route.origin.offset() == index || route.leaves.iter().any(|pc| pc.offset() == index)
    })
}

/// The departing statement a source event row commits, by admitted syntax.
fn departing(draft: &RegionDraft, index: usize) -> Result<Option<ExitKind>, Refused> {
    let at = &draft.steps[index].at;
    if at.phase != Phase::Commit || index >= draft.source.room.events.len() {
        return Ok(None);
    }
    Ok(match resources::node(&draft.source, &at.site)? {
        doc::Node::Stmt(doc::Stmt::Return { .. }) => {
            matches!(draft.ops[index], RegionOp::Return { .. }).then_some(ExitKind::Return)
        }
        doc::Node::Stmt(doc::Stmt::Break) => Some(ExitKind::Break),
        doc::Node::Stmt(doc::Stmt::Continue) => Some(ExitKind::Continue),
        doc::Node::Stmt(doc::Stmt::Throw { .. }) => matches!(
            draft.ops[index],
            RegionOp::Move { .. } | RegionOp::Raise { .. }
        )
        .then_some(ExitKind::Raise),
        _ => None,
    })
}

/// Whether no try of the frame encloses the statement at `at`, so that a
/// raise there leaves the frame.
pub(super) fn uncaught(source: &Source, at: &Point) -> Result<bool, Refused> {
    if at.site.unit != source.site.unit || !at.site.path.starts_with(&source.site.path) {
        return Err(Refused::Path);
    }
    let mut site = at.site.clone();
    while site.path.len() > source.site.path.len() {
        site.path.pop();
        if matches!(
            resources::node(source, &site)?,
            doc::Node::Stmt(doc::Stmt::Try(_))
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Where a departure resumes after its Catch: a loop's Commit for a Break,
/// the loop's repeated test for a Continue, the catch block for a raise.
fn resume(
    draft: &RegionDraft,
    exit: ExitKind,
    resumed: Option<&Point>,
    catch: &Point,
) -> Result<Pc, Refused> {
    let expected = match (exit, resumed) {
        (ExitKind::Break, Some(at)) => {
            if *at
                != (Point {
                    site: catch.site.clone(),
                    phase: Phase::Commit,
                })
            {
                return Err(Refused::Path);
            }
            at.clone()
        }
        (ExitKind::Continue | ExitKind::Raise, Some(at)) => at.clone(),
        (ExitKind::Continue, None) => Point {
            site: catch.site.clone(),
            phase: Phase::Test,
        },
        _ => return Err(Refused::Path),
    };
    super::stages::target(draft, catch, &expected)
}

/// A catching Catch moves the departure into its binding, or a shared
/// binding's fresh cell.
fn catch(draft: &RegionDraft, step: &Step, row: usize, next: Pc) -> Result<RegionOp, Refused> {
    let event = &step.event;
    let [input] = event.inputs.as_ref() else {
        return Err(Refused::Source);
    };
    let src = lower::reg(&draft.roles, &draft.source, input)?;
    let dst = lower::reg(
        &draft.roles,
        &draft.source,
        event.output.as_ref().ok_or(Refused::Source)?,
    )?;
    Ok(if event.entities.is_empty() {
        RegionOp::Move { dst, src, next }
    } else {
        RegionOp::Heap {
            kernel: HeapKernel::compile(
                &draft.source,
                &draft.roles,
                event,
                row,
                super::heap::HeapKind::Cell,
            )?,
            args: Box::new([src]),
            dst: Some(dst),
            next,
        }
    })
}

pub(super) fn append(draft: &mut RegionDraft, sink: Reg<LeaseValue>) -> Result<(), Refused> {
    let mut ops = draft.ops.to_vec();
    let mut steps = draft.steps.to_vec();
    let mut routes = Vec::new();
    for index in 0..draft.source.room.events.len() {
        let Some(exit) = departing(draft, index)? else {
            continue;
        };
        // A raise no try of the frame encloses leaves the frame unrouted.
        if exit == ExitKind::Raise && uncaught(&draft.source, &steps[index].at)? {
            let RegionOp::Move { dst, src, .. } = ops[index] else {
                return Err(Refused::Source);
            };
            ops[index] = RegionOp::Raise { dst, src };
            continue;
        }
        let interval = match draft.source.compile_departure_suffix(&steps[index].at) {
            Err(Refused::Path) if exit == ExitKind::Raise => None,
            interval => Some(interval?),
        };
        let Some(interval) = interval else {
            ops[index] = RegionOp::Boundary {
                spec: BoundarySpec {
                    action: None,
                    at: steps[index].at.clone(),
                    origin: None,
                    next: None,
                },
            };
            continue;
        };
        let events: Vec<_> = interval.events().skip(1).collect();
        let (last, leaves_only) = match events.split_last() {
            Some((last, rest)) => (last, rest.iter().all(|e| e.point.phase == Phase::Leave)),
            None => return Err(Refused::Path),
        };
        let ends = match exit {
            ExitKind::Return => last.point.phase == Phase::Leave,
            _ => last.point.phase == Phase::Catch,
        };
        if !leaves_only || !ends {
            // Cleanup needs its own verified continuation before it can execute.
            ops[index] = RegionOp::Boundary {
                spec: BoundarySpec {
                    action: None,
                    at: steps[index].at.clone(),
                    origin: None,
                    next: None,
                },
            };
            continue;
        }
        let first = Pc(u32::try_from(steps.len()).map_err(|_| Refused::Facts)?);
        let resumed = (exit != ExitKind::Return)
            .then(|| resume(draft, exit, interval.resume(), &last.point))
            .transpose()?;
        let mut leaves = Vec::new();
        for (position, event) in events.iter().enumerate() {
            let row = steps.len();
            let pc = Pc(u32::try_from(row).map_err(|_| Refused::Facts)?);
            let before = PrefixId(
                u32::try_from(row.checked_mul(3).ok_or(Refused::Facts)?)
                    .map_err(|_| Refused::Facts)?,
            );
            let template = draft
                .steps
                .iter()
                .find(|step| step.stage.is_none() && step.at == event.point)
                .ok_or(Refused::Source)?;
            let mut step = template.clone();
            step.event = (*event).clone();
            step.before = before;
            step.pending = PrefixId(before.0.checked_add(1).ok_or(Refused::Facts)?);
            step.after = PrefixId(before.0.checked_add(2).ok_or(Refused::Facts)?);
            step.peak = IndexedAmount::compile(
                &draft.source,
                &draft.roles,
                &event.peak,
                event.output.as_ref(),
            )?;
            step.delta = IndexedAmount::compile(
                &draft.source,
                &draft.roles,
                &event.delta,
                event.output.as_ref(),
            )?;
            step.scalar_resources = false;
            step.guard = None;
            step.depth = 0;
            step.scratch = Box::new([]);
            let next = if position + 1 == events.len() {
                resumed
            } else {
                Some(Pc(pc.0.checked_add(1).ok_or(Refused::Facts)?))
            };
            let catches = exit == ExitKind::Raise && position + 1 == events.len();
            ops.push(match next {
                None => RegionOp::Exit { prefix: step.after },
                Some(next) if catches => catch(draft, &step, row, next)?,
                Some(next) => RegionOp::Checked {
                    kernel: CheckedKernel {
                        kind: CheckedKind::Phase,
                    },
                    args: Box::new([]),
                    dst: sink,
                    next,
                },
            });
            steps.push(step);
            leaves.push(pc);
        }
        if let RegionOp::Checked { next, .. } | RegionOp::Move { next, .. } = &mut ops[index] {
            *next = first;
        }
        routes.push(Departure {
            origin: Pc(u32::try_from(index).map_err(|_| Refused::Facts)?),
            leaves: leaves.into(),
        });
    }
    draft.ops = ops.into();
    draft.steps = steps.into();
    draft.departures = routes.into();
    Ok(())
}

/// Check the exact row order against canonical syntax, including unreachable
/// departures. No supplied successor, price or recovery summary is an oracle.
pub(super) fn verify(
    draft: &RegionDraft,
    charges: &BTreeMap<Point, Vec<CanonicalClose>>,
) -> Result<usize, Refused> {
    let base = draft
        .source
        .room
        .events
        .len()
        .checked_add(draft.loops.len().checked_mul(2).ok_or(Refused::Facts)?)
        .ok_or(Refused::Facts)?;
    let mut cursor = base;
    let mut routes = draft.departures.iter();
    for index in 0..draft.source.room.events.len() {
        let Some(exit) = departing(draft, index)? else {
            continue;
        };
        // check_op verifies an uncaught raise against its event.
        if matches!(
            draft.ops[index],
            RegionOp::Boundary { .. } | RegionOp::Raise { .. }
        ) {
            continue;
        }
        let route = routes.next().ok_or(Refused::Path)?;
        if route.origin.offset() != index {
            return Err(Refused::Path);
        }
        let interval = draft
            .source
            .compile_departure_suffix(&draft.steps[index].at)?;
        let mut events = interval.events();
        if events.next() != Some(&draft.steps[index].event)
            || route.leaves.len() != events.len()
            || route.leaves.is_empty()
        {
            return Err(Refused::Source);
        }
        if exit == ExitKind::Raise && !matches!(draft.ops[index], RegionOp::Move { .. }) {
            return Err(Refused::Source);
        }
        if !matches!(exit, ExitKind::Return | ExitKind::Raise)
            && !matches!(
                &draft.ops[index],
                RegionOp::Checked { kernel, args, dst, .. }
                    if kernel.kind == CheckedKind::Phase
                        && args.is_empty()
                        && dst.offset() == draft.roles.len()
                        && draft.steps[index].event.entities.is_empty()
                        && draft.steps[index].event.inputs.is_empty()
            )
        {
            return Err(Refused::Source);
        }
        let count = route.leaves.len();
        for (position, (pc, event)) in route.leaves.iter().zip(events).enumerate() {
            if pc.offset() != cursor {
                return Err(Refused::Path);
            }
            let step = draft.steps.get(cursor).ok_or(Refused::Exit)?;
            let last = position + 1 == count;
            let phase = if last && exit != ExitKind::Return {
                Phase::Catch
            } else {
                Phase::Leave
            };
            if event.point.phase != phase
                || step.at != event.point
                || step.event != *event
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
            let expected = match (last, exit) {
                (true, ExitKind::Return) => None,
                (true, _) => Some(resume(draft, exit, interval.resume(), &step.at)?),
                (false, _) => Some(Pc(u32::try_from(cursor + 1).map_err(|_| Refused::Facts)?)),
            };
            let shaped = match (&draft.ops[cursor], expected) {
                (RegionOp::Exit { prefix }, None) => *prefix == step.after,
                // check_op verifies the catch's move against its event.
                (RegionOp::Move { next, .. } | RegionOp::Heap { next, .. }, Some(expected))
                    if last && exit == ExitKind::Raise =>
                {
                    *next == expected
                }
                (
                    RegionOp::Checked {
                        kernel,
                        args,
                        dst,
                        next,
                    },
                    Some(expected),
                ) => {
                    kernel.kind == CheckedKind::Phase
                        && args.is_empty()
                        && dst.offset() == draft.roles.len()
                        && *next == expected
                }
                _ => false,
            };
            if !shaped {
                return Err(Refused::Path);
            }
            cursor += 1;
        }
        let last = route.leaves.last().ok_or(Refused::Exit)?;
        if exit == ExitKind::Return
            && draft.steps[last.offset()].at
                != (Point {
                    site: draft.source.site.clone(),
                    phase: Phase::Leave,
                })
        {
            return Err(Refused::Exit);
        }
    }
    if routes.next().is_some() {
        return Err(Refused::Path);
    }
    cursor.checked_sub(base).ok_or(Refused::Facts)
}

pub(super) fn check_edge(draft: &RegionDraft, index: usize) -> Result<(), Refused> {
    let route = draft
        .departures
        .iter()
        .find(|route| {
            route.origin.offset() == index || route.leaves.iter().any(|pc| pc.offset() == index)
        })
        .ok_or(Refused::Path)?;
    let expected = if route.origin.offset() == index {
        route.leaves.first().copied()
    } else {
        let position = route
            .leaves
            .iter()
            .position(|pc| pc.offset() == index)
            .ok_or(Refused::Path)?;
        match route.leaves.get(position + 1) {
            Some(next) => Some(*next),
            // A loop route resumes at its loop; a Return route exits.
            None => match departing(draft, route.origin.offset())?.ok_or(Refused::Path)? {
                ExitKind::Return => None,
                exit => {
                    let interval = draft
                        .source
                        .compile_departure_suffix(&draft.steps[route.origin.offset()].at)?;
                    Some(resume(
                        draft,
                        exit,
                        interval.resume(),
                        &draft.steps[index].at,
                    )?)
                }
            },
        }
    };
    if recovery::successors(draft, index).as_slice() != expected.as_slice() {
        return Err(Refused::Path);
    }
    Ok(())
}
