//! A Return, Break, Continue or Throw transfers its operand (if any) before
//! leaving entered scopes. Each departure owns its Leave rows because normal
//! and departing successors differ. A Return ends at the frame Leave; a loop
//! departure ends at its target loop's Catch, and a raise at its catching
//! try's Catch, which moves it into the catch binding. Both resume where
//! source does.
use super::*;
use crate::compile::room::{CompiledInterval, ExitKind, Owner};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct Departure {
    pub(super) origin: Pc,
    pub(super) leaves: Box<[Pc]>,
    /// Each cleanup that suspends the departure, in route order: its try
    /// and its restore rows, a Restore and then the Leaves that go on from
    /// the try (`K-FORM-017`).
    pub(super) restores: Box<[(Site, Box<[Pc]>)]>,
}
impl Departure {
    /// Every row after the origin, in route order.
    pub(super) fn rows(&self) -> impl Iterator<Item = Pc> + '_ {
        self.leaves.iter().copied().chain(
            self.restores
                .iter()
                .flat_map(|(_, rows)| rows.iter().copied()),
        )
    }
}

/// Whether an exit carries a value in the frame's active departure.
pub(super) fn valued(exit: ExitKind) -> bool {
    matches!(
        exit,
        ExitKind::Return | ExitKind::Raise | ExitKind::ReturnVia
    )
}

/// The rows that restore an exit suspended at the try at `at`.
pub(super) fn restores_at<'d>(
    draft: &'d RegionDraft,
    at: &'d Site,
) -> impl Iterator<Item = Pc> + 'd {
    draft.departures.iter().flat_map(move |route| {
        route
            .restores
            .iter()
            .filter(move |(site, _)| site == at)
            .filter_map(|(_, rows)| rows.first().copied())
    })
}

/// The restore row of the route departing from `origin` at the try at `at`.
pub(super) fn restore_row(draft: &RegionDraft, at: &Site, origin: &Point) -> Option<Pc> {
    draft
        .departures
        .iter()
        .filter(|route| draft.steps[route.origin.offset()].at == *origin)
        .flat_map(|route| route.restores.iter())
        .find(|(site, _)| site == at)
        .and_then(|(_, rows)| rows.first().copied())
}

/// A departure's route: its own suffix, then the restore suffix of each
/// cleanup that suspends it, each but the last ending at that try's
/// Suspend.
fn segments(
    source: &Source,
    exit: ExitKind,
    first: Arc<CompiledInterval>,
) -> Result<Vec<Arc<CompiledInterval>>, Refused> {
    let mut segments = vec![first];
    let mut enclosing: Option<Site> = None;
    loop {
        let Some(at) = segments
            .last()
            .and_then(|interval| interval.events().last())
            .filter(|event| event.point.phase == Phase::Suspend)
            .map(|event| event.point.site.clone())
        else {
            return Ok(segments);
        };
        // Each restore leaves a try strictly enclosing the one before.
        if enclosing.as_ref().is_some_and(|inner| {
            inner.unit != at.unit
                || inner.path.len() <= at.path.len()
                || !inner.path.starts_with(&at.path)
        }) {
            return Err(Refused::Path);
        }
        segments.push(source.compile_restore_suffix(&at, exit)?);
        enclosing = Some(at);
    }
}

/// A segment's events: a restore's from its Restore, the departure's own
/// after its origin.
fn segment_events(interval: &CompiledInterval, position: usize) -> Vec<&Event> {
    interval.events().skip(usize::from(position == 0)).collect()
}

