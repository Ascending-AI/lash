//! The bytecode core behind the machine interface ([`crate::Machine`]):
//! each start compiles its own [`Program`](crate::bytecode::program::Program)
//! and runs it on a slot heap.
//!
//! Sealing and opening wait on the durable heap (handoff to L2): until it
//! lands, `seal` and `open` refuse as faults.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lash_kernel_doc::{Datum, Decoder, Document, Value};
use lash_kernel_state::paged::{Checkpoint, PageSource, SealBudget};
use lash_kernel_value::names::{FunctionName, Name};
use lash_kernel_value::package::OperationMode;
use lash_kernel_value::types::Type;
use lash_kernel_value::{HelperOperationId, OperationSlot};

use super::tasks::{Committed, Operation, Operations};
use crate::bytecode::program::ActionSite;

use super::watch::Watch;
use super::{Stop, Vm, value_depth};
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;
use crate::interface::{
    BoundExceeded, Bounds, Charge, DeliverError, Delivered, End, Host, Meters, Outcome, Program,
    RunError, StartError, Step, Target, WaitId,
};
use crate::paged::{CellStart, Finished, Machine, MachineError, OpenError, SealError, SealedRoot};
use crate::{ChargeTape, ExecutionPath, MachineOptions, SiteCounts};

/// One run of one document on the bytecode core.
pub struct CoreMachine {
    vm: Vm<SlotHeap>,
    document: Arc<Document>,
    ended: bool,
    convention: Option<lash_kernel_doc::StartConvention>,
}

fn linked(problem: impl ToString) -> StartError {
    StartError::Linked {
        problem: problem.to_string(),
    }
}

impl CoreMachine {
    /// Records every charge the run makes from here on in `tape`, in order
    /// (`K-CHG-002`); recording changes no result, charge or slice return.
    pub fn record_charges(&mut self, tape: ChargeTape) {
        match self.vm.watch.as_deref_mut() {
            Some(watch) => watch.tape = Some(tape),
            None => {
                self.vm.watch =
                    Watch::new(&self.document, &self.vm.program, false, false, Some(tape))
                        .map(Box::new);
            }
        }
    }

    /// Pages every object the heap can out at each statement boundary:
    /// the laws' forced-cold runs.
    #[cfg(test)]
    pub(crate) fn force_cold(&mut self) {
        self.vm.chill = Some(|heap, roots| {
            heap.page_out(roots)
                .expect("a forced-cold run seals its heap");
        });
    }

    /// Builds `datum` into the run's heap, every mutable object fresh.
    /// An input past the memory bound is held for the first step, as a
    /// refused program or constant is (`K-BND-001`).
    fn copy_in(vm: &mut Vm<SlotHeap>, datum: &Datum) -> Result<Value, StartError> {
        if vm.unadmitted() {
            return Ok(Value::Absent);
        }
        let built = super::datum::copy_in(vm, datum);
        vm.admitted(built, Value::Absent)
            .map_err(|flow| linked(format!("{flow:?}")))
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
        source: Box<dyn PageSource>,
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
        let export = match &start.target {
            Target::Entry(name) => match document.exports.get(name) {
                Some(lash_kernel_doc::Export::Function(export)) => Some(export),
                _ => None,
            },
            _ => None,
        };
        let convention = export.map(|export| export.convention.clone());
        let registry = program.library.registry().clone();
        let mut code = crate::bytecode::conventions::compile_selected(
            &document,
            &registry,
            program.link,
            entry,
            convention.as_ref(),
        )
        .map_err(linked)?;
        let heights = crate::bytecode::validate::validate(&code).map_err(linked)?;
        crate::bytecode::fuse::fuse(&mut code);
        let paced = matches!(bounds.charge, Charge::Off);
        let operations = operations(&program, &code, start.modes)?;
        let mut vm = Vm::new(
            Arc::clone(&document),
            Arc::new(code),
            &heights,
            // The run writes its objects to its own source.
            SlotHeap::durable(
                bounds.memory,
                crate::heap::open::Durable {
                    directory: lash_kernel_state::paged::run::directory::Directory::new(source),
                    high_water: lash_kernel_state::paged::run::ids::HighWater(0),
                },
            ),
            0,
            bounds.charge_limit,
            paced,
            bounds.call_depth,
        )
        .map_err(|stop| linked(format!("{stop:?}")))?;
        vm.tasks.operations = operations;
        vm.tasks.live_limit = bounds.live_tasks;
        vm.tasks.requests_limit = bounds.requests_per_park;
        vm.tasks.join_limit = bounds.join_members;
        let decoder = Decoder {
            max_depth: value_depth(),
            policy: document.numbers,
            declared: &|callable| document.functions.contains_key(&callable.function),
        };
        let mut args = Vec::with_capacity(params.len());
        if let Some(decoded) =
            crate::bytecode::conventions::arguments(&document, export, &start.args)?
        {
            for datum in &decoded {
                args.push(Self::copy_in(&mut vm, datum)?);
            }
        } else {
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
            convention,
        })
    }

    fn open(
        _program: Program,
        _bounds: Bounds,
        root: &Checkpoint,
        _source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        crate::paged::admit_format(root)?;
        Err(OpenError::Malformed {
            problem: "the bytecode core opens no checkpoint before the durable heap lands".into(),
        })
    }

    fn set_options(&mut self, options: MachineOptions) {
        self.vm.limiter = options.charge_limiter;
        let tape = self.vm.watch.take().and_then(|watch| watch.tape);
        self.vm.watch = Watch::new(
            &self.document,
            &self.vm.program,
            !options.observation.is_off(),
            options.trace_path,
            tape,
        )
        .map(Box::new);
    }

    fn take_counts(&mut self) -> SiteCounts {
        self.vm
            .watch
            .as_deref_mut()
            .map(Watch::take_counts)
            .unwrap_or_default()
    }

    fn take_path(&mut self) -> Result<ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.vm
            .watch
            .as_deref_mut()
            .map_or_else(|| Ok(ExecutionPath::default()), Watch::take_path)
    }

    fn take_caught(&mut self) -> Vec<WaitId> {
        Vec::new()
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        match self.vm.run(host, slice) {
            Stop::Returned(result) => self.concluded(
                None,
                |result| {
                    End::Finished(Box::new(Finished {
                        result,
                        finish: false,
                        carry: None,
                        not_carried: Vec::new(),
                    }))
                },
                crate::bytecode::conventions::result(self.convention.as_ref(), result),
            ),
            Stop::Finish(result) => {
                let ender = Some(self.vm.tasks.running);
                self.concluded(
                    ender,
                    |result| {
                        End::Finished(Box::new(Finished {
                            result,
                            finish: true,
                            carry: None,
                            not_carried: Vec::new(),
                        }))
                    },
                    result,
                )
            }
            Stop::Fail(datum) => {
                let ender = Some(self.vm.tasks.running);
                self.concluded(ender, End::Failed, datum)
            }
            Stop::Park(park) => Ok(Step::Parked(park)),
            Stop::Deadlock(waiting) => self.ended(End::Error(RunError::Deadlock { waiting })),
            Stop::Cancelled => self.ended(End::Cancelled),
            Stop::Host(fault) => {
                self.ended = true;
                Err(MachineError::Host(fault))
            }
            Stop::Uncaught(value) => self.uncaught(value),
            Stop::Slice => {
                if host.cancel_requested() {
                    return self.ended(End::Cancelled);
                }
                Ok(Step::Slice)
            }
            Stop::Bound(exceeded) => self.ended(Self::exceeded(*exceeded)),
            Stop::Fault(problem) => {
                self.ended = true;
                Err(MachineError::Fault { problem })
            }
        }
    }

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        if self.ended {
            return Err(DeliverError::Ended);
        }
        let document = &self.document;
        let n = lash_kernel_doc::kernel_internals::wait_number(wait);
        self.vm.tasks.deliver(n, |result| match outcome {
            Outcome::Completed(datum) => match super::tasks::decode(document, &datum, result) {
                Ok(decoded) => Committed::Completed(decoded),
                Err(refusal) => Committed::Rejected(refusal),
            },
            Outcome::Failed(error) => {
                let datum = Datum::Error(Box::new(error));
                match super::tasks::decode(document, &datum, &Type::Any) {
                    Ok(decoded) => Committed::Failed(decoded),
                    Err(refusal) => Committed::Rejected(refusal),
                }
            }
        })
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
            live_tasks: self.vm.tasks.live,
        }
    }
}

