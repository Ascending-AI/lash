//! The admission relation of a sealed run (cutover contract §5): the
//! relation over the whole saved state that every state the machine
//! reaches satisfies. Open checks it before restoring anything, and each
//! broken rule refuses with its own [`Inadmissible`].
//!
//! What the record cannot express needs no check: a call completes inside
//! one op, so no frame saves a returned or raised completion or a quoted
//! fee; a loop's pass counter is its slot, so no second counter can
//! disagree with it; a wait consumed is no longer held, so its receipt is
//! the wait number below [`RunRecord::next_wait`], which delivery refuses.

use std::collections::{HashMap, HashSet};

use lash_kernel_state::paged::run::continuation::{
    PointPhase, RunFrame, RunJoiner, RunRecord, RunResume, RunTask, RunTaskState, UnitKind,
    WaitPhase,
};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::sites::{Site, TaskIdentity};

use super::Units;
use crate::bytecode::program::{ActionSite, Program};
use crate::paged::Inadmissible;
use crate::vm::tasks::Operations;

/// Checks `record` against the relation, the program it opens under and
/// the operations the opening build resolved.
///
/// # Errors
///
/// The first rule the record breaks.
pub(crate) fn admit(
    program: &Program,
    operations: &Operations,
    record: &RunRecord,
) -> Result<(), Inadmissible> {
    let units = Units::new(program);
    let tasks = &record.tasks;
    tasks_in_order(program, &units, tasks)?;
    running(record)?;
    queue(record)?;
    waits(&units, operations, record)?;
    joins(record)?;
    for (number, task) in tasks.iter().enumerate() {
        frames(number as u32, task)?;
        turn(&units, number as u32, task)?;
    }
    Ok(())
}

/// Only a delivered `then yield` takes a turn before it resumes (contract
/// §5.1, `K-TASK-005`): a task that holds one is ready, its top frame at a
/// `perform` that yields after it.
fn turn(units: &Units<'_>, number: u32, task: &RunTask) -> Result<(), Inadmissible> {
    if !task.turn {
        return Ok(());
    }
    let yields = matches!(task.state, RunTaskState::Ready(_))
        && task.frames.last().is_some_and(|top| {
            top.phase == PointPhase::Issued
                && matches!(
                    perform(units, top, number == 0 && task.frames.len() == 1),
                    Some(ActionSite::Perform {
                        then_yield: true,
                        ..
                    })
                )
        });
    if yields {
        Ok(())
    } else {
        Err(Inadmissible::Turn { task: number })
    }
}

/// Spawn order, roots, ended tasks and the counters spawning minted.
fn tasks_in_order(
    program: &Program,
    units: &Units<'_>,
    tasks: &[RunTask],
) -> Result<(), Inadmissible> {
    for (number, task) in tasks.iter().enumerate() {
        let at = number as u32;
        match (&task.identity, number) {
            (TaskIdentity::Main, 0) => {}
            (TaskIdentity::Spawned(spawned), 1..) => {
                let parent = tasks[..number]
                    .iter()
                    .position(|earlier| earlier.identity == *spawned.parent)
                    .ok_or(Inadmissible::SpawnOrder { task: at })?;
                // A spawn identity names its site but not its Link: the
                // spawner's counter at that site in any Link it ran.
                let spawns = tasks[parent]
                    .occurrences
                    .iter()
                    .filter(|occurrence| occurrence.site == spawned.site)
                    .map(|occurrence| occurrence.count)
                    .max()
                    .unwrap_or(0);
                if spawns <= spawned.occurrence {
                    return Err(Inadmissible::Counter {
                        task: parent as u32,
                    });
                }
            }
            _ => return Err(Inadmissible::SpawnOrder { task: at }),
        }
        let ended = matches!(
            task.state,
            RunTaskState::Returned(_) | RunTaskState::Raised(_)
        );
        if ended && !(task.frames.is_empty() && task.handlers.is_empty()) {
            return Err(Inadmissible::Ended { task: at });
        }
        if let Some(root) = task.frames.first() {
            let unit = units.get(&root.unit, number == 0);
            let starts = match number {
                0 => unit.is_some_and(|(unit, _)| unit == program.main),
                _ => root.unit.kind == UnitKind::Spawn,
            };
            if !starts {
                return Err(Inadmissible::Root { task: at });
            }
        }
    }
    Ok(())
}

/// How many times `task` has run the action at `site`.
fn count(task: &RunTask, link: LinkId, site: &Site) -> u64 {
    task.occurrences
        .iter()
        .find(|occurrence| occurrence.link == link && occurrence.site == *site)
        .map_or(0, |occurrence| occurrence.count)
}

