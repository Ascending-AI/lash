//! The bytecode core behind the machine interface ([`crate::Machine`]):
//! each start compiles its own [`Program`](crate::bytecode::program::Program)
//! and runs it on a slot heap.
//!
//! Sealing and opening wait on the durable heap (handoff to L2): until it
//! lands, `seal` and `open` refuse as faults.

use std::sync::Arc;

use lash_kernel_doc::{Datum, Decoder, Document, Value};
use lash_kernel_state::paged::{Checkpoint, PageSource, SealBudget};
use lash_kernel_value::names::{FunctionName, Name};

use super::{Stop, Vm, value_depth};
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;
use crate::interface::{
    BoundExceeded, Bounds, Charge, DeliverError, Delivered, End, Host, Meters, Outcome, Program,
    RunError, StartError, Step, Target, WaitId,
};
use crate::paged::{CellStart, Finished, Machine, MachineError, OpenError, SealError, SealedRoot};
use crate::{ExecutionPath, MachineOptions, SiteCounts};

/// One run of one document on the bytecode core.
pub struct CoreMachine {
    vm: Vm<SlotHeap>,
    document: Arc<Document>,
    ended: bool,
}

fn linked(problem: impl ToString) -> StartError {
    StartError::Linked {
        problem: problem.to_string(),
    }
}

impl CoreMachine {
    /// Builds `datum` into the run's heap, every mutable object fresh.
    fn copy_in(vm: &mut Vm<SlotHeap>, datum: &Datum) -> Result<Value, StartError> {
        let built = super::datum::copy_in(vm, datum);
        built.map_err(|flow| linked(format!("{flow:?}")))
    }

    fn ended(&mut self, end: End) -> Result<Step, MachineError> {
        self.ended = true;
        Ok(Step::Ended(end))
    }

    fn exceeded(exceeded: super::Exceeded) -> End {
        End::Error(RunError::Bound(BoundExceeded {
            bound: exceeded.bound,
            limit: exceeded.limit,
            function: exceeded
                .function
                .and_then(|name| FunctionName::new(name.to_string()).ok()),
            site: exceeded.site.map(Box::new),
        }))
    }
}

impl Machine for CoreMachine {
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        _source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        if start.session.is_some() {
            return Err(StartError::Bindings {
                problem: "the bytecode core opens no session before the durable heap lands".into(),
            });
        }
        let document = program.document.clone();
        let (entry, params) = match &start.target {
            Target::Main => (None, Vec::new()),
            Target::Entry(name) => match document.exports.get(name) {
                Some(lash_kernel_doc::Export::Function(export)) => {
                    (Some(export.function), export.signature.params.clone())
                }
                _ => {
                    return Err(StartError::UnknownEntry {
                        entry: name.clone(),
                    });
                }
            },
            Target::Callable(_) => {
                return Err(linked("the bytecode core starts no callable yet"));
            }
        };
        let registry = program.library.registry().clone();
        let code = crate::bytecode::compile::compile(&document, &registry, program.link, entry)
            .map_err(linked)?;
        let paced = matches!(bounds.charge, Charge::Off);
        let mut vm = Vm::new(
            Arc::new(code),
            SlotHeap::new(bounds.memory),
            0,
            bounds.charge_limit,
            paced,
            bounds.call_depth,
        )
        .map_err(|stop| linked(format!("{stop:?}")))?;
        let decoder = Decoder {
            max_depth: value_depth(),
            policy: document.numbers,
            declared: &|callable| document.functions.contains_key(&callable.function),
        };
        let mut args = Vec::with_capacity(params.len());
        for (at, param) in params.iter().enumerate() {
            let value = match start.args.get(at) {
                Some(datum) => {
                    let decoded = decoder
                        .decode(datum, &param.ty)
                        .map_err(StartError::Decode)?;
                    Self::copy_in(&mut vm, &decoded)?
                }
                None if param.optional => Value::Absent,
                None => {
                    return Err(StartError::MissingArgument {
                        name: param.name.clone(),
                    });
                }
            };
            args.push(value);
        }
        let globals: Vec<Name> = vm.program.globals.clone();
        for (at, name) in globals.iter().enumerate() {
            if let Some((_, datum)) = start.host.iter().find(|(bound, _)| bound == name) {
                vm.globals[at] = Self::copy_in(&mut vm, datum)?;
            }
        }
        let main = vm.program.main;
        vm.start(main, &args, None)
            .map_err(|flow| linked(format!("{flow:?}")))?;
        Ok(Self {
            vm,
            document,
            ended: false,
        })
    }

    fn open(
        _program: Program,
        _bounds: Bounds,
        _root: &Checkpoint,
        _source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        Err(OpenError::Malformed {
            problem: "the bytecode core opens no checkpoint before the durable heap lands".into(),
        })
    }

    fn set_options(&mut self, options: MachineOptions) {
        self.vm.limiter = options.charge_limiter;
    }

    fn take_counts(&mut self) -> SiteCounts {
        SiteCounts::default()
    }

    fn take_path(&mut self) -> Result<ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        Ok(ExecutionPath::default())
    }

    fn take_caught(&mut self) -> Vec<WaitId> {
        Vec::new()
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        let _ = &self.document;
        match self.vm.run(host, slice) {
            Stop::Returned(result) => self.ended(End::Finished(Box::new(Finished {
                result,
                finish: false,
                carry: None,
                not_carried: Vec::new(),
            }))),
            Stop::Finish(result) => self.ended(End::Finished(Box::new(Finished {
                result,
                finish: true,
                carry: None,
                not_carried: Vec::new(),
            }))),
            Stop::Fail(datum) => self.ended(End::Failed(datum)),
            Stop::Uncaught(value) => self.uncaught(value),
            Stop::Slice => {
                if host.cancel_requested() {
                    return self.ended(End::Cancelled);
                }
                Ok(Step::Slice)
            }
            Stop::Bound(exceeded) => self.ended(Self::exceeded(*exceeded)),
            Stop::Action(_) => Err(MachineError::Fault {
                problem: "the bytecode core runs no action before its task layer lands".into(),
            }),
            Stop::Fault(problem) => {
                self.ended = true;
                Err(MachineError::Fault { problem })
            }
        }
    }

    fn deliver(&mut self, wait: WaitId, _outcome: Outcome) -> Result<Delivered, DeliverError> {
        if self.ended {
            return Err(DeliverError::Ended);
        }
        Err(DeliverError::UnknownWait { wait })
    }

    fn seal(&mut self, _budget: SealBudget) -> Result<Option<SealedRoot>, SealError> {
        if self.ended {
            return Err(SealError::Ended);
        }
        Err(SealError::Fault {
            problem: "the bytecode core seals nothing before the durable heap lands".into(),
        })
    }

    fn meters(&self) -> Meters {
        Meters {
            charged: (!self.vm.paced()).then(|| self.vm.charged()),
            memory: self.vm.heap.held(),
            live_tasks: 1,
        }
    }
}

impl CoreMachine {
    fn uncaught(&mut self, value: Value) -> Result<Step, MachineError> {
        match crate::data::copy_out(self.vm.heap.arena(), value_depth(), &value) {
            Ok(error) => self.ended(End::Error(RunError::Uncaught { error, site: None })),
            Err(error) => {
                self.ended = true;
                Err(MachineError::Fault {
                    problem: format!("an uncaught value does not copy out: {error:?}"),
                })
            }
        }
    }
}
