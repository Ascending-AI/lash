//! Canonical loop entry/test phases are explicit instructions. Entry charges
//! run once; backedges reach the test phase without re-entering the statement.
use super::*;
use crate::compile::room::{Amount, Owner};

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
            if let CheckedKind::Cursor { exhausted, .. } = &mut kernel.kind {
                replace(exhausted);
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
pub(super) fn event_pc(draft: &RegionDraft, point: &Point) -> Result<Pc, Refused> {
    draft
        .steps
        .iter()
        .position(|step| step.stage.is_none() && step.at == *point)
        .map(|index| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
        .ok_or(Refused::Path)?
}

fn point(info: &LoopInfo, phase: LoopPhase) -> Point {
    Point {
        site: info.site.clone(),
        phase: if phase == LoopPhase::Enter {
            Phase::Enter
        } else {
            Phase::Next
        },
    }
}

pub(super) fn append(draft: &mut RegionDraft, sink: Reg<LeaseValue>) -> Result<(), Refused> {
    let events = draft.ops.len();
    let mut ops = std::mem::take(&mut draft.ops).into_vec();
    let mut steps = std::mem::take(&mut draft.steps).into_vec();
    let pc_for = |at: &Point| {
        steps
            .iter()
            .position(|step| step.at == *at)
            .map(|index| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
            .ok_or(Refused::Path)?
    };
    let routes: Vec<_> = draft
        .loops
        .iter()
        .map(|info| Ok((pc_for(&info.entry)?, pc_for(&info.test)?)))
        .collect::<Result<_, Refused>>()?;
    for (index, info) in draft.loops.iter().enumerate() {
        let enter = Pc(u32::try_from(
            events
                .checked_add(index.checked_mul(2).ok_or(Refused::Facts)?)
                .ok_or(Refused::Facts)?,
        )
        .map_err(|_| Refused::Facts)?);
        let test = Pc(enter.0.checked_add(1).ok_or(Refused::Facts)?);
        let (entry_event, test_event) = routes[index];
        for i in 0..events {
            let from = &steps[i].at;
            let backedge = from.phase == Phase::Leave && from.site == info.site.child(1);
            if info.while_loop {
                redirect(
                    &mut ops[i],
                    entry_event,
                    if backedge { test } else { enter },
                );
            } else {
                if from.site.path.starts_with(&info.site.child(0).path)
                    && from.site.unit == info.site.unit
                {
                    redirect(&mut ops[i], entry_event, enter);
                }
                if backedge || from.site == info.site && from.phase == Phase::Open {
                    redirect(&mut ops[i], test_event, test);
                }
            }
        }
        for phase in [LoopPhase::Enter, LoopPhase::Test] {
            let stage = LoopStage {
                info: u32::try_from(index).map_err(|_| Refused::Facts)?,
                phase,
            };
            let at = point(info, phase);
            let iterable = if phase == LoopPhase::Enter && !info.while_loop {
                Some(lower::reg(
                    &draft.roles,
                    &draft.source,
                    &Owner::Operand(info.site.child(0)),
                )?)
            } else {
                None
            };
            let next = if phase == LoopPhase::Enter && (info.while_loop || info.entry == info.test)
            {
                test
            } else if phase == LoopPhase::Enter {
                entry_event
            } else {
                test_event
            };
            let before = PrefixId(
                u32::try_from(steps.len().checked_mul(3).ok_or(Refused::Facts)?)
                    .map_err(|_| Refused::Facts)?,
            );
            let pending = PrefixId(before.0.checked_add(1).ok_or(Refused::Facts)?);
            let after = PrefixId(before.0.checked_add(2).ok_or(Refused::Facts)?);
            let event = Event {
                point: at.clone(),
                inputs: Box::new([]),
                output: None,
                entities: Box::new([]),
                peak: Amount::default(),
                delta: Amount::default(),
            };
            steps.push(Step {
                at,
                event,
                before,
                pending,
                after,
                peak: IndexedAmount {
                    fixed: 0,
                    terms: Box::new([]),
                },
                delta: IndexedAmount {
                    fixed: 0,
                    terms: Box::new([]),
                },
                charges: if phase == LoopPhase::Enter {
                    info.entry_charges.clone()
                } else {
                    info.test_charges.clone()
                },
                depth: 0,
                scalar_resources: true,
                reachable: false,
                guard: None,
                stage: Some(stage),
                scratch: Box::new([]),
            });
            ops.push(RegionOp::Checked {
                kernel: CheckedKernel {
                    kind: CheckedKind::LoopControl { stage, iterable },
                },
                args: iterable.into_iter().collect(),
                dst: sink,
                next,
            });
        }
    }
    draft.ops = ops.into();
    draft.steps = steps.into();
    Ok(())
}

pub(super) fn check(draft: &RegionDraft, index: usize) -> Result<(), Refused> {
    let step = &draft.steps[index];
    let stage = step.stage.ok_or(Refused::Source)?;
    let info = draft
        .loops
        .get(stage.info as usize)
        .ok_or(Refused::Source)?;
    let at = &point(info, stage.phase);
    if &step.at != at
        || step.event.point != *at
        || !step.event.inputs.is_empty()
        || step.event.output.is_some()
        || !step.event.entities.is_empty()
        || step.event.peak != Amount::default()
        || step.event.delta != Amount::default()
        || step.depth != 0
        || step.guard.is_some()
        || !step.scratch.is_empty()
    {
        return Err(Refused::Source);
    }
    let charges = if stage.phase == LoopPhase::Enter {
        &info.entry_charges
    } else {
        &info.test_charges
    };
    if step.charges.as_ref() != charges.as_ref() {
        return Err(Refused::Source);
    }
    if step.peak
        != (IndexedAmount {
            fixed: 0,
            terms: Box::new([]),
        })
        || step.delta != step.peak
    {
        return Err(Refused::Peak);
    }
    let RegionOp::Checked {
        kernel,
        args,
        dst,
        next,
    } = &draft.ops[index]
    else {
        return Err(Refused::Source);
    };
    let iterable = if stage.phase == LoopPhase::Enter && !info.while_loop {
        Some(lower::reg(
            &draft.roles,
            &draft.source,
            &Owner::Operand(info.site.child(0)),
        )?)
    } else {
        None
    };
    if kernel.kind != (CheckedKind::LoopControl { stage, iterable })
        || args.as_ref() != iterable.as_slice()
        || dst.offset() != draft.roles.len()
    {
        return Err(Refused::Facts);
    }
    let expected =
        if stage.phase == LoopPhase::Enter && (info.while_loop || info.entry == info.test) {
            draft
                .steps
                .iter()
                .position(|step| {
                    step.stage
                        == Some(LoopStage {
                            info: stage.info,
                            phase: LoopPhase::Test,
                        })
                })
                .map(|index| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
                .ok_or(Refused::Path)??
        } else {
            event_pc(
                draft,
                if stage.phase == LoopPhase::Enter {
                    &info.entry
                } else {
                    &info.test
                },
            )?
        };
    if *next != expected {
        return Err(Refused::Path);
    }
    Ok(())
}

/// Source continuations choose a control phase by source identity, not by a
/// runtime executor selector. The verifier uses canonical loop definitions.
pub(super) fn target(draft: &RegionDraft, from: &Point, at: &Point) -> Result<Pc, Refused> {
    for (index, info) in draft.loops.iter().enumerate() {
        // A Continue's Catch at the loop resumes like the body's backedge.
        let backedge = from.phase == Phase::Leave && from.site == info.site.child(1)
            || from.phase == Phase::Catch && from.site == info.site;
        let phase = if info.while_loop && *at == info.entry {
            Some(if backedge {
                LoopPhase::Test
            } else {
                LoopPhase::Enter
            })
        } else if !info.while_loop
            && *at == info.entry
            && from.site.unit == info.site.unit
            && from.site.path.starts_with(&info.site.child(0).path)
        {
            Some(LoopPhase::Enter)
        } else if !info.while_loop
            && *at == info.test
            && (backedge || from.site == info.site && from.phase == Phase::Open)
        {
            Some(LoopPhase::Test)
        } else {
            None
        };
        if let Some(phase) = phase {
            let stage = LoopStage {
                info: u32::try_from(index).map_err(|_| Refused::Facts)?,
                phase,
            };
            return draft
                .steps
                .iter()
                .position(|step| step.stage == Some(stage))
                .map(|index| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
                .ok_or(Refused::Path)?;
        }
    }
    if let Some(pc) = super::joins::target(draft, from, at)? {
        return Ok(pc);
    }
    event_pc(draft, at)
}
