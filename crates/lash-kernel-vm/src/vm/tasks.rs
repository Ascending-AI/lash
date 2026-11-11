//! Tasks and actions (`K-TASK-001` … `K-TASK-026`, `K-EFF-001` …
//! `K-EFF-012`): the scheduler, `perform`, `spawn`, `join`, `yield` and
//! `cancel`, parks and deliveries.
//!
//! One task runs at a time; its context is the machine's own
//! ([`Vm::cx`]), and every other task keeps its context in its row. A task
//! that waits leaves the machine with no task installed; the next run, or
//! the action that made it wait, installs the front of the ready queue and
//! hands it what it resumes with.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use lash_kernel_doc::arena::Body;
use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::{Datum, JoinMode, TaskId, Value};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{FunctionId, ImportSlot, OperationUse};
use lash_kernel_value::sites::{
    EffectIdentity, LoopIteration, Site, SpawnIdentity, TaskIdentity, TaskLabel, WaitCancellation,
};
use lash_kernel_value::types::Type;

use super::frames::Context;
use super::{Exceeded, Flow, Run, Stop, Vm, fault};
use crate::Bound;
use crate::bytecode::op::Op;
use crate::bytecode::program::{ActionSite, CallSite, Callee, LIB, Mark, Unit, UnitId};
use crate::heap::Heap;
use crate::interface::{EffectRequest, Park, Request, RunError};
use crate::{Host, HostFault};

/// The first task: `main`, or the entry the run started on.
pub(crate) const MAIN: usize = 0;

/// One task of the run.
pub(crate) struct Task {
    pub identity: Arc<TaskIdentity>,
    /// Its frames and stack while it is not running.
    pub cx: Context,
    pub state: State,
    /// A delivered `then yield` wait owes one more turn before it resumes
    /// (`K-TASK-005`).
    pub turn: bool,
    /// How many times the task has entered each authored call and action
    /// site, indexed by the site's program-wide occurrence number
    /// (`K-EFF-008`); a site past the end has not been entered.
    pub occurrences: Vec<u64>,
    /// How many times the task has run each library action site, by
    /// [`crate::bytecode::program::Program::counter`].
    pub library: HashMap<u64, u64>,
    /// Where its spawner stood, when none of its own frames is document
    /// code (`K-EFF-008`).
    pub spawn_origin: Option<Site>,
    /// A member of a list `join` that returned or raised (`K-TASK-016`).
    pub passed: bool,
    /// A `join` raised its error (`K-TASK-016`).
    pub observed: bool,
    /// The joins waiting on it, in the order they began.
    pub joiners: Vec<Joiner>,
}

pub(crate) enum State {
    Running,
    Ready(Resume),
    Waiting(Wait),
    Ended(Result<Value, Value>),
}

/// What a ready task resumes with.
pub(crate) enum Resume {
    /// It goes on where it stands.
    Continue,
    Value(Value),
    Raise(Value),
    /// The committed outcome of its wait.
    Outcome(u64),
    /// The task it joined has ended.
    Joined(usize),
}

