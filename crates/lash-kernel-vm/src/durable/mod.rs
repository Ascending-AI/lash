//! Seal and open of the bytecode core (cutover contract §5–§7): the
//! machine's state as a [`RunRecord`] keyed by document coordinates, and
//! back.
//!
//! Every value the machine holds outside an object becomes one of the
//! sealed heap's root values, in a fixed walk: the host bindings, the
//! scheduler's held values, then each task's frames, operands, bills and
//! state. Each frame is named by the point its unit's resume table saves
//! it at (`K-SITE-001`); opening maps that point back through the opening
//! build's table and refuses a point the table does not hold.

mod admit;
mod code;
pub(crate) mod session;

use std::collections::HashMap;
use std::sync::Arc;

use lash_kernel_doc::{Datum, JoinMode, Value};
use lash_kernel_state::paged::run::continuation::{
    JoinKind, PointPhase, RunBill, RunFrame, RunHandler, RunJoin, RunJoiner, RunOccurrence,
    RunRecord, RunRequest, RunResume, RunSlot, RunTask, RunTaskState, RunUnit, RunWait,
    SlotRole as SavedRole, UnitKind as SavedKind, WaitPhase,
};
use lash_kernel_value::names::{Name, OperationUse};
use lash_kernel_value::sites::Site;

use crate::bytecode::op::Op;
use crate::bytecode::program::{Program, Unit, UnitId, UnitKind};
use crate::bytecode::site_tables::{Entry, Phase, SlotRole};
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;
use crate::interface::EffectRequest;
use crate::paged::SealError;
use crate::vm::Vm;
use crate::vm::frames::{Bill, Context, Frame, Handler};
use crate::vm::tasks::{Joiner, ListJoin, Pending, Phase as WaitState, Resume, State, Task, Wait};
pub(crate) use admit::admit;
pub(crate) use code::live;

/// The phases a frame can stand in, in the order a saved point is looked
/// for: a frame under another stands at its call.
const TOP: [Phase; 6] = [
    Phase::Issued,
    Phase::Went,
    Phase::Ready,
    Phase::Statement,
    Phase::Back,
    Phase::Call,
];
const UNDER: [Phase; 1] = [Phase::Call];

fn fault(problem: impl Into<String>) -> SealError {
    SealError::Fault {
        problem: problem.into(),
    }
}

/// Seals `vm`'s heap and state, started on `entry` and binding `reads` as
/// reads.
///
/// # Errors
///
/// [`SealError::Fault`] when the machine stands where no resume table
/// saves it; the heap's seal errors.
pub(crate) fn seal(
    vm: &mut Vm<SlotHeap>,
    entry: Option<Name>,
    reads: Vec<OperationUse>,
) -> Result<RunRecord, SealError> {
    if vm.restart.is_some() || vm.retrying || vm.unadmitted.is_some() {
        return Err(fault("the machine is not at a safe point"));
    }
    let program = Arc::clone(&vm.program);
    let occurrences = occurrence_sites(&program);
    let mut values = Values::default();
    let globals = vm.globals.iter().map(|value| values.put(*value)).collect();
    let held = vm.held.iter().map(|value| values.put(*value)).collect();
    let tasks = &vm.tasks;
    let mut saved = Vec::with_capacity(tasks.all.len());
    for (number, task) in tasks.all.iter().enumerate() {
        let cx = if tasks.installed && number == tasks.running {
            &vm.cx
        } else {
            &task.cx
        };
        saved.push(save_task(&program, &occurrences, task, cx, &mut values)?);
    }
    let waits = tasks
        .waits
        .iter()
        .map(|(number, pending)| save_wait(*number, pending))
        .collect::<Result<_, _>>()?;
    let joins = tasks
        .joins
        .iter()
        .map(|(number, join)| save_join(*number, join))
        .collect::<Result<_, _>>()?;
    let mut withdrawn: Vec<u64> = tasks.withdrawn.iter().copied().collect();
    withdrawn.sort_unstable();
    let running = number(tasks.running)?;
    let ready = tasks
        .ready
        .iter()
        .map(|task| number(*task))
        .collect::<Result<_, _>>()?;
    let (installed, live, next_wait, next_join) = (
        tasks.installed,
        tasks.live,
        tasks.next_wait,
        tasks.next_join,
    );
    let issued = tasks.issued.clone();
    let cancellations = tasks.cancellations.clone();
    let (charged, refused) = (vm.charged(), vm.refused);
    let pins = code::live(vm)
        .map_err(|helper| {
            fault(format!(
                "a resume point reaches unavailable helper {helper}"
            ))
        })?
        .into_iter()
        .collect();
    let heap = vm.heap.seal(&values.0)?;
    let held_code = code::held(&program, &vm.heap.code).into_iter().collect();
    Ok(RunRecord {
        heap,
        entry,
        reads,
        charged,
        refused,
        globals,
        session: vm.session.clone(),
        held,
        running,
        installed,
        live,
        tasks: saved,
        ready,
        waits,
        withdrawn,
        next_wait,
        issued,
        cancellations,
        joins,
        next_join,
        pins,
        held_code,
    })
}