/// Whether a segment's rows are Leaves, after a restore's Restore, ending
/// where its exit goes: a Suspend, a Return's frame Leave, or a Catch.
fn shaped(events: &[&Event], exit: ExitKind, restored: bool) -> bool {
    let Some((last, rest)) = events.split_last() else {
        return false;
    };
    // A restored exit no try of the frame takes stops at its Restore.
    if restored && rest.is_empty() && last.point.phase == Phase::Restore {
        return escapes(exit);
    }
    let rest = match (restored, rest.split_first()) {
        (true, Some((first, rest))) if first.point.phase == Phase::Restore => rest,
        (true, _) => return false,
        (false, _) => rest,
    };
    rest.iter()
        .all(|event| matches!(event.point.phase, Phase::Leave | Phase::Supersede))
        && match last.point.phase {
            Phase::Suspend => true,
            Phase::Leave => exit == ExitKind::Return,
            Phase::Catch => exit != ExitKind::Return,
            _ => false,
        }
}

/// A route's segments when every one is shaped, or `None`.
fn routed(
    source: &Source,
    exit: ExitKind,
    first: Arc<CompiledInterval>,
) -> Result<Option<Vec<Arc<CompiledInterval>>>, Refused> {
    let segments = match segments(source, exit, first) {
        Err(Refused::Path) => return Ok(None),
        segments => segments?,
    };
    Ok(segments
        .iter()
        .enumerate()
        .all(|(position, interval)| shaped(&segment_events(interval, position), exit, position > 0))
        .then_some(segments))
}

/// Whether a restored exit may leave the frame from its Restore.
fn escapes(exit: ExitKind) -> bool {
    matches!(exit, ExitKind::Raise | ExitKind::ReturnVia)
}

/// Where a segment's last row goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// An Escape stops the region for the machine.
    Escape,
    /// A Return's frame Leave exits.
    Exit,
    /// A Catch resumes its loop or takes the raise.
    Resume(Pc),
    /// A Suspend runs its cleanup, from the cleanup's first row.
    Suspend(Pc),
}

/// The base row at `point`.
fn base(draft: &RegionDraft, point: &Point) -> Result<Pc, Refused> {
    let index = draft
        .source
        .room
        .events
        .keys()
        .position(|at| at == point)
        .ok_or(Refused::Path)?;
    Ok(Pc(u32::try_from(index).map_err(|_| Refused::Facts)?))
}

fn end(
    draft: &RegionDraft,
    exit: ExitKind,
    interval: &CompiledInterval,
    last: &Event,
) -> Result<End, Refused> {
    Ok(match last.point.phase {
        Phase::Restore if escapes(exit) => End::Escape,
        Phase::Suspend => End::Suspend(base(draft, interval.resume().ok_or(Refused::Path)?)?),
        _ if exit == ExitKind::Return => End::Exit,
        _ => End::Resume(resume(draft, exit, interval.resume(), &last.point)?),
    })
}

/// The frame's active departure register, for a valued exit.
fn active(draft: &RegionDraft, exit: ExitKind) -> Result<Option<Reg<LeaseValue>>, Refused> {
    valued(exit)
        .then(|| {
            lower::reg(
                &draft.roles,
                &draft.source,
                &Owner::ActiveDeparture(draft.source.site.clone()),
            )
        })
        .transpose()
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
    draft
        .departures
        .iter()
        .any(|route| route.origin.offset() == index || route.rows().any(|pc| pc.offset() == index))
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
        doc::Node::Stmt(doc::Stmt::ReturnVia { .. }) => matches!(
            draft.ops[index],
            RegionOp::Move { .. } | RegionOp::ReturnVia { .. }
        )
        .then_some(ExitKind::ReturnVia),
        _ => None,
    })
}

