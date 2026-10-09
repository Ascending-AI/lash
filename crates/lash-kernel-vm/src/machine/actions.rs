//! Actions: calls, waits and tasks (`K-STMT-001`, `K-TASK`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use lash_kernel_doc::{
    EffectIdentity, ErrorValue, JoinMode, LoopIteration, ObjectId, Site, SpawnIdentity, TaskId,
    TaskIdentity, Value,
};
use num_traits::{Signed, ToPrimitive};

use super::exec::Call;
use super::{
    Control, Eval, Halt, Incoming, Interrupt, Joiner, KernelMachine, ListJoin, PendingWait, Task,
    TaskState, Wait, bound, error, fault, raise,
};
use crate::compile::{
    Action, ActionKind, Atom, Callee, CodeId, Executable, LibId, LibRun, Stmt, StmtId,
};
use crate::heap::Obj;
use crate::interface::{Bound, EffectRequest, Request, SleepRequest, WaitId};

/// What a call or a `spawn` runs, once its callee is read.
enum Target {
    Code(CodeId, Vec<ObjectId>),
    Library(LibId),
}

fn cancelled() -> Value {
    error("cancelled", "the task was cancelled")
}

impl KernelMachine {
    /// Starts an action. `Some` is the value of one that completed at
    /// once; `None` is one the task now waits in, or a call whose frame
    /// now runs.
    pub(super) fn act(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        action: &Action,
    ) -> Eval<Option<Value>> {
        match &action.kind {
            ActionKind::Call { callee, args } => {
                let target = self.callee(task, exe, callee)?;
                let args = self.atoms(task, exe, args)?;
                self.frame(task)?.awaiting = Some(stmt);
                match target {
                    Target::Code(code, captures) => {
                        self.push_frame(task, exe, Call::new(code, args).sharing(&captures))?;
                        Ok(None)
                    }
                    Target::Library(lib) => match self.call_library(task, exe, lib, args)? {
                        Ok(value) => Ok(Some(value)),
                        Err(args) => {
                            let LibRun::Body(code) = &exe.libs[lib.0 as usize].run else {
                                return Err(fault("a function with no body was run as one").into());
                            };
                            self.push_frame(task, exe, Call::new(*code, args).of_library(lib))?;
                            Ok(None)
                        }
                    },
                }
            }
            ActionKind::Spawn { callee, args } => {
                let target = self.callee(task, exe, callee)?;
                let args = self.atoms(task, exe, args)?;
                self.spawn(task, exe, stmt, &action.site, target, args)
            }
            ActionKind::Perform {
                effect,
                args,
                result,
            } => {
                let values = self.atoms(task, exe, args)?;
                let mut copied = Vec::with_capacity(values.len());
                for value in &values {
                    copied.push(self.copy_out(value)?);
                }
                let (wait, identity) = self.request(task, exe, stmt, &action.site)?;
                self.issue(
                    task,
                    wait,
                    Request::Effect(EffectRequest {
                        wait,
                        identity,
                        effect: effect.clone(),
                        args: copied,
                        result: result.clone(),
                    }),
                );
                Ok(None)
            }
            ActionKind::Sleep(duration) => {
                let duration = match self.atom(task, exe, duration)? {
                    Value::Int(integer) if !integer.as_bigint().is_negative() => {
                        Duration::from_millis(integer.as_bigint().to_u64().unwrap_or(u64::MAX))
                    }
                    Value::Float(float) if float.get().is_finite() && float.get() >= 0.0 => {
                        Duration::try_from_secs_f64(float.get() / 1000.0).unwrap_or(Duration::MAX)
                    }
                    Value::Int(_) | Value::Float(_) => {
                        return raise(
                            "number_range",
                            "`sleep` takes a duration that is finite and not negative",
                        );
                    }
                    _ => return raise("type_error", "`sleep` takes a number of milliseconds"),
                };
                let (wait, identity) = self.request(task, exe, stmt, &action.site)?;
                self.issue(
                    task,
                    wait,
                    Request::Sleep(SleepRequest {
                        wait,
                        identity,
                        duration,
                    }),
                );
                Ok(None)
            }
            ActionKind::Join(handle) => {
                let Value::Task(joined) = self.atom(task, exe, handle)? else {
                    return raise("type_error", "`join` takes a task handle");
                };
                if joined == task {
                    return raise("join_self", "a task cannot join its own handle");
                }
                if matches!(self.task(joined)?.state, TaskState::Ended(_)) {
                    return self.join_result(joined).map(Some);
                }
                self.task(joined)?.joiners.push(Joiner::Single(task));
                self.suspend(task, stmt, Wait::Join(joined))?;
                Ok(None)
            }
            ActionKind::JoinMany(mode, handles) => {
                let members: Vec<Value> = match self.atom(task, exe, handles)? {
                    Value::Tuple(items) => items.to_vec(),
                    Value::List(list) => self.heap.list(list).cloned().unwrap_or_default(),
                    _ => {
                        return raise(
                            "type_error",
                            "`join` takes a list or a tuple of task handles",
                        );
                    }
                };
                let mut tasks = Vec::with_capacity(members.len());
                for member in &members {
                    match member {
                        Value::Task(member) => tasks.push(*member),
                        _ => {
                            return raise(
                                "type_error",
                                "`join` takes a list or a tuple of task handles",
                            );
                        }
                    }
                }
                let limit = u64::from(self.bounds.join_members);
                if tasks.len() as u64 > limit {
                    return Err(bound(Bound::JoinMembers, limit).into());
                }
                if tasks.contains(&task) {
                    return raise("join_self", "a task cannot join its own handle");
                }
                if tasks.is_empty() && matches!(mode, JoinMode::Race | JoinMode::Any) {
                    return raise("empty_join", "this `join` needs at least one task");
                }
                match self.decide(*mode, &tasks, None)? {
                    Some(decision) => {
                        self.pass(&tasks)?;
                        decision.map(Some).map_err(Interrupt::Raise)
                    }
                    None => {
                        let order = self.next_join;
                        self.next_join += 1;
                        // Repeated handles keep their places in the result,
                        // but register only one wake on each live member.
                        for member in tasks.iter().copied().collect::<BTreeSet<_>>() {
                            if !matches!(self.task(member)?.state, TaskState::Ended(_)) {
                                self.task(member)?.joiners.push(Joiner::List(order));
                            }
                        }
                        self.joins.insert(
                            order,
                            ListJoin {
                                joiner: task,
                                mode: *mode,
                                members: tasks,
                            },
                        );
                        self.suspend(task, stmt, Wait::JoinMany(order))?;
                        Ok(None)
                    }
                }
            }
            ActionKind::Yield => {
                let yielding = self.task(task)?;
                yielding.incoming = Some(Incoming::Value(Value::Null));
                self.frame(task)?.awaiting = Some(stmt);
                self.ready.push_back(task);
                self.current = None;
                Ok(None)
            }
            ActionKind::Cancel(handle) => {
                let Value::Task(target) = self.atom(task, exe, handle)? else {
                    return raise("type_error", "`cancel` takes a task handle");
                };
                if target == task {
                    return Err(Interrupt::Raise(cancelled()));
                }
                self.cancel(target)?;
                Ok(Some(Value::Null))
            }
        }
    }

