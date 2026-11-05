//! A Return transfers its operand before leaving entered scopes. Each departure
//! owns its Leave rows because normal and departing successors differ.
use super::*;
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

pub(super) fn append(draft: &mut RegionDraft, sink: Reg<LeaseValue>) -> Result<(), Refused> {
    let mut ops = draft.ops.to_vec();
    let mut steps = draft.steps.to_vec();
    let mut routes = Vec::new();
    for index in 0..draft.source.room.events.len() {
        if !matches!(ops[index], RegionOp::Return { .. }) {
            continue;
        }
        let interval = draft.source.compile_departure_suffix(&steps[index].at)?;
        let events: Vec<_> = interval.events().skip(1).collect();
        if events.is_empty() || events.iter().any(|event| event.point.phase != Phase::Leave) {
            // Cleanup needs its own verified continuation before it can execute.
            ops[index] = RegionOp::Boundary {
                spec: BoundarySpec {
                    at: steps[index].at.clone(),
                    origin: None,
                    next: None,
                },
            };
            continue;
        }
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
            ops.push(if position + 1 == events.len() {
                RegionOp::Exit { prefix: step.after }
            } else {
                RegionOp::Checked {
                    kernel: CheckedKernel {
                        kind: CheckedKind::Phase,
                    },
                    args: Box::new([]),
                    dst: sink,
                    next: Pc(pc.0.checked_add(1).ok_or(Refused::Facts)?),
                }
            });
            steps.push(step);
            leaves.push(pc);
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
/// returns. No supplied successor, price or recovery summary is an oracle.
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
    for (index, op) in draft
        .ops
        .iter()
        .take(draft.source.room.events.len())
        .enumerate()
    {
        if !matches!(op, RegionOp::Return { .. }) {
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
        for (pc, event) in route.leaves.iter().zip(events) {
            if pc.offset() != cursor {
                return Err(Refused::Path);
            }
            let step = draft.steps.get(cursor).ok_or(Refused::Exit)?;
            if event.point.phase != Phase::Leave
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
            cursor += 1;
        }
        let last = route.leaves.last().ok_or(Refused::Exit)?;
        if draft.steps[last.offset()].at
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
        route.leaves.get(position + 1).copied()
    };
    if recovery::successors(draft, index).as_slice() != expected.as_slice() {
        return Err(Refused::Path);
    }
    Ok(())
}