#[derive(Clone, Copy)]
pub(crate) enum Wait {
    Effect(u64),
    Join(usize),
    JoinMany(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Joiner {
    Single(usize),
    List(u64),
}

pub(crate) struct ListJoin {
    pub(crate) joiner: usize,
    pub(crate) mode: JoinMode,
    pub(crate) members: Vec<usize>,
}

/// A waited `perform`'s wait.
pub(crate) struct Pending {
    pub(crate) task: usize,
    pub(crate) phase: Phase,
    pub(crate) result: Type,
    pub(crate) then_yield: bool,
    /// What the next park hands over, until it does.
    pub(crate) request: Option<EffectRequest>,
}

pub(crate) enum Phase {
    /// Requested since the last park: no embedder has it.
    Issued,
    /// Handed over at a park.
    Admitted,
    /// Delivered: the host's outcome as it gave it, held raw until the
    /// task consumes it, which decodes it once (contract §5).
    Delivered(crate::Outcome),
}

/// How a perform's operation resolved at the start.
#[derive(Clone)]
pub(crate) struct Operation {
    pub use_: OperationUse,
    /// The run binds it as a read (`K-EFF-012`).
    pub read: bool,
}

/// Each perform's operation, by the owner of the code it stands in and
/// its import slot; `Err` when it resolves to none.
pub(crate) type Operations =
    HashMap<(LinkId, Option<FunctionId>, ImportSlot), Result<Operation, String>>;

/// Every task of the run and what waits between them.
pub(crate) struct Tasks {
    pub all: Vec<Task>,
    /// The task whose context the machine holds.
    pub running: usize,
    /// Whether a task is installed in the machine's context.
    pub installed: bool,
    pub(crate) ready: VecDeque<usize>,
    pub(crate) waits: BTreeMap<u64, Pending>,
    pub(crate) withdrawn: HashSet<u64>,
    pub(crate) next_wait: u64,
    /// The waits requested since the last park, in request order.
    pub(crate) issued: Vec<u64>,
    pub(crate) cancellations: Vec<WaitCancellation>,
    pub(crate) joins: BTreeMap<u64, ListJoin>,
    pub(crate) next_join: u64,
    /// The tasks that have not ended, `main` included.
    pub live: u32,
    pub live_limit: u32,
    pub requests_limit: u32,
    pub join_limit: u32,
    pub operations: Operations,
    /// How many tasks the run had when its heap last sealed or opened: only
    /// these may have a handle on a page, where the retirement of unreached
    /// results cannot look.
    pub(crate) paged: usize,
}

impl Tasks {
    pub(crate) fn new() -> Self {
        Self {
            all: vec![Task::new(Arc::new(TaskIdentity::Main), None)],
            running: MAIN,
            installed: true,
            ready: VecDeque::new(),
            waits: BTreeMap::new(),
            withdrawn: HashSet::new(),
            next_wait: 0,
            issued: Vec::new(),
            cancellations: Vec::new(),
            joins: BTreeMap::new(),
            next_join: 0,
            live: 1,
            live_limit: u32::MAX,
            requests_limit: u32::MAX,
            join_limit: u32::MAX,
            operations: HashMap::new(),
            paged: 0,
        }
    }

    /// Every value a task holds outside the machine's context.
    pub(crate) fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        for task in &self.all {
            task.cx.roots(&mut *visit);
            match &task.state {
                State::Ready(Resume::Value(value) | Resume::Raise(value)) => visit(value),
                State::Ended(Ok(value) | Err(value)) => visit(value),
                _ => {}
            }
        }
    }

    fn label(&self, task: usize) -> TaskLabel {
        lash_kernel_value::kernel_internals::task_label((*self.all[task].identity).clone())
    }

    fn ended(&self, task: usize) -> Option<&Result<Value, Value>> {
        match &self.all[task].state {
            State::Ended(result) => Some(result),
            _ => None,
        }
    }

    /// Makes `task` ready, at the back of the queue (`K-TASK-005`).
    fn wake(&mut self, task: usize, resume: Resume, turn: bool) {
        let woken = &mut self.all[task];
        woken.state = State::Ready(resume);
        woken.turn = turn;
        self.ready.push_back(task);
    }

    fn remove_joiner(&mut self, task: usize, joiner: Joiner) {
        self.all[task].joiners.retain(|other| *other != joiner);
    }

    fn remove_list_join(&mut self, order: u64) -> Option<ListJoin> {
        let join = self.joins.remove(&order)?;
        for &member in &join.members {
            self.remove_joiner(member, Joiner::List(order));
        }
        Some(join)
    }

    fn pass(&mut self, members: &[usize]) {
        for &member in members {
            self.all[member].passed = true;
        }
    }

    /// The tasks other than `main` and `ender` that make the run's end an
    /// error (`K-TASK-018`), with where each came from.
    pub(crate) fn outstanding(
        &self,
        ender: Option<usize>,
        program: &crate::bytecode::program::Program,
    ) -> Option<RunError> {
        let mut unfinished = Vec::new();
        let mut unobserved = Vec::new();
        let mut sources = Vec::new();
        for (id, task) in self.all.iter().enumerate().skip(1) {
            if ender == Some(id) || task.passed {
                continue;
            }
            match &task.state {
                State::Ended(Err(_)) if !task.observed => unobserved.push(self.label(id)),
                State::Ended(_) => continue,
                _ => unfinished.push(self.label(id)),
            }
            let body = task
                .cx
                .frames
                .iter()
                .map(|frame| program.unit(frame.unit))
                .find(|unit| unit.authored)
                .map(|unit| unit.site.clone());
            if let Some(site) = body.or_else(|| task.spawn_origin.clone()) {
                sources.push((self.label(id), site.position()));
            }
        }
        if unfinished.is_empty() && unobserved.is_empty() {
            return None;
        }
        Some(RunError::TasksOutstanding {
            unfinished,
            unobserved,
            sources,
        })
    }

    fn count_outstanding(&self) -> u64 {
        self.all
            .iter()
            .skip(1)
            .filter(|task| {
                !task.passed
                    && match &task.state {
                        State::Ended(Err(_)) => !task.observed,
                        State::Ended(Ok(_)) => false,
                        _ => true,
                    }
            })
            .count() as u64
    }

    /// The handles of every task that has not ended but `caller`, in spawn
    /// order (`K-TASK-021`).
    pub(crate) fn unfinished(&self) -> Vec<Value> {
        (0..self.all.len())
            .filter(|&id| id != self.running && self.ended(id).is_none())
            .map(|id| Value::Task(TaskId(id as u64)))
            .collect()
    }

    /// Commits a delivery's decoded outcome (`K-MACH-004`).
    pub(crate) fn deliver(
        &mut self,
        wait: u64,
        outcome: crate::Outcome,
    ) -> Result<crate::Delivered, crate::DeliverError> {
        let id = lash_kernel_doc::kernel_internals::wait_id(wait);
        let Some(pending) = self.waits.get_mut(&wait) else {
            return if self.withdrawn.contains(&wait) {
                Ok(crate::Delivered::Dropped)
            } else if wait < self.next_wait {
                Err(crate::DeliverError::AlreadyDelivered { wait: id })
            } else {
                Err(crate::DeliverError::UnknownWait { wait: id })
            };
        };
        match pending.phase {
            Phase::Issued => return Err(crate::DeliverError::UnknownWait { wait: id }),
            Phase::Delivered(_) => return Err(crate::DeliverError::AlreadyDelivered { wait: id }),
            Phase::Admitted => {}
        }
        pending.phase = Phase::Delivered(outcome);
        let (task, turn) = (pending.task, pending.then_yield);
        self.wake(task, Resume::Outcome(wait), turn);
        Ok(crate::Delivered::Accepted)
    }
}

impl Task {
    pub(crate) fn new(identity: Arc<TaskIdentity>, spawn_origin: Option<Site>) -> Self {
        Self {
            identity,
            cx: Context::default(),
            state: State::Running,
            turn: false,
            occurrences: Vec::new(),
            library: HashMap::new(),
            spawn_origin,
            passed: false,
            observed: false,
            joiners: Vec::new(),
        }
    }