    fn atom(&mut self, task: TaskId, exe: &Executable, atom: &Atom) -> Eval<Value> {
        self.charge(1)?;
        match atom {
            Atom::Var(var) => self.read_var(task, exe, var),
            Atom::Literal(value) => Ok(value.clone()),
        }
    }

    /// Reads an action's atoms, left to right (`K-EVAL-004`).
    fn atoms(&mut self, task: TaskId, exe: &Executable, atoms: &[Atom]) -> Eval<Vec<Value>> {
        atoms
            .iter()
            .map(|atom| self.atom(task, exe, atom))
            .collect()
    }

    /// Reads a callee. One held in a variable is a closure or a function
    /// reference (`K-FN-005`).
    fn callee(&mut self, task: TaskId, exe: &Executable, callee: &Callee) -> Eval<Target> {
        match callee {
            Callee::Declared(code) => Ok(Target::Code(*code, Vec::new())),
            Callee::Library(lib) => Ok(Target::Library(*lib)),
            Callee::Value(var) => match self.read_var(task, exe, var)? {
                Value::Closure(closure) => match self.heap.get(closure) {
                    Some(Obj::Closure(closure)) => {
                        Ok(Target::Code(closure.code, closure.captures.clone()))
                    }
                    _ => Err(fault("a closure value names no closure").into()),
                },
                Value::Function(name) => match exe.declared.get(&name) {
                    Some(code) => Ok(Target::Code(*code, Vec::new())),
                    None => raise("type_error", format!("no function `{name}` is declared")),
                },
                _ => raise(
                    "type_error",
                    "only a closure or a function reference can be called",
                ),
            },
        }
    }

    /// Parks the running task in a wait.
    fn suspend(&mut self, task: TaskId, stmt: StmtId, wait: Wait) -> Result<(), Halt> {
        self.frame(task)?.awaiting = Some(stmt);
        self.task(task)?.state = TaskState::Waiting(wait);
        self.current = None;
        Ok(())
    }

