//! Actions: calls, waits and tasks (`K-STMT-001`, `K-TASK`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, EntityId, JoinMode, LoopIteration, Site, SpawnIdentity,
    TaskId, TaskIdentity, Type, Value,
};
use num_traits::ToPrimitive;

use super::exec::Call;
use super::ops::{Entry, Flow, Reg, Status};
use super::{
    Control, Eval, Halt, Incoming, Interrupt, Joiner, KernelMachine, ListJoin, Task, TaskState,
    Wait, bound, error, fault, raise,
};
use crate::compile::{Action, ActionKind, Atom, Callee, CodeId, Executable, Form, LibId, StmtId};
use crate::interface::{Bound, EffectMode, EffectRequest, Host, Request, SleepRequest, WaitId};
use lash_kernel_doc::NativeHeap;
use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::{ErrorRef, Payload};
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::{Access, Cell, Tree};
use lash_kernel_state::paged::{ObjectInit, SemanticHeap, WaitKindRecord, WaitPhase, WaitRecord};

/// What a call or a `spawn` runs, once its callee is read.
enum Target {
    Code(CodeId, Vec<EntityId>),
    Library(LibId),
}

fn cancelled() -> Value {
    error("cancelled", "the task was cancelled")
}

impl KernelMachine {
    /// Action dispatch: starts the action of statement `stmt`, its
    /// arguments read into the registers from `out`. `Done` is one that
    /// completed at once, its value in `out`; `Step` is one the task now
    /// waits in, or a call whose frame now runs.
    pub(super) fn act(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: StmtId,
        action: &Action,
        out: Reg,
    ) -> Status {
        match &action.kind {
            ActionKind::Call { callee, args } => {
                let target = self.callee(task, exe, callee)?;
                self.put_atoms(task, exe, args, out)?;
                self.call(task, exe, stmt, target, out, args.len())
            }
            ActionKind::Spawn { callee, args } => {
                let target = self.callee(task, exe, callee)?;
                self.put_atoms(task, exe, args, out)?;
                self.spawn(task, exe, stmt, &action.site, target, out, args.len())
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
                if self.effect_mode(effect)? == EffectMode::Read {
                    // A read completes here, inside its statement: no wait,
                    // and the task keeps its turn (`K-EFF-012`).
                    let value =
                        self.perform_read(host, exe, effect, &copied, &action.site, result)?;
                    // Held as a delivered outcome is, until the statement
                    // binds it.
                    self.put(out, value);
                    self.pin(out)?;
                    return Ok(Flow::Done);
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
                )?;
                Ok(Flow::Step)
            }
            ActionKind::Sleep(duration) => {
                let duration = match self.atom(task, exe, duration)? {
                    Value::Int(integer)
                        if !match &integer {
                            lash_kernel_doc::heap::IntRef::Inline(i) => i.is_negative(),
                            lash_kernel_doc::heap::IntRef::Paged { summary, .. } => {
                                summary.negative
                            }
                        } =>
                    {
                        Duration::from_millis(match integer {
                            lash_kernel_doc::heap::IntRef::Inline(integer) => {
                                integer.to_u64().unwrap_or(u64::MAX)
                            }
                            _ => u64::MAX,
                        })
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
                )?;
                Ok(Flow::Step)
            }
            ActionKind::Join(handle) => {
                let Value::Task(joined) = self.atom(task, exe, handle)? else {
                    return raise("type_error", "`join` takes a task handle");
                };
                if joined == task {
                    return raise("join_self", "a task cannot join its own handle");
                }
                if matches!(self.task(joined)?.state, TaskState::Ended(_)) {
                    let value = self.join_result(joined)?;
                    self.put(out, value);
                    return Ok(Flow::Done);
                }
                self.add_joiner(joined, Joiner::Single(task))?;
                self.suspend(task, stmt, Wait::Join(joined))?;
                Ok(Flow::Step)
            }
            ActionKind::JoinMany(mode, handles) => {
                let handles = self.atom(task, exe, handles)?;
                let len = match &handles {
                    Value::Tuple(tuple) => tuple.summary.len,
                    Value::List(id) => self.heap.len(*id)?,
                    _ => {
                        return raise(
                            "type_error",
                            "`join` takes a list or a tuple of task handles",
                        );
                    }
                };
                let limit = u64::from(self.bounds.join_members);
                if len > limit {
                    return Err(bound(Bound::JoinMembers, limit).into());
                }
                let mut tasks = Tree::new(true);
                for at in 0..len {
                    let member = match &handles {
                        Value::Tuple(tuple) => self.heap.tuple_get(tuple, at)?,
                        Value::List(id) => self.heap.list_get(*id, at)?,
                        _ => unreachable!(),
                    };
                    let Some(Value::Task(member)) = member else {
                        return raise(
                            "type_error",
                            "`join` takes a list or a tuple of task handles",
                        );
                    };
                    if member == task {
                        return raise("join_self", "a task cannot join its own handle");
                    }
                    tasks.write(ordinal_key(at), member, &self.heap.access)?;
                }
                if len == 0 && matches!(mode, JoinMode::Race | JoinMode::Any) {
                    return raise("empty_join", "this `join` needs at least one task");
                }
                match self.decide(*mode, &tasks, None)? {
                    Some(decision) => {
                        self.pass(&tasks)?;
                        self.put(out, decision.map_err(Interrupt::Raise)?);
                        Ok(Flow::Done)
                    }
                    None => {
                        let order = self.next_join;
                        self.next_join += 1;
                        // Repeated handles keep their places in the result,
                        // but register only one wake on each live member.
                        for member in task_members(&tasks, &self.heap.access)?
                            .into_iter()
                            .collect::<BTreeSet<_>>()
                        {
                            if !matches!(self.task(member)?.state, TaskState::Ended(_)) {
                                self.add_joiner(member, Joiner::List(order))?;
                            }
                        }
                        self.joins.write(
                            ordinal_key(order),
                            ListJoin {
                                joiner: task,
                                mode: *mode,
                                members: tasks,
                            },
                            &self.heap.access,
                        )?;
                        self.suspend(task, stmt, Wait::JoinMany(order))?;
                        Ok(Flow::Step)
                    }
                }
            }
            ActionKind::Yield => {
                let yielding = self.task(task)?;
                yielding.wake(Incoming::Value(Value::Null));
                self.frame(task)?.awaiting = Some(stmt);
                self.ready.push_back(task)?;
                self.current = None;
                Ok(Flow::Step)
            }
            ActionKind::Cancel(handle) => {
                let Value::Task(target) = self.atom(task, exe, handle)? else {
                    return raise("type_error", "`cancel` takes a task handle");
                };
                if target == task {
                    return Err(Interrupt::Raise(cancelled()));
                }
                self.cancel(target)?;
                self.put(out, Value::Null);
                Ok(Flow::Done)
            }
        }
    }

    fn atom(&mut self, task: TaskId, exe: &Executable, atom: &Atom) -> Eval<Value> {
        self.charge(1)?;
        self.observe_expression();
        match atom {
            Atom::Var(var) => self.read_var(task, exe, var),
            Atom::Literal(value) => self.literal(value),
        }
    }

    /// A call action, its `count` arguments the registers from `args`, its
    /// value in `args` when it completes at once. The statement waits in
    /// the call only when the call runs in a frame of its own: one that
    /// completed at once has left its statement behind (`K-MACH-007`).
    fn call(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        target: Target,
        args: Reg,
        count: usize,
    ) -> Status {
        match target {
            Target::Code(code, captures) => {
                let call = Call::new(code, args, count)
                    .sharing(&captures)
                    .awaited_by(Some(stmt));
                self.push_frame(task, exe, call)?;
                Ok(Flow::Step)
            }
            Target::Library(lib) => {
                self.call_lib(task, exe, lib, args, count, args, Entry::Frame(Some(stmt)))
            }
        }
    }

    /// Reads an action's atoms into the registers from `at`, left to right
    /// (`K-EVAL-004`).
    fn put_atoms(&mut self, task: TaskId, exe: &Executable, atoms: &[Atom], at: Reg) -> Eval<()> {
        for (index, atom) in atoms.iter().enumerate() {
            let value = self.atom(task, exe, atom)?;
            self.put(at + index, value);
        }
        Ok(())
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
                Value::Closure(id) => {
                    let closure = self.heap.closure(id)?;
                    let site = Site::new(
                        closure.site.unit.clone(),
                        closure
                            .site
                            .path
                            .iter()
                            .copied()
                            .chain([0])
                            .collect::<Vec<_>>(),
                    );
                    let code = exe
                        .code_at(&site)
                        .ok_or_else(|| fault("a closure names no admitted code"))?;
                    let captures = exe
                        .code(code)
                        .captures
                        .iter()
                        .map(|capture| {
                            closure
                                .captures
                                .iter()
                                .find(|(name, _)| {
                                    *name == exe.code(code).slots[capture.inner as usize].name
                                })
                                .map(|(_, id)| *id)
                                .ok_or_else(|| fault("a closure capture is missing"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Target::Code(code, captures))
                }
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

    /// A read-bound `perform`, answered by the host at once: its result
    /// decoded by the `perform`'s type, or the error the host answered
    /// raised here, each charged as a recorded `perform`'s outcome is
    /// (`K-EFF-005`, `K-EFF-009`, `K-EFF-012`).
    fn perform_read(
        &mut self,
        host: &mut dyn Host,
        exe: &Executable,
        effect: &EffectName,
        args: &[Datum],
        site: &Site,
        result: &Type,
    ) -> Eval<Value> {
        match host.perform(effect, args, site) {
            Ok(Ok(answer)) => self.decode(exe, &answer, result),
            Ok(Err(error)) => {
                let datum = Datum::Error(Box::new(error));
                Err(Interrupt::Raise(self.decode(exe, &datum, &Type::Any)?))
            }
            Err(fault) => Err(Halt::Host(fault).into()),
        }
    }

    /// Parks the running task in a wait.
    fn suspend(&mut self, task: TaskId, stmt: StmtId, wait: Wait) -> Result<(), Halt> {
        self.frame(task)?.awaiting = Some(stmt);
        self.task(task)?.suspend(wait);
        self.current = None;
        Ok(())
    }

    fn occurrence(&mut self, task: TaskId, site: &Site) -> Result<u64, Halt> {
        let key = encode_body(site);
        let access = self.heap.access.clone();
        let task = self.task(task)?;
        let occurrence = task.occurrences.get(&key, &access)?.unwrap_or(0);
        let next = occurrence
            .checked_add(1)
            .ok_or_else(|| fault("occurrence counter overflow"))?;
        task.occurrences.write(key, next, &access)?;
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
        if self.issued_count()? >= limit {
            return Err(bound(Bound::RequestsPerPark, limit));
        }
        let occurrence = self.occurrence(task, site)?;
        let running = self.task(task)?;
        let loops = running.frames.loop_coordinates()?;
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
    fn issue(&mut self, task: TaskId, wait: WaitId, request: Request) -> Result<(), Halt> {
        let (identity, request, kind) = match request {
            Request::Effect(request) => (
                request.identity,
                lash_kernel_state::AdmittedRequest::Effect {
                    effect: request.effect,
                    args: request.args,
                },
                WaitKindRecord::Effect,
            ),
            Request::Sleep(request) => (
                request.identity,
                lash_kernel_state::AdmittedRequest::Sleep {
                    duration: request.duration.into(),
                },
                WaitKindRecord::Sleep,
            ),
        };
        let intent = lash_kernel_state::AdmittedWait { identity, request };
        let datum = Datum::Bytes(lash_kernel_doc::Bytes::new(encode_body(&intent)));
        let frozen = self
            .heap
            .freeze_datum(&datum)
            .map_err(|error| self.heap_halt(error))?;
        self.waits.write(
            ordinal_key(wait.0),
            WaitRecord {
                task,
                kind,
                phase: WaitPhase::Issued { request: frozen },
            },
            &self.heap.access,
        )?;
        Ok(())
    }
    pub(super) fn report_request(
        &mut self,
        wait: WaitId,
        task: TaskId,
        frozen: &lash_kernel_state::paged::FrozenRef,
    ) -> Result<Request, lash_kernel_doc::heap::HeapFault> {
        let Datum::Bytes(bytes) = self.heap.frozen_datum(frozen)? else {
            return Err(lash_kernel_doc::heap::HeapFault::Malformed(
                "issued request is not its token envelope".into(),
            ));
        };
        let intent: lash_kernel_state::AdmittedWait = decode_body(bytes.as_slice())
            .map_err(|error| lash_kernel_doc::heap::HeapFault::Malformed(error.to_string()))?;
        Ok(match intent.request {
            lash_kernel_state::AdmittedRequest::Sleep { duration } => {
                Request::Sleep(SleepRequest {
                    wait,
                    identity: intent.identity,
                    duration: duration.duration().ok_or_else(|| {
                        lash_kernel_doc::heap::HeapFault::Malformed("invalid sleep duration".into())
                    })?,
                })
            }
            lash_kernel_state::AdmittedRequest::Effect { effect, args } => {
                let stmt = self
                    .frame(task)
                    .map_err(|error| {
                        lash_kernel_doc::heap::HeapFault::Malformed(format!("{error:?}"))
                    })?
                    .awaiting
                    .ok_or_else(|| {
                        lash_kernel_doc::heap::HeapFault::Malformed(
                            "issued effect has no action".into(),
                        )
                    })?;
                let Form::Action(action) = &self.exe.stmt(stmt).form else {
                    return Err(lash_kernel_doc::heap::HeapFault::Malformed(
                        "issued effect has no action".into(),
                    ));
                };
                let ActionKind::Perform { result, .. } = &action.kind else {
                    return Err(lash_kernel_doc::heap::HeapFault::Malformed(
                        "issued effect is not a perform".into(),
                    ));
                };
                Request::Effect(EffectRequest {
                    wait,
                    identity: intent.identity,
                    effect,
                    args,
                    result: result.clone(),
                })
            }
        })
    }
    fn issued_count(&self) -> Result<u64, Halt> {
        let mut count = 0;
        self.waits.visit(&self.heap.access, &mut |_, wait| {
            count += u64::from(matches!(wait.phase, WaitPhase::Issued { .. }));
            true
        })?;
        Ok(count)
    }
    fn add_joiner(&mut self, task: TaskId, joiner: Joiner) -> Result<(), Halt> {
        let access = self.heap.access.clone();
        let task = self.task(task)?;
        append(&mut task.joiners, joiner, &access)?;
        Ok(())
    }

    /// `spawn`: the new task runs at once, and the spawning task goes on
    /// when it first waits or ends, before any other ready task
    /// (`K-TASK-002`). Its arguments are the `count` registers from `args`;
    /// the new task's handle is in `args` when the spawning task goes on at
    /// once.
    #[expect(
        clippy::too_many_arguments,
        reason = "a spawn is its statement, its site, its callee and its argument registers"
    )]
    fn spawn(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        site: &Site,
        target: Target,
        args: Reg,
        count: usize,
    ) -> Status {
        if let Target::Code(code, _) = &target
            && count > exe.code(*code).params.len()
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
        let occurrence = self.occurrence(task, site)?;
        let parent = self.task(task)?.identity.clone();
        let child = TaskId(self.tasks.len() as u64);
        self.tasks.push(Task::fresh(
            TaskIdentity::Spawned(SpawnIdentity {
                parent: Arc::new(parent),
                site: site.clone(),
                occurrence,
            }),
            self.heap.access.clone(),
            Arc::clone(&self.exe),
        ))?;
        self.live_tasks += 1;
        let handle = Value::Task(child);
        let started = match target {
            Target::Code(code, captures) => self
                .push_frame(child, exe, Call::new(code, args, count).sharing(&captures))
                .map(|()| Flow::Step),
            Target::Library(lib) => {
                self.call_lib(child, exe, lib, args, count, args, Entry::Frame(None))
            }
        };
        match started {
            Ok(Flow::Step) => {}
            // A function with no frame has run to its end already.
            Ok(Flow::Done) => {
                let value = self.take(args);
                self.end_task(child, Ok(value))?;
                self.put(args, handle);
                return Ok(Flow::Done);
            }
            // What the new task raises before it has a frame is its error.
            Err(Interrupt::Raise(value)) => {
                self.end_task(child, Err(value))?;
                self.put(args, handle);
                return Ok(Flow::Done);
            }
            Err(halt) => return Err(halt),
        }
        let spawner = self.task(task)?;
        spawner.wake(Incoming::Value(handle));
        self.frame(task)?.awaiting = Some(stmt);
        self.ready.push_front(task)?;
        self.current = Some(child);
        Ok(Flow::Step)
    }

    /// Marks tasks as members of a list `join` that has returned or
    /// raised: their errors are observed, and the run's end cancels them
    /// (`K-TASK-016`, `K-TASK-018`).
    pub(super) fn pass(&mut self, members: &Tree<TaskId>) -> Result<(), Halt> {
        for member in task_members(members, &self.heap.access)? {
            self.task(member)?.passed = true;
        }
        Ok(())
    }

    /// Removes a decided or cancelled list join from every member's wake
    /// queue, so no later ending can wake its joiner again.
    pub(super) fn remove_list_join(&mut self, order: u64) -> Result<Option<ListJoin>, Halt> {
        let join = self.joins.remove(&ordinal_key(order), &self.heap.access)?;
        if let Some(join) = &join {
            for member in task_members(&join.members, &self.heap.access)? {
                self.remove_joiner(member, Joiner::List(order))?;
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
        members: &Tree<TaskId>,
        ended: Option<TaskId>,
    ) -> Result<Option<Result<Value, Value>>, Halt> {
        let immediate = |result: &Result<Value, Value>| match mode {
            JoinMode::Race => true,
            JoinMode::Any => result.is_ok(),
            JoinMode::All => result.is_err(),
            JoinMode::AllSettled => false,
        };
        if let Some(ended) = ended {
            let result = match &self.task(ended)?.state {
                TaskState::Ended(super::EndedTask::Returned(value)) => Some(Ok(value.clone())),
                TaskState::Ended(super::EndedTask::Raised(value)) => Some(Err(value.clone())),
                _ => None,
            };
            if let Some(result) = result
                && immediate(&result)
            {
                return Ok(Some(result));
            }
        }
        let mut results = Vec::new();
        let mut complete = true;
        for at in 0..members.len() {
            let (_, member) = members
                .rank_get(at, &self.heap.access)?
                .ok_or_else(|| fault("a join member is missing"))?;
            let result = match &self.task(member)?.state {
                TaskState::Ended(super::EndedTask::Returned(value)) => Some(Ok(value.clone())),
                TaskState::Ended(super::EndedTask::Raised(value)) => Some(Err(value.clone())),
                _ => None,
            };
            if (ended.is_none() || matches!(mode, JoinMode::All | JoinMode::Any))
                && let Some(result) = &result
                && immediate(result)
            {
                return Ok(Some(result.clone()));
            }
            match result {
                Some(result) => results.push(result),
                None => complete = false,
            }
        }
        let all = complete.then_some(results);
        let first = |_wanted: fn(&Result<Value, Value>) -> bool| None;
        Ok(match mode {
            JoinMode::All => match (first(Result::is_err), all) {
                (Some(failure), _) => Some(failure),
                (None, Some(results)) => {
                    let values = results.into_iter().filter_map(Result::ok).collect();
                    Some(Ok(Value::List(self.alloc(ObjectInit::List(values))?)))
                }
                (None, None) => None,
            },
            JoinMode::AllSettled => match all {
                Some(results) => {
                    let mut records = Vec::with_capacity(results.len());
                    for result in results {
                        let fields = match result {
                            Ok(value) => vec![
                                (field_name("status"), Value::text("ok")),
                                (field_name("value"), value),
                            ],
                            Err(value) => vec![
                                (field_name("status"), Value::text("error")),
                                (field_name("error"), value),
                            ],
                        };
                        records.push(Value::Record(self.alloc(ObjectInit::Record(fields))?));
                    }
                    Some(Ok(Value::List(self.alloc(ObjectInit::List(records))?)))
                }
                None => None,
            },
            JoinMode::Race => first(|_| true),
            JoinMode::Any => match (first(Result::is_ok), all) {
                (Some(success), _) => Some(success),
                (None, Some(results)) if !results.is_empty() => {
                    let errors = results.into_iter().filter_map(Result::err).collect();
                    let errors = Value::List(self.alloc(ObjectInit::List(errors))?);
                    Some(Err(Value::Error(Arc::new(ErrorRef {
                        kind: field_name("all_failed"),
                        message: field_name("every task of the `join any` failed"),
                        cost: lash_kernel_doc::heap::LogicalBytes(
                            crate::heap::value_bytes(&errors).saturating_add(
                                "all_failed".len() as u64
                                    + "every task of the `join any` failed".len() as u64,
                            ),
                        ),
                        depth: 1,
                        has_edges: true,
                        task_edges: 0,
                        family: None,
                        data: Payload::Resident(Arc::new(errors)),
                    }))))
                }
                _ => None,
            },
        })
    }

    fn remove_joiner(&mut self, task: TaskId, wanted: Joiner) -> Result<(), Halt> {
        let access = self.heap.access.clone();
        let task = self.task(task)?;
        let mut key = None;
        while let Some((next, joiner)) = task.joiners.after(key.as_deref(), &access)? {
            if joiner == wanted {
                task.joiners.remove(&next, &access)?;
            }
            key = Some(next);
        }
        Ok(())
    }

    /// `cancel`: raises `cancelled` in a task at its wait (`K-TASK-017`).
    fn cancel(&mut self, target: TaskId) -> Result<(), Halt> {
        if matches!(self.task(target)?.state, TaskState::Ended(_)) {
            return Ok(());
        }
        let Some(wait) = self.task(target)?.wait() else {
            self.task(target)?.wake(Incoming::Raise(cancelled()));
            return Ok(());
        };
        match wait {
            Wait::Request(wait) => {
                if let Some(pending) = self.waits.remove(&ordinal_key(wait.0), &self.heap.access)? {
                    if !matches!(pending.phase, WaitPhase::Issued { .. }) {
                        self.withdrawn.push_back(wait.0)?;
                        self.withdrawn_ever.write(
                            ordinal_key(wait.0),
                            wait.0,
                            &self.heap.access,
                        )?;
                    }
                }
            }
            Wait::Join(joined) => self.remove_joiner(joined, Joiner::Single(target))?,
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

fn task_members(
    tree: &Tree<TaskId>,
    access: &Access<'_>,
) -> Result<Vec<TaskId>, lash_kernel_doc::heap::HeapFault> {
    let mut members = Vec::new();
    tree.visit(access, &mut |_, task| {
        members.push(*task);
        true
    })?;
    Ok(members)
}
fn append<T: Cell>(
    tree: &mut Tree<T>,
    value: T,
    access: &Access<'_>,
) -> Result<(), lash_kernel_doc::heap::HeapFault> {
    let index = match tree.rank_get(tree.len().saturating_sub(1), access)? {
        None => 0,
        Some((key, _)) => u64::from_be_bytes(key.as_slice().try_into().map_err(|_| {
            lash_kernel_doc::heap::HeapFault::Malformed("sequence key width".into())
        })?)
        .checked_add(1)
        .ok_or_else(|| {
            lash_kernel_doc::heap::HeapFault::Malformed("sequence position overflow".into())
        })?,
    };
    tree.write(ordinal_key(index), value, access)
}

fn field_name(text: &str) -> lash_kernel_doc::heap::TextRef {
    lash_kernel_doc::heap::TextRef {
        normal: Default::default(),
        summary: lash_kernel_doc::heap_values::text_summary(text),
        body: Payload::Resident(Arc::from(text)),
    }
}
