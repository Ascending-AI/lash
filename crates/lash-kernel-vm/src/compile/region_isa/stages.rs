//! Canonical loop entry/test phases are explicit instructions. Entry charges
//! run once; backedges reach the test phase without re-entering the statement.
//! A protocol `for`'s header is its own rows (`K-ITER-005`): it takes what
//! `open` returned, steps its own cursor or dispatches to `next`, takes
//! what `next` and `check` return, and lets an exhausted header go. Its
//! hooks are calls, whose results the header rows take.
use super::*;
use crate::compile::room::charges::region_isa::CanonicalClose;
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
            match &mut kernel.kind {
                CheckedKind::Cursor { exhausted, .. } => replace(exhausted),
                CheckedKind::Header {
                    driver,
                    ended,
                    unchecked,
                    ..
                } => {
                    if let Some(driver) = driver {
                        replace(driver);
                    }
                    if let Some(unchecked) = unchecked {
                        replace(unchecked);
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
pub(super) fn event_pc(draft: &RegionDraft, point: &Point) -> Result<Pc, Refused> {
    draft
        .steps
        .iter()
        .position(|step| step.stage.is_none() && step.at == *point)
        .map(|index| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
        .ok_or(Refused::Path)?
}

/// Whether `from` is a plain `for`'s Enter event, which admits what the
/// loop walks before the loop is entered.
fn entered(info: &LoopInfo, from: &Point) -> bool {
    !info.while_loop && info.via.is_none() && from.site == info.site && from.phase == Phase::Enter
}

/// The stage rows of a loop, in their layout order.
pub(super) fn phases(info: &LoopInfo) -> Vec<LoopPhase> {
    let mut phases = vec![LoopPhase::Enter, LoopPhase::Test];
    if let Some(via) = info.via {
        phases.extend([LoopPhase::Opened, LoopPhase::Step]);
        if via.next {
            phases.push(LoopPhase::Pulled);
        }
        if via.check {
            phases.push(LoopPhase::Checked);
        }
        if via.next || via.check {
            phases.push(LoopPhase::Ended);
        }
    }
    phases
}

/// Every loop stage, in layout order, after `events` source events.
pub(super) fn layout(loops: &[LoopInfo]) -> Result<Vec<LoopStage>, Refused> {
    let mut stages = Vec::new();
    for (index, info) in loops.iter().enumerate() {
        let info_index = u32::try_from(index).map_err(|_| Refused::Facts)?;
        stages.extend(phases(info).into_iter().map(|phase| LoopStage {
            info: info_index,
            phase,
        }));
    }
    Ok(stages)
}

fn point(info: &LoopInfo, phase: LoopPhase) -> Point {
    Point {
        site: info.site.clone(),
        phase: match (info.via.is_some(), phase) {
            (_, LoopPhase::Enter | LoopPhase::Ended) => Phase::Enter,
            (false, _) => Phase::Next,
            (true, LoopPhase::Test) => Phase::Leave,
            (true, LoopPhase::Opened) => Phase::Restore,
            (true, LoopPhase::Step) => Phase::Suspend,
            (true, LoopPhase::Pulled) => Phase::Fallthrough,
            (true, LoopPhase::Checked) => Phase::Supersede,
        },
    }
}

fn stage_pc(stages: &[LoopStage], events: usize, stage: LoopStage) -> Result<Pc, Refused> {
    let index = stages
        .iter()
        .position(|actual| *actual == stage)
        .ok_or(Refused::Path)?;
    Ok(Pc(u32::try_from(
        events.checked_add(index).ok_or(Refused::Facts)?,
    )
    .map_err(|_| Refused::Facts)?))
}

/// A stage row as the source defines it: its event, charges and checked op.
struct Expected {
    event: Event,
    charges: Box<[CanonicalClose]>,
    kind: CheckedKind,
    args: Box<[Reg<LeaseValue>]>,
    dst: Reg<LeaseValue>,
    next: Pc,
}

fn expected(
    draft: &RegionDraft,
    stages: &[LoopStage],
    events: usize,
    stage: LoopStage,
    event_pc: &dyn Fn(&Point) -> Result<Pc, Refused>,
) -> Result<Expected, Refused> {
    let info = draft
        .loops
        .get(stage.info as usize)
        .ok_or(Refused::Source)?;
    let sink = Reg::new(draft.roles.len())?;
    let at = point(info, stage.phase);
    let reg = |owner: &Owner| lower::reg(&draft.roles, &draft.source, owner);
    let own = |phase| stage_pc(stages, events, LoopStage { phase, ..stage });
    let mut event = Event {
        point: at,
        inputs: Box::new([]),
        output: None,
        entities: Box::new([]),
        peak: Amount::default(),
        delta: Amount::default(),
    };
    let charges = match stage.phase {
        LoopPhase::Enter => info.entry_charges.clone(),
        LoopPhase::Test => info.test_charges.clone(),
        _ => Box::new([]),
    };
    let Some(via) = info.via else {
        let iterable = if stage.phase == LoopPhase::Enter && !info.while_loop {
            Some(reg(&Owner::Operand(info.site.child(0)))?)
        } else {
            None
        };
        let next = match stage.phase {
            LoopPhase::Enter if info.while_loop || info.entry == info.test => own(LoopPhase::Test)?,
            LoopPhase::Enter => event_pc(&info.entry)?,
            LoopPhase::Test => event_pc(&info.test)?,
            _ => return Err(Refused::Source),
        };
        return Ok(Expected {
            event,
            charges,
            kind: CheckedKind::LoopControl { stage, iterable },
            args: iterable.into_iter().collect(),
            dst: sink,
            next,
        });
    };
    let site = &info.site;
    let incoming = Owner::Incoming(site.clone());
    let cursor = Owner::LoopCursor(site.clone());
    let header = Owner::LoopHeader(site.clone());
    let hook = |phase| event_pc(&lower::point(site, phase));
    // What the header gives the pass: `item`'s call, or the pass's Test.
    let pass = if via.item {
        hook(Phase::Item)?
    } else {
        hook(Phase::Test)?
    };
    let done = match resources::node(&draft.source, site)? {
        doc::Node::Stmt(doc::Stmt::For { else_block, .. }) if !else_block.is_empty() => {
            hook_at(event_pc, &site.child(2), Phase::Enter)?
        }
        doc::Node::Stmt(doc::Stmt::For { .. }) => hook(Phase::Commit)?,
        _ => return Err(Refused::Source),
    };
    let header_kind = |driver, ended| CheckedKind::Header {
        stage,
        driver,
        ended,
        unchecked: None,
    };
    let ended = || own(LoopPhase::Ended);
    Ok(match stage.phase {
        LoopPhase::Enter => Expected {
            event,
            charges,
            kind: CheckedKind::LoopControl {
                stage,
                iterable: None,
            },
            args: Box::new([]),
            dst: sink,
            next: event_pc(&info.entry)?,
        },
        LoopPhase::Test => Expected {
            event,
            charges,
            kind: CheckedKind::LoopControl {
                stage,
                iterable: None,
            },
            args: Box::new([]),
            dst: sink,
            next: own(LoopPhase::Step)?,
        },
        LoopPhase::Opened => {
            event.inputs = Box::new([incoming]);
            event.output = Some(cursor.clone());
            Expected {
                event,
                charges,
                kind: header_kind(None, None),
                args: Box::new([sink]),
                dst: reg(&cursor)?,
                next: own(LoopPhase::Test)?,
            }
        }
        // The header holds the item it steps to, or what `next` gave
        // in place of the tuple it let go of.
        LoopPhase::Step => {
            event.output = Some(header.clone());
            event.peak = Amount::header(Some(header.clone()), None);
            event.delta = event.peak.clone();
            // `check` runs only on a map's or a set's walk (`K-ITER-005`).
            Expected {
                event,
                charges,
                kind: CheckedKind::Header {
                    stage,
                    driver: via.next.then(|| hook(Phase::Next)).transpose()?,
                    ended: Some(done),
                    unchecked: via.check.then_some(pass),
                },
                args: Box::new([reg(&cursor)?]),
                dst: reg(&header)?,
                next: if via.check { hook(Phase::Check)? } else { pass },
            }
        }
        LoopPhase::Pulled => {
            event.inputs = Box::new([incoming.clone()]);
            event.output = Some(header.clone());
            event.peak = Amount::header(Some(header.clone()), None);
            event.delta = Amount::header(Some(header.clone()), Some(incoming));
            Expected {
                event,
                charges,
                kind: header_kind(None, Some(ended()?)),
                args: Box::new([sink]),
                dst: reg(&header)?,
                next: pass,
            }
        }
        // `check`'s count is let go of once read.
        LoopPhase::Checked => {
            event.inputs = Box::new([incoming.clone()]);
            event.delta = Amount::header(None, Some(incoming));
            Expected {
                event,
                charges,
                kind: header_kind(None, Some(ended()?)),
                args: Box::new([sink, reg(&header)?]),
                dst: sink,
                next: pass,
            }
        }
        // The loop lets go of the header's last hold as it leaves.
        LoopPhase::Ended => {
            event.inputs = Box::new([header.clone()]);
            event.delta = Amount::header(None, Some(header.clone()));
            Expected {
                event,
                charges,
                kind: header_kind(None, None),
                args: Box::new([reg(&header)?]),
                dst: sink,
                next: done,
            }
        }
    })
}

fn hook_at(
    event_pc: &dyn Fn(&Point) -> Result<Pc, Refused>,
    site: &Site,
    phase: Phase,
) -> Result<Pc, Refused> {
    event_pc(&lower::point(site, phase))
}

pub(super) fn append(draft: &mut RegionDraft) -> Result<(), Refused> {
    let events = draft.ops.len();
    let stages = layout(&draft.loops)?;
    let pc_for = |steps: &[Step], at: &Point| {
        steps
            .iter()
            .position(|step| step.stage.is_none() && step.at == *at)
            .map(|index| u32::try_from(index).map(Pc).map_err(|_| Refused::Facts))
            .ok_or(Refused::Path)?
    };
    let routes: Vec<_> = draft
        .loops
        .iter()
        .map(|info| {
            Ok((
                pc_for(&draft.steps, &info.entry)?,
                pc_for(&draft.steps, &info.test)?,
            ))
        })
        .collect::<Result<_, Refused>>()?;
    let mut ops = std::mem::take(&mut draft.ops).into_vec();
    for (index, info) in draft.loops.iter().enumerate() {
        let info_index = u32::try_from(index).map_err(|_| Refused::Facts)?;
        let enter = stage_pc(
            &stages,
            events,
            LoopStage {
                info: info_index,
                phase: LoopPhase::Enter,
            },
        )?;
        let test = stage_pc(
            &stages,
            events,
            LoopStage {
                info: info_index,
                phase: LoopPhase::Test,
            },
        )?;
        let (entry_event, test_event) = routes[index];
        for (i, op) in ops.iter_mut().enumerate().take(events) {
            let from = &draft.steps[i].at;
            let backedge = from.phase == Phase::Leave && from.site == info.site.child(1);
            if info.while_loop {
                redirect(op, entry_event, if backedge { test } else { enter });
            } else {
                if from.site.path.starts_with(&info.site.child(0).path)
                    && from.site.unit == info.site.unit
                    || entered(info, from)
                {
                    redirect(op, entry_event, enter);
                }
                if backedge {
                    redirect(op, test_event, test);
                }
            }
        }
    }
    draft.ops = ops.into();
    let mut ops = std::mem::take(&mut draft.ops).into_vec();
    let mut steps = std::mem::take(&mut draft.steps).into_vec();
    // A protocol `for`'s hook calls return to the header rows that take
    // their results.
    for (index, info) in draft.loops.iter().enumerate() {
        if info.via.is_none() {
            continue;
        }
        let info_index = u32::try_from(index).map_err(|_| Refused::Facts)?;
        for (hook, phase) in [
            (Phase::Open, LoopPhase::Opened),
            (Phase::Next, LoopPhase::Pulled),
            (Phase::Check, LoopPhase::Checked),
        ] {
            let Ok(pc) = pc_for(&steps, &lower::point(&info.site, hook)) else {
                continue;
            };
            let target = stage_pc(
                &stages,
                events,
                LoopStage {
                    info: info_index,
                    phase,
                },
            )?;
            if let RegionOp::Call { next, .. } | RegionOp::Checked { next, .. } =
                &mut ops[pc.offset()]
            {
                *next = target;
            }
        }
    }
    let event_pc = |at: &Point| pc_for(&steps, at);
    let mut rows = Vec::new();
    for stage in &stages {
        rows.push(expected(draft, &stages, events, *stage, &event_pc)?);
    }
    for (stage, row) in stages.iter().zip(rows) {
        let before = PrefixId(
            u32::try_from(steps.len().checked_mul(3).ok_or(Refused::Facts)?)
                .map_err(|_| Refused::Facts)?,
        );
        let pending = PrefixId(before.0.checked_add(1).ok_or(Refused::Facts)?);
        let after = PrefixId(before.0.checked_add(2).ok_or(Refused::Facts)?);
        let (peak, delta) = priced(draft, &row.event)?;
        steps.push(Step {
            at: row.event.point.clone(),
            event: row.event,
            before,
            pending,
            after,
            peak,
            delta,
            charges: row.charges,
            depth: 0,
            scalar_resources: true,
            reachable: false,
            guard: None,
            stage: Some(*stage),
            scratch: Box::new([]),
        });
        ops.push(RegionOp::Checked {
            kernel: CheckedKernel { kind: row.kind },
            args: row.args,
            dst: row.dst,
            next: row.next,
        });
    }
    draft.ops = ops.into();
    draft.steps = steps.into();
    Ok(())
}

/// A stage row's price: its event's, the header's write as its result.
fn priced(draft: &RegionDraft, event: &Event) -> Result<(IndexedAmount, IndexedAmount), Refused> {
    let price =
        |amount| IndexedAmount::compile(&draft.source, &draft.roles, amount, event.output.as_ref());
    Ok((price(&event.peak)?, price(&event.delta)?))
}

pub(super) fn check(draft: &RegionDraft, index: usize) -> Result<(), Refused> {
    let step = &draft.steps[index];
    let stage = step.stage.ok_or(Refused::Source)?;
    let events = draft.source.room.events.len();
    let stages = layout(&draft.loops)?;
    if stage_pc(&stages, events, stage)?.offset() != index {
        return Err(Refused::Source);
    }
    let row = expected(draft, &stages, events, stage, &|at| event_pc(draft, at))?;
    if step.at != row.event.point
        || step.event.point != row.event.point
        || step.event.inputs != row.event.inputs
        || step.event.output != row.event.output
        || !step.event.entities.is_empty()
        || step.event.peak != row.event.peak
        || step.event.delta != row.event.delta
        || step.depth != 0
        || step.guard.is_some()
        || !step.scratch.is_empty()
    {
        return Err(Refused::Source);
    }
    if step.charges.as_ref() != row.charges.as_ref() {
        return Err(Refused::Source);
    }
    let (peak, delta) = priced(draft, &row.event)?;
    if step.peak != peak || step.delta != delta {
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
    if kernel.kind != row.kind || *args != row.args || *dst != row.dst {
        return Err(Refused::Facts);
    }
    if *next != row.next {
        return Err(Refused::Path);
    }
    Ok(())
}

/// The stage a protocol `for`'s hook call returns to.
pub(super) fn returned(draft: &RegionDraft, at: &Point) -> Result<Option<Pc>, Refused> {
    let phase = match at.phase {
        Phase::Open => LoopPhase::Opened,
        Phase::Next => LoopPhase::Pulled,
        Phase::Check => LoopPhase::Checked,
        _ => return Ok(None),
    };
    let Some(index) = draft
        .loops
        .iter()
        .position(|info| info.via.is_some() && info.site == at.site)
    else {
        return Ok(None);
    };
    let stages = layout(&draft.loops)?;
    stage_pc(
        &stages,
        draft.source.room.events.len(),
        LoopStage {
            info: u32::try_from(index).map_err(|_| Refused::Facts)?,
            phase,
        },
    )
    .map(Some)
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
            && (from.site.unit == info.site.unit
                && from.site.path.starts_with(&info.site.child(0).path)
                || entered(info, from))
        {
            Some(LoopPhase::Enter)
        } else if !info.while_loop && *at == info.test && backedge {
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
