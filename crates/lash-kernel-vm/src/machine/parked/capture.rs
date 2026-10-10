//! Writing a run's state at a safe point.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{ClosureObject, Identity, Name, Object, ObjectId, Site, Value};
use lash_kernel_state as state;
use lash_kernel_state::{Baseline, Objects, ParkedCall, ParkedRun, ParkedTask, Saved};

use super::super::{
    Completion, Control, Cursor, Frame, Incoming, Joiner, KernelMachine, SlotState, Task,
    TaskState, TryPhase, Wait,
};
use super::action_of;
use crate::compile::{Executable, Stmt};
use crate::heap::{Heap, Obj};
use crate::interface::{ExportError, Outcome, Request};

fn fault(problem: impl Into<String>) -> ExportError {
    ExportError::Fault {
        problem: problem.into(),
    }
}

/// Adds the heap objects a value names, through tuples, errors and refs.
fn objects_of(value: &Value, out: &mut Vec<ObjectId>) {
    match value {
        Value::Tuple(members) => members.iter().for_each(|member| objects_of(member, out)),
        Value::Error(error) => objects_of(&error.data, out),
        Value::Ref(Identity::Object(id)) => out.push(*id),
        other => out.extend(other.object()),
    }
}

/// The machine's heap as a save reads it.
pub(in crate::machine) struct HeapView<'a> {
    pub(in crate::machine) heap: &'a Heap,
    pub(in crate::machine) exe: &'a Executable,
}

impl HeapView<'_> {
    pub(in crate::machine) fn data(&self, object: &Obj) -> Option<Object> {
        Some(match object {
            Obj::List(items) => Object::List(items.clone()),
            Obj::Map(table) => Object::Map(
                table
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
            Obj::Set(table) => {
                Object::Set(table.iter().map(|(member, _)| member.clone()).collect())
            }
            Obj::Record(fields) => Object::Record(fields.clone()),
            Obj::Variable(value) => Object::Variable(value.clone()),
            Obj::Closure(closure) => {
                let code = self.exe.get_code(closure.code)?;
                // A closure's body is the one child of its expression.
                let mut path = code.site.path.clone();
                path.pop()?;
                let captures = code
                    .captures
                    .iter()
                    .zip(&closure.captures)
                    .map(|(capture, cell)| (code.slots[capture.inner as usize].name.clone(), *cell))
                    .collect();
                Object::Closure(ClosureObject {
                    site: Site::new(code.site.unit.clone(), path),
                    captures,
                })
            }
        })
    }
}

impl Objects for HeapView<'_> {
    fn object(&self, id: ObjectId) -> Option<Object> {
        self.data(self.heap.get(id)?)
    }

    fn references(&self, id: ObjectId, out: &mut Vec<ObjectId>) -> bool {
        let Some(object) = self.heap.get(id) else {
            return false;
        };
        match object {
            Obj::List(items) => items.iter().for_each(|item| objects_of(item, out)),
            Obj::Map(table) | Obj::Set(table) => {
                for (key, value) in table.iter() {
                    objects_of(key, out);
                    objects_of(value, out);
                }
            }
            Obj::Record(fields) => fields.iter().for_each(|(_, value)| objects_of(value, out)),
            Obj::Closure(closure) => out.extend(closure.captures.iter().copied()),
            Obj::Variable(value) => objects_of(value, out),
        }
        true
    }

    fn written(&self, id: ObjectId) -> u64 {
        self.heap.written(id)
    }
}

type Roots = (state::Run, BTreeMap<Name, Value>, Vec<ParkedTask>);

impl KernelMachine {
    /// Everything of the run but its heap, at this safe point. The heap is
    /// collected first, so that what is saved is what is live and the
    /// machine that goes on accounts its memory as one that imports the
    /// state will.
    fn roots(&mut self) -> Result<Roots, ExportError> {
        if self.ended {
            return Err(ExportError::Ended);
        }
        // What the last statement held outside any variable is bound or
        // dropped by now.
        self.pins.clear();
        self.fresh.clear();
        self.collect();
        let document = match self.document {
            Some(document) => document,
            None => {
                let document = self
                    .program
                    .document
                    .identity()
                    .map_err(|error| fault(error.to_string()))?;
                self.document = Some(document);
                document
            }
        };
        let exe = Arc::clone(&self.exe);
        let mut tasks = Vec::with_capacity(self.tasks.len());
        for task in &self.tasks {
            tasks.push(self.task_data(&exe, task)?);
        }
        let run = state::Run {
            kernel: self.program.document.manifest.kernel,
            document,
            functions: self
                .program
                .document
                .manifest
                .functions
                .keys()
                .copied()
                .collect(),
            charged: self.charged,
            objects_allocated: self.heap.allocated(),
            waits_issued: self.next_wait,
            // The task a slice interrupted runs next: it is the first of
            // the ready.
            ready: self
                .current
                .into_iter()
                .chain(self.ready.iter().copied())
                .collect(),
            withdrawn: self.withdrawn_ever.clone(),
            unreported: self.withdrawn.clone(),
        };
        Ok((run, self.session.clone(), tasks))
    }