    /// Counts a run of the site of `mark` by a frame running as Link
    /// `link`, giving how many came before.
    fn advance(&mut self, mark: &Mark, link: u32) -> Run<u64> {
        if mark.occurrence & LIB == 0 {
            return self.count(mark.occurrence);
        }
        let key = crate::bytecode::program::Program::counter(link, mark.occurrence);
        let count = self.library.entry(key).or_insert(0);
        let before = *count;
        *count = next_occurrence(before)?;
        Ok(before)
    }
}

impl Task {
    /// Counts an entry into document site `occurrence`, giving how many
    /// came before.
    #[inline]
    fn count(&mut self, occurrence: u32) -> Run<u64> {
        let at = occurrence as usize;
        match self.occurrences.get_mut(at) {
            Some(count) => {
                let before = *count;
                *count = next_occurrence(before)?;
                Ok(before)
            }
            None => {
                self.occurrences.resize(at + 1, 0);
                self.occurrences[at] = 1;
                Ok(0)
            }
        }
    }
}

/// The counter after `count`: an exhausted counter is a machine fault,
/// never a wrapped identity (`K-EFF-008`).
#[inline]
fn next_occurrence(count: u64) -> Run<u64> {
    count
        .checked_add(1)
        .ok_or_else(|| super::fault("occurrence counter overflow"))
}

const CANCELLED: (&str, &str) = ("cancelled", "the task was cancelled");

impl<H: Heap> Vm<H> {
    /// Retires the result of every ended task no handle reaches, before a
    /// collection (`K-TASK-009`, `K-TASK-018`, S7): no `join` can read it
    /// again, so it is no longer a root. The task keeps how it ended, Ok or
    /// raised, for the run's end. Handles are traced from every root but
    /// the ended results themselves, then through the results of the tasks
    /// so reached. A handle on a page is not visible: where the trace meets
    /// a cold object, every task the run had at its last seal or open
    /// counts as reached.
    pub(crate) fn retire_unreached(&mut self) {
        let holds = |task: &Task| matches!(&task.state, State::Ended(Ok(value) | Err(value)) if value.handle().is_some());
        if !self.tasks.all.iter().skip(1).any(holds) {
            return;
        }
        let count = self.tasks.all.len();
        let mut reach = Reach {
            reached: vec![false; count],
            pending: Vec::new(),
            tasks: Vec::new(),
        };
        {
            let mut visit = |value: &Value| reach.value(value);
            self.constants.iter().for_each(&mut visit);
            self.globals.iter().for_each(&mut visit);
            self.top.iter().flatten().for_each(&mut visit);
            self.held.iter().for_each(&mut visit);
            self.cx.roots(&mut visit);
            if let Some(restart) = &self.restart {
                restart.roots(&mut visit);
            }
        }
        for task in &self.tasks.all {
            task.cx.roots(&mut |value| reach.value(value));
            match &task.state {
                State::Ready(Resume::Value(value) | Resume::Raise(value)) => reach.value(value),
                State::Ready(Resume::Joined(joined)) | State::Waiting(Wait::Join(joined)) => {
                    reach.task(*joined);
                }
                _ => {}
            }
        }
        for join in self.tasks.joins.values() {
            join.members.iter().for_each(|&member| reach.task(member));
        }
        let arena = self.heap.arena();
        let mut seen = HashSet::new();
        let mut cold = false;
        loop {
            while let Some(handle) = reach.pending.pop() {
                if !seen.insert(handle.index()) {
                    continue;
                }
                let (Ok(body), Ok(facts)) = (arena.body(handle), arena.facts(handle)) else {
                    continue;
                };
                cold |= facts.cold
                    || match body {
                        Body::Cold(_) => true,
                        Body::List(sequence) | Body::Tuple(sequence) => {
                            sequence.chunks.first_cold().is_some()
                        }
                        Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                            table.chunks.first_cold().is_some()
                        }
                        _ => false,
                    };
                lash_kernel_state::arena::each_member(body, &mut |value| reach.value(value));
                if let Body::Closure { captures, .. } = body {
                    reach.pending.extend(captures.iter().copied());
                }
            }
            let Some(task) = reach.tasks.pop() else {
                break;
            };
            if let State::Ended(Ok(value) | Err(value)) = &self.tasks.all[task].state {
                reach.value(value);
            }
        }
        let kept = if cold { self.tasks.paged } else { 0 };
        for (id, task) in self.tasks.all.iter_mut().enumerate().skip(kept.max(1)) {
            if reach.reached[id] {
                continue;
            }
            if let State::Ended(Ok(value) | Err(value)) = &mut task.state {
                *value = Value::Null;
            }
        }
    }

    // --- Where a task stands.