/// The installed task runs, no other does, and the live count is the
/// tasks that have not ended.
fn running(record: &RunRecord) -> Result<(), Inadmissible> {
    let mut counted = 0;
    for (number, task) in record.tasks.iter().enumerate() {
        let at = number as u32;
        if matches!(task.state, RunTaskState::Running)
            && !(record.installed && at == record.running)
        {
            return Err(Inadmissible::Running { task: at });
        }
        if !matches!(
            task.state,
            RunTaskState::Returned(_) | RunTaskState::Raised(_)
        ) {
            counted += 1;
        }
    }
    if record.installed
        && let Some(task) = record.tasks.get(record.running as usize)
        && !matches!(task.state, RunTaskState::Running)
    {
        return Err(Inadmissible::Running {
            task: record.running,
        });
    }
    if counted != record.live {
        return Err(Inadmissible::Live {
            saved: record.live,
            counted,
        });
    }
    Ok(())
}

/// The ready queue holds each ready task once and nothing else
/// (`K-TASK-003`).
fn queue(record: &RunRecord) -> Result<(), Inadmissible> {
    let mut queued = HashSet::new();
    for &task in &record.ready {
        if !queued.insert(task) {
            return Err(Inadmissible::ReadyDuplicate { task });
        }
        if !matches!(
            record.tasks.get(task as usize).map(|task| &task.state),
            Some(RunTaskState::Ready(_))
        ) {
            return Err(Inadmissible::Queue { task });
        }
    }
    for (number, task) in record.tasks.iter().enumerate() {
        if matches!(task.state, RunTaskState::Ready(_)) && !queued.contains(&(number as u32)) {
            return Err(Inadmissible::Queue {
                task: number as u32,
            });
        }
    }
    Ok(())
}

/// Each wait is owned by the one task whose state names it in its phase:
/// requested or handed over while the task waits issued at its perform,
/// delivered with its raw outcome while the task is ready to resume with
/// it. No wait performs a read.
fn waits(
    units: &Units<'_>,
    operations: &Operations,
    record: &RunRecord,
) -> Result<(), Inadmissible> {
    let mut held = HashMap::new();
    for wait in &record.waits {
        let number = wait.number;
        if held.insert(number, wait).is_some() {
            return Err(Inadmissible::DuplicateWait { wait: number });
        }
        if number >= record.next_wait || record.withdrawn.contains(&number) {
            return Err(Inadmissible::WaitNumber { wait: number });
        }
        let requested = matches!(wait.phase, WaitPhase::Issued);
        if requested != wait.request.is_some() || requested != record.issued.contains(&number) {
            return Err(Inadmissible::Issued { wait: number });
        }
        if matches!(&wait.phase, WaitPhase::Failed(datum) if !matches!(datum, lash_kernel_doc::Datum::Error(_)))
        {
            return Err(Inadmissible::Outcome { wait: number });
        }
        let task = record
            .tasks
            .get(wait.task as usize)
            .ok_or(Inadmissible::OrphanWait { wait: number })?;
        let owned = match &task.state {
            RunTaskState::Effect(waiting) => *waiting == number,
            RunTaskState::Ready(RunResume::Outcome(resuming)) => *resuming == number,
            _ => false,
        };
        if !owned {
            return Err(Inadmissible::OrphanWait { wait: number });
        }
        let top = task
            .frames
            .last()
            .filter(|top| top.phase == PointPhase::Issued)
            .ok_or(Inadmissible::WaitPoint { wait: number })?;
        let link = top.unit.link;
        let root = wait.task == 0 && task.frames.len() == 1;
        let action = perform(units, top, root).ok_or(Inadmissible::WaitPoint { wait: number })?;
        let ActionSite::Perform {
            owner,
            slot,
            result,
            then_yield,
            site,
            ..
        } = action
        else {
            return Err(Inadmissible::WaitPoint { wait: number });
        };
        if wait.result != *result || wait.then_yield != *then_yield {
            return Err(Inadmissible::WaitPoint { wait: number });
        }
        if let Some(request) = &wait.request
            && (request.identity.task != task.identity
                || request.identity.site != *site
                || count(task, link, site) <= request.identity.occurrence)
        {
            return Err(Inadmissible::WaitPoint { wait: number });
        }
        if matches!(operations.get(&(link, *owner, *slot)), Some(Ok(operation)) if operation.read) {
            return Err(Inadmissible::ReadBoundWait { wait: number });
        }
    }
    for &issued in &record.issued {
        if !held.contains_key(&issued) {
            return Err(Inadmissible::Issued { wait: issued });
        }
    }
    for (number, task) in record.tasks.iter().enumerate() {
        let (wait, delivered) = match &task.state {
            RunTaskState::Effect(wait) => (*wait, false),
            RunTaskState::Ready(RunResume::Outcome(wait)) => (*wait, true),
            _ => continue,
        };
        let phase_fits = held.get(&wait).is_some_and(|held| {
            delivered == matches!(held.phase, WaitPhase::Completed(_) | WaitPhase::Failed(_))
        });
        if !phase_fits {
            return Err(Inadmissible::MissingWait {
                task: number as u32,
                wait,
            });
        }
    }
    Ok(())
}