    fn task_data(&self, exe: &Executable, task: &Task) -> Result<ParkedTask, ExportError> {
        let state = match (&task.state, &task.incoming) {
            (TaskState::Ended(Ok(value)), _) => {
                state::TaskState::Ended(state::Ended::Returned(value.clone()))
            }
            (TaskState::Ended(Err(value)), _) => {
                state::TaskState::Ended(state::Ended::Raised(value.clone()))
            }
            (TaskState::Waiting(Wait::Request(wait)), _) => {
                let pending = self
                    .waits
                    .get(wait)
                    .ok_or_else(|| fault("a task waits on a request the machine does not hold"))?;
                state::TaskState::Performing(Box::new(state::Perform {
                    wait: *wait,
                    request: request_data(&pending.request),
                    state: if pending.handed_out {
                        state::PerformState::Admitted
                    } else {
                        state::PerformState::Requested
                    },
                }))
            }
            (TaskState::Waiting(Wait::Join(joined)), _) => state::TaskState::Joining(*joined),
            (TaskState::Waiting(Wait::JoinMany(order)), _) => {
                let join = self
                    .joins
                    .get(order)
                    .ok_or_else(|| fault("a task waits in a join the machine does not hold"))?;
                state::TaskState::JoiningMany(state::ListJoin {
                    mode: join.mode,
                    members: join.members.clone(),
                })
            }
            (TaskState::Ready, None) => state::TaskState::Ready,
            (TaskState::Ready, Some(Incoming::Value(value))) => {
                state::TaskState::Resuming(state::Incoming::Value(value.clone()))
            }
            (TaskState::Ready, Some(Incoming::Raise(value))) => {
                state::TaskState::Resuming(state::Incoming::Raise(value.clone()))
            }
            (TaskState::Ready, Some(Incoming::Join(joined))) => {
                state::TaskState::Resuming(state::Incoming::Joined(*joined))
            }
            (TaskState::Ready, Some(Incoming::Outcome(answered))) => {
                state::TaskState::Performing(Box::new(state::Perform {
                    wait: answered.wait,
                    request: request_data(&answered.request),
                    state: state::PerformState::Committed(match &answered.outcome {
                        Outcome::Completed(datum) => state::Outcome::Completed(datum.clone()),
                        Outcome::Failed(error) => state::Outcome::Failed(error.clone()),
                        Outcome::Elapsed => state::Outcome::Elapsed,
                    }),
                }))
            }
        };
        let mut occurrences = Vec::with_capacity(task.occurrences.len());
        for (stmt, count) in &task.occurrences {
            let action = action_of(exe, *stmt)
                .ok_or_else(|| fault("an occurrence is counted at a statement with no action"))?;
            occurrences.push(state::Occurrence {
                site: action.site.clone(),
                count: *count,
            });
        }
        occurrences.sort_by(|a, b| a.site.cmp(&b.site));
        let mut calls = Vec::with_capacity(task.frames.len());
        for frame in &task.frames {
            calls.push(self.call_data(exe, frame)?);
        }
        let mut joiners = Vec::with_capacity(task.joiners.len());
        for joiner in &task.joiners {
            joiners.push(match joiner {
                Joiner::Single(joiner) => *joiner,
                Joiner::List(order) => {
                    self.joins
                        .get(order)
                        .ok_or_else(|| {
                            fault("a task is joined by a join the machine does not hold")
                        })?
                        .joiner
                }
            });
        }
        Ok(ParkedTask {
            handle: state::Task {
                identity: task.identity.clone(),
                state,
                joiners,
                observed: task.observed,
                passed: task.passed,
                failed: task.failed,
                occurrences,
            },
            calls,
        })
    }