    /// Where frame `at` of `cx` stands: at the call it made, or at the
    /// action the top frame runs.
    fn standing<'p>(&'p self, cx: &Context, at: usize) -> Option<&'p Mark> {
        let frame = cx.frames.get(at)?;
        let unit = self.program.unit(frame.unit);
        match unit.original((frame.pc as usize).checked_sub(1)?)? {
            Op::Call(call) => Some(&unit.calls[call as usize].mark),
            Op::Action(action) => Some(&unit.action_marks[action as usize]),
            _ => None,
        }
    }

    /// The loops the running task is inside, outermost first across its
    /// frames, each with its iteration (`K-EFF-008`).
    fn loops(&self) -> Vec<LoopIteration> {
        let mut loops = Vec::new();
        for at in 0..self.cx.frames.len() {
            let Some(mark) = self.standing(&self.cx, at) else {
                continue;
            };
            let base = self.cx.frames[at].base as usize;
            for enclosing in mark.loops.iter() {
                let started = match self.cx.stack[base + usize::from(enclosing.started)] {
                    Value::Int(count) => count.max(0) as u64,
                    _ => 0,
                };
                loops.push(LoopIteration {
                    site: enclosing.site.clone(),
                    iteration: started.saturating_sub(1),
                });
            }
        }
        loops
    }

    /// The running task's origin (`K-EFF-008`): the statement its topmost
    /// document frame stands at, else where its spawner stood, else where
    /// its root frame stands.
    fn task_origin(&self) -> Site {
        let frames = self.cx.frames.len();
        for at in (0..frames).rev() {
            let unit = self.program.unit(self.cx.frames[at].unit);
            if unit.authored
                && let Some(mark) = self.standing(&self.cx, at)
            {
                return mark.statement.clone();
            }
        }
        let task = &self.tasks.all[self.tasks.running];
        if let Some(origin) = &task.spawn_origin {
            return origin.clone();
        }
        match self.standing(&self.cx, 0) {
            Some(mark) => mark.statement.clone(),
            None => self
                .cx
                .frames
                .first()
                .map(|frame| self.program.unit(frame.unit).site.clone())
                .unwrap_or_else(|| Site::new(lash_kernel_value::sites::Unit::Main, Vec::new())),
        }
    }

    /// The origin of an action at `site` of `unit`: its own site in
    /// document code.
    fn origin(&self, unit: &Unit, site: &Site) -> Site {
        if unit.authored {
            site.clone()
        } else {
            self.task_origin()
        }
    }

    /// Counts the running task's entry into the authored call or action at
    /// `mark` and `site` of `unit` (`K-EFF-008`), and tells a host that
    /// observes sites where it stands. Library code enters no site.
    pub(super) fn entered(
        &mut self,
        host: &mut dyn Host,
        unit: &Unit,
        mark: &Mark,
        site: &Site,
    ) -> Run<()> {
        if !unit.authored {
            return Ok(());
        }
        let link = self.running_link();
        let occurrence = self.running().advance(mark, link)?;
        if self.sites {
            self.tell_entry(host, site, occurrence);
        }
        Ok(())
    }

    /// Counts the running task's entry into authored call `call`, whose
    /// counter is `entry` (`K-EFF-008`), and tells a host that observes
    /// sites where it stands, the frame's `pc` past the call. The call op
    /// counts before the call reads anything, so a call its arguments
    /// refuse is entered too.
    #[inline]
    pub(super) fn enter_call(
        &mut self,
        host: &mut dyn Host,
        entry: u32,
        call: &CallSite,
    ) -> Run<()> {
        let occurrence = self.running().count(entry)?;
        if self.sites {
            self.tell_entry(host, &call.site, occurrence);
        }
        Ok(())
    }

    /// Hands an observing host the running task's entry into `site`, the
    /// `occurrence`th (`K-EFF-008`).
    #[inline(never)]
    fn tell_entry(&self, host: &mut dyn Host, site: &Site, occurrence: u64) {
        let identity = EffectIdentity {
            task: (*self.tasks.all[self.tasks.running].identity).clone(),
            site: site.clone(),
            occurrence,
            loops: self.loops(),
        };
        host.enter_site(&lash_kernel_value::sites::EffectLocation {
            at: lash_kernel_value::kernel_internals::effect_id(identity),
            origin: site.clone(),
        });
    }

    fn running(&mut self) -> &mut Task {
        let running = self.tasks.running;
        &mut self.tasks.all[running]
    }

    // --- Switching.

    /// The running task leaves the machine in `state`.
    fn suspend(&mut self, state: State) {
        // An inline region reaches no action, step or slice point.
        debug_assert!(
            self.cx.inline.is_none(),
            "a task suspends inside an inline region"
        );
        let cx = std::mem::take(&mut self.cx);
        let task = self.running();
        task.cx = cx;
        task.state = state;
        self.tasks.installed = false;
    }

    /// After a step that ended the running task's turn: the slice ends
    /// while another task is ready, else the next one runs
    /// (`K-MACH-002`).
    pub(super) fn next_turn(&mut self, host: &mut dyn Host) -> Run<Option<Stop>> {
        if self.meter.spent() && !self.tasks.ready.is_empty() {
            return Ok(Some(Stop::Slice));
        }
        self.schedule(host)
    }

    /// Installs the front of the ready queue and hands it what it resumes
    /// with, or gives the park or the deadlock when none is ready.
    pub(super) fn schedule(&mut self, host: &mut dyn Host) -> Run<Option<Stop>> {
        loop {
            let Some(next) = self.tasks.ready.pop_front() else {
                return Ok(Some(self.idle(host)));
            };
            let task = &mut self.tasks.all[next];
            if std::mem::take(&mut task.turn) {
                // A delivered `then yield` takes its turn, charging nothing.
                self.tasks.ready.push_back(next);
                continue;
            }
            let State::Ready(resume) = std::mem::replace(&mut task.state, State::Running) else {
                return Err(fault("a queued task is not ready"));
            };
            self.cx = std::mem::take(&mut task.cx);
            self.tasks.running = next;
            self.tasks.installed = true;
            self.apply(resume)?;
            return Ok(None);
        }
    }

    fn apply(&mut self, resume: Resume) -> Run<()> {
        match resume {
            Resume::Continue => Ok(()),
            Resume::Value(value) => {
                self.push(value);
                Ok(())
            }
            Resume::Raise(value) => Err(Flow::Raise(value)),
            Resume::Joined(joined) => self.joined(joined),
            Resume::Outcome(wait) => {
                let Some(pending) = self.tasks.waits.remove(&wait) else {
                    return Err(fault("a resuming outcome has no wait"));
                };
                let Phase::Delivered(outcome) = pending.phase else {
                    return Err(fault("a resuming outcome is not delivered"));
                };
                self.heap.release(crate::heap::meter::outcome(&outcome));
                // The raw outcome is decoded here, once: a failure by any
                // type, a result by the `perform`'s; a result that does not
                // fit raises its refusal, uncharged.
                let (raised, raw, ty) = match outcome {
                    crate::Outcome::Completed(datum) => (false, datum, pending.result),
                    crate::Outcome::Failed(error) => {
                        (true, Datum::Error(Box::new(error)), Type::Any)
                    }
                };
                match decode(
                    &self.document,
                    &|c| self.callable(c),
                    self.value_depth,
                    &raw,
                    &ty,
                ) {
                    Ok(datum) if raised => Err(Flow::Raise(self.decoded(&datum)?)),
                    Ok(datum) => {
                        let value = self.decoded(&datum)?;
                        self.push(value);
                        Ok(())
                    }
                    Err(refusal) => Err(Flow::Raise(super::datum::copy_in(self, &refusal)?)),
                }
            }
        }
    }

    /// Builds a decoded outcome and charges its deep size, a boundary
    /// charge in whatever code the `perform` stands (`K-CHG-002`).
    fn decoded(&mut self, datum: &Datum) -> Run<Value> {
        let value = super::datum::copy_in(self, datum)?;
        if !self.off {
            #[cfg(test)]
            super::charge_work();
            let size = crate::data::deep_size(self.heap.arena(), &value).map_err(super::read)?;
            self.held.push(value);
            let charged = self.fee(size);
            self.held.pop();
            charged?;
        }
        Ok(value)
    }

    /// What a `join` on ended task `joined` gives, or raises
    /// (`K-TASK-009`).
    fn joined(&mut self, joined: usize) -> Run<()> {
        match self.tasks.ended(joined) {
            Some(Ok(value)) => {
                let value = *value;
                self.push(value);
                Ok(())
            }
            Some(Err(error)) => {
                let error = *error;
                self.tasks.all[joined].observed = true;
                Err(Flow::Raise(error))
            }
            None => Err(fault("a join resumed before its task ended")),
        }
    }

    /// No task is ready: the run parks, or nothing can wake it
    /// (`K-TASK-023`, `K-TASK-024`, `K-MACH-006`).
    fn idle(&mut self, host: &mut dyn Host) -> Stop {
        if host.cancel_requested() {
            return Stop::Cancelled;
        }
        if self.tasks.waits.is_empty() && self.tasks.cancellations.is_empty() {
            let waiting = (0..self.tasks.all.len())
                .filter(|&id| self.tasks.ended(id).is_none())
                .map(|id| self.tasks.label(id))
                .collect();
            return Stop::Deadlock(waiting);
        }
        self.park()
    }

    fn park(&mut self) -> Stop {
        let mut requests = Vec::new();
        for wait in std::mem::take(&mut self.tasks.issued) {
            if let Some(pending) = self.tasks.waits.get_mut(&wait) {
                pending.phase = Phase::Admitted;
                requests.extend(pending.request.take().map(Request::Effect));
            }
        }
        Stop::Park(Park {
            requests,
            cancel_requests: std::mem::take(&mut self.tasks.cancellations),
            outstanding: self.tasks.count_outstanding(),
            live: u64::from(self.tasks.live.saturating_sub(1)),
        })
    }

    /// The running task, not `main`, ends: what waits on it wakes in the
    /// order it began (`K-TASK-008`).
    pub(super) fn end_task(
        &mut self,
        host: &mut dyn Host,
        result: Result<Value, Value>,
    ) -> Run<Option<Stop>> {
        let ended = self.tasks.running;
        self.cx = Context::default();
        let task = &mut self.tasks.all[ended];
        task.state = State::Ended(result);
        task.cx = Context::default();
        let joiners = std::mem::take(&mut task.joiners);
        self.tasks.installed = false;
        self.tasks.live = self.tasks.live.saturating_sub(1);
        for joiner in joiners {
            match joiner {
                Joiner::Single(joiner) => self.tasks.wake(joiner, Resume::Joined(ended), false),
                Joiner::List(order) => {
                    let Some(join) = self.tasks.joins.get(&order) else {
                        continue;
                    };
                    let (mode, members, joiner) = (join.mode, join.members.clone(), join.joiner);
                    if let Some(decision) = self.decide(mode, &members, Some(ended))? {
                        self.tasks.remove_list_join(order);
                        self.tasks.pass(&members);
                        let resume = match decision {
                            Ok(value) => Resume::Value(value),
                            Err(error) => Resume::Raise(error),
                        };
                        self.tasks.wake(joiner, resume, false);
                    }
                }
            }
        }
        self.next_turn(host)
    }

    // --- Actions.

    /// Action `n` of `unit`, its operands on the stack: what the running
    /// task's step ends with (`K-MACH-002`). An action run again after a
    /// cold load (`rerun`) has counted its entry.
    pub(super) fn act(
        &mut self,
        host: &mut dyn Host,
        unit: &Unit,
        n: u32,
        rerun: bool,
    ) -> Run<Option<Stop>> {
        let mark = Arc::clone(&unit.action_marks[n as usize]);
        match &unit.actions[n as usize] {
            ActionSite::Join { site }
            | ActionSite::JoinMany { site, .. }
            | ActionSite::Yield { site }
            | ActionSite::Cancel { site }
            | ActionSite::CancelEffect { site }
                if !rerun =>
            {
                self.entered(host, unit, &mark, site)?
            }
            _ => {}
        }
        match &unit.actions[n as usize] {
            ActionSite::Perform {
                owner,
                slot,
                argc,
                result,
                then_yield,
                site,
            } => {
                let link = self.program.links[self.running_link() as usize];
                let operation = match self.tasks.operations.get(&(link, *owner, *slot)) {
                    Some(Ok(operation)) => operation.clone(),
                    Some(Err(problem)) => return Err(fault(problem.clone())),
                    None => return Err(fault("a perform whose operation was never resolved")),
                };
                // Every argument is read before the first is billed: a cold
                // read runs the action again with no fee billed twice. Each
                // copy then bills in order, up to the first that raises.
                let from = self.cx.stack.len() - usize::from(*argc);
                let mut copies = Vec::with_capacity(usize::from(*argc));
                for at in from..self.cx.stack.len() {
                    let copied = self.crossed(&operation.use_, at - from, &self.cx.stack[at])?;
                    let raises = copied.raises();
                    copies.push(copied);
                    if raises {
                        break;
                    }
                }
                let mut args = Vec::with_capacity(usize::from(*argc));
                for copied in copies {
                    args.push(self.bill_copy(copied)?);
                }
                self.cx.stack.truncate(from);
                if operation.read {
                    self.read(host, unit, &operation.use_, &args, site, result)?;
                    return self.went_on();
                }
                let limit = self.tasks.requests_limit;
                if self.tasks.issued.len() as u64 >= u64::from(limit) {
                    return Err(Flow::from(Stop::bound(Exceeded::run(
                        Bound::RequestsPerPark,
                        u64::from(limit),
                    ))));
                }
                let link = self.running_link();
                let occurrence = self.running().advance(&mark, link)?;
                let wait = self.tasks.next_wait;
                self.tasks.next_wait += 1;
                let identity = EffectIdentity {
                    task: (*self.tasks.all[self.tasks.running].identity).clone(),
                    site: site.clone(),
                    occurrence,
                    loops: self.loops(),
                };
                let request = EffectRequest {
                    wait: lash_kernel_doc::kernel_internals::wait_id(wait),
                    identity: lash_kernel_value::kernel_internals::effect_id(identity),
                    use_: operation.use_,
                    args,
                    result: result.clone(),
                    origin: self.origin(unit, site),
                };
                self.tasks.waits.insert(
                    wait,
                    Pending {
                        task: self.tasks.running,
                        phase: Phase::Issued,
                        result: result.clone(),
                        then_yield: *then_yield,
                        request: Some(request),
                    },
                );
                self.tasks.issued.push(wait);
                self.suspend(State::Waiting(Wait::Effect(wait)));
                self.next_turn(host)
            }
            ActionSite::Spawn {
                unit: stub,
                argc,
                site,
            } => self.spawn(unit, &mark, *stub, usize::from(*argc), site),
            ActionSite::Join { .. } => {
                let handle = self.pop();
                let joined = self.handle(handle, "`join` takes a task handle")?;
                if joined == self.tasks.running {
                    return Err(self.error("join_self", "a task cannot join its own handle"));
                }
                if self.tasks.ended(joined).is_some() {
                    self.joined(joined)?;
                    return self.went_on();
                }
                let running = self.tasks.running;
                self.tasks.all[joined].joiners.push(Joiner::Single(running));
                self.suspend(State::Waiting(Wait::Join(joined)));
                self.next_turn(host)
            }
            ActionSite::JoinMany { mode, .. } => {
                let [handles] = self.operands();
                self.join_many(host, *mode, handles)
            }
            ActionSite::Yield { .. } => {
                let running = self.tasks.running;
                self.suspend(State::Ready(Resume::Value(Value::Null)));
                self.tasks.ready.push_back(running);
                self.next_turn(host)
            }
            ActionSite::Cancel { .. } => {
                let handle = self.pop();
                let target = self.handle(handle, "`cancel` takes a task handle")?;
                if target == self.tasks.running {
                    return Err(self.error(CANCELLED.0, CANCELLED.1));
                }
                self.cancel_task(target)?;
                self.push(Value::Null);
                self.quiet_point(host)
            }
            ActionSite::CancelEffect { .. } => {
                let handle = self.pop();
                let target = self.handle(handle, "`cancel` takes a task handle")?;
                self.cancel_effect(target)?;
                self.push(Value::Null);
                self.quiet_point(host)
            }
        }
    }

    /// The running task goes on after an action that completed at once;
    /// the slice is tested at the step's end.
    fn went_on(&mut self) -> Run<Option<Stop>> {
        Ok(self.meter.spent().then_some(Stop::Slice))
    }

    /// A call-cancel request forces a park before any other guest
    /// operation (`K-MACH-003`); the running task goes on first after it.
    fn quiet_point(&mut self, host: &mut dyn Host) -> Run<Option<Stop>> {
        if self.tasks.cancellations.is_empty() {
            return self.went_on();
        }
        let running = self.tasks.running;
        self.suspend(State::Ready(Resume::Continue));
        self.tasks.ready.push_front(running);
        if host.cancel_requested() {
            return Ok(Some(Stop::Cancelled));
        }
        Ok(Some(self.park()))
    }

    fn handle(&mut self, value: Value, problem: &str) -> Run<usize> {
        match value {
            Value::Task(TaskId(id)) if (id as usize) < self.tasks.all.len() => Ok(id as usize),
            _ => Err(self.error("type_error", problem)),
        }
    }

    /// A read-bound `perform`, answered by the host inside the statement
    /// (`K-EFF-012`).
    fn read(
        &mut self,
        host: &mut dyn Host,
        unit: &Unit,
        use_: &OperationUse,
        args: &[Datum],
        site: &Site,
        result: &Type,
    ) -> Run<()> {
        let origin = self.origin(unit, site);
        self.report(host);
        match host.perform(use_, args, &site.position(), &origin) {
            Ok(Ok(answer)) => {
                let value = match self.decode(&answer, result) {
                    Ok(datum) => self.decoded(&datum)?,
                    Err(refusal) => {
                        return Err(Flow::Raise(super::datum::copy_in(self, &refusal)?));
                    }
                };
                self.push(value);
                Ok(())
            }
            Ok(Err(error)) => {
                let datum = Datum::Error(Box::new(error));
                match self.decode(&datum, &Type::Any) {
                    Ok(datum) => Err(Flow::Raise(self.decoded(&datum)?)),
                    Err(refusal) => Err(Flow::Raise(super::datum::copy_in(self, &refusal)?)),
                }
            }
            Err(problem) => Err(Flow::from(Stop::Host(problem))),
        }
    }

    /// Decodes `datum` by `ty`, or gives the error its refusal raises
    /// (`K-EFF-005`, `K-EFF-007`).
    pub(crate) fn decode(&self, datum: &Datum, ty: &Type) -> Result<Datum, Datum> {
        decode(
            &self.document,
            &|c| self.callable(c),
            self.value_depth,
            datum,
            ty,
        )
    }

    /// `spawn`: the new task runs at once, and the spawner goes on when it
    /// first waits or ends, before any other ready task (`K-TASK-002`).
    fn spawn(
        &mut self,
        unit: &Unit,
        mark: &Mark,
        stub: UnitId,
        argc: usize,
        site: &Site,
    ) -> Run<Option<Stop>> {
        let limit = self.tasks.live_limit;
        if self.tasks.live >= limit {
            return Err(Flow::from(Stop::bound(Exceeded::run(
                Bound::LiveTasks,
                u64::from(limit),
            ))));
        }
        // The callee is read before the occurrence advances: a cold read
        // runs the spawn again with nothing advanced.
        let from = self.cx.stack.len() - argc;
        let rooted_in_document = self.callee_authored(stub, self.cx.stack.get(from).copied())?;
        let link = self.running_link();
        let occurrence = self.running().advance(mark, link)?;
        let spawn_origin = (!unit.authored || !rooted_in_document).then(|| self.task_origin());
        let parent = Arc::clone(&self.tasks.all[self.tasks.running].identity);
        let identity = Arc::new(TaskIdentity::Spawned(SpawnIdentity {
            parent,
            site: site.clone(),
            occurrence,
        }));
        let child = self.tasks.all.len();
        self.tasks.all.push(Task::new(identity, spawn_origin));
        self.tasks.live += 1;
        let values = self.take(argc);
        let spawner = self.tasks.running;
        self.suspend(State::Ready(Resume::Value(Value::Task(TaskId(
            child as u64,
        )))));
        self.tasks.ready.push_front(spawner);
        self.tasks.running = child;
        self.tasks.installed = true;
        self.cx.stack.extend_from_slice(&values);
        self.enter(stub, argc, None, None, link)?;
        Ok(None)
    }

    /// Whether the code a spawn stub calls is document code.
    fn callee_authored(&self, stub: UnitId, value: Option<Value>) -> Run<bool> {
        let program = &self.program;
        let Some(call) = program.unit(stub).calls.first() else {
            return Ok(false);
        };
        let unit = match call.callee {
            Callee::Unit { unit, .. } => Some(unit),
            Callee::Value => match value {
                Some(Value::Closure(handle)) => found(self.heap.arena().closure(handle))?
                    .and_then(|(identity, _, _)| self.closure_unit(&identity.callable)),
                Some(Value::Function(handle)) => found(self.heap.arena().callable(handle))?
                    .and_then(|callable| self.function_unit(&callable))
                    .and_then(|trampoline| program.unit(trampoline).calls.first())
                    .and_then(|call| match call.callee {
                        Callee::Unit { unit, .. } => Some(unit),
                        _ => None,
                    }),
                _ => None,
            },
            Callee::Native { .. } | Callee::Machine { .. } => None,
        };
        Ok(unit.is_some_and(|unit| program.unit(unit).authored))
    }

    /// A list `join` in `mode` (`K-TASK-010` … `K-TASK-015`).
    fn join_many(
        &mut self,
        host: &mut dyn Host,
        mode: JoinMode,
        handles: Value,
    ) -> Run<Option<Stop>> {
        const NOT_HANDLES: &str = "`join` takes a list or a tuple of task handles";
        let (Value::List(items) | Value::Tuple(items)) = handles else {
            return Err(self.error("type_error", NOT_HANDLES));
        };
        let len = self
            .heap
            .arena()
            .len(items)
            .map_err(|error| self.native(error))?;
        let limit = u64::from(self.tasks.join_limit);
        if len > limit {
            return Err(Flow::from(Stop::bound(Exceeded::run(
                Bound::JoinMembers,
                limit,
            ))));
        }
        let mut members = Vec::with_capacity(len as usize);
        for at in 0..len {
            let member = self
                .heap
                .arena()
                .item(items, at)
                .map_err(|error| self.native(error))?;
            let Some(Value::Task(TaskId(member))) = member else {
                return Err(self.error("type_error", NOT_HANDLES));
            };
            let member = member as usize;
            if member >= self.tasks.all.len() {
                return Err(self.error("type_error", NOT_HANDLES));
            }
            if member == self.tasks.running {
                return Err(self.error("join_self", "a task cannot join its own handle"));
            }
            members.push(member);
        }
        // The handles are read: from here the join commits.
        self.popped(1);
        if len == 0 && matches!(mode, JoinMode::Race | JoinMode::Any) {
            return Err(self.error("empty_join", "this `join` needs at least one task"));
        }
        if let Some(decision) = self.decide(mode, &members, None)? {
            self.tasks.pass(&members);
            match decision {
                Ok(value) => self.push(value),
                Err(error) => return Err(Flow::Raise(error)),
            }
            return self.went_on();
        }
        let order = self.tasks.next_join;
        self.tasks.next_join += 1;
        // Repeated handles keep their places in the result, but register
        // one wake on each live member.
        let mut seen = HashSet::new();
        for &member in &members {
            if seen.insert(member) && self.tasks.ended(member).is_none() {
                self.tasks.all[member].joiners.push(Joiner::List(order));
            }
        }
        let joiner = self.tasks.running;
        self.tasks.joins.insert(
            order,
            ListJoin {
                joiner,
                mode,
                members,
            },
        );
        self.suspend(State::Waiting(Wait::JoinMany(order)));
        self.next_turn(host)
    }

    /// Whether a list `join` is decided, and how (`K-TASK-011` …
    /// `K-TASK-014`). `ended` is the member that has just ended; `None`
    /// is a `join` that is starting, which reads its members in list
    /// order.
    fn decide(
        &mut self,
        mode: JoinMode,
        members: &[usize],
        ended: Option<usize>,
    ) -> Run<Option<Result<Value, Value>>> {
        let immediate = |result: &Result<Value, Value>| match mode {
            JoinMode::Race => true,
            JoinMode::Any => result.is_ok(),
            JoinMode::All => result.is_err(),
            JoinMode::AllSettled => false,
        };
        if let Some(ended) = ended
            && let Some(result) = self.tasks.ended(ended)
            && immediate(result)
        {
            return Ok(Some(*result));
        }
        let mut results = Vec::with_capacity(members.len());
        let mut complete = true;
        for &member in members {
            let result = self.tasks.ended(member).copied();
            if (ended.is_none() || matches!(mode, JoinMode::All | JoinMode::Any))
                && let Some(result) = &result
                && immediate(result)
            {
                return Ok(Some(*result));
            }
            match result {
                Some(result) => results.push(result),
                None => complete = false,
            }
        }
        if !complete {
            return Ok(None);
        }
        Ok(match mode {
            JoinMode::All => {
                let values = results.into_iter().filter_map(Result::ok).collect();
                Some(Ok(self.alloc(NewObject::List(values))?))
            }
            JoinMode::AllSettled => Some(Ok(self.settled(&results)?)),
            JoinMode::Race => None,
            JoinMode::Any if results.is_empty() => None,
            JoinMode::Any => {
                let errors = results.into_iter().filter_map(Result::err).collect();
                let errors = self.alloc(NewObject::List(errors))?;
                self.held.push(errors);
                let error =
                    self.new_error("all_failed", "every task of the `join any` failed", errors);
                self.held.pop();
                Some(Err(error?))
            }
        })
    }

    /// `join settled`'s list of status records (`K-TASK-012`).
    fn settled(&mut self, results: &[Result<Value, Value>]) -> Run<Value> {
        let from = self.held.len();
        for result in results {
            let (status, key, value) = match result {
                Ok(value) => ("ok", "value", *value),
                Err(error) => ("error", "error", *error),
            };
            let arena = self.heap.arena_mut();
            let fields = vec![
                (arena.text("status"), arena.text(status)),
                (arena.text(key), value),
            ];
            let record = self.alloc(NewObject::Record(fields));
            match record {
                Ok(record) => self.held.push(record),
                Err(flow) => {
                    self.held.truncate(from);
                    return Err(flow);
                }
            }
        }
        let records = self.held[from..].to_vec();
        let list = self.alloc(NewObject::List(records));
        self.held.truncate(from);
        list
    }

    /// `cancel`: raises `cancelled` in a task at its wait (`K-TASK-017`).
    fn cancel_task(&mut self, target: usize) -> Run<()> {
        let wait = match &self.tasks.all[target].state {
            State::Ended(_) => return Ok(()),
            State::Running => return Err(fault("a task other than the running one runs")),
            State::Ready(_) => None,
            State::Waiting(wait) => Some(*wait),
        };
        let cancelled = self.error_value(CANCELLED.0, CANCELLED.1)?;
        let Some(wait) = wait else {
            // The raise replaces what a ready task resumes with; a
            // delivered one still takes its turn first (`K-TASK-005`).
            self.tasks.all[target].state = State::Ready(Resume::Raise(cancelled));
            return Ok(());
        };
        let mut turn = false;
        match wait {
            Wait::Effect(wait) => {
                if let Some(pending) = self.tasks.waits.remove(&wait) {
                    turn = pending.then_yield;
                    match pending.phase {
                        Phase::Issued => self.tasks.issued.retain(|issued| *issued != wait),
                        phase => {
                            if let Phase::Delivered(outcome) = &phase {
                                self.heap.release(crate::heap::meter::outcome(outcome));
                            }
                            self.tasks.cancellations.push(WaitCancellation::Withdraw(
                                lash_kernel_doc::kernel_internals::wait_id(wait),
                            ));
                            self.tasks.withdrawn.insert(wait);
                        }
                    }
                }
            }
            Wait::Join(joined) => self.tasks.remove_joiner(joined, Joiner::Single(target)),
            Wait::JoinMany(order) => {
                // The join raises `cancelled`, so its members have been
                // through a join that raised.
                if let Some(join) = self.tasks.remove_list_join(order) {
                    self.tasks.pass(&join.members);
                }
            }
        }
        self.tasks.wake(target, Resume::Raise(cancelled), turn);
        Ok(())
    }

    /// Asks for the cancellation of a task's owned effect without ending
    /// its wait; a committed outcome stays observable (`K-MACH-003`).
    fn cancel_effect(&mut self, target: usize) -> Run<()> {
        let State::Waiting(Wait::Effect(wait)) = self.tasks.all[target].state else {
            return Ok(());
        };
        let Some(pending) = self.tasks.waits.get(&wait) else {
            return Ok(());
        };
        match pending.phase {
            Phase::Delivered(_) => return Ok(()),
            Phase::Issued => {
                // No embedder has it: the local request is refused.
                let turn = pending.then_yield;
                self.tasks.waits.remove(&wait);
                self.tasks.issued.retain(|issued| *issued != wait);
                self.tasks.withdrawn.insert(wait);
                let cancelled = self.error_value(CANCELLED.0, CANCELLED.1)?;
                self.tasks.wake(target, Resume::Raise(cancelled), turn);
            }
            Phase::Admitted => {}
        }
        self.tasks.cancellations.push(WaitCancellation::Observe(
            lash_kernel_doc::kernel_internals::wait_id(wait),
        ));
        Ok(())
    }

    /// A new error value, admitted.
    fn error_value(&mut self, kind: &str, message: &str) -> Run<Value> {
        match self.error(kind, message) {
            Flow::Raise(value) => Ok(value),
            flow => Err(flow),
        }
    }
}