/// The values a sealed run holds, in walk order.
#[derive(Default)]
struct Values(Vec<Value>);

impl Values {
    fn put(&mut self, value: Value) -> u32 {
        self.0.push(value);
        (self.0.len() - 1) as u32
    }
}

fn number(at: usize) -> Result<u32, SealError> {
    u32::try_from(at).map_err(|_| fault("a number past u32"))
}

/// Each action and call site's occurrence counter, by its index.
///
/// Only the root Link's: a saved site names no Link, so another Link's
/// counter is refused where it is saved, never read as the root's.
fn occurrence_sites(program: &Program) -> HashMap<u32, Site> {
    let mut sites = HashMap::new();
    for unit in program
        .units
        .iter()
        .filter(|unit| unit.link == program.link)
    {
        for call in &unit.calls {
            sites.insert(call.mark.occurrence, call.site.clone());
        }
        for (action, mark) in unit.actions.iter().zip(&unit.action_marks) {
            sites.insert(mark.occurrence, action.site().clone());
        }
    }
    sites
}

fn save_task(
    program: &Program,
    occurrences: &HashMap<u32, Site>,
    task: &Task,
    cx: &Context,
    values: &mut Values,
) -> Result<RunTask, SealError> {
    let mut frames = Vec::with_capacity(cx.frames.len());
    for (at, frame) in cx.frames.iter().enumerate() {
        let end = cx
            .frames
            .get(at + 1)
            .map_or(cx.stack.len(), |next| next.base as usize);
        let phases: &[Phase] = if at + 1 < cx.frames.len() {
            &UNDER
        } else {
            &TOP
        };
        frames.push(save_frame(program, cx, frame, end, phases, values)?);
    }
    let handlers = cx
        .handlers
        .iter()
        .map(|handler| save_handler(program, cx, handler))
        .collect::<Result<_, _>>()?;
    let billed = cx.billed.iter().map(|value| values.put(*value)).collect();
    let state = match &task.state {
        State::Running => RunTaskState::Running,
        State::Ready(resume) => RunTaskState::Ready(match resume {
            Resume::Continue => RunResume::Continue,
            Resume::Value(value) => RunResume::Value(values.put(*value)),
            Resume::Raise(value) => RunResume::Raise(values.put(*value)),
            Resume::Outcome(wait) => RunResume::Outcome(*wait),
            Resume::Joined(task) => RunResume::Joined(number(*task)?),
        }),
        State::Waiting(Wait::Effect(wait)) => RunTaskState::Effect(*wait),
        State::Waiting(Wait::Join(task)) => RunTaskState::Join(number(*task)?),
        State::Waiting(Wait::JoinMany(join)) => RunTaskState::JoinMany(*join),
        State::Ended(Ok(value)) => RunTaskState::Returned(values.put(*value)),
        State::Ended(Err(value)) => RunTaskState::Raised(values.put(*value)),
    };
    let mut counts = Vec::with_capacity(task.occurrences.len());
    for (occurrence, count) in &task.occurrences {
        let site = occurrences
            .get(occurrence)
            .ok_or_else(|| fault("an occurrence counter names no root Link site"))?;
        counts.push(RunOccurrence {
            site: site.clone(),
            count: *count,
        });
    }
    counts.sort_by(|a, b| a.site.cmp(&b.site));
    let joiners = task
        .joiners
        .iter()
        .map(|joiner| {
            Ok(match joiner {
                Joiner::Single(task) => RunJoiner::Single(number(*task)?),
                Joiner::List(join) => RunJoiner::List(*join),
            })
        })
        .collect::<Result<_, SealError>>()?;
    Ok(RunTask {
        identity: (*task.identity).clone(),
        frames,
        handlers,
        billed,
        state,
        turn: task.turn,
        occurrences: counts,
        spawn_origin: task.spawn_origin.clone(),
        passed: task.passed,
        observed: task.observed,
        joiners,
    })
}

