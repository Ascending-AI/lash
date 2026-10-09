//! Rebuilding a run from a parked state, against an executable compiled
//! afresh.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use lash_kernel_doc::{
    FunctionId, Identity, KERNEL_VERSION, Object, ObjectId, Site, TaskId, Unit, Value,
    validate_document,
};
use lash_kernel_state as state;
use lash_kernel_state::{ParkedCall, ParkedRun};

use super::super::{
    Answered, Completion, Control, Cursor, Frame, Incoming, Joiner, KernelMachine, LibraryCall,
    ListJoin, PendingWait, SlotState, Task, TaskState, TryPhase, Wait,
};
use super::action_of;
use crate::Layout;
use crate::compile::{ActionKind, BlockId, CodeId, Executable, LibId, Stmt, compile};
use crate::heap::{ClosureObj, Heap, Key, MAX_VALUE_DEPTH, Obj, Table, within_depth};
use crate::interface::{
    Bounds, EffectRequest, ImportError, Outcome, Program, Request, SleepRequest, WaitId,
};

fn malformed(problem: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        problem: problem.into(),
    }
}

/// The code whose body holds the statement at `site`: the unit's, or the
/// closure body nearest above it.
fn code_of(exe: &Executable, site: &Site) -> Option<CodeId> {
    let block = site.path.len().checked_sub(1)?;
    (0..=block).rev().find_map(|length| {
        exe.code_at
            .get(&Site::new(site.unit.clone(), &site.path[..length]))
            .copied()
    })
}

/// A parked run being put back.
struct Restore<'a> {
    exe: &'a Executable,
    heap: &'a Heap,
    tasks: usize,
}