/// The action a frame stands at, by its point's site; `root` when it is
/// the run's root frame.
fn perform<'u>(units: &Units<'u>, frame: &RunFrame, root: bool) -> Option<&'u ActionSite> {
    let (unit, _) = units.get(&frame.unit, root)?;
    units
        .program
        .unit(unit)
        .actions
        .iter()
        .find(|action| matches!(action, ActionSite::Perform { site, .. } if *site == frame.site))
}

/// Single joins and list joins name each other both ways, and only tasks
/// that have not ended are joined.
fn joins(record: &RunRecord) -> Result<(), Inadmissible> {
    let tasks = &record.tasks;
    let ended = |task: u32| {
        tasks.get(task as usize).is_none_or(|task| {
            matches!(
                task.state,
                RunTaskState::Returned(_) | RunTaskState::Raised(_)
            )
        })
    };
    let mut numbers = HashSet::new();
    for join in &record.joins {
        let broken = Inadmissible::ListJoin { join: join.number };
        if !numbers.insert(join.number) || join.number >= record.next_join {
            return Err(broken);
        }
        let joiner = tasks.get(join.joiner as usize).ok_or(broken.clone())?;
        if joiner.state != RunTaskState::JoinMany(join.number) {
            return Err(broken);
        }
        for &member in &join.members {
            let named = tasks
                .get(member as usize)
                .is_some_and(|task| task.joiners.contains(&RunJoiner::List(join.number)));
            if !named && !ended(member) {
                return Err(broken);
            }
        }
    }
    for (number, task) in tasks.iter().enumerate() {
        let at = number as u32;
        let broken = Inadmissible::Join { task: at };
        if !task.joiners.is_empty() && ended(at) {
            return Err(broken);
        }
        for joiner in &task.joiners {
            let names = match joiner {
                RunJoiner::Single(joiner) => tasks
                    .get(*joiner as usize)
                    .is_some_and(|joiner| joiner.state == RunTaskState::Join(at)),
                RunJoiner::List(join) => record
                    .joins
                    .iter()
                    .any(|list| list.number == *join && list.members.contains(&at)),
            };
            if !names {
                return Err(broken);
            }
        }
        match &task.state {
            RunTaskState::Join(joined) => {
                let named = tasks
                    .get(*joined as usize)
                    .is_some_and(|joined| joined.joiners.contains(&RunJoiner::Single(at)));
                if !named || ended(*joined) {
                    return Err(broken);
                }
            }
            RunTaskState::JoinMany(join)
                if !record
                    .joins
                    .iter()
                    .any(|list| list.number == *join && list.joiner == at) =>
            {
                return Err(broken);
            }
            _ => {}
        }
    }
    Ok(())
}

/// A frame under another stands at its call; closure bodies, and only
/// they, run a closure; bills stand within the task's billed values, each
/// at or above the bill of the frame under it.
fn frames(number: u32, task: &RunTask) -> Result<(), Inadmissible> {
    let top = task.frames.len().saturating_sub(1);
    let mut floor = 0;
    for (at, frame) in task.frames.iter().enumerate() {
        let index = at as u32;
        if at < top && frame.phase != PointPhase::Call {
            return Err(Inadmissible::SuspendedCall {
                task: number,
                frame: index,
            });
        }
        if (frame.unit.kind == UnitKind::Closure) != frame.closure.is_some() {
            return Err(Inadmissible::Captures {
                task: number,
                frame: index,
            });
        }
        if let Some(bill) = &frame.bill {
            if bill.from < floor || bill.from as usize > task.billed.len() {
                return Err(Inadmissible::Bill {
                    task: number,
                    frame: index,
                });
            }
            floor = bill.from;
        }
    }
    Ok(())
}