/// The point `frame` stands at: the first of `phases` its unit's resume
/// table saves at its pc whose stack fits the frame's.
fn point<'a>(unit: &'a Unit, frame: &Frame, size: usize, phases: &[Phase]) -> Option<&'a Entry> {
    let slots = usize::from(unit.locals);
    phases.iter().find_map(|phase| {
        unit.sites
            .at(frame.pc, *phase)
            .filter(|entry| slots + entry.resume.stack.len() == size)
    })
}

fn save_frame(
    program: &Program,
    cx: &Context,
    frame: &Frame,
    end: usize,
    phases: &[Phase],
    values: &mut Values,
) -> Result<RunFrame, SealError> {
    let unit = &program.units[frame.unit as usize];
    // A saved unit names no Link: another Link's frame cannot be saved
    // until it does, and is never reopened as the root's.
    if unit.link != program.link {
        return Err(fault(format!(
            "unit {} of another Link stands in the run",
            unit.name
        )));
    }
    let base = frame.base as usize;
    let size = end
        .checked_sub(base)
        .ok_or_else(|| fault("a frame's stack ends below its base"))?;
    let entry = point(unit, frame, size, phases).ok_or_else(|| {
        fault(format!(
            "unit {} saves no point at op {} for a stack of {size}",
            unit.name, frame.pc
        ))
    })?;
    let locals = unit.sites.locals();
    if locals.len() != usize::from(unit.locals) {
        return Err(fault(format!(
            "unit {} names {} of its slots",
            unit.name,
            locals.len()
        )));
    }
    let slots = locals
        .iter()
        .enumerate()
        .map(|(at, local)| RunSlot {
            site: local.site.to_vec(),
            role: save_role(&local.role),
            copy: local.copy,
            value: values.put(cx.stack[base + at]),
        })
        .collect();
    let operands = cx.stack[base + locals.len()..end]
        .iter()
        .map(|value| values.put(*value))
        .collect();
    Ok(RunFrame {
        unit: RunUnit {
            kind: save_kind(&unit.kind),
            site: unit.site.clone(),
        },
        site: entry.site.clone(),
        phase: save_phase(entry.phase),
        copy: entry.copy,
        slots,
        operands,
        closure: frame
            .closure
            .map(|closure| values.put(Value::Closure(closure))),
        bill: frame.bill.map(|bill| RunBill {
            charge: bill.charge,
            from: bill.from,
        }),
    })
}