impl Restore<'_> {
    /// Checks that a value names only what the run holds, as the kind it
    /// says.
    fn value(&self, value: &Value) -> Result<(), ImportError> {
        if !within_depth(value, MAX_VALUE_DEPTH) {
            return Err(malformed("a value nests too deep"));
        }
        self.named(value)
    }

    fn named(&self, value: &Value) -> Result<(), ImportError> {
        let held = |id: &ObjectId| self.heap.get(*id);
        let fits = match value {
            Value::Tuple(members) => {
                return members.iter().try_for_each(|member| self.named(member));
            }
            Value::Error(error) => return self.named(&error.data),
            Value::List(id) => matches!(held(id), Some(Obj::List(_))),
            Value::Map(id) => matches!(held(id), Some(Obj::Map(_))),
            Value::Set(id) => matches!(held(id), Some(Obj::Set(_))),
            Value::Record(id) => matches!(held(id), Some(Obj::Record(_))),
            Value::Closure(id) => matches!(held(id), Some(Obj::Closure(_))),
            Value::Ref(Identity::Object(id)) => {
                matches!(held(id), Some(object) if !matches!(object, Obj::Variable(_)))
            }
            Value::Task(task) | Value::Ref(Identity::Task(task)) => (task.0 as usize) < self.tasks,
            _ => true,
        };
        if fits {
            Ok(())
        } else {
            Err(malformed(
                "a value names an object or a task the state does not hold",
            ))
        }
    }

    fn values<'v>(&self, values: impl IntoIterator<Item = &'v Value>) -> Result<(), ImportError> {
        values.into_iter().try_for_each(|value| self.value(value))
    }

    fn cell(&self, cell: ObjectId) -> Result<(), ImportError> {
        match self.heap.get(cell) {
            Some(Obj::Variable(_)) => Ok(()),
            _ => Err(malformed("a shared variable has no cell")),
        }
    }

    fn object(&self, object: &Obj) -> Result<(), ImportError> {
        match object {
            Obj::List(items) => self.values(items),
            Obj::Map(table) | Obj::Set(table) => table.iter().try_for_each(|(key, value)| {
                self.value(key)?;
                self.value(value)
            }),
            Obj::Record(fields) => self.values(fields.iter().map(|(_, value)| value)),
            Obj::Closure(closure) => closure
                .captures
                .iter()
                .try_for_each(|cell| self.cell(*cell)),
            Obj::Variable(value) => self.value(value),
        }
    }

    /// Rebuilds one call's frame: the blocks, loops and `try` statements
    /// around its pending statement, from the statement's site. A call
    /// that `continues` stands in the statement's action.
    fn frame(&self, parked: &ParkedCall, continues: bool) -> Result<Frame, ImportError> {
        let exe = self.exe;
        let ParkedCall { call, held } = parked;
        let code_id = code_of(exe, &call.statement)
            .ok_or_else(|| malformed(format!("no function body holds {}", call.statement)))?;
        let code = &exe.codes[code_id.0 as usize];
        let nowhere = || {
            malformed(format!(
                "{} is no statement of the document",
                call.statement
            ))
        };
        let path = &call.statement.path;
        let mut depth = code.site.path.len();
        let mut block = code.body;
        let mut control = Vec::new();
        let mut loops = call.loops.iter();
        let mut finally = call.finally.iter();
        let mut iterated = held.iterated.iter();
        let mut departing = held.departing.iter();
        let mut awaiting = None;
        loop {
            let index = *path.get(depth).ok_or_else(nowhere)? as usize;
            let stmts = &exe.blocks[block.0 as usize].stmts;
            if depth + 1 == path.len() {
                let next = if continues {
                    let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                    if action_of(exe, stmt).is_none() {
                        return Err(malformed(format!(
                            "{} has no action to continue",
                            call.statement
                        )));
                    }
                    awaiting = Some(stmt);
                    index + 1
                } else if index <= stmts.len() {
                    index
                } else {
                    return Err(nowhere());
                };
                control.push(Control::Block { block, next });
                break;
            }
            // The statement holds the block the path goes on into: it has
            // been issued.
            let stmt = *stmts.get(index).ok_or_else(nowhere)?;
            control.push(Control::Block {
                block,
                next: index + 1,
            });
            let inner = Site::new(call.statement.unit.clone(), &path[..depth + 2]);
            let at = |candidate: BlockId| exe.blocks[candidate.0 as usize].site == inner;
            block = match &exe.stmts[stmt.0 as usize] {
                Stmt::If {
                    then_block,
                    else_block,
                    ..
                } => [*then_block, *else_block]
                    .into_iter()
                    .find(|candidate| at(*candidate))
                    .ok_or_else(nowhere)?,
                Stmt::For { site, body, .. } if at(*body) => {
                    let saved = loops.next().filter(|saved| saved.site == *site);
                    let (Some(saved), Some(iterated)) = (saved, iterated.next()) else {
                        return Err(malformed(format!("the loop at {site} is not saved")));
                    };
                    let position = saved
                        .position
                        .ok_or_else(|| malformed(format!("the `for` at {site} has no position")))?;
                    control.push(Control::For {
                        stmt,
                        cursor: self.cursor(iterated, position)?,
                        started: saved.started,
                    });
                    *body
                }
                Stmt::While { site, body, .. } if at(*body) => {
                    let saved = loops
                        .next()
                        .filter(|saved| saved.site == *site && saved.position.is_none())
                        .ok_or_else(|| malformed(format!("the loop at {site} is not saved")))?;
                    control.push(Control::While {
                        stmt,
                        started: saved.started,
                    });
                    *body
                }
                Stmt::Try {
                    site,
                    body,
                    catch,
                    finally: cleanup,
                } => {
                    let (phase, inner) = if at(*body) {
                        (TryPhase::Body, *body)
                    } else if let Some((_, catch)) = catch.filter(|(_, catch)| at(*catch)) {
                        (TryPhase::Catch, catch)
                    } else if let Some(cleanup) = cleanup.filter(|cleanup| at(*cleanup)) {
                        let saved = finally
                            .next()
                            .filter(|saved| saved.site == *site)
                            .ok_or_else(|| {
                                malformed(format!("the `finally` at {site} is not saved"))
                            })?;
                        let mut value = || {
                            departing.next().cloned().ok_or_else(|| {
                                malformed(format!("the `finally` at {site} leaves with no value"))
                            })
                        };
                        let departure = match saved.entered {
                            state::Entered::Normal => Completion::Normal,
                            state::Entered::Break => Completion::Break,
                            state::Entered::Continue => Completion::Continue,
                            state::Entered::Throw => Completion::Throw(value()?),
                            state::Entered::Return => Completion::Return(value()?),
                        };
                        (TryPhase::Finally(departure), cleanup)
                    } else {
                        return Err(nowhere());
                    };
                    control.push(Control::Try { stmt, phase });
                    inner
                }
                _ => return Err(nowhere()),
            };
            depth += 2;
        }
        if loops.next().is_some()
            || finally.next().is_some()
            || iterated.next().is_some()
            || departing.next().is_some()
        {
            return Err(malformed(format!(
                "{} is inside fewer loops or cleanup blocks than are saved",
                call.statement
            )));
        }

        let mut slots = vec![SlotState::Empty; code.slots.len()];
        for binding in &call.bindings {
            let slot = code
                .slots
                .iter()
                .enumerate()
                .position(|(slot, info)| {
                    info.name == binding.name
                        && info.declared == binding.declared
                        && matches!(slots[code.positions[slot] as usize], SlotState::Empty)
                })
                .ok_or_else(|| {
                    malformed(format!(
                        "`{}` is no variable declared at {}",
                        binding.name, binding.declared
                    ))
                })?;
            let shared = code.slots[slot].shared;
            slots[code.positions[slot] as usize] = match &binding.value {
                state::Bound::Value(value) if !shared => {
                    self.value(value)?;
                    SlotState::Value(value.clone())
                }
                state::Bound::Shared(cell) if shared => {
                    self.cell(*cell)?;
                    SlotState::Cell(*cell)
                }
                _ => {
                    return Err(malformed(format!(
                        "`{}` is saved as the wrong kind of variable",
                        binding.name
                    )));
                }
            };
        }

        // A library function's body is charged its formula when it ends,
        // over the arguments it was called with.
        let library = match (&code.site.unit, code.site.path.is_empty()) {
            (Unit::Library(function), true) => Some(*function),
            _ => None,
        };
        let library = match (library, &held.arguments) {
            (Some(function), Some(args)) => {
                self.values(args)?;
                let lib = lib_of(exe, function).ok_or_else(|| {
                    malformed("a call runs a function the document does not reach")
                })?;
                Some(LibraryCall {
                    lib,
                    args: args.clone(),
                })
            }
            (None, None) => None,
            _ => {
                return Err(malformed(
                    "arguments are held for a library function's body, and only for one",
                ));
            }
        };
        self.values(held.departing.iter())?;
        Ok(Frame {
            code: code_id,
            slots,
            control,
            awaiting,
            library,
            inline: false,
        })
    }

    fn cursor(&self, iterated: &Value, position: u64) -> Result<Cursor, ImportError> {
        self.value(iterated)?;
        let beyond = || malformed("a loop's position is beyond its collection");
        let index = usize::try_from(position).map_err(|_| beyond())?;
        match iterated {
            Value::List(list) => Ok(Cursor::List(*list, index)),
            Value::Tuple(items) if index <= items.len() => {
                Ok(Cursor::Tuple(Arc::clone(items), index))
            }
            Value::Map(table) | Value::Set(table) => {
                let length = self.heap.table(*table).map_or(0, Table::len);
                if index > length {
                    return Err(beyond());
                }
                // The entries were numbered from 0 when they were put back.
                Ok(Cursor::Table(*table, position.checked_sub(1)))
            }
            _ => Err(malformed(
                "a `for` iterates a list, a tuple, a map or a set",
            )),
        }
    }
}