    fn call_data(&self, exe: &Executable, frame: &Frame) -> Result<ParkedCall, ExportError> {
        if frame.inline {
            return Err(fault("a call inside an expression is live at a safe point"));
        }
        let code = exe.code(frame.code);
        let Some(Control::Block { block, next }) = frame.control.last() else {
            return Err(fault("a call stands in no block at a safe point"));
        };
        // A statement whose action is under way has been stepped past. A
        // call a raise is about to land on is saved at the statement it
        // stood in: the raise leaves it either way.
        let index = match frame.awaiting {
            Some(_) => next
                .checked_sub(1)
                .ok_or_else(|| fault("a call awaits a statement its block has not reached"))?,
            None => *next,
        };
        let mut call = state::Call {
            statement: exe.block(*block).site.child(index as u32),
            bindings: Vec::new(),
            loops: Vec::new(),
            finally: Vec::new(),
        };
        for (slot, info) in code.slots.iter().enumerate() {
            let value = match &frame.slots[code.positions[slot] as usize] {
                SlotState::Empty => continue,
                SlotState::Value(value) => state::Bound::Value(value.clone()),
                SlotState::Cell(cell) => state::Bound::Shared(*cell),
            };
            call.bindings.push(state::Binding {
                name: info.name.clone(),
                declared: info.declared.clone(),
                value,
            });
        }
        let mut held = state::Held {
            arguments: frame.library.as_ref().map(|library| library.args.clone()),
            ..state::Held::default()
        };
        for control in &frame.control {
            match control {
                Control::Block { .. } => {}
                Control::For {
                    stmt,
                    cursor,
                    started,
                } => {
                    let Stmt::For { site, .. } = exe.stmt(*stmt) else {
                        return Err(fault("a `for` loop is not at a `for`"));
                    };
                    let (iterated, position) = match cursor {
                        Cursor::List(list, position) => (Value::List(*list), *position as u64),
                        Cursor::Tuple(items, position) => {
                            (Value::Tuple(Arc::clone(items)), *position as u64)
                        }
                        Cursor::Table(table, last) => {
                            let passed = self.heap.table(*table).map(|table| table.passed(*last));
                            match (self.heap.value_of(*table), passed) {
                                (Some(iterated), Some(passed)) => (iterated, passed),
                                _ => return Err(fault("a loop iterates no collection")),
                            }
                        }
                    };
                    held.iterated.push(iterated);
                    call.loops.push(state::Loop {
                        site: site.clone(),
                        started: *started,
                        position: Some(position),
                    });
                }
                Control::While { stmt, started } => {
                    let Stmt::While { site, .. } = exe.stmt(*stmt) else {
                        return Err(fault("a `while` loop is not at a `while`"));
                    };
                    call.loops.push(state::Loop {
                        site: site.clone(),
                        started: *started,
                        position: None,
                    });
                }
                Control::Try {
                    phase: TryPhase::Body | TryPhase::Catch,
                    ..
                } => {}
                Control::Try {
                    stmt,
                    phase: TryPhase::Finally(departure),
                } => {
                    let Stmt::Try { site, .. } = exe.stmt(*stmt) else {
                        return Err(fault("a `try` is not at a `try`"));
                    };
                    let entered = match departure {
                        Completion::Normal => state::Entered::Normal,
                        Completion::Break => state::Entered::Break,
                        Completion::Continue => state::Entered::Continue,
                        Completion::Throw(value) => {
                            held.departing.push(value.clone());
                            state::Entered::Throw
                        }
                        Completion::Return(value) => {
                            held.departing.push(value.clone());
                            state::Entered::Return
                        }
                    };
                    call.finally.push(state::Finally {
                        site: site.clone(),
                        entered,
                    });
                }
            }
        }
        Ok(ParkedCall { call, held })
    }
}

fn request_data(request: &Request) -> state::Request {
    match request {
        Request::Effect(effect) => state::Request::Effect {
            identity: effect.identity.clone(),
            effect: effect.effect.clone(),
            args: effect.args.clone(),
        },
        Request::Sleep(sleep) => state::Request::Sleep {
            identity: sleep.identity.clone(),
            duration: sleep.duration.into(),
        },
    }
}

pub(in crate::machine) fn export(machine: &mut KernelMachine) -> Result<ParkedRun, ExportError> {
    let (run, session, tasks) = machine.roots()?;
    let view = HeapView {
        heap: &machine.heap,
        exe: &machine.exe,
    };
    let mut objects = BTreeMap::new();
    for (id, object) in machine.heap.iter() {
        let object = view
            .data(object)
            .ok_or_else(|| fault("a closure names no closure expression"))?;
        objects.insert(id, object);
    }
    Ok(ParkedRun {
        run,
        session,
        tasks,
        objects,
    })
}

pub(in crate::machine) fn save(
    machine: &mut KernelMachine,
    since: &Baseline,
) -> Result<Saved, ExportError> {
    let (run, session, tasks) = machine.roots()?;
    let view = HeapView {
        heap: &machine.heap,
        exe: &machine.exe,
    };
    state::save(run, session, tasks, &view, since).map_err(|error| fault(error.to_string()))
}