/// A handler, by the `try` that installed it.
fn save_handler(
    program: &Program,
    cx: &Context,
    handler: &Handler,
) -> Result<RunHandler, SealError> {
    let frame = cx
        .frames
        .get(handler.frame as usize)
        .ok_or_else(|| fault("a handler names no frame"))?;
    let unit = &program.units[frame.unit as usize];
    let at = unit
        .ops
        .iter()
        .position(|op| matches!(op, Op::TryEnter(to) if *to == handler.pc))
        .ok_or_else(|| fault("a handler names no try"))?;
    let site = origin(unit, at);
    let copy = (0..at)
        .filter(|&before| {
            matches!(unit.ops[before], Op::TryEnter(_)) && origin(unit, before) == site
        })
        .count();
    Ok(RunHandler {
        frame: handler.frame,
        site: site.to_vec(),
        copy: copy as u32,
        height: handler
            .height
            .checked_sub(frame.base)
            .ok_or_else(|| fault("a handler's height is below its frame"))?,
    })
}

fn origin(unit: &Unit, pc: usize) -> &[u32] {
    unit.origin
        .get(pc)
        .map_or(unit.site.path.as_slice(), |path| path)
}

fn save_wait(number: u64, pending: &Pending) -> Result<RunWait, SealError> {
    Ok(RunWait {
        number,
        task: self::number(pending.task)?,
        phase: match &pending.phase {
            WaitState::Issued => WaitPhase::Issued,
            WaitState::Admitted => WaitPhase::Admitted,
            WaitState::Delivered(crate::Outcome::Completed(datum)) => {
                WaitPhase::Completed(datum.clone())
            }
            WaitState::Delivered(crate::Outcome::Failed(error)) => {
                WaitPhase::Failed(Datum::Error(Box::new(error.clone())))
            }
        },
        result: pending.result.clone(),
        then_yield: pending.then_yield,
        request: pending.request.as_ref().map(|request| RunRequest {
            identity: lash_kernel_value::kernel_internals::effect_parts(&request.identity).clone(),
            use_: request.use_,
            args: request.args.clone(),
            result: request.result.clone(),
            origin: request.origin.clone(),
        }),
    })
}

fn save_join(number: u64, join: &ListJoin) -> Result<RunJoin, SealError> {
    Ok(RunJoin {
        number,
        joiner: self::number(join.joiner)?,
        mode: match join.mode {
            JoinMode::All => JoinKind::All,
            JoinMode::AllSettled => JoinKind::AllSettled,
            JoinMode::Race => JoinKind::Race,
            JoinMode::Any => JoinKind::Any,
        },
        members: join
            .members
            .iter()
            .map(|member| self::number(*member))
            .collect::<Result<_, _>>()?,
    })
}

fn save_kind(kind: &UnitKind) -> SavedKind {
    match kind {
        UnitKind::Main => SavedKind::Main,
        UnitKind::Declared(_) => SavedKind::Declared,
        UnitKind::Body(_) => SavedKind::Body,
        UnitKind::Closure(_) => SavedKind::Closure,
        UnitKind::Spawn => SavedKind::Spawn,
    }
}

fn save_phase(phase: Phase) -> PointPhase {
    match phase {
        Phase::Call => PointPhase::Call,
        Phase::Ready => PointPhase::Ready,
        Phase::Issued => PointPhase::Issued,
        Phase::Went => PointPhase::Went,
        Phase::Statement => PointPhase::Statement,
        Phase::Back => PointPhase::Back,
    }
}

fn open_phase(phase: PointPhase) -> Phase {
    match phase {
        PointPhase::Call => Phase::Call,
        PointPhase::Ready => Phase::Ready,
        PointPhase::Issued => Phase::Issued,
        PointPhase::Went => Phase::Went,
        PointPhase::Statement => Phase::Statement,
        PointPhase::Back => Phase::Back,
    }
}

