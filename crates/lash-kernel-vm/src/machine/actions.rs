//! Actions: calls, waits and tasks (`K-TASK`), as a statement's own action
//! or nested in its expression (`K-EVAL-008`).

use lash_kernel_state::paged::HeapLifecycle;
use lash_kernel_value::sites::EffectIdentity;
use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_doc::arena::{ArenaHandle, LocalCode};
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::{Datum, JoinMode, NewObject, TaskId, Value};

use lash_kernel_value::sites::Site;
use lash_kernel_value::types::Type;

use super::exec::Call;
use super::ops::{Flow, Reg, Status};
use super::stream::Running;
use super::{
    Eval, Halt, Incoming, Interrupt, Joiner, KernelMachine, ListJoin, Task, TaskState, Wait, bound,
    fault, raise,
};
use crate::compile::{
    ActionKind, Atom, Callee, CodeId, Executable, Form, LibId, Nested, NestedCallee, NestedKind,
    StmtId,
};
use crate::interface::{Bound, EffectRequest, Host, Request, WaitId};
use lash_kernel_doc::heap::ErrorRef;
use lash_kernel_state::kernel_internals::directory::ordinal_key;
use lash_kernel_state::kernel_internals::tree::{Access, Cell, Tree};
use lash_kernel_state::paged::{PhaseRecord, WaitKindRecord, WaitPhase, WaitRecord};
use {lash_kernel_value::tokens::decode_body, lash_kernel_value::tokens::encode_body};

/// What a call or a `spawn` runs, once its callee is read.
pub(super) enum Target {
    Code(
        CodeId,
        Vec<ArenaHandle>,
        lash_kernel_value::content_ids::LinkId,
    ),
    Library(LibId, lash_kernel_value::content_ids::LinkId),
    Export(
        lash_kernel_value::names::CallableId,
        Box<lash_kernel_doc::FunctionExport>,
    ),
}

/// The raise a `cancel` delivers, in page form (`K-TASK-017`).
fn cancelled() -> PageValue {
    PageValue::Error(Arc::new(ErrorRef::new(
        "cancelled",
        "the task was cancelled",
    )))
}

/// A task's own `cancel` of itself raises at once.
pub(in crate::machine) fn cancelled_here() -> Interrupt {
    Interrupt::refused("cancelled", "the task was cancelled")
}

impl KernelMachine {
    /// A `join` on `handle`: what it gives when the task has ended
    /// (`K-TASK-007`, `K-TASK-009`), or the task it waits on.
    pub(in crate::machine) fn join_one(
        &mut self,
        task: TaskId,
        handle: Value,
    ) -> Eval<Result<Value, TaskId>> {
        let Value::Task(joined) = handle else {
            return raise("type_error", "`join` takes a task handle");
        };
        if joined == task {
            return raise("join_self", "a task cannot join its own handle");
        }
        if matches!(self.task(joined)?.state, TaskState::Ended(_)) {
            return Ok(Ok(self.join_result(joined)?));
        }
        Ok(Err(joined))
    }

    /// Waits in statement `stmt` until `joined` ends (`K-TASK-008`).
    pub(in crate::machine) fn wait_one(
        &mut self,
        task: TaskId,
        stmt: StmtId,
        joined: TaskId,
    ) -> Result<(), Halt> {
        self.add_joiner(joined, Joiner::Single(task))?;
        self.suspend(task, stmt, Wait::Join(joined))
    }