/// What a read found: only a cold read stops the answer; any other failure
/// finds nothing.
fn found<T>(found: Result<T, lash_kernel_doc::NativeError>) -> Run<Option<T>> {
    match found {
        Ok(found) => Ok(Some(found)),
        Err(error) => match super::read(error) {
            cold @ Flow::Cold { .. } => Err(cold),
            _ => Ok(None),
        },
    }
}

/// Decodes `datum` by `ty` under `document`'s number policy, or gives the
/// error datum its refusal raises. A function value is admitted only when
/// `callable` holds it: by its Link and function.
pub(crate) fn decode(
    document: &lash_kernel_doc::Document,
    callable: &dyn Fn(&lash_kernel_value::CallableId) -> bool,
    max_depth: usize,
    datum: &Datum,
    ty: &Type,
) -> Result<Datum, Datum> {
    let decoder = lash_kernel_doc::Decoder {
        max_depth,
        policy: document.numbers,
        declared: callable,
    };
    decoder
        .decode(datum, ty)
        .map_err(|refusal| match refusal.error_datum() {
            Ok(error) => Datum::Error(Box::new(error)),
            Err(error) => Datum::Error(Box::new(lash_kernel_doc::ErrorDatum::new(
                "decode_refused".into(),
                error.to_string(),
                Datum::Null,
            ))),
        })
}

/// The `host` fault a read raised, for the machine.
pub(crate) type ReadFault = HostFault;

/// The tasks a retirement trace has reached, and what it has still to visit.
struct Reach {
    reached: Vec<bool>,
    pending: Vec<lash_kernel_doc::arena::ArenaHandle>,
    tasks: Vec<usize>,
}

impl Reach {
    fn value(&mut self, value: &Value) {
        match value {
            Value::Task(TaskId(id)) | Value::TaskRef(TaskId(id)) => self.task(*id as usize),
            _ => self.pending.extend(value.handle()),
        }
    }

    fn task(&mut self, task: usize) {
        if let Some(reached) = self.reached.get_mut(task)
            && !*reached
        {
            *reached = true;
            self.tasks.push(task);
        }
    }
}