fn save_role(role: &SlotRole) -> SavedRole {
    match role {
        SlotRole::Variable(name) => SavedRole::Variable(name.clone()),
        SlotRole::Argument(at) => SavedRole::Argument(u32::from(*at)),
        SlotRole::Held => SavedRole::Held,
        SlotRole::Passes => SavedRole::Passes,
        SlotRole::Loop(at) => SavedRole::Loop(u32::from(*at)),
    }
}

fn open_role(role: &SavedRole) -> Result<SlotRole, String> {
    let narrow = |at: u32| format!("slot role {at} is out of range");
    Ok(match role {
        SavedRole::Variable(name) => SlotRole::Variable(name.clone()),
        SavedRole::Argument(at) => SlotRole::Argument(u16::try_from(*at).map_err(|_| narrow(*at))?),
        SavedRole::Held => SlotRole::Held,
        SavedRole::Passes => SlotRole::Passes,
        SavedRole::Loop(at) => SlotRole::Loop(u8::try_from(*at).map_err(|_| narrow(*at))?),
    })
}

/// Restores `record`'s machine state into `vm`, opened over the heap the
/// record names, whose root values are `values` in walk order. Admits
/// each frame's holding.
///
/// # Errors
///
/// A coordinate the program's tables do not hold, a value index past the
/// roots, or a frame the memory bound does not admit.
pub(crate) fn open(
    vm: &mut Vm<SlotHeap>,
    record: &RunRecord,
    values: &[Value],
) -> Result<(), String> {
    let program = Arc::clone(&vm.program);
    let value = |at: u32| {
        values
            .get(at as usize)
            .copied()
            .ok_or_else(|| format!("value {at} is past the run's roots"))
    };
    if record.session.windows(2).any(|pair| pair[0] >= pair[1])
        || record
            .session
            .iter()
            .any(|name| program.globals.contains(name))
    {
        return Err("the run's session names are not ascending past the program's".into());
    }
    vm.globals
        .resize(program.globals.len() + record.session.len(), Value::Absent);
    vm.session.clone_from(&record.session);
    if record.globals.len() != vm.globals.len() {
        return Err("the run holds another number of host bindings".into());
    }
    vm.heap.code = record.held_code.iter().copied().collect();
    for (slot, at) in vm.globals.iter_mut().zip(&record.globals) {
        *slot = value(*at)?;
    }
    vm.held = record
        .held
        .iter()
        .map(|at| value(*at))
        .collect::<Result<_, _>>()?;
    let units: HashMap<(SavedKind, &Site), UnitId> = program
        .units
        .iter()
        .enumerate()
        .filter(|(_, unit)| unit.link == program.link)
        .map(|(at, unit)| ((save_kind(&unit.kind), &unit.site), at as UnitId))
        .collect();
    let occurrences: HashMap<Site, u32> = occurrence_sites(&program)
        .into_iter()
        .map(|(at, site)| (site, at))
        .collect();
    let count = record.tasks.len();
    let task_number = |at: u32| {
        let at = at as usize;
        if at < count {
            Ok(at)
        } else {
            Err(format!("task {at} is past the task table"))
        }
    };
    let mut tasks = Vec::with_capacity(count);
    for saved in &record.tasks {
        let mut cx = Context::default();
        for frame in &saved.frames {
            open_frame(vm, &program, &units, &mut cx, frame, &value)?;
        }
        for handler in &saved.handlers {
            cx.handlers.push(open_handler(&program, &cx, handler)?);
        }
        cx.billed = saved
            .billed
            .iter()
            .map(|at| value(*at))
            .collect::<Result<_, _>>()?;
        let mut task = Task::new(Arc::new(saved.identity.clone()), saved.spawn_origin.clone());
        task.cx = cx;
        task.state = match &saved.state {
            RunTaskState::Running => State::Running,
            RunTaskState::Ready(resume) => State::Ready(match resume {
                RunResume::Continue => Resume::Continue,
                RunResume::Value(at) => Resume::Value(value(*at)?),
                RunResume::Raise(at) => Resume::Raise(value(*at)?),
                RunResume::Outcome(wait) => Resume::Outcome(*wait),
                RunResume::Joined(task) => Resume::Joined(task_number(*task)?),
            }),
            RunTaskState::Effect(wait) => State::Waiting(Wait::Effect(*wait)),
            RunTaskState::Join(task) => State::Waiting(Wait::Join(task_number(*task)?)),
            RunTaskState::JoinMany(join) => State::Waiting(Wait::JoinMany(*join)),
            RunTaskState::Returned(at) => State::Ended(Ok(value(*at)?)),
            RunTaskState::Raised(at) => State::Ended(Err(value(*at)?)),
        };
        task.turn = saved.turn;
        for occurrence in &saved.occurrences {
            let at = occurrences
                .get(&occurrence.site)
                .ok_or_else(|| format!("no action stands at {:?}", occurrence.site))?;
            task.occurrences.insert(*at, occurrence.count);
        }
        task.passed = saved.passed;
        task.observed = saved.observed;
        task.joiners = saved
            .joiners
            .iter()
            .map(|joiner| {
                Ok(match joiner {
                    RunJoiner::Single(task) => Joiner::Single(task_number(*task)?),
                    RunJoiner::List(join) => Joiner::List(*join),
                })
            })
            .collect::<Result<_, String>>()?;
        tasks.push(task);
    }
    let running = task_number(record.running)?;
    let scheduler = &mut vm.tasks;
    scheduler.all = tasks;
    scheduler.running = running;
    scheduler.installed = record.installed;
    if record.installed {
        vm.cx = std::mem::take(&mut vm.tasks.all[running].cx);
    }
    let scheduler = &mut vm.tasks;
    scheduler.live = record.live;
    scheduler.ready = record
        .ready
        .iter()
        .map(|task| task_number(*task))
        .collect::<Result<_, _>>()?;
    scheduler.waits = record
        .waits
        .iter()
        .map(|wait| Ok((wait.number, open_wait(wait, task_number(wait.task)?)?)))
        .collect::<Result<_, String>>()?;
    scheduler.withdrawn = record.withdrawn.iter().copied().collect();
    scheduler.next_wait = record.next_wait;
    scheduler.issued = record.issued.clone();
    scheduler.cancellations = record.cancellations.clone();
    scheduler.joins = record
        .joins
        .iter()
        .map(|join| {
            Ok((
                join.number,
                ListJoin {
                    joiner: task_number(join.joiner)?,
                    mode: match join.mode {
                        JoinKind::All => JoinMode::All,
                        JoinKind::AllSettled => JoinMode::AllSettled,
                        JoinKind::Race => JoinMode::Race,
                        JoinKind::Any => JoinMode::Any,
                    },
                    members: join
                        .members
                        .iter()
                        .map(|member| task_number(*member))
                        .collect::<Result<_, String>>()?,
                },
            ))
        })
        .collect::<Result<_, String>>()?;
    scheduler.next_join = record.next_join;
    vm.refused = record.refused;
    let raw: u64 = vm
        .tasks
        .waits
        .values()
        .filter_map(|pending| match &pending.phase {
            WaitState::Delivered(outcome) => Some(crate::heap::meter::outcome(outcome)),
            _ => None,
        })
        .fold(0, u64::saturating_add);
    vm.hold_raw(raw);
    Ok(())
}