fn lib_of(exe: &Executable, function: FunctionId) -> Option<LibId> {
    exe.libs
        .iter()
        .position(|lib| lib.id == function)
        .map(|index| LibId(index as u32))
}

fn table_of(entries: impl IntoIterator<Item = (Value, Value)>) -> Result<Table, ImportError> {
    let mut table = Table::default();
    for (key, value) in entries {
        let normal = Key::of(&key).ok_or_else(|| malformed("a key is not a legal key"))?;
        if table.contains(&normal) {
            return Err(malformed("a map or a set holds one key twice"));
        }
        table.insert(normal, key, value);
    }
    Ok(table)
}

fn object_of(exe: &Executable, object: Object) -> Result<Obj, ImportError> {
    Ok(match object {
        Object::List(items) => Obj::List(items),
        Object::Map(entries) => Obj::Map(table_of(entries)?),
        Object::Set(members) => Obj::Set(table_of(
            members.into_iter().map(|member| (member, Value::Null)),
        )?),
        Object::Record(fields) => {
            let mut names = BTreeSet::new();
            if !fields.iter().all(|(name, _)| names.insert(name.as_str())) {
                return Err(malformed("a record holds one field twice"));
            }
            Obj::Record(fields)
        }
        Object::Variable(value) => Obj::Variable(value),
        Object::Closure(closure) => {
            let code = exe
                .code_at
                .get(&closure.site.child(0))
                .copied()
                .ok_or_else(|| malformed(format!("no closure is written at {}", closure.site)))?;
            let wanted = &exe.codes[code.0 as usize];
            if wanted.captures.len() != closure.captures.len() {
                return Err(malformed(format!(
                    "the closure at {} shares {} variable(s)",
                    closure.site,
                    wanted.captures.len()
                )));
            }
            let mut captures = Vec::with_capacity(wanted.captures.len());
            for capture in &wanted.captures {
                let name = &wanted.slots[capture.inner as usize].name;
                let cell = closure
                    .captures
                    .iter()
                    .find(|(captured, _)| captured == name)
                    .ok_or_else(|| {
                        malformed(format!("the closure at {} shares `{name}`", closure.site))
                    })?;
                captures.push(cell.1);
            }
            Obj::Closure(ClosureObj { code, captures })
        }
    })
}