    fn occurrence(&mut self, task: TaskId, stmt: StmtId) -> Result<u64, Halt> {
        let count = self.task(task)?.occurrences.entry(stmt).or_insert(0);
        let occurrence = *count;
        *count += 1;
        Ok(occurrence)
    }

    /// Takes the next wait for a `perform` or a `sleep` of the running
    /// task and gives its identity (`K-EFF-008`).
    fn request(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        site: &Site,
    ) -> Result<(WaitId, EffectIdentity), Halt> {
        let limit = u64::from(self.bounds.requests_per_park);
        if self.requests.len() as u64 >= limit {
            return Err(bound(Bound::RequestsPerPark, limit));
        }
        let occurrence = self.occurrence(task, stmt)?;
        let running = self.task(task)?;
        let mut loops = Vec::new();
        for frame in &running.frames {
            for control in &frame.control {
                let (stmt, started) = match control {
                    Control::For { stmt, started, .. } | Control::While { stmt, started } => {
                        (stmt, started)
                    }
                    _ => continue,
                };
                if let Stmt::For { site, .. } | Stmt::While { site, .. } =
                    &exe.stmts[stmt.0 as usize]
                {
                    loops.push(LoopIteration {
                        site: site.clone(),
                        iteration: started.saturating_sub(1),
                    });
                }
            }
        }
        let identity = EffectIdentity {
            task: running.identity.clone(),
            site: site.clone(),
            occurrence,
            loops,
        };
        let wait = WaitId(self.next_wait);
        self.next_wait += 1;
        self.suspend(task, stmt, Wait::Request(wait))?;
        Ok((wait, identity))
    }

    /// Records what a wait asks for, for the next park to hand out.
    fn issue(&mut self, task: TaskId, wait: WaitId, request: Request) {
        self.waits.insert(
            wait,
            PendingWait {
                task,
                request,
                handed_out: false,
            },
        );
        self.requests.push(wait);
    }

    /// `spawn`: the new task runs at once, and the spawning task goes on
    /// when it first waits or ends, before any other ready task
    /// (`K-TASK-002`).
    fn spawn(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        site: &Site,
        target: Target,
        args: Vec<Value>,
    ) -> Eval<Option<Value>> {
        if let Target::Code(code, _) = &target
            && args.len() > exe.codes[code.0 as usize].params.len()
        {
            return raise(
                "arity",
                "the function takes fewer arguments than were given",
            );
        }
        let limit = u64::from(self.bounds.live_tasks);
        if u64::from(self.live_tasks) >= limit {
            return Err(bound(Bound::LiveTasks, limit).into());
        }
        let occurrence = self.occurrence(task, stmt)?;
        let parent = self.task(task)?.identity.clone();
        let child = TaskId(self.tasks.len() as u64);
        self.tasks.push(Task {
            identity: TaskIdentity::Spawned(SpawnIdentity {
                parent: Arc::new(parent),
                site: site.clone(),
                occurrence,
            }),
            frames: Vec::new(),
            state: TaskState::Ready,
            incoming: None,
            joiners: Vec::new(),
            observed: false,
            passed: false,
            failed: false,
            occurrences: Default::default(),
        });
        self.live_tasks += 1;
        let handle = Value::Task(child);
        let started = match target {
            Target::Code(code, captures) => {
                self.push_frame(child, exe, Call::new(code, args).sharing(&captures))
            }
            Target::Library(lib) => match self.call_library(child, exe, lib, args) {
                // A function with no frame has run to its end already.
                Ok(Ok(value)) => {
                    self.end_task(child, Ok(value))?;
                    return Ok(Some(handle));
                }
                Ok(Err(args)) => match &exe.libs[lib.0 as usize].run {
                    LibRun::Body(code) => {
                        self.push_frame(child, exe, Call::new(*code, args).of_library(lib))
                    }
                    _ => Err(fault("a function with no body was run as one").into()),
                },
                Err(interrupt) => Err(interrupt),
            },
        };
        match started {
            Ok(()) => {}
            // What the new task raises before it has a frame is its error.
            Err(Interrupt::Raise(value)) => {
                self.end_task(child, Err(value))?;
                return Ok(Some(handle));
            }
            Err(halt) => return Err(halt),
        }
        let spawner = self.task(task)?;
        spawner.incoming = Some(Incoming::Value(handle));
        self.frame(task)?.awaiting = Some(stmt);
        self.ready.push_front(task);
        self.current = Some(child);
        Ok(None)
    }

    /// Marks tasks as members of a list `join` that has returned or
    /// raised: their errors are observed, and the run's end cancels them
    /// (`K-TASK-016`, `K-TASK-018`).
    pub(super) fn pass(&mut self, members: &[TaskId]) -> Result<(), Halt> {
        for member in members {
            self.task(*member)?.passed = true;
        }
        Ok(())
    }