fn open_frame(
    vm: &mut Vm<SlotHeap>,
    program: &Program,
    units: &HashMap<(SavedKind, &Site), UnitId>,
    cx: &mut Context,
    saved: &RunFrame,
    value: &dyn Fn(u32) -> Result<Value, String>,
) -> Result<(), String> {
    let id = *units
        .get(&(saved.unit.kind, &saved.unit.site))
        .ok_or_else(|| format!("the program has no unit {:?}", saved.unit.site))?;
    let unit = &program.units[id as usize];
    let entry = unit
        .sites
        .resume(&saved.site, open_phase(saved.phase), saved.copy)
        .ok_or_else(|| {
            format!(
                "unit {} saves no point {:?} {:?} copy {}",
                unit.name, saved.site, saved.phase, saved.copy
            )
        })?;
    if saved.slots.len() != usize::from(unit.locals) {
        return Err(format!(
            "a frame of {} holds another number of slots",
            unit.name
        ));
    }
    if saved.operands.len() != entry.resume.stack.len() {
        return Err(format!(
            "a frame of {} holds another stack at its point",
            unit.name
        ));
    }
    vm.heap
        .admit(vm.frames[id as usize])
        .map_err(|refused| format!("the memory bound does not admit a frame: {refused:?}"))?;
    let base = cx.stack.len();
    cx.stack
        .resize(base + usize::from(unit.locals), Value::Absent);
    let mut filled = vec![false; usize::from(unit.locals)];
    for slot in &saved.slots {
        let local = unit
            .sites
            .local(&slot.site, &open_role(&slot.role)?, slot.copy)
            .ok_or_else(|| format!("unit {} declares no slot {:?}", unit.name, slot.role))?;
        let at = usize::from(local.slot);
        if std::mem::replace(&mut filled[at], true) {
            return Err(format!("a frame of {} names slot {at} twice", unit.name));
        }
        cx.stack[base + at] = value(slot.value)?;
    }
    for operand in &saved.operands {
        cx.stack.push(value(*operand)?);
    }
    let closure = match saved.closure {
        None => None,
        Some(at) => match value(at)? {
            Value::Closure(handle) => Some(handle),
            _ => return Err("a frame's closure is not a closure".into()),
        },
    };
    cx.frames.push(Frame {
        unit: id,
        pc: entry.resume.pc,
        base: base as u32,
        closure,
        bill: saved.bill.as_ref().map(|bill| Bill {
            charge: bill.charge,
            from: bill.from,
        }),
    });
    Ok(())
}