/// The request a saved wait stands for. An effect's result type is the
/// one its `perform` states.
fn request_of(
    exe: &Executable,
    frame: Option<&Frame>,
    wait: WaitId,
    request: &state::Request,
) -> Result<Request, ImportError> {
    let action = frame
        .and_then(|frame| frame.awaiting)
        .and_then(|stmt| action_of(exe, stmt))
        .map(|action| &action.kind);
    match (request, action) {
        (
            state::Request::Effect {
                identity,
                effect,
                args,
            },
            Some(ActionKind::Perform { result, .. }),
        ) => Ok(Request::Effect(EffectRequest {
            wait,
            identity: identity.clone(),
            effect: effect.clone(),
            args: args.clone(),
            result: result.clone(),
        })),
        (state::Request::Sleep { identity, duration }, Some(ActionKind::Sleep(_))) => {
            Ok(Request::Sleep(SleepRequest {
                wait,
                identity: identity.clone(),
                duration: duration
                    .duration()
                    .ok_or_else(|| malformed("a sleep's nanoseconds reach a second"))?,
            }))
        }
        _ => Err(malformed(format!(
            "wait {} is not at a `perform` or a `sleep` of its kind",
            wait.0
        ))),
    }
}

pub(in crate::machine) fn import(
    program: Program,
    bounds: Bounds,
    parked: ParkedRun,
    layout: Layout,
) -> Result<KernelMachine, ImportError> {
    let ParkedRun {
        run,
        session,
        tasks: parked_tasks,
        objects,
    } = parked;
    if run.kernel != KERNEL_VERSION {
        return Err(ImportError::KernelVersion {
            parked: run.kernel,
            supported: KERNEL_VERSION,
        });
    }
    let manifest = &program.document.manifest.functions;
    if let Some(function) = manifest
        .keys()
        .find(|id| program.registry.get(id).is_none())
    {
        return Err(ImportError::MissingFunction {
            function: *function,
        });
    }
    validate_document(&program.document, program.registry.as_ref())?;
    let document = program
        .document
        .identity()
        .map_err(|error| malformed(error.to_string()))?;
    if run.document != document {
        return Err(ImportError::DocumentMismatch {
            parked: run.document,
            given: document,
        });
    }
    if !run.functions.iter().eq(manifest.keys()) {
        return Err(malformed(
            "the functions pinned are not the ones the document's manifest lists",
        ));
    }

    // A call parked inside a library function's kernel body goes on there,
    // whichever implementation this registry runs new calls with.
    let in_flight: BTreeSet<FunctionId> = parked_tasks
        .iter()
        .flat_map(|task| &task.calls)
        .filter_map(|call| match &call.call.statement.unit {
            Unit::Library(function) => Some(*function),
            _ => None,
        })
        .collect();
    let exe =
        compile(&program.document, &program.registry, layout, &in_flight).map_err(|missing| {
            ImportError::MissingFunction {
                function: missing.0,
            }
        })?;
    let exe = Arc::new(exe);

    let mut heap = Heap::new(bounds.memory);
    for (id, object) in objects {
        if !heap.restore(id, object_of(&exe, object)?) {
            return Err(malformed("an object is held twice"));
        }
    }
    if !heap.set_allocated(run.objects_allocated) {
        return Err(malformed(
            "an object's identity is one the run has not allocated",
        ));
    }
    let restore = Restore {
        exe: &exe,
        heap: &heap,
        tasks: parked_tasks.len(),
    };
    for (_, object) in heap.iter() {
        restore.object(object)?;
    }
    restore.values(session.values())?;

    let task_of = |task: TaskId| {
        parked_tasks
            .get(task.0 as usize)
            .ok_or_else(|| malformed("a task names a task the state does not hold"))
    };
    let mut tasks = Vec::with_capacity(parked_tasks.len());
    let mut waits = BTreeMap::new();
    let mut requests = Vec::new();
    let mut joins = BTreeMap::new();
    let mut ready_tasks = BTreeSet::new();
    for (index, parked_task) in parked_tasks.iter().enumerate() {
        let id = TaskId(index as u64);
        let handle = &parked_task.handle;
        // Every call but the innermost stands in the statement that made
        // the next. The innermost stands in its statement's action too,
        // unless the task is merely ready or a raise lands on it: a raise
        // leaves the statement either way.
        let issues = matches!(
            handle.state,
            state::TaskState::Ready | state::TaskState::Resuming(state::Incoming::Raise(_))
        );
        let innermost = parked_task.calls.len().saturating_sub(1);
        let frames = parked_task
            .calls
            .iter()
            .enumerate()
            .map(|(depth, call)| restore.frame(call, depth != innermost || !issues))
            .collect::<Result<Vec<_>, _>>()?;
        let ended = matches!(handle.state, state::TaskState::Ended(_));
        if ended != frames.is_empty() {
            return Err(malformed("a task has calls exactly until it ends"));
        }
        let (state, incoming) = match &handle.state {
            state::TaskState::Ready => (TaskState::Ready, None),
            state::TaskState::Resuming(incoming) => (
                TaskState::Ready,
                Some(match incoming {
                    state::Incoming::Value(value) => {
                        restore.value(value)?;
                        Incoming::Value(value.clone())
                    }
                    state::Incoming::Raise(value) => {
                        restore.value(value)?;
                        Incoming::Raise(value.clone())
                    }
                    state::Incoming::Joined(joined) => {
                        if !matches!(task_of(*joined)?.handle.state, state::TaskState::Ended(_)) {
                            return Err(malformed("a join resumes before its task ended"));
                        }
                        Incoming::Join(*joined)
                    }
                }),
            ),
            state::TaskState::Performing(perform) => {
                if perform.wait.0 >= run.waits_issued {
                    return Err(malformed("a wait's number is one the run has not issued"));
                }
                let request = request_of(&exe, frames.last(), perform.wait, &perform.request)?;
                let sleep = matches!(request, Request::Sleep(_));
                let pending = |handed_out| PendingWait {
                    task: id,
                    request: request.clone(),
                    handed_out,
                };
                let held = match &perform.state {
                    state::PerformState::Requested => {
                        requests.push(perform.wait);
                        waits.insert(perform.wait, pending(false))
                    }
                    state::PerformState::Admitted => waits.insert(perform.wait, pending(true)),
                    state::PerformState::Committed(_) => None,
                };
                if held.is_some() {
                    return Err(malformed("two tasks are in one wait"));
                }
                match &perform.state {
                    state::PerformState::Committed(outcome) => {
                        let outcome = match outcome {
                            state::Outcome::Completed(datum) => Outcome::Completed(datum.clone()),
                            state::Outcome::Failed(error) => Outcome::Failed(error.clone()),
                            state::Outcome::Elapsed => Outcome::Elapsed,
                        };
                        if sleep != matches!(outcome, Outcome::Elapsed) {
                            return Err(malformed("a wait holds an outcome it cannot have"));
                        }
                        let answered = Answered {
                            wait: perform.wait,
                            request,
                            outcome,
                        };
                        (
                            TaskState::Ready,
                            Some(Incoming::Outcome(Box::new(answered))),
                        )
                    }
                    _ => (TaskState::Waiting(Wait::Request(perform.wait)), None),
                }
            }
            state::TaskState::Joining(joined) => {
                let waiting = task_of(*joined)?
                    .handle
                    .joiners
                    .iter()
                    .filter(|joiner| **joiner == id)
                    .count();
                if waiting != 1 {
                    return Err(malformed(
                        "a joining task is not among its handle's joiners",
                    ));
                }
                (TaskState::Waiting(Wait::Join(*joined)), None)
            }
            state::TaskState::JoiningMany(join) => {
                // Each member still running wakes the join once, however
                // often the list names it; one that ended wakes nobody.
                for member in &join.members {
                    let member = &task_of(*member)?.handle;
                    let waiting = member
                        .joiners
                        .iter()
                        .filter(|joiner| **joiner == id)
                        .count();
                    let running = !matches!(member.state, state::TaskState::Ended(_));
                    if waiting != usize::from(running) {
                        return Err(malformed(
                            "a list join is not among its running members' joiners",
                        ));
                    }
                }
                // A task waits in one join at a time, so its own number
                // names the join.
                joins.insert(
                    id.0,
                    ListJoin {
                        joiner: id,
                        mode: join.mode,
                        members: join.members.clone(),
                    },
                );
                (TaskState::Waiting(Wait::JoinMany(id.0)), None)
            }
            state::TaskState::Ended(state::Ended::Returned(value)) => {
                restore.value(value)?;
                (TaskState::Ended(Ok(value.clone())), None)
            }
            state::TaskState::Ended(state::Ended::Raised(value)) => {
                restore.value(value)?;
                (TaskState::Ended(Err(value.clone())), None)
            }
        };
        if matches!(state, TaskState::Ready) {
            ready_tasks.insert(id);
        }
        let raised = matches!(
            handle.state,
            state::TaskState::Ended(state::Ended::Raised(_))
        );
        if (handle.failed && !ended) || (raised && !handle.failed) {
            return Err(malformed("a task's failure does not fit how it ended"));
        }
        let mut joiners = Vec::with_capacity(handle.joiners.len());
        for joiner in &handle.joiners {
            joiners.push(match &task_of(*joiner)?.handle.state {
                state::TaskState::Joining(joined) if *joined == id => Joiner::Single(*joiner),
                state::TaskState::JoiningMany(join) if join.members.contains(&id) => {
                    Joiner::List(joiner.0)
                }
                _ => {
                    return Err(malformed(
                        "a handle lists a joiner that does not wait on it",
                    ));
                }
            });
        }
        let mut occurrences = BTreeMap::new();
        for occurrence in &handle.occurrences {
            // An action is a child of its statement (`K-SITE-002`).
            let stmt = occurrence
                .site
                .path
                .split_last()
                .and_then(|(_, statement)| statement.split_last())
                .and_then(|(index, block)| {
                    let block = Site::new(occurrence.site.unit.clone(), block);
                    let block = exe.block_at.get(&block)?;
                    exe.blocks[block.0 as usize]
                        .stmts
                        .get(*index as usize)
                        .copied()
                })
                .filter(|stmt| {
                    action_of(&exe, *stmt).is_some_and(|action| action.site == occurrence.site)
                })
                .ok_or_else(|| malformed(format!("{} is no action", occurrence.site)))?;
            if occurrences.insert(stmt, occurrence.count).is_some() {
                return Err(malformed(format!("{} is counted twice", occurrence.site)));
            }
        }
        tasks.push(Task {
            identity: handle.identity.clone(),
            frames,
            state,
            incoming,
            joiners,
            observed: handle.observed,
            passed: handle.passed,
            failed: handle.failed,
            occurrences,
        });
    }

    let ready: VecDeque<TaskId> = run.ready.iter().copied().collect();
    if ready.len() != ready_tasks.len() || !ready.iter().all(|task| ready_tasks.contains(task)) {
        return Err(malformed("the ready queue is not the tasks that are ready"));
    }
    if !run
        .unreported
        .iter()
        .all(|wait| run.withdrawn.contains(wait))
        || run.withdrawn.iter().any(|wait| wait.0 >= run.waits_issued)
        || run.withdrawn.iter().any(|wait| waits.contains_key(wait))
    {
        return Err(malformed("the withdrawn waits do not fit the waits issued"));
    }
    // `main` is a session cell's own frame; an entry's is a declared
    // function's.
    let Some(outermost) = parked_tasks.first().and_then(|main| main.calls.first()) else {
        return Err(malformed("`main` has ended: the run is over"));
    };
    let session_cell = outermost.call.statement.unit == Unit::Main;
    if !session_cell && !session.is_empty() {
        return Err(malformed("a run of an entry holds session bindings"));
    }
    let live_tasks = tasks
        .iter()
        .filter(|task| !matches!(task.state, TaskState::Ended(_)))
        .count();
    let next_join = joins.keys().next_back().map_or(0, |last| last + 1);

    let mut machine = KernelMachine {
        program,
        document: Some(document),
        exe,
        bounds,
        heap,
        tasks,
        ready,
        current: None,
        joins,
        next_join,
        waits,
        next_wait: run.waits_issued,
        requests,
        withdrawn: run.unreported,
        withdrawn_ever: run.withdrawn,
        session,
        session_cell,
        pins: Vec::new(),
        fresh: Vec::new(),
        charging: true,
        charged: run.charged,
        live_tasks: u32::try_from(live_tasks).unwrap_or(u32::MAX),
        inline_depth: 0,
        inline_result: None,
        end: None,
        ended: false,
    };
    // Accounts the memory of what was put back, as the machine that parked
    // it did when it wrote the state.
    machine.collect();
    Ok(machine)
}
