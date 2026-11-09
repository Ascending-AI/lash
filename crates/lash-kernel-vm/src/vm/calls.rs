//! Calls (cutover contract §1, §2, §3): frame calls of units and values,
//! the native-call ABI and the machine's own functions.
//!
//! A native reads its arguments through zero-copy [`Args`] views over the
//! heap's bodies and builds into a [`Nursery`] the machine installs after
//! it returns; what it built is admitted under the memory schedule, after
//! one collection, or the run ends at the memory bound. A Decline runs the
//! definition's body (`K-LIB-006`), covered, its formula billed when its
//! frame is left; it creates no step.
//!
//! Completion formulas are dynamic fees (`K-CHG-003`): a call bills its
//! formula once, at return, or at raise with a result of size 0.

use lash_kernel_doc::Value;
use lash_kernel_doc::WorkCounter;
use lash_kernel_doc::arena::native::{
    Args, InlineOperation, NativeCall, NativeError, NewObject, Nursery, Retry,
};
use lash_kernel_doc::heap::LogicalBytes;

use super::frames::Bill;
use super::{Exceeded, Flow, Restart, Run, Stop, Vm, fault, read, value_depth};
use crate::Bound;
use crate::bytecode::program::{Callee, NativeEntry, Program, Unit};
use crate::functions::MachineFunction;
use crate::heap::Heap;

/// How a native call that ran ended, before its completion bills.
#[derive(Debug)]
pub(crate) enum Done {
    Value(Value),
    Raised(Value),
    Refused(&'static str, String),
}

impl Done {
    /// The value it holds, for the collector.
    pub(super) fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        match self {
            Self::Value(value) | Self::Raised(value) => visit(value),
            Self::Refused(..) => {}
        }
    }
}

impl<H: Heap> Vm<H> {
    /// Call site `site` of `unit`, its arguments on the stack.
    pub(super) fn call(&mut self, program: &Program, unit: &Unit, site: u32) -> Run<()> {
        let site = &unit.calls[site as usize];
        let argc = usize::from(site.argc);
        match site.callee {
            Callee::Unit {
                unit: callee,
                charge,
            } => {
                let bill = self.owed(charge, argc);
                self.meter.leave();
                self.exact = true;
                self.enter(callee, argc, None, bill)
            }
            Callee::Value => self.call_value(argc),
            Callee::Native { native, charged } => self.call_native(program, native, argc, charged),
            Callee::Machine {
                function, charge, ..
            } => self.call_machine(function, charge, argc),
        }
    }

    /// The bill a call of formula `charge` owes, over its top `argc`
    /// operands.
    pub(super) fn owed(&mut self, charge: Option<u32>, argc: usize) -> Option<Bill> {
        let charge = charge.filter(|_| !self.paced)?;
        Some(self.owe(charge, argc))
    }

    /// Calls the closure or function value below the top `argc` operands
    /// (`K-FN-004`, `K-CLO-001`). The callee is read where it stands and
    /// removed once read.
    fn call_value(&mut self, argc: usize) -> Run<()> {
        let at = self.cx.stack.len() - argc - 1;
        let callee = self.cx.stack[at];
        let (unit, closure) = match callee {
            Value::Closure(handle) => {
                let function = {
                    let (identity, _, _) = self.heap.arena().closure(handle).map_err(read)?;
                    identity.callable.function
                };
                let unit = *self
                    .closures
                    .get(&function)
                    .ok_or_else(|| fault("a closure of a body the program lacks"))?;
                (unit, Some(handle))
            }
            Value::Function(handle) => {
                let callable = self.heap.arena().callable(handle).map_err(read)?;
                let unit = *self
                    .functions
                    .get(&callable.function)
                    .ok_or_else(|| fault("a function value the program cannot call"))?;
                (unit, None)
            }
            _ => {
                return Err(self.error(
                    "type_error",
                    "only a closure or a function reference can be called",
                ));
            }
        };
        self.cx.stack.remove(at);
        let params = usize::from(self.program.units[unit as usize].params);
        if argc > params {
            return Err(self.error(
                "arity",
                &format!("the function takes {params} argument(s); {argc} given"),
            ));
        }
        self.meter.leave();
        self.exact = true;
        self.enter(unit, argc, closure, None)
    }