    /// A list `join` in `mode` on `handles`: what it gives or raises when
    /// it is decided as it starts (`K-TASK-011` to `K-TASK-014`), or its
    /// members to wait on.
    pub(in crate::machine) fn join_list(
        &mut self,
        task: TaskId,
        mode: JoinMode,
        handles: &Value,
    ) -> Eval<Result<Value, Tree<TaskId>>> {
        let len = match *handles {
            Value::Tuple(items) | Value::List(items) => self.in_arena(|arena| arena.len(items))?,
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
            let member = match *handles {
                Value::Tuple(items) | Value::List(items) => {
                    self.in_arena(|arena| arena.item(items, at))?
                }
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
            tasks.write(ordinal_key(at), member, self.heap.pages())?;
        }
        if len == 0 && matches!(mode, JoinMode::Race | JoinMode::Any) {
            return raise("empty_join", "this `join` needs at least one task");
        }
        match self.decide(mode, &tasks, None)? {
            Some(decision) => {
                self.pass(&tasks)?;
                Ok(Ok(decision.map_err(Interrupt::raised)?))
            }
            None => Ok(Err(tasks)),
        }
    }

    /// Waits in statement `stmt` until the list `join` of `members` in
    /// `mode` is decided.
    pub(in crate::machine) fn wait_list(
        &mut self,
        task: TaskId,
        stmt: StmtId,
        mode: JoinMode,
        members: Tree<TaskId>,
    ) -> Result<(), Halt> {
        let order = self.next_join;
        self.next_join += 1;
        // Repeated handles keep their places in the result, but register
        // only one wake on each live member.
        for member in task_members(&members, self.heap.pages())?
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
                mode,
                members,
            },
            self.heap.pages(),
        )?;
        self.suspend(task, stmt, Wait::JoinMany(order))
    }

    /// An action that completed at once left its value in `at`, its
    /// arguments' first register: the value is the action's, in `dst`.
    /// A nested operator (`Nested::operator`): its call answered as an
    /// operator's ([`Self::operator`]).
    #[inline(always)]
    pub(super) fn nested_operator(
        &mut self,
        running: &Running<'_>,
        act: lash_kernel_exec::ActIx,
        lib: LibId,
        args: Reg,
    ) -> Status {
        let answer = self
            .operator(running.task, running.exe, lib, args, args)
            .map_err(|interrupt| interrupt.answering(running.exe.lib(lib)))?;
        match answer {
            super::ops::Answer::Done => Ok(Flow::Done),
            answer => self.body_answered(running, act, lib, args, answer),
        }
    }

    /// An invoke keeps its action observation and occurrence even when
    /// its prepared operator answers without a frame.
    #[inline(always)]
    pub(super) fn nested_invoke_operator(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        act: lash_kernel_exec::ActIx,
        lib: LibId,
        args: lash_kernel_exec::RegRange,
    ) -> Status {
        let count = args.len as usize;
        let args = args.start.0 as usize;
        let entry = &running.stream.nested[act.ix()];
        self.observe_action(running.exe, &entry.site);
        if entry.guest {
            self.enter(running.task, host, &entry.site, entry.occurrence)?;
        }
        let function = running.exe.lib(lib);
        if matches!(entry.kind, NestedKind::Call(NestedCallee::CheckedHelper(_))) {
            self.check_call_arguments(
                running.exe,
                lash_kernel_value::CallableId {
                    function: function.specialization.function,
                    link: self.current_link(),
                },
                &function.header.signature,
                args,
                count,
                true,
            )?;
        }
        // Observed deferred plans validate before offering their prepared
        // entry, just as general invoke dispatch does.
        let answer = if !entry.operator
            || (self
                .observer
                .as_deref()
                .is_some_and(|o| o.observes_bodies())
                && matches!(function.run, crate::compile::LibRun::Body(_)))
        {
            self.answer(running.task, running.exe, lib, args, count, args)
        } else {
            self.operator(running.task, running.exe, lib, args, args)
        }
        .map_err(|interrupt| interrupt.answering(function))?;
        self.body_answered(running, act, lib, args, answer)
    }

    pub(in crate::machine) fn answered_at(&mut self, flow: Flow, at: Reg, dst: Reg) -> Flow {
        if flow == Flow::Done && at != dst {
            let value = self.take(at);
            self.put(dst, value);
        }
        flow
    }

    /// Counts the running task's entry into the action at `site` and tells
    /// a host that observes sites.
    pub(in crate::machine) fn enter(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        site: &Site,
        index: usize,
    ) -> Result<(), Halt> {
        let occurrence = self.task(task)?.counters.advance(index)?;
        if host.observes_sites() {
            let identity = self.action_location(task, site, occurrence)?;
            host.enter_site(&identity);
        }
        Ok(())
    }

    /// Saves where the top frame of `task` stands: at the nested action of
    /// `entry`, in `phase`, keeping the operands its statement evaluated as
    /// the continuation's roles. Registers are never saved: each role is
    /// saved by its site, as the path suffix under its statement's, held by
    /// the frame as a variable's value is (`K-EVAL-008`). The suffixes are
    /// the entry's own; each role moves from its register to the frame.
    pub(super) fn capture(
        &mut self,
        task: TaskId,
        entry: &Nested,
        act: lash_kernel_exec::ActIx,
        phase: PhaseRecord,
    ) -> Result<(), Halt> {
        let pending =
            self.continuation(&entry.suffix, phase, entry.roles.len(), |machine, at| {
                let role = &entry.roles[at];
                let held = machine.take_source_role(role.reg, &role.site)?;
                machine.take(role.reg.0 as usize);
                Ok(super::frames::Role {
                    site: Arc::clone(&role.suffix),
                    value: held.value,
                    holding: held.holding,
                })
            })?;
        self.frame(task)?.stand_at(act, pending);
        Ok(())
    }

    pub(in crate::machine) fn atom(
        &mut self,
        task: TaskId,
        exe: &Executable,
        atom: &Atom,
    ) -> Eval<Value> {
        if self.charging {
            self.charge(1)?;
        } else if self.paced {
            self.charged += 1;
        }
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
    pub(super) fn call(
        &mut self,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        target: Target,
        args: Reg,
        count: usize,
    ) -> Status {
        match target {
            Target::Export(callable, descriptor) => self.call_export(
                task,
                exe,
                callable,
                &descriptor,
                super::exec::Invocation::new(args, count).awaited_by(Some(stmt)),
            ),
            Target::Code(code, captures, link) => {
                let call = Call::new(code, args, count)
                    .in_link(link)
                    .sharing(&captures)
                    .awaited_by(Some(stmt));
                let pushed = self.push_frame(task, exe, call);
                self.spent_captures(captures);
                pushed?;
                Ok(Flow::Step)
            }
            Target::Library(lib, link) => self.call_lib_linked(
                task,
                exe,
                (lib, link),
                super::exec::Invocation::new(args, count)
                    .awaited_by(Some(stmt))
                    .with_boundary(None),
            ),
        }
    }

    pub(super) fn call_hook(
        &mut self,
        task: TaskId,
        exe: &Executable,
        hook: crate::compile::LinkedHook,
        args: Reg,
        count: usize,
        awaiting: Option<StmtId>,
    ) -> Status {
        let target = match hook {
            crate::compile::LinkedHook::Library(lib) => Target::Library(lib, self.current_link()),
            crate::compile::LinkedHook::Code(code) => {
                Target::Code(code, Vec::new(), self.current_link())
            }
            crate::compile::LinkedHook::Import(slot, _) => self.import_target(exe, slot)?,
        };
        match target {
            Target::Export(callable, descriptor) => self.call_export(
                task,
                exe,
                callable,
                &descriptor,
                super::exec::Invocation::new(args, count).awaited_by(awaiting),
            ),
            Target::Library(lib, link) => self.call_lib_linked(
                task,
                exe,
                (lib, link),
                super::exec::Invocation::new(args, count)
                    .awaited_by(awaiting)
                    .with_boundary(None),
            ),
            Target::Code(code, captures, link) => {
                let result = self.push_frame(
                    task,
                    exe,
                    Call::new(code, args, count)
                        .in_link(link)
                        .sharing(&captures)
                        .awaited_by(awaiting),
                );
                self.spent_captures(captures);
                result?;
                Ok(Flow::Step)
            }
        }
    }

    /// The caller supplies public slots; the defining Export owns its private ABI.
    /// Keeps a spent capture buffer for the next closure call's cells.
    pub(super) fn spent_captures(&mut self, mut captures: Vec<ArenaHandle>) {
        if captures.capacity() > self.storage.captures.capacity() {
            captures.clear();
            self.storage.captures = captures;
        }
    }

    /// Reads an action's atoms into the registers from `at`, left to right
    /// (`K-EVAL-004`).
    pub(in crate::machine) fn put_atoms(
        &mut self,
        task: TaskId,
        exe: &Executable,
        atoms: &[Atom],
        at: Reg,
    ) -> Eval<()> {
        for (index, atom) in atoms.iter().enumerate() {
            let value = self.atom(task, exe, atom)?;
            self.source_atom(index, value)?;
            self.put(at + index, value);
        }
        Ok(())
    }

    /// Reads a `perform`'s atoms, left to right (`K-EVAL-004`), each a
    /// boundary charge of 1 in whatever code the `perform` stands
    /// (`K-CHG-002`, `K-CHG-007`).
    pub(in crate::machine) fn perform_atoms(
        &mut self,
        task: TaskId,
        exe: &Executable,
        atoms: &[Atom],
    ) -> Eval<Vec<Value>> {
        let mut values = Vec::with_capacity(atoms.len());
        for (index, atom) in atoms.iter().enumerate() {
            if self.paced {
                self.charged += 1;
            } else {
                self.charge_boundary(1)?;
            }
            self.observe_expression();
            let value = match atom {
                Atom::Var(var) => self.read_var(task, exe, var)?,
                Atom::Literal(value) => self.literal(value)?,
            };
            self.source_atom(index, value)?;
            values.push(value);
        }
        Ok(values)
    }

    /// Reads a callee. One held in a variable is a closure or a function
    /// reference (`K-FN-005`).
    pub(in crate::machine) fn callee(
        &mut self,
        task: TaskId,
        exe: &Executable,
        callee: &Callee,
    ) -> Eval<Target> {
        match callee {
            Callee::Declared(code) => Ok(Target::Code(*code, Vec::new(), self.current_link())),
            Callee::Library(lib) => Ok(Target::Library(*lib, self.current_link())),
            Callee::CheckedHelper(lib) => Ok(self.build_helper_target(exe, *lib)),
            Callee::Import(slot, _) => self.import_target(exe, *slot),
            Callee::Value(var) => {
                let value = self.read_var(task, exe, var)?;
                self.target(exe, value)
            }
        }
    }

    pub(super) fn callable_target(
        &self,
        exe: &Executable,
        callable: lash_kernel_value::names::CallableId,
    ) -> Eval<Target> {
        if !exe.admits_callable(&callable) {
            return Err(fault("callable is not admitted by its defining Link").into());
        }
        match exe
            .native_starts
            .get(&callable.function)
            .or_else(|| exe.declared.get(&callable.function))
        {
            Some(code) => Ok(Target::Code(*code, Vec::new(), callable.link)),
            None => exe
                .library_lib(&callable.function)
                .map(|lib| Target::Library(lib, callable.link))
                .ok_or_else(|| fault("callable definition is not loaded").into()),
        }
    }

    /// The function a closure or a function reference names (`K-FN-005`).
    /// A closure finds its code by its site once per executable and keeps
    /// it; its cells are copied into the machine's spare capture buffer.
    pub(super) fn target(&mut self, exe: &Executable, value: Value) -> Eval<Target> {
        match value {
            Value::Closure(closure) => {
                let executable = exe.local();
                let mut cells = std::mem::take(&mut self.storage.captures);
                let found = self.in_arena(|arena| {
                    let (identity, captures, code) = arena.closure(closure)?;
                    cells.clear();
                    cells.extend_from_slice(captures);
                    Ok(match code {
                        Some(code) if code.executable == executable => {
                            Ok((CodeId(code.code), identity.callable.link))
                        }
                        _ => Err(identity.clone()),
                    })
                })?;
                let (code, link) = match found {
                    Ok(found) => found,
                    Err(identity) => {
                        let code = super::project::closure_code(
                            exe,
                            &identity.callable,
                            &identity.site.site,
                            &identity.capture_names,
                        )?;
                        let found = LocalCode {
                            executable,
                            code: code.0,
                        };
                        self.in_arena(|arena| arena.found_closure_code(closure, found))?;
                        (code, identity.callable.link)
                    }
                };
                // Saved environments are immutable templates. Each invocation owns
                // fresh cells and fresh instances of its frozen data.
                let entity = self.in_arena(|arena| Ok(arena.facts(closure)?.entity))?;
                if let Some(entity) = entity {
                    let functions = super::session::open_tree::<
                        lash_kernel_state::paged::FunctionRecord,
                    >(self.functions);
                    let mut saved = None;
                    functions.visit(self.heap.pages(), &mut |_, record| {
                        if record.environment == entity {
                            saved = Some(record.clone());
                            false
                        } else {
                            true
                        }
                    })?;
                    if let Some(saved) = saved {
                        cells.clear();
                        let compiled = exe.code(code);
                        for name in compiled
                            .captures
                            .iter()
                            .map(|capture| &compiled.slots[capture.inner as usize].name)
                            .chain(compiled.free.iter().map(|(name, _)| name))
                        {
                            let frozen = saved
                                .captures
                                .get(name)
                                .ok_or_else(|| fault("saved capture is missing"))?;
                            let page = self.heap.instantiate(frozen).map_err(Interrupt::from)?;
                            let value = self.admit_page(&page)?;
                            cells.push(self.new_cell(value));
                        }
                    }
                }
                Ok(Target::Code(code, cells, link))
            }
            Value::Function(handle) => {
                let callable = self.in_arena(|arena| arena.callable(handle))?;
                self.callable_target(exe, callable)
            }
            _ => raise(
                "type_error",
                "only a closure or a function reference can be called",
            ),
        }
    }

    /// What a nested call or `spawn` runs, and the registers its arguments
    /// start at: after the callee's, when a variable held it.
    pub(in crate::machine) fn nested_target(
        &mut self,
        exe: &Executable,
        callee: &NestedCallee,
        args: Reg,
    ) -> Eval<(Target, Reg)> {
        Ok(match callee {
            NestedCallee::Declared(code) => {
                (Target::Code(*code, Vec::new(), self.current_link()), args)
            }
            NestedCallee::Library(lib) => (Target::Library(*lib, self.current_link()), args),
            NestedCallee::CheckedHelper(lib) => (self.build_helper_target(exe, *lib), args),
            NestedCallee::Import(slot, _) => (self.import_target(exe, *slot)?, args),
            NestedCallee::Read => {
                let value = self.take(args);
                (self.target(exe, value)?, args + 1)
            }
        })
    }

    /// A recorded `perform` of `effect` at `site` of statement `stmt`, its
    /// arguments copied out: the task waits in it from here (`K-EFF-001`,
    /// `K-EFF-008`). The top frame stands at the action. A body's frame
    /// and the plan standing as that frame issue it here alike.
    pub(super) fn issue_effect(
        &mut self,
        task: TaskId,
        stmt: StmtId,
        site: &Site,
        effect: lash_kernel_value::names::OperationUse,
        args: Vec<Datum>,
    ) -> Result<(), Halt> {
        let wait = self.request(task, stmt, site)?;
        self.issue(
            task,
            wait,
            lash_kernel_state::AdmittedRequest::Effect { use_: effect, args },
        )
    }

    /// A read-bound `perform`, answered by the host at once: its result
    /// decoded by the `perform`'s type, or the error the host answered
    /// raised here, each charged as a recorded `perform`'s outcome is
    /// (`K-EFF-005`, `K-EFF-009`, `K-EFF-012`).
    #[expect(
        clippy::too_many_arguments,
        reason = "a read is its task, its effect, its copied arguments, its site and its type"
    )]
    pub(super) fn perform_read(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        effect: &lash_kernel_value::names::OperationUse,
        args: &[Datum],
        site: &Site,
        result: &Type,
    ) -> Eval<Value> {
        let origin = self.origin(task)?;
        self.report_ran(host);
        match host.perform(effect, args, &site.position(), &origin) {
            Ok(Ok(answer)) => self.decode_result(exe, &answer, result, effect),
            Ok(Err(error)) => {
                let datum = Datum::Error(Box::new(error));
                Err(Interrupt::raised(self.decode_answer(
                    exe,
                    &datum,
                    &Type::Any,
                )?))
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

    /// Takes the next wait for a `perform` or a `sleep` of the running
    /// task and gives its identity (`K-EFF-008`).
    fn request(&mut self, task: TaskId, stmt: StmtId, site: &Site) -> Result<WaitId, Halt> {
        let limit = u64::from(self.bounds.requests_per_park);
        if self.issued_count()? >= limit {
            return Err(bound(Bound::RequestsPerPark, limit));
        }
        let index = self
            .exe
            .occurrence_index(site)
            .ok_or_else(|| fault("request site has no occurrence index"))?;
        self.task(task)?.counters.advance(index)?;
        let wait = lash_kernel_doc::kernel_internals::wait_id(self.next_wait);
        self.next_wait = self
            .next_wait
            .checked_add(1)
            .ok_or_else(|| fault("wait counter overflow"))?;
        self.suspend(task, stmt, Wait::Request(wait))?;
        Ok(wait)
    }

    /// The intent records request data. Identity is projected at its host
    /// boundary from the waiting task's stable semantic coordinates.
    fn issue(
        &mut self,
        task: TaskId,
        wait: WaitId,
        request: lash_kernel_state::AdmittedRequest,
    ) -> Result<(), Halt> {
        let kind = match request {
            lash_kernel_state::AdmittedRequest::Effect { .. } => WaitKindRecord::Effect,
        };
        let datum = Datum::Bytes(lash_kernel_doc::Bytes::new(encode_body(&request)));
        let frozen = self.heap.freeze_datum(&datum).map_err(super::write_halt)?;
        self.waits.write(
            ordinal_key(lash_kernel_doc::kernel_internals::wait_number(wait)),
            WaitRecord {
                task,
                kind,
                phase: WaitPhase::Issued { request: frozen },
            },
            self.heap.pages(),
        )?;
        self.count_issued()?;
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
        let intent: lash_kernel_state::AdmittedRequest = decode_body(bytes.as_slice())
            .map_err(|error| lash_kernel_doc::heap::HeapFault::Malformed(error.to_string()))?;
        let access = self.heap.pages().clone();
        self.reconcile_sweep()?;
        let origin = self.origin(task)?;
        let running = self.tasks.get_mut(task.0 as usize)?.ok_or_else(|| {
            lash_kernel_doc::heap::HeapFault::Malformed("issued task is missing".into())
        })?;
        let loops = running.frames.loop_coordinates()?;
        let (site, result) = running
            .frames
            .last_mut()?
            .and_then(|frame| waiting(&self.exe, frame))
            .ok_or_else(|| {
                lash_kernel_doc::heap::HeapFault::Malformed("issued request has no action".into())
            })?;
        let index = self.exe.occurrence_index(&site).ok_or_else(|| {
            lash_kernel_doc::heap::HeapFault::Malformed("issued occurrence site is missing".into())
        })?;
        let occurrence = running.counters.issued(index).ok_or_else(|| {
            lash_kernel_doc::heap::HeapFault::Malformed("issued occurrence is missing".into())
        })?;
        let origin = if self.exe.authored_unit(&site.unit) {
            site.clone()
        } else {
            origin
        };
        let identity = lash_kernel_value::kernel_internals::effect_id(EffectIdentity {
            task: running.identity.resolve(&access)?,
            site: site.clone(),
            occurrence,
            loops,
        });
        Ok(match intent {
            lash_kernel_state::AdmittedRequest::Effect { use_: effect, args } => {
                let Some((_, result)) = result else {
                    return Err(lash_kernel_doc::heap::HeapFault::Malformed(
                        "issued effect is not a perform".into(),
                    ));
                };
                Request::Effect(EffectRequest {
                    wait,
                    identity,
                    use_: effect,
                    args,
                    result: result.clone(),
                    origin,
                })
            }
        })
    }
    fn add_joiner(&mut self, task: TaskId, joiner: Joiner) -> Result<(), Halt> {
        let access = self.heap.pages().clone();
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
    pub(in crate::machine) fn spawn(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: StmtId,
        site: &Site,
        target: Target,
        args: Reg,
        count: usize,
    ) -> Status {
        let (target, count, export) = match target {
            Target::Export(callable, descriptor) => {
                let count = self.export_arguments(exe, callable, &descriptor, args, count)?;
                (
                    self.callable_target(exe, callable)?,
                    count,
                    Some(descriptor),
                )
            }
            target => (target, count, None),
        };
        if let Target::Code(code, _, link) = &target
            && *link == self.current_link()
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
        let index = exe
            .occurrence_index(site)
            .ok_or_else(|| fault("spawn site has no occurrence index"))?;
        let occurrence = self.task(task)?.counters.advance(index)?;
        if !matches!(site.unit, lash_kernel_value::sites::Unit::Library(_)) && host.observes_sites()
        {
            let at = self.action_location(task, site, occurrence)?;
            host.enter_site(&at);
        }

        let spawn_origin = self.spawn_origin(task, exe, site, &target)?;
        let parent = self.task(task)?.identity;
        let child = TaskId(self.task_table()?.len() as u64);
        self.tasks.push(Task::fresh(
            lash_kernel_state::paged::TaskIdentityRef::spawn(
                parent,
                lash_kernel_state::paged::SiteRef::new(site, self.heap.pages())?,
                occurrence,
                self.heap.pages(),
            )?,
            spawn_origin,
            self.heap.pages().clone(),
            std::rc::Rc::clone(&self.arena),
            Arc::clone(&self.exe),
        ))?;
        self.live_tasks += 1;
        let handle = Value::Task(child);
        // A task root is outside its spawner's synchronous library call.
        // A native answer bills the same root formula as a body frame.
        self.refresh_charging(child)?;
        let exported_callable = export.as_ref().map(|export| lash_kernel_value::CallableId {
            function: export.function,
            link: match &target {
                Target::Library(_, link) | Target::Code(_, _, link) => *link,
                Target::Export(callable, _) => callable.link,
            },
        });
        let started = match target {
            Target::Code(code, captures, link) => {
                let pushed = self.push_frame(
                    child,
                    exe,
                    Call::new(code, args, count)
                        .in_link(link)
                        .sharing(&captures)
                        .exported(export.as_deref()),
                );
                self.spent_captures(captures);
                pushed.map(|()| Flow::Step)
            }
            Target::Library(lib, link) => self.call_lib_linked(
                child,
                exe,
                (lib, link),
                super::exec::Invocation::new(args, count)
                    .awaited_by(None)
                    .with_boundary(export.as_deref().map(Into::into)),
            ),
            Target::Export(..) => Err(fault("an export resolved to another export").into()),
        };
        let started = started.and_then(|flow| {
            if flow == Flow::Done
                && let Some(export) = &export
            {
                let value = self.regs[args];
                self.check_public_result(
                    exe,
                    &value,
                    &export.signature.result,
                    exported_callable.ok_or_else(|| fault("exported spawn has no callable"))?,
                )?;
            }
            Ok(flow)
        });
        match started {
            Ok(Flow::Step) => {}
            // A function with no frame has run to its end already.
            Ok(Flow::Done) => {
                let value = self.take(args);
                self.end_task(child, Ok(value))?;
                self.put(args, handle);
                self.refresh_charging(task)?;
                return Ok(Flow::Done);
            }
            // What the new task raises before it has a frame is its error.
            Err(raised @ (Interrupt::Raise(_) | Interrupt::Refuse(_))) => {
                let value = self.thrown_by(child, raised)?;
                self.end_task(child, Err(*value))?;
                self.put(args, handle);
                self.refresh_charging(task)?;
                return Ok(Flow::Done);
            }
            Err(halt) => return Err(halt),
        }
        let spawner = self.task(task)?;
        spawner.wake(Incoming::Value(PageValue::Task(child)), false);
        self.frame(task)?.awaiting = Some(stmt);
        self.ready.push_front(task)?;
        // A child whose plan already waits in its `perform` (FIG-6916) has
        // taken its first turn, as its body's frame would have: the
        // spawner goes on.
        if self.task(child)?.wait().is_none() {
            self.current = Some(child);
        }
        Ok(Flow::Step)
    }

    /// Marks tasks as members of a list `join` that has returned or
    /// raised: their errors are observed, and the run's end cancels them
    /// (`K-TASK-016`, `K-TASK-018`).
    pub(super) fn pass(&mut self, members: &Tree<TaskId>) -> Result<(), Halt> {
        for member in task_members(members, self.heap.pages())? {
            self.task(member)?.passed = true;
        }
        Ok(())
    }

    /// Removes a decided or cancelled list join from every member's wake
    /// queue, so no later ending can wake its joiner again.
    pub(super) fn remove_list_join(&mut self, order: u64) -> Result<Option<ListJoin>, Halt> {
        let join = self.joins.remove(&ordinal_key(order), self.heap.pages())?;
        if let Some(join) = &join {
            for member in task_members(&join.members, self.heap.pages())? {
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
        let immediate = |result: &Result<PageValue, PageValue>| match mode {
            JoinMode::Race => true,
            JoinMode::Any => result.is_ok(),
            JoinMode::All => result.is_err(),
            JoinMode::AllSettled => false,
        };
        if let Some(ended) = ended {
            let result = self.ended_how(ended)?;
            if let Some(result) = result
                && immediate(&result)
            {
                return Ok(Some(self.admit_result(&result)?));
            }
        }
        let mut results = Vec::new();
        let mut complete = true;
        for at in 0..members.len() {
            let (_, member) = members
                .rank_get(at, self.heap.pages())?
                .ok_or_else(|| fault("a join member is missing"))?;
            let result = self.ended_how(member)?;
            if (ended.is_none() || matches!(mode, JoinMode::All | JoinMode::Any))
                && let Some(result) = &result
                && immediate(result)
            {
                return Ok(Some(self.admit_result(result)?));
            }
            match result {
                Some(result) => results.push(result),
                None => complete = false,
            }
        }
        let all = match complete {
            true => Some(
                results
                    .iter()
                    .map(|result| self.admit_result(result))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            false => None,
        };
        let first = |_wanted: fn(&Result<Value, Value>) -> bool| None;
        Ok(match mode {
            JoinMode::All => match (first(Result::is_err), all) {
                (Some(failure), _) => Some(failure),
                (None, Some(results)) => {
                    let values = results.into_iter().filter_map(Result::ok).collect();
                    Some(Ok(self.new_object(NewObject::List(values))?))
                }
                (None, None) => None,
            },
            JoinMode::AllSettled => match all {
                Some(results) => {
                    let mut records = Vec::with_capacity(results.len());
                    for result in results {
                        let fields = {
                            let mut arena = self.arena.borrow_mut();
                            let status = arena.text("status");
                            match result {
                                Ok(value) => {
                                    vec![(status, arena.text("ok")), (arena.text("value"), value)]
                                }
                                Err(value) => vec![
                                    (status, arena.text("error")),
                                    (arena.text("error"), value),
                                ],
                            }
                        };
                        records.push(self.new_object(NewObject::Record(fields))?);
                    }
                    Some(Ok(self.new_object(NewObject::List(records))?))
                }
                None => None,
            },
            JoinMode::Race => first(|_| true),
            JoinMode::Any => match (first(Result::is_ok), all) {
                (Some(success), _) => Some(success),
                (None, Some(results)) if !results.is_empty() => {
                    let errors = results.into_iter().filter_map(Result::err).collect();
                    let errors = self.new_object(NewObject::List(errors))?;
                    Some(Err(self.arena.borrow_mut().error(
                        "all_failed",
                        "every task of the `join any` failed",
                        errors,
                    )))
                }
                _ => None,
            },
        })
    }

    /// How `task` ended, if it has: its record's page form.
    fn ended_how(&mut self, task: TaskId) -> Result<Option<Result<PageValue, PageValue>>, Halt> {
        let TaskState::Ended(ended) = &self.task(task)?.state else {
            return Ok(None);
        };
        match ended.result() {
            Some(Ok(value)) => Ok(Some(Ok(value.clone()))),
            Some(Err(value)) => Ok(Some(Err(value.clone()))),
            None => Err(fault("a join read a retired task result")),
        }
    }

    /// A task's ended result, admitted to the arena.
    fn admit_result(
        &mut self,
        result: &Result<PageValue, PageValue>,
    ) -> Result<Result<Value, Value>, Halt> {
        Ok(match result {
            Ok(value) => Ok(self.admit_page(value)?),
            Err(value) => Err(self.admit_page(value)?),
        })
    }

    fn remove_joiner(&mut self, task: TaskId, wanted: Joiner) -> Result<(), Halt> {
        let access = self.heap.pages().clone();
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
    pub(in crate::machine) fn cancel(&mut self, target: TaskId) -> Result<(), Halt> {
        if matches!(self.task(target)?.state, TaskState::Ended(_)) {
            return Ok(());
        }
        let Some(wait) = self.task(target)?.wait() else {
            // The raise replaces what a ready task resumes with; a
            // delivered one still takes its turn first (`K-TASK-005`).
            let ready = self.task(target)?;
            let requeue = ready.owes_turn();
            ready.wake(Incoming::Raise(cancelled()), requeue);
            return Ok(());
        };
        // A `then yield` wait the cancel withdraws takes its turn too.
        let requeue = matches!(wait, Wait::Request(_)) && {
            let exe = Arc::clone(&self.exe);
            then_yields(&exe, self.frame(target)?)
        };
        match wait {
            Wait::Request(wait) => {
                match self.waits.remove(
                    &ordinal_key(lash_kernel_doc::kernel_internals::wait_number(wait)),
                    self.heap.pages(),
                )? {
                    Some(pending) if matches!(pending.phase, WaitPhase::Issued { .. }) => {
                        self.uncount_issued()?;
                    }
                    Some(_) => {
                        self.withdrawn
                            .push_back(lash_kernel_doc::kernel_internals::wait_number(wait))?;
                        self.withdrawn_ever.write(
                            ordinal_key(lash_kernel_doc::kernel_internals::wait_number(wait)),
                            lash_kernel_doc::kernel_internals::wait_number(wait),
                            self.heap.pages(),
                        )?;
                    }
                    None => {}
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
        self.task(target)?
            .wake(Incoming::Raise(cancelled()), requeue);
        self.ready.push_back(target)?;
        Ok(())
    }
}

/// The site of the action the frame waits at and, for a `perform`, its
/// effect and the type its result is decoded by: the frame's statement's
/// own action, or the nested action it stands at.
pub(super) fn waiting(
    exe: &Executable,
    frame: &super::Frame,
) -> Option<(Site, Option<(lash_kernel_value::names::ImportSlot, Type)>)> {
    let (stream, statement, _) = exe.statement(frame.awaiting?);
    match frame.nested {
        Some(act) => {
            let entry = stream.nested.get(act.ix())?;
            let perform = match &entry.kind {
                NestedKind::Perform { slot, result, .. } => Some((*slot, result.clone())),
                _ => None,
            };
            Some((entry.site.clone(), perform))
        }
        None => {
            let Form::Action(action) = &statement.form else {
                return None;
            };
            let perform = match &action.kind {
                ActionKind::Perform { slot, result, .. } => Some((*slot, result.clone())),
                _ => None,
            };
            Some((action.site.clone(), perform))
        }
    }
}

/// Whether the action the frame waits at waits `then yield`: its task
/// takes one more turn once the outcome is in (`K-TASK-005`).
pub(super) fn then_yields(exe: &Executable, frame: &super::Frame) -> bool {
    let Some(stmt) = frame.awaiting else {
        return false;
    };
    let (stream, statement, _) = exe.statement(stmt);
    match frame.nested {
        Some(act) => stream
            .nested
            .get(act.ix())
            .is_some_and(|entry| entry.kind.then_yield()),
        None => matches!(&statement.form, Form::Action(action) if action.kind.then_yield()),
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

#[cfg(test)]
mod laws {
    use super::Target;
    use crate::compile::CodeId;
    use crate::laws::embedder::Embedder;
    use lash_kernel_doc::Value;
    use lash_kernel_doc::arena::LocalCode;
    use lash_kernel_value::sites::Unit;

    /// K-FN-005: the code a call found for a closure's site is cached on
    /// the closure for the executable that found it alone. A cache naming
    /// any other executable's numbering is never read: the call finds the
    /// code by the closure's site again, and keeps that.
    #[test]
    fn a_closure_reads_its_cached_code_only_under_the_executable_that_found_it() {
        let mut run = Embedder::new(
            "main { let f = fn() { return 1 } let g = fn() { return 2 } return null }",
        );
        let exe = std::sync::Arc::clone(&run.machine.exe);
        let mut closures: Vec<CodeId> = (0..exe.code_count() as u32)
            .map(CodeId)
            .filter(|code| {
                Some(*code) != exe.main
                    && exe
                        .code(*code)
                        .created
                        .as_ref()
                        .is_some_and(|site| site.unit == Unit::Main)
            })
            .collect();
        closures.sort_by(|a, b| {
            exe.code(*a)
                .created
                .as_ref()
                .unwrap()
                .path
                .cmp(&exe.code(*b).created.as_ref().unwrap().path)
        });
        let [f, g] = closures[..] else {
            panic!("the document makes two closures: {closures:?}");
        };
        let closure = run.machine.make_closure(&exe, f, &[]).unwrap();
        let cached = |run: &Embedder| run.machine.arena.borrow().closure(closure).unwrap().2;
        let called = |run: &mut Embedder| match run.machine.target(&exe, Value::Closure(closure)) {
            Ok(Target::Code(code, _, _)) => code,
            _ => panic!("a closure calls its code"),
        };
        let found = LocalCode {
            executable: exe.local(),
            code: f.0,
        };
        assert_eq!(cached(&run), Some(found));
        assert_eq!(called(&mut run), f);
        assert_eq!(cached(&run), Some(found));
        // A separately allocated executable has its own code numbering,
        // even when both machines hold the same source.
        let other = Embedder::new(
            "main { let f = fn() { return 1 } let g = fn() { return 2 } return null }",
        );
        assert_ne!(exe.local(), other.machine.exe.local());
        let elsewhere = LocalCode {
            executable: other.machine.exe.local(),
            code: g.0,
        };
        run.machine
            .arena
            .borrow_mut()
            .found_closure_code(closure, elsewhere)
            .unwrap();
        assert_eq!(called(&mut run), f);
        assert_eq!(cached(&run), Some(found));
    }
}