impl CoreMachine {
    /// The run reached its end: an error when tasks other than `main` and
    /// `ender` are outstanding (`K-TASK-018`), else `end` of `result`.
    fn concluded(
        &mut self,
        ender: Option<usize>,
        end: impl FnOnce(Datum) -> End,
        result: Datum,
    ) -> Result<Step, MachineError> {
        match self.vm.tasks.outstanding(ender, &self.vm.program.units) {
            Some(error) => self.ended(End::Error(error)),
            None => self.ended(end(result)),
        }
    }

    fn uncaught(&mut self, value: Value) -> Result<Step, MachineError> {
        match self.vm.copy_stopped(value) {
            Ok(error) => self.ended(End::Error(RunError::Uncaught { error, site: None })),
            Err(error) => {
                self.ended = true;
                Err(MachineError::Fault {
                    problem: format!("an uncaught value does not copy out: {error}"),
                })
            }
        }
    }
}

/// Resolves every `perform`'s operation in `code` against the program's
/// Link, and which the run binds as reads (`K-EFF-012`).
fn operations(
    program: &Program,
    code: &crate::bytecode::program::Program,
    modes: crate::interface::EffectModes,
) -> Result<Operations, StartError> {
    let mut reads = HashSet::new();
    for (use_, mode) in modes {
        let declared = program
            .instances
            .get(&use_.link)
            .and_then(|instance| instance.operation(use_.slot));
        if !matches!(declared, Some(found) if found == use_) {
            return Err(StartError::UndeclaredRead { use_ });
        }
        if mode == OperationMode::Read {
            reads.insert(use_);
        }
    }
    let link = program.link;
    let instance = program.instances.get(&link);
    let document = &program.document;
    let mut operations = HashMap::new();
    for unit in &code.units {
        for action in &unit.actions {
            let ActionSite::Perform { owner, slot, .. } = action else {
                continue;
            };
            operations.entry((*owner, *slot)).or_insert_with(|| {
                let instance = instance.ok_or("the program's Link is not loaded")?;
                let key = match owner {
                    Some(function) if !document.functions.contains_key(function) => {
                        let registered = program
                            .helpers()
                            .registered(function)
                            .or_else(|| program.library.registry().get(function))
                            .ok_or("helper operation body is not loaded")?;
                        let import = registered
                            .imports()
                            .get(slot)
                            .ok_or("helper does not declare this operation")?;
                        OperationSlot::Helper(HelperOperationId::of_import(import))
                    }
                    _ => (*slot).into(),
                };
                let use_ = instance
                    .operation(key)
                    .filter(|use_| use_.link == link && use_.slot == key)
                    .ok_or("perform has no explicit operation fill in its defining Link")?;
                Ok(Operation {
                    read: reads.contains(&use_),
                    use_,
                })
            });
        }
    }
    Ok(operations)
}