/// Whether no try of the frame takes or cleans up after a raise of the
/// statement at `at`, so that the raise leaves the frame.
pub(super) fn uncaught(source: &Source, at: &Point) -> Result<bool, Refused> {
    if at.site.unit != source.site.unit || !at.site.path.starts_with(&source.site.path) {
        return Err(Refused::Path);
    }
    let mut site = at.site.clone();
    while site.path.len() > source.site.path.len() {
        let child = site.path.pop().ok_or(Refused::Path)?;
        // A try takes a raise of its body; a raise of its catch body or
        // its finally passes it by, unless a finally still runs after it.
        if let doc::Node::Stmt(doc::Stmt::Try(scope)) = resources::node(source, &site)? {
            let catch_body = scope.catch.is_some() && child == 1;
            let finally_body =
                scope.finally.is_some() && child == 1 + u32::from(scope.catch.is_some());
            if !finally_body && !(catch_body && scope.finally.is_none()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Whether no `finally` of the frame runs after a ReturnVia at `at`: no
/// try with one holds it in its body or catch block.
pub(super) fn unguarded(source: &Source, at: &Point) -> Result<bool, Refused> {
    if at.site.unit != source.site.unit || !at.site.path.starts_with(&source.site.path) {
        return Err(Refused::Path);
    }
    let mut site = at.site.clone();
    while site.path.len() > source.site.path.len() {
        let child = site.path.pop().ok_or(Refused::Path)?;
        if let doc::Node::Stmt(doc::Stmt::Try(scope)) = resources::node(source, &site)?
            && scope.finally.is_some()
            && (child == 0 || (scope.catch.is_some() && child == 1))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the event a base row commits may raise toward a try of the
/// frame besides taking its own exit: a delivered action's Commit, a
/// call's Restore, or a step the machine serves or refuses before it runs.
/// Its raise departs from a row of its own.
fn raising(draft: &RegionDraft, index: usize) -> Result<bool, Refused> {
    let Some(step) = draft.steps.get(index) else {
        return Ok(false);
    };
    if index >= draft.source.room.events.len()
        || step.stage.is_some()
        || draft.source.raising(&step.at).is_none()
    {
        return Ok(false);
    }
    Ok(!uncaught(&draft.source, &step.at)?)
}

/// A raising event's route: its suffix through Leaves to the catching
/// try's Catch or a cleanup's Suspend, and on through each restore, or
/// `None` when the raise has no such route and leaves through the machine.
fn raise_suffix(
    draft: &RegionDraft,
    at: &Point,
) -> Result<Option<Vec<Arc<CompiledInterval>>>, Refused> {
    let interval = match draft.source.compile_departure_suffix(at) {
        Err(Refused::Path) => return Ok(None),
        interval => interval?,
    };
    if interval.events().len() < 2 {
        return Ok(None);
    }
    routed(&draft.source, ExitKind::Raise, interval)
}

/// The exit a route departs with: its origin row's statement, or a raise
/// for a raising event's own origin row.
fn route_exit(draft: &RegionDraft, route: &Departure) -> Result<ExitKind, Refused> {
    if route.origin.offset() >= draft.source.room.events.len() {
        return Ok(ExitKind::Raise);
    }
    departing(draft, route.origin.offset())?.ok_or(Refused::Path)
}

/// A route row for `event` at `row`, priced from the event alone.
fn route_step(
    draft: &RegionDraft,
    template: &Step,
    event: &Event,
    row: usize,
) -> Result<Step, Refused> {
    let before = PrefixId(
        u32::try_from(row.checked_mul(3).ok_or(Refused::Facts)?).map_err(|_| Refused::Facts)?,
    );
    let mut step = template.clone();
    step.event = event.clone();
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
    Ok(step)
}

/// Check a route row's shape against its event, its row and its charges.
fn check_route_step(
    draft: &RegionDraft,
    charges: &BTreeMap<Point, Vec<CanonicalClose>>,
    step: &Step,
    event: &Event,
    row: usize,
) -> Result<(), Refused> {
    if step.at != event.point
        || step.event != *event
        || step.stage.is_some()
        || step.guard.is_some()
        || step.depth != 0
        || !step.scratch.is_empty()
    {
        return Err(Refused::Source);
    }
    if step.before.offset() != row * 3
        || step.pending.offset() != row * 3 + 1
        || step.after.offset() != row * 3 + 2
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
    Ok(())
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
    let boundary = |at: &Point| RegionOp::Boundary {
        spec: BoundarySpec {
            action: None,
            at: at.clone(),
            origin: None,
            next: None,
        },
    };
    for index in 0..draft.source.room.events.len() {
        // A raising event's own origin row moves its delivered value into
        // the departure, then leaves as a Throw would.
        if raising(draft, index)? {
            let Some(segments) = raise_suffix(draft, &steps[index].at)? else {
                continue;
            };
            let first = segments[0].events().next().ok_or(Refused::Path)?;
            let row = steps.len();
            let origin = Pc(u32::try_from(row).map_err(|_| Refused::Facts)?);
            let mut step = route_step(draft, &draft.steps[index], first, row)?;
            // The machine billed the raising step before it delivered the
            // raise here.
            step.charges = Default::default();
            ops.push(RegionOp::Move {
                dst: lower::reg(
                    &draft.roles,
                    &draft.source,
                    first.output.as_ref().ok_or(Refused::Source)?,
                )?,
                src: sink,
                next: Pc(origin.0.checked_add(1).ok_or(Refused::Facts)?),
            });
            steps.push(step);
            let (leaves, restores) = route_rows(
                draft,
                &mut ops,
                &mut steps,
                &segments,
                ExitKind::Raise,
                sink,
            )?;
            routes.push(Departure {
                origin,
                leaves,
                restores,
            });
            continue;
        }
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
        // A ReturnVia no cleanup of the frame follows leaves to its policy.
        if exit == ExitKind::ReturnVia && unguarded(&draft.source, &steps[index].at)? {
            let RegionOp::Move { dst, src, .. } = ops[index] else {
                return Err(Refused::Source);
            };
            ops[index] = RegionOp::ReturnVia { dst, src };
            continue;
        }
        let interval = match draft.source.compile_departure_suffix(&steps[index].at) {
            Err(Refused::Path) if exit == ExitKind::Raise => None,
            interval => Some(interval?),
        };
        let segments = match interval {
            Some(interval) if interval.events().len() >= 2 => {
                routed(&draft.source, exit, interval)?
            }
            Some(_) => return Err(Refused::Path),
            None => None,
        };
        let Some(segments) = segments else {
            ops[index] = boundary(&steps[index].at);
            continue;
        };
        let first = Pc(u32::try_from(steps.len()).map_err(|_| Refused::Facts)?);
        let (leaves, restores) = route_rows(draft, &mut ops, &mut steps, &segments, exit, sink)?;
        if let RegionOp::Checked { next, .. } | RegionOp::Move { next, .. } = &mut ops[index] {
            *next = first;
        }
        routes.push(Departure {
            origin: Pc(u32::try_from(index).map_err(|_| Refused::Facts)?),
            leaves,
            restores,
        });
    }
    // A cleanup whose try suspends an exit completes to that exit's
    // Restore when one is pending.
    for route in &routes {
        for (at, _) in route.restores.iter() {
            let leave = base(draft, &cleanup_leave(&draft.source, at)?)?;
            match &mut ops[leave.offset()] {
                RegionOp::Checked { kernel, args, .. }
                    if args.is_empty()
                        && matches!(
                            kernel.kind,
                            CheckedKind::Phase | CheckedKind::Cleanup { .. }
                        ) =>
                {
                    kernel.kind = CheckedKind::Cleanup { at: at.clone() };
                }
                _ => return Err(Refused::Source),
            }
        }
    }
    draft.ops = ops.into();
    draft.steps = steps.into();
    draft.departures = routes.into();
    Ok(())
}

/// A finally row stands where its kind names: a Suspend reads the exit's
/// value, a Restore writes it back, and a cleanup's Leave reads nothing.
/// Which route a row is on, and where it goes, `verify` checks.
pub(super) fn check_finally(
    draft: &RegionDraft,
    index: usize,
    kind: &CheckedKind,
    args: &[Reg<LeaseValue>],
    dst: Reg<LeaseValue>,
) -> Result<(), Refused> {
    let step = &draft.steps[index];
    let sink = Reg::new(draft.roles.len())?;
    let routed = index >= draft.source.room.events.len();
    let (at, phase) = match kind {
        CheckedKind::Suspend { at, exit } => {
            if !routed || args != active(draft, *exit)?.as_slice() || dst != sink {
                return Err(Refused::Facts);
            }
            (at, Phase::Suspend)
        }
        CheckedKind::Restore { at, exit } | CheckedKind::Escape { at, exit } => {
            if !routed || !args.is_empty() || Some(dst) != active(draft, *exit)?.or(Some(sink)) {
                return Err(Refused::Facts);
            }
            (at, Phase::Restore)
        }
        CheckedKind::Supersede { at } => {
            if !routed || !args.is_empty() || dst != sink {
                return Err(Refused::Facts);
            }
            (at, Phase::Supersede)
        }
        CheckedKind::Cleanup { at } => {
            if routed || !args.is_empty() || dst != sink || !step.event.entities.is_empty() {
                return Err(Refused::Facts);
            }
            if step.at != cleanup_leave(&draft.source, at)? {
                return Err(Refused::Source);
            }
            return Ok(());
        }
        _ => return Err(Refused::Source),
    };
    if step.at.site != *at || step.at.phase != phase {
        return Err(Refused::Source);
    }
    cleanup_leave(&draft.source, at).map(|_| ())
}

/// The Leave of the cleanup block of the try at `at`.
pub(super) fn cleanup_leave(source: &Source, at: &Site) -> Result<Point, Refused> {
    let doc::Node::Stmt(doc::Stmt::Try(t)) = resources::node(source, at)? else {
        return Err(Refused::Source);
    };
    if t.finally.is_none() {
        return Err(Refused::Source);
    }
    Ok(Point {
        site: at.child(if t.catch.is_some() { 2 } else { 1 }),
        phase: Phase::Leave,
    })
}

/// A route's rows after its origin: the departure's own Leaves, then each
/// restore's.
#[expect(clippy::type_complexity, reason = "a route's two row tables")]
fn route_rows(
    draft: &RegionDraft,
    ops: &mut Vec<RegionOp>,
    steps: &mut Vec<Step>,
    segments: &[Arc<CompiledInterval>],
    exit: ExitKind,
    sink: Reg<LeaseValue>,
) -> Result<(Box<[Pc]>, Box<[(Site, Box<[Pc]>)]>), Refused> {
    let mut leaves = Vec::new();
    let mut restores = Vec::new();
    for (position, interval) in segments.iter().enumerate() {
        let events = segment_events(interval, position);
        let last = events.last().ok_or(Refused::Path)?;
        let end = end(draft, exit, interval, last)?;
        let rows = leave_rows(draft, ops, steps, &events, exit, end, position > 0, sink)?;
        if position == 0 {
            leaves = rows;
        } else {
            restores.push((events[0].point.site.clone(), rows.into()));
        }
    }
    Ok((leaves.into(), restores.into()))
}

/// The op of a segment's row: a restore's Restore, a Leave, or its last
/// row, which exits, takes the departure, resumes its loop or suspends.
#[expect(clippy::too_many_arguments, reason = "one row's coordinates")]
fn segment_op(
    draft: &RegionDraft,
    step: &Step,
    row: usize,
    position: usize,
    count: usize,
    exit: ExitKind,
    end: End,
    restored: bool,
    sink: Reg<LeaseValue>,
) -> Result<RegionOp, Refused> {
    let following = Pc(u32::try_from(row + 1).map_err(|_| Refused::Facts)?);
    let phase = |next| RegionOp::Checked {
        kernel: CheckedKernel {
            kind: CheckedKind::Phase,
        },
        args: Box::new([]),
        dst: sink,
        next,
    };
    let active = active(draft, exit)?;
    Ok(if end == End::Escape {
        if !restored || position != 0 || count != 1 {
            return Err(Refused::Path);
        }
        RegionOp::Checked {
            kernel: CheckedKernel {
                kind: CheckedKind::Escape {
                    at: step.at.site.clone(),
                    exit,
                },
            },
            args: Box::new([]),
            dst: active.unwrap_or(sink),
            next: Pc(u32::try_from(row).map_err(|_| Refused::Facts)?),
        }
    } else if restored && position == 0 {
        RegionOp::Checked {
            kernel: CheckedKernel {
                kind: CheckedKind::Restore {
                    at: step.at.site.clone(),
                    exit,
                },
            },
            args: Box::new([]),
            dst: active.unwrap_or(sink),
            next: following,
        }
    } else if position + 1 < count && step.at.phase == Phase::Supersede {
        RegionOp::Checked {
            kernel: CheckedKernel {
                kind: CheckedKind::Supersede {
                    at: step.at.site.clone(),
                },
            },
            args: Box::new([]),
            dst: sink,
            next: following,
        }
    } else if position + 1 < count {
        phase(following)
    } else {
        match end {
            End::Exit => RegionOp::Exit { prefix: step.after },
            End::Escape => return Err(Refused::Path),
            End::Resume(next) if exit == ExitKind::Raise => catch(draft, step, row, next)?,
            End::Resume(next) => phase(next),
            End::Suspend(next) => RegionOp::Checked {
                kernel: CheckedKernel {
                    kind: CheckedKind::Suspend {
                        at: step.at.site.clone(),
                        exit,
                    },
                },
                args: active.into_iter().collect(),
                dst: sink,
                next,
            },
        }
    })
}

/// A segment's rows, from the next free row on.
#[expect(clippy::too_many_arguments, reason = "one segment's coordinates")]
fn leave_rows(
    draft: &RegionDraft,
    ops: &mut Vec<RegionOp>,
    steps: &mut Vec<Step>,
    events: &[&Event],
    exit: ExitKind,
    end: End,
    restored: bool,
    sink: Reg<LeaseValue>,
) -> Result<Vec<Pc>, Refused> {
    let mut rows = Vec::new();
    for (position, event) in events.iter().enumerate() {
        let row = steps.len();
        let template = draft
            .steps
            .iter()
            .find(|step| step.stage.is_none() && step.at == event.point)
            .ok_or(Refused::Source)?;
        let step = route_step(draft, template, event, row)?;
        ops.push(segment_op(
            draft,
            &step,
            row,
            position,
            events.len(),
            exit,
            end,
            restored,
            sink,
        )?);
        steps.push(step);
        rows.push(Pc(u32::try_from(row).map_err(|_| Refused::Facts)?));
    }
    Ok(rows)
}

/// Check the exact row order against canonical syntax, including unreachable
/// departures. No supplied successor, price or recovery summary is an oracle.
pub(super) fn verify(
    draft: &RegionDraft,
    charges: &BTreeMap<Point, Vec<CanonicalClose>>,
) -> Result<usize, Refused> {
    let base_rows = draft
        .source
        .room
        .events
        .len()
        .checked_add(super::stages::layout(&draft.loops)?.len())
        .ok_or(Refused::Facts)?;
    let mut cursor = base_rows;
    let mut routes = draft.departures.iter();
    let sink = Reg::new(draft.roles.len())?;
    for index in 0..draft.source.room.events.len() {
        if raising(draft, index)? {
            let Some(segments) = raise_suffix(draft, &draft.steps[index].at)? else {
                continue;
            };
            let route = routes.next().ok_or(Refused::Path)?;
            let first = segments[0].events().next().ok_or(Refused::Path)?;
            if route.origin.offset() != cursor {
                return Err(Refused::Path);
            }
            let step = draft.steps.get(cursor).ok_or(Refused::Exit)?;
            // The origin row bills nothing: the raising step was billed.
            check_route_step(draft, &BTreeMap::new(), step, first, cursor)?;
            // check_op verifies the delivered move against its event.
            if !matches!(
                draft.ops[cursor],
                RegionOp::Move { src, next, .. }
                    if src == sink && next.offset() == cursor + 1
            ) {
                return Err(Refused::Source);
            }
            cursor += 1;
            check_route(
                draft,
                charges,
                route,
                &segments,
                ExitKind::Raise,
                &mut cursor,
            )?;
            continue;
        }
        let Some(exit) = departing(draft, index)? else {
            continue;
        };
        // check_op verifies an uncaught raise against its event.
        if matches!(
            draft.ops[index],
            RegionOp::Boundary { .. } | RegionOp::Raise { .. } | RegionOp::ReturnVia { .. }
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
        if interval.events().next() != Some(&draft.steps[index].event) {
            return Err(Refused::Source);
        }
        let segments = routed(&draft.source, exit, interval)?.ok_or(Refused::Path)?;
        if matches!(exit, ExitKind::Raise | ExitKind::ReturnVia)
            && !matches!(draft.ops[index], RegionOp::Move { .. })
        {
            return Err(Refused::Source);
        }
        if !matches!(
            exit,
            ExitKind::Return | ExitKind::Raise | ExitKind::ReturnVia
        ) && !matches!(
            &draft.ops[index],
            RegionOp::Checked { kernel, args, dst, .. }
                if kernel.kind == CheckedKind::Phase
                    && args.is_empty()
                    && dst.offset() == draft.roles.len()
                    && draft.steps[index].event.entities.is_empty()
                    && draft.steps[index].event.inputs.is_empty()
        ) {
            return Err(Refused::Source);
        }
        check_route(draft, charges, route, &segments, exit, &mut cursor)?;
        let last = route.rows().last().ok_or(Refused::Exit)?;
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
    // Exactly the cleanups that suspend an exit complete to its Restore.
    for (index, at) in draft.source.room.events.keys().enumerate() {
        let RegionOp::Checked { kernel, .. } = &draft.ops[index] else {
            continue;
        };
        let suspends = |site: &Site| {
            draft
                .departures
                .iter()
                .any(|route| route.restores.iter().any(|(restored, _)| restored == site))
        };
        match kernel.kind() {
            CheckedKind::Cleanup { at: site } => {
                if cleanup_leave(&draft.source, site)? != *at || !suspends(site) {
                    return Err(Refused::Path);
                }
            }
            _ if at.phase == Phase::Leave
                && at.site.path.len() > draft.source.site.path.len()
                && {
                    let mut parent = at.site.clone();
                    parent.path.pop();
                    suspends(&parent) && cleanup_leave(&draft.source, &parent).as_ref() == Ok(at)
                } =>
            {
                return Err(Refused::Path);
            }
            _ => {}
        }
    }
    cursor.checked_sub(base_rows).ok_or(Refused::Facts)
}

/// Check a route's rows from `cursor` on: each segment in order, a
/// restore's at the try its last segment suspended at.
fn check_route(
    draft: &RegionDraft,
    charges: &BTreeMap<Point, Vec<CanonicalClose>>,
    route: &Departure,
    segments: &[Arc<CompiledInterval>],
    exit: ExitKind,
    cursor: &mut usize,
) -> Result<(), Refused> {
    if route.restores.len() + 1 != segments.len() {
        return Err(Refused::Path);
    }
    for (position, interval) in segments.iter().enumerate() {
        let events = segment_events(interval, position);
        let rows: &[Pc] = match position.checked_sub(1) {
            None => &route.leaves,
            Some(restore) => {
                let (at, rows) = &route.restores[restore];
                if events.first().map(|event| &event.point.site) != Some(at) {
                    return Err(Refused::Path);
                }
                rows
            }
        };
        let last = events.last().ok_or(Refused::Path)?;
        let end = end(draft, exit, interval, last)?;
        check_rows(
            draft,
            charges,
            rows,
            &events,
            exit,
            end,
            position > 0,
            cursor,
        )?;
    }
    Ok(())
}

/// Check a segment's rows and their ops, from `cursor` on.
#[expect(clippy::too_many_arguments, reason = "one segment's coordinates")]
fn check_rows(
    draft: &RegionDraft,
    charges: &BTreeMap<Point, Vec<CanonicalClose>>,
    rows: &[Pc],
    events: &[&Event],
    exit: ExitKind,
    end: End,
    restored: bool,
    cursor: &mut usize,
) -> Result<(), Refused> {
    if rows.len() != events.len() || !shaped(events, exit, restored) {
        return Err(Refused::Source);
    }
    let sink = Reg::new(draft.roles.len())?;
    for (position, (pc, event)) in rows.iter().zip(events).enumerate() {
        if pc.offset() != *cursor {
            return Err(Refused::Path);
        }
        let step = draft.steps.get(*cursor).ok_or(Refused::Exit)?;
        check_route_step(draft, charges, step, event, *cursor)?;
        let expected = segment_op(
            draft,
            step,
            *cursor,
            position,
            events.len(),
            exit,
            end,
            restored,
            sink,
        )?;
        let shaped = match (&draft.ops[*cursor], &expected) {
            // check_op verifies the catch's move against its event.
            (
                RegionOp::Move { next, .. } | RegionOp::Heap { next, .. },
                RegionOp::Move { next: expected, .. } | RegionOp::Heap { next: expected, .. },
            ) => next == expected,
            (RegionOp::Exit { prefix }, RegionOp::Exit { prefix: expected }) => prefix == expected,
            (
                RegionOp::Checked {
                    kernel,
                    args,
                    dst,
                    next,
                },
                RegionOp::Checked {
                    kernel: expected_kernel,
                    args: expected_args,
                    dst: expected_dst,
                    next: expected_next,
                },
            ) => {
                kernel.kind == expected_kernel.kind
                    && args == expected_args
                    && dst == expected_dst
                    && next == expected_next
            }
            _ => false,
        };
        if !shaped {
            return Err(Refused::Path);
        }
        *cursor += 1;
    }
    Ok(())
}

pub(super) fn check_edge(draft: &RegionDraft, index: usize) -> Result<(), Refused> {
    let route = draft
        .departures
        .iter()
        .find(|route| route.origin.offset() == index || route.rows().any(|pc| pc.offset() == index))
        .ok_or(Refused::Path)?;
    let expected = if route.origin.offset() == index {
        route.leaves.first().copied()
    } else {
        let exit = route_exit(draft, route)?;
        let interval = draft
            .source
            .compile_departure_suffix(&draft.steps[route.origin.offset()].at)?;
        let segments = routed(&draft.source, exit, interval)?.ok_or(Refused::Path)?;
        let rows = std::iter::once(&route.leaves[..])
            .chain(route.restores.iter().map(|(_, rows)| &rows[..]));
        let (interval, rows) = segments
            .iter()
            .zip(rows)
            .find(|(_, rows)| rows.iter().any(|pc| pc.offset() == index))
            .ok_or(Refused::Path)?;
        let position = rows
            .iter()
            .position(|pc| pc.offset() == index)
            .ok_or(Refused::Path)?;
        match rows.get(position + 1) {
            Some(next) => Some(*next),
            None => {
                let last = interval.events().last().ok_or(Refused::Path)?;
                match end(draft, exit, interval, last)? {
                    End::Exit | End::Escape => None,
                    End::Resume(next) | End::Suspend(next) => Some(next),
                }
            }
        }
    };
    if recovery::successors(draft, index).as_slice() != expected.as_slice() {
        return Err(Refused::Path);
    }
    Ok(())
}