fn open_handler(program: &Program, cx: &Context, saved: &RunHandler) -> Result<Handler, String> {
    let frame = cx
        .frames
        .get(saved.frame as usize)
        .ok_or("a handler names no frame")?;
    let unit = &program.units[frame.unit as usize];
    let pc = unit
        .ops
        .iter()
        .enumerate()
        .filter(|(at, op)| {
            matches!(op, Op::TryEnter(_)) && origin(unit, *at) == saved.site.as_slice()
        })
        .nth(saved.copy as usize)
        .and_then(|(_, op)| match op {
            Op::TryEnter(to) => Some(*to),
            _ => None,
        })
        .ok_or_else(|| format!("unit {} has no try {:?}", unit.name, saved.site))?;
    Ok(Handler {
        frame: saved.frame,
        pc,
        height: frame.base + saved.height,
    })
}

fn open_wait(saved: &RunWait, task: usize) -> Result<Pending, String> {
    Ok(Pending {
        task,
        phase: match &saved.phase {
            WaitPhase::Issued => WaitState::Issued,
            WaitPhase::Admitted => WaitState::Admitted,
            WaitPhase::Completed(datum) => {
                WaitState::Delivered(crate::Outcome::Completed(datum.clone()))
            }
            WaitPhase::Failed(Datum::Error(error)) => {
                WaitState::Delivered(crate::Outcome::Failed((**error).clone()))
            }
            WaitPhase::Failed(_) => {
                return Err(format!("wait {} failed with no error", saved.number));
            }
        },
        result: saved.result.clone(),
        then_yield: saved.then_yield,
        request: saved.request.as_ref().map(|request| EffectRequest {
            wait: lash_kernel_doc::kernel_internals::wait_id(saved.number),
            identity: lash_kernel_value::kernel_internals::effect_id(request.identity.clone()),
            use_: request.use_,
            args: request.args.clone(),
            result: request.result.clone(),
            origin: request.origin.clone(),
        }),
    })
}