    /// Calls native `native` over its top `argc` operands; `charged` when
    /// the call bills the definition's formula (`K-CHG-007`).
    fn call_native(
        &mut self,
        program: &Program,
        native: u32,
        argc: usize,
        charged: bool,
    ) -> Run<()> {
        let len = self.cx.stack.len();
        if let Some(answer) = self.answer(program, native, charged, &self.cx.stack[len - argc..]) {
            let (value, fee) = answer?;
            self.cx.stack.truncate(len - argc);
            self.push(value);
            if let Some(units) = fee {
                self.fee(units)?;
            }
            return Ok(());
        }
        let entry: &NativeEntry = &program.natives[native as usize];
        let params = usize::from(entry.params);
        // The missing parameters are padded absent, and the padding is
        // taken back when a cold read runs the call again.
        for _ in argc..params {
            self.push(Value::Absent);
        }
        let from = self.cx.stack.len() - params.max(argc);
        let charged = charged && !self.paced;
        let limit = match entry.guard {
            Some(guard) => match self.formula(guard, &self.cx.stack[from..], None) {
                Ok(limit) => Some(limit),
                Err(flow) => {
                    self.cx.stack.truncate(len);
                    return Err(flow);
                }
            },
            None => None,
        };
        // The call builds into a nursery with the room the bound leaves.
        // What it built is admitted at its peak, the larger of what it
        // reserved and what its objects hold, before any of it is placed;
        // its inputs are still held on the stack (contract §2). Short of
        // room, the run collects and calls the native once more: it is
        // pure, so the second call does what the first would have.
        let mut collected = false;
        let (answer, built, peak) = loop {
            let attempt = {
                let objects = self.heap.objects();
                let semantics = self.heap.semantics();
                let room = self.heap.bound().saturating_sub(self.heap.held());
                let mut nursery = Nursery::new(objects, LogicalBytes(room));
                let mut counter = WorkCounter::new(limit);
                let answer = entry.native.call(NativeCall {
                    args: Args::new(&self.cx.stack[from..], objects, &semantics),
                    new: &mut nursery,
                    counter: &mut counter,
                });
                match &answer {
                    Ok(_) | Err(NativeError::Raised(_)) => {
                        let placed: u64 = nursery
                            .built()
                            .iter()
                            .map(|(_, object)| crate::heap::meter::new_object(object))
                            .fold(0, u64::saturating_add);
                        let peak = placed.max(nursery.held().0);
                        if peak <= room {
                            Some((answer, nursery.into_new(), peak))
                        } else {
                            nursery.discard();
                            None
                        }
                    }
                    Err(NativeError::Retry(Retry::Room { .. })) => {
                        nursery.discard();
                        None
                    }
                    Err(_) => {
                        nursery.discard();
                        Some((answer, Vec::new(), 0))
                    }
                }
            };
            match attempt {
                Some(done) => break done,
                None if !collected => {
                    collected = true;
                    self.collect();
                }
                None => {
                    let held = self.heap.held();
                    // The native that would have exceeded the bound is
                    // named, as a guard that ends the run is.
                    return Err(Flow::from(Stop::bound(Exceeded {
                        function: Some(entry.name.clone()),
                        ..super::memory(crate::heap::MemoryRefused {
                            held,
                            wanted: self.heap.bound().saturating_sub(held).saturating_add(1),
                            bound: self.heap.bound(),
                        })
                    })));
                }
            }
        };
        if peak > 0 {
            // Admitted within the room just measured.
            let _fits = self.heap.admit(peak);
            debug_assert!(_fits.is_ok());
            let placed: u64 = built
                .iter()
                .map(|(_, object)| crate::heap::meter::new_object(object))
                .fold(0, u64::saturating_add);
            self.heap
                .install(built)
                .map_err(|error| self.native(error))?;
            // The transient part of the peak ends with the call.
            self.heap.release(peak - placed);
        }
        // What the call built is installed: from here a cold read resumes
        // its completion, never the native.
        let charge = charged.then_some(entry.charge);
        match answer {
            Ok(value) => {
                // An immediate answer holds no other value.
                let limit = value_depth();
                if crate::data::immediate_size(&value).is_none()
                    && self.heap.arena().summary_of(&value).depth as usize > limit
                {
                    return Err(self.error(
                        "too_deep",
                        &format!("a value exceeds the configured {limit}-level value depth"),
                    ));
                }
                self.complete_native(from, charge, Done::Value(value))
            }
            Err(NativeError::Raised(value)) => {
                self.complete_native(from, charge, Done::Raised(value))
            }
            Err(NativeError::Refused { kind, message }) => {
                self.complete_native(from, charge, Done::Refused(kind, message))
            }
            Err(NativeError::Guard(guard)) => Err(Flow::from(Stop::bound(Exceeded {
                bound: Bound::Guard {
                    function: entry.function,
                },
                limit: guard.limit,
                function: Some(entry.name.clone()),
                site: None,
            }))),
            Err(NativeError::Decline) => {
                let Some(body) = entry.body else {
                    return Err(fault("a native with no kernel body declined"));
                };
                let argc = self.cx.stack.len() - from;
                let bill = charged.then(|| self.owe(entry.charge, argc));
                // The body is a frame: the caller's block is left at the
                // call, and the caller continues event by event.
                self.cancel();
                self.enter(body, argc, None, bill)
            }
            // It read a cold chunk and did nothing a run sees: the call
            // runs again, unpadded, once the chunk is loaded.
            Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                self.cx.stack.truncate(len);
                Err(Flow::Cold { object, chunk })
            }
            Err(NativeError::Retry(Retry::Room { .. })) => {
                Err(fault("a native asked for room the core gives at once"))
            }
            Err(NativeError::Host(error)) => Err(fault(error.to_string())),
            Err(NativeError::Malformed(problem)) => Err(fault(problem)),
        }
    }

    /// The inline answer of native `native` over `args`, and the fee the
    /// call bills when `charged`: the native's own answer, with no work or
    /// allocation to count (R8, `K-LIB-011`). `None` where the native
    /// declares no inline answer over them. An operation, a native with a
    /// body beside it, is one unit of call depth whichever implementation
    /// answers: at the bound it is refused here, before any is offered the
    /// call (`K-BND-002`, `K-LIB-012`), and a Decline into its body enters
    /// that same unit.
    #[inline(always)]
    pub(super) fn answer(
        &self,
        program: &Program,
        native: u32,
        charged: bool,
        args: &[Value],
    ) -> Option<Run<(Value, Option<u64>)>> {
        let entry = &program.natives[native as usize];
        if entry.body.is_some() && self.cx.frames.len() >= self.call_depth as usize {
            return Some(Err(Flow::from(Stop::bound(
                self.exceeded(Bound::CallDepth, u64::from(self.call_depth)),
            ))));
        }
        let inline = entry.scalar?;
        if args.len() != usize::from(entry.params) {
            return None;
        }
        let value = self.immediate(inline, args)?;
        if !charged || self.paced {
            return Some(Ok((value, None)));
        }
        let units = match inline {
            InlineOperation::Kind | InlineOperation::Not | InlineOperation::Length(_) => {
                self.formula(entry.charge, args, Some(&value))
            }
            _ => {
                let sized = |value: &Value| crate::data::immediate_size(value).unwrap_or(0);
                let sizes = [sized(&args[0]), sized(&args[1]), sized(&value)];
                match program.charges[entry.charge as usize].pair(sizes) {
                    Some(units) => Ok(units),
                    None => self.formula(entry.charge, args, Some(&value)),
                }
            }
        };
        Some(units.map(|units| (value, Some(units))))
    }

    /// What inline operation `inline` answers over `args`, with no effect:
    /// `None` outside the domain the core answers it in, where the
    /// native's ordinary call runs.
    #[inline(always)]
    pub(super) fn immediate(&self, inline: InlineOperation, args: &[Value]) -> Option<Value> {
        match inline {
            InlineOperation::Scalar { operation, .. } if args.len() == 2 => {
                if !inline.accepts(args) {
                    return None;
                }
                match *args {
                    [Value::Int(a), Value::Int(b)] => {
                        lash_kernel_doc::scalar::ints(operation, a, b)
                    }
                    [Value::Float(a), Value::Float(b)] => {
                        lash_kernel_doc::scalar::floats(operation, a, b)
                    }
                    _ => None,
                }
            }
            InlineOperation::Kind | InlineOperation::Not if args.len() == 1 => inline.answer(args),
            InlineOperation::Length(_) if args.len() == 1 => {
                let semantics = self.heap.semantics();
                inline
                    .answer_read(Args::new(args, self.heap.objects(), &semantics))
                    .ok()
                    .flatten()
            }
            InlineOperation::Same if args.len() == 2 => inline.answer(args),
            _ => None,
        }
    }

    /// Completes a native call that answered or raised: bills formula
    /// `charge` over its arguments at `from` and its result, of size 0
    /// when it raised, drops the arguments and gives the result or the
    /// raise. A cold read of the formula holds the completion, so that it
    /// resumes once the chunk is loaded.
    pub(super) fn complete_native(
        &mut self,
        from: usize,
        charge: Option<u32>,
        done: Done,
    ) -> Run<()> {
        let fee = match charge {
            Some(charge) => {
                let result = match &done {
                    Done::Value(value) => Some(value),
                    Done::Raised(_) | Done::Refused(..) => None,
                };
                match self.formula(charge, &self.cx.stack[from..], result) {
                    Ok(units) => Some(units),
                    Err(flow) => {
                        if let Flow::Cold { .. } = flow {
                            self.restart = Some(Restart::Native {
                                from: from as u32,
                                charge: Some(charge),
                                done,
                            });
                        }
                        return Err(flow);
                    }
                }
            }
            None => None,
        };
        self.cx.stack.truncate(from);
        let raised = match done {
            Done::Value(value) => {
                self.push(value);
                None
            }
            Done::Raised(value) => Some(Ok(value)),
            Done::Refused(kind, message) => Some(Err((kind, message))),
        };
        if let Some(units) = fee {
            self.fee(units)?;
        }
        match raised {
            None => Ok(()),
            Some(Ok(value)) => Err(Flow::Raise(value)),
            Some(Err((kind, message))) => Err(self.error(kind, &message)),
        }
    }

    /// A function the machine answers itself (`K-LIB-010`).
    fn call_machine(
        &mut self,
        function: MachineFunction,
        charge: Option<u32>,
        argc: usize,
    ) -> Run<()> {
        let from = self.cx.stack.len() - argc;
        let value = match function {
            MachineFunction::Deref => {
                match self.cx.stack.get(from).copied().unwrap_or(Value::Absent) {
                    Value::TaskRef(task) => Value::Task(task),
                    Value::ObjectRef(object) => {
                        let found = self
                            .heap
                            .arena()
                            .object_value(object)
                            .map_err(|error| self.native(error))?;
                        match found {
                            Some(value) => value,
                            None => return Err(self.error("type_error", "the ref names no object")),
                        }
                    }
                    _ => return Err(self.error("type_error", "`deref` takes a ref")),
                }
            }
            MachineFunction::TasksUnfinished => {
                let tasks = self.tasks.unfinished();
                self.alloc(NewObject::List(tasks))?
            }
            MachineFunction::BrandBuilt => {
                return Err(self.error(
                    "type_error",
                    "`brand.built` is called by name in an expression",
                ));
            }
        };
        let fee = match charge.filter(|_| !self.paced) {
            Some(charge) => Some(self.formula(charge, &self.cx.stack[from..], Some(&value))?),
            None => None,
        };
        self.cx.stack.truncate(from);
        self.push(value);
        if let Some(units) = fee {
            self.fee(units)?;
        }
        Ok(())
    }
}
