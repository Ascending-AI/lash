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
    Args, InlineOperation, NativeCall, NativeError, NewObject, Nursery,
};
use lash_kernel_doc::heap::LogicalBytes;

use super::frames::Bill;
use super::{Exceeded, Flow, Run, Stop, Vm, fault, value_depth};
use crate::Bound;
use crate::bytecode::program::{Callee, NativeEntry, Program, Unit};
use crate::functions::MachineFunction;
use crate::heap::Heap;

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
            Callee::Machine { function, charge } => self.call_machine(function, charge, argc),
        }
    }

    /// The bill a call of formula `charge` owes, over its top `argc`
    /// operands.
    fn owed(&self, charge: Option<u32>, argc: usize) -> Option<Bill> {
        let charge = charge.filter(|_| !self.paced)?;
        let from = self.cx.stack.len() - argc;
        Some(Bill {
            charge,
            args: self.cx.stack[from..].into(),
        })
    }

    /// Calls the closure or function value below the top `argc` operands
    /// (`K-FN-004`, `K-CLO-001`).
    fn call_value(&mut self, argc: usize) -> Run<()> {
        let at = self.cx.stack.len() - argc - 1;
        let callee = self.cx.stack.remove(at);
        let (unit, closure) = match callee {
            Value::Closure(handle) => {
                let function = {
                    let (identity, _, _) = self
                        .heap
                        .arena()
                        .closure(handle)
                        .map_err(|error| fault(error.to_string()))?;
                    identity.callable.function
                };
                let unit = *self
                    .closures
                    .get(&function)
                    .ok_or_else(|| fault("a closure of a body the program lacks"))?;
                (unit, Some(handle))
            }
            Value::Function(handle) => {
                let callable = self
                    .heap
                    .arena()
                    .callable(handle)
                    .map_err(|error| fault(error.to_string()))?;
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
        let entry: &NativeEntry = &program.natives[native as usize];
        if let Some(inline) = entry.scalar
            && let Some(value) = self.scalar(inline, argc, usize::from(entry.params))
        {
            // The declared inline answer: the native's own, with no work,
            // allocation or depth to count (`K-LIB-011`).
            let from = self.cx.stack.len() - argc;
            let fee = if charged && !self.paced {
                let args = &self.cx.stack[from..];
                let sized = |value: &Value| crate::data::immediate_size(value).unwrap_or(0);
                let sizes = [sized(&args[0]), sized(&args[1]), sized(&value)];
                match program.charges[entry.charge as usize].pair(sizes) {
                    Some(units) => Some(units),
                    None => Some(self.formula(entry.charge, args, Some(&value))?),
                }
            } else {
                None
            };
            self.cx.stack.truncate(from);
            self.push(value);
            if let Some(units) = fee {
                self.fee(units)?;
            }
            return Ok(());
        }
        let params = usize::from(entry.params);
        for _ in argc..params {
            self.push(Value::Absent);
        }
        let from = self.cx.stack.len() - params.max(argc);
        let charged = charged && !self.paced;
        let limit = match entry.guard {
            Some(guard) => Some(self.formula(guard, &self.cx.stack[from..], None)?),
            None => None,
        };
        let (answer, built) = {
            let objects = self.heap.objects();
            let semantics = self.heap.semantics();
            let mut nursery = Nursery::new(objects, LogicalBytes(u64::MAX));
            let mut counter = WorkCounter::new(limit);
            let answer = entry.native.call(NativeCall {
                args: Args::new(&self.cx.stack[from..], objects, &semantics),
                new: &mut nursery,
                counter: &mut counter,
            });
            match &answer {
                Ok(_) | Err(NativeError::Raised(_)) => (answer, nursery.into_new()),
                Err(_) => {
                    nursery.discard();
                    (answer, Vec::new())
                }
            }
        };
        if !built.is_empty() {
            let units: u64 = built
                .iter()
                .map(|(_, object)| crate::heap::slots::schedule(object))
                .fold(0, u64::saturating_add);
            self.heap
                .install(built)
                .map_err(|error| self.native(error))?;
            // Installed, what the call built is reachable only from its
            // answer until it is published: held across the collection
            // the admission may make.
            let answered = match &answer {
                Ok(value) | Err(NativeError::Raised(value)) => Some(*value),
                Err(_) => None,
            };
            self.held.extend(answered);
            let admitted = self.admit_after(units);
            if answered.is_some() {
                self.held.pop();
            }
            admitted?;
        }
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
                let fee = if charged {
                    Some(self.formula(entry.charge, &self.cx.stack[from..], Some(&value))?)
                } else {
                    None
                };
                self.cx.stack.truncate(from);
                self.push(value);
                if let Some(units) = fee {
                    self.fee(units)?;
                }
                Ok(())
            }
            Err(NativeError::Raised(value)) => {
                self.billed_raise(charged, entry.charge, from)?;
                Err(Flow::Raise(value))
            }
            Err(NativeError::Refused { kind, message }) => {
                self.billed_raise(charged, entry.charge, from)?;
                Err(self.error(kind, &message))
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
                let bill = charged.then(|| Bill {
                    charge: entry.charge,
                    args: self.cx.stack[from..].into(),
                });
                // The body is a frame: the caller's block is left at the
                // call, and the caller continues event by event.
                self.cancel();
                let argc = self.cx.stack.len() - from;
                self.enter(body, argc, None, bill)
            }
            Err(NativeError::Retry(_)) => {
                Err(fault("a native asked for what the core gives at once"))
            }
            Err(NativeError::Host(error)) => Err(fault(error.to_string())),
            Err(NativeError::Malformed(problem)) => Err(fault(problem)),
        }
    }

    /// The inline answer of a native declaring `inline` over its top two
    /// operands, when they are in its declared domain and the operation
    /// has an immediate answer.
    #[inline]
    fn scalar(&self, inline: InlineOperation, argc: usize, params: usize) -> Option<Value> {
        let InlineOperation::Scalar { operation, .. } = inline else {
            return None;
        };
        if argc != 2 || params != 2 {
            return None;
        }
        let args = &self.cx.stack[self.cx.stack.len() - 2..];
        if !inline.accepts(args) {
            return None;
        }
        match *args {
            [Value::Int(a), Value::Int(b)] => lash_kernel_doc::scalar::ints(operation, a, b),
            [Value::Float(a), Value::Float(b)] => lash_kernel_doc::scalar::floats(operation, a, b),
            _ => None,
        }
    }

    /// Bills a raising call's formula with a result of size 0, and drops
    /// its arguments.
    fn billed_raise(&mut self, charged: bool, charge: u32, from: usize) -> Run<()> {
        let fee = if charged {
            Some(self.formula(charge, &self.cx.stack[from..], None)?)
        } else {
            None
        };
        self.cx.stack.truncate(from);
        if let Some(units) = fee {
            self.fee(units)?;
        }
        Ok(())
    }

    /// Admits what a native built, collecting once before it refuses.
    fn admit_after(&mut self, units: u64) -> Run<()> {
        self.admit(units)
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