    /// Removes a decided or cancelled list join from every member's wake
    /// queue, so no later ending can wake its joiner again.
    pub(super) fn remove_list_join(&mut self, order: u64) -> Result<Option<ListJoin>, Halt> {
        let join = self.joins.remove(&order);
        if let Some(join) = &join {
            for member in &join.members {
                self.task(*member)?
                    .joiners
                    .retain(|joiner| *joiner != Joiner::List(order));
            }
        }
        Ok(join)
    }

    /// Whether a list `join` is decided, and how (`K-TASK-011` to
    /// `K-TASK-014`). `ended` is the member that has just ended; `None`
    /// is a `join` that is starting, which reads its members in list
    /// order.
    pub(super) fn decide(
        &mut self,
        mode: JoinMode,
        members: &[TaskId],
        ended: Option<TaskId>,
    ) -> Result<Option<Result<Value, Value>>, Halt> {
        let mut results: Vec<Option<Result<Value, Value>>> = Vec::with_capacity(members.len());
        for member in members {
            results.push(match &self.task(*member)?.state {
                TaskState::Ended(result) => Some(result.clone()),
                _ => None,
            });
        }
        // The member that decides a waiting join is the one that just
        // ended; a starting join takes the first in list order.
        let first = |wanted: fn(&Result<Value, Value>) -> bool| -> Option<Result<Value, Value>> {
            match ended {
                Some(ended) => members
                    .iter()
                    .position(|member| *member == ended)
                    .and_then(|index| results[index].clone())
                    .filter(wanted),
                None => results
                    .iter()
                    .flatten()
                    .find(|result| wanted(result))
                    .cloned(),
            }
        };
        let all: Option<Vec<Result<Value, Value>>> = results.iter().cloned().collect();
        Ok(match mode {
            JoinMode::All => match (first(Result::is_err), all) {
                (Some(failure), _) => Some(failure),
                (None, Some(results)) => {
                    let values = results.into_iter().filter_map(Result::ok).collect();
                    Some(Ok(Value::List(self.alloc(Obj::List(values))?)))
                }
                (None, None) => None,
            },
            JoinMode::AllSettled => match all {
                Some(results) => {
                    let mut records = Vec::with_capacity(results.len());
                    for result in results {
                        let fields = match result {
                            Ok(value) => vec![
                                ("status".to_string(), Value::text("ok")),
                                ("value".to_string(), value),
                            ],
                            Err(value) => vec![
                                ("status".to_string(), Value::text("error")),
                                ("error".to_string(), value),
                            ],
                        };
                        records.push(Value::Record(self.alloc(Obj::Record(fields))?));
                    }
                    Some(Ok(Value::List(self.alloc(Obj::List(records))?)))
                }
                None => None,
            },
            JoinMode::Race => first(|_| true),
            JoinMode::Any => match (first(Result::is_ok), all) {
                (Some(success), _) => Some(success),
                (None, Some(results)) if !results.is_empty() => {
                    let errors = results.into_iter().filter_map(Result::err).collect();
                    let errors = Value::List(self.alloc(Obj::List(errors))?);
                    Some(Err(Value::Error(Arc::new(ErrorValue {
                        kind: "all_failed".to_string(),
                        message: "every task of the `join any` failed".to_string(),
                        data: errors,
                    }))))
                }
                _ => None,
            },
        })
    }

    /// `cancel`: raises `cancelled` in a task at its wait (`K-TASK-017`).
    fn cancel(&mut self, target: TaskId) -> Result<(), Halt> {
        let wait = match self.task(target)?.state {
            TaskState::Ended(_) => return Ok(()),
            // A ready task resumes from a wait that has completed, or from
            // a `yield` or a `spawn`: the raise replaces that result.
            TaskState::Ready => {
                self.task(target)?.incoming = Some(Incoming::Raise(cancelled()));
                return Ok(());
            }
            TaskState::Waiting(wait) => wait,
        };
        match wait {
            Wait::Request(wait) => {
                if let Some(pending) = self.waits.remove(&wait) {
                    if pending.handed_out {
                        self.withdrawn.push(wait);
                        self.withdrawn_ever.insert(wait);
                    } else {
                        self.requests.retain(|requested| *requested != wait);
                    }
                }
            }
            Wait::Join(joined) => self
                .task(joined)?
                .joiners
                .retain(|joiner| *joiner != Joiner::Single(target)),
            Wait::JoinMany(order) => {
                // The join raises `cancelled`, so its members have been
                // through a join that raised.
                if let Some(join) = self.remove_list_join(order)? {
                    self.pass(&join.members)?;
                }
            }
        }
        self.wake(target, Incoming::Raise(cancelled()))
    }
}
