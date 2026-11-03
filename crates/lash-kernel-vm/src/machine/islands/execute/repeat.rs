//! The fixed graph executes in source order and publishes only a complete
//! admitted iteration. A miss leaves that iteration to ordinary packets.

use super::super::code::repeat::proof::Values as IterationValues;
use super::super::code::repeat::{Binary, Copy, Input, Repeat, Scalar};
use super::super::compiled::recurrence::BoundRecurrence;
use super::*;
use crate::admission::repeat::RepeatResources;
use crate::compile::room::placement::DenseActivation;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::kernel_internals::{
    PagedHeap,
    logical::{Invocation, Ledger},
};

macro_rules! stop {
    ($exit:ident) => {{
        witnessed!(Repeated, $exit);
        break;
    }};
}

pub(super) struct Window {
    pub(super) iteration_max: Option<u64>,
}

/// The induction and accumulator stay in their native Rust domain. Value
/// decoding from the activation happens at entry, invariants bind once, and only final
/// survivors are written back to the activation.
trait Number: std::marker::Copy {
    fn read(value: Value) -> Option<Self>;
    fn value(self) -> Value;
    fn operation(pair: &lash_kernel_doc::scalar::Pair)
    -> Option<lash_kernel_doc::scalar::ScalarOp>;
    fn number(op: lash_kernel_doc::scalar::ScalarOp, a: Self, b: Self) -> Option<Self>;
    fn truth(op: lash_kernel_doc::scalar::ScalarOp, a: Self, b: Self) -> Option<bool>;
}

impl Number for i64 {
    #[inline(always)]
    fn read(value: Value) -> Option<Self> {
        if let Value::Int(value) = value {
            Some(value)
        } else {
            None
        }
    }
    #[inline(always)]
    fn value(self) -> Value {
        Value::Int(self)
    }
    fn operation(
        pair: &lash_kernel_doc::scalar::Pair,
    ) -> Option<lash_kernel_doc::scalar::ScalarOp> {
        pair.ints
    }
    #[inline(always)]
    fn number(op: lash_kernel_doc::scalar::ScalarOp, a: Self, b: Self) -> Option<Self> {
        lash_kernel_doc::scalar::int_number(op, a, b)
    }
    #[inline(always)]
    fn truth(op: lash_kernel_doc::scalar::ScalarOp, a: Self, b: Self) -> Option<bool> {
        lash_kernel_doc::scalar::int_truth(op, a, b)
    }
}

impl Number for f64 {
    #[inline(always)]
    fn read(value: Value) -> Option<Self> {
        if let Value::Float(value) = value {
            Some(value.get())
        } else {
            None
        }
    }
    #[inline(always)]
    fn value(self) -> Value {
        Value::Float(self.into())
    }
    fn operation(
        pair: &lash_kernel_doc::scalar::Pair,
    ) -> Option<lash_kernel_doc::scalar::ScalarOp> {
        pair.floats
    }
    #[inline(always)]
    fn number(op: lash_kernel_doc::scalar::ScalarOp, a: Self, b: Self) -> Option<Self> {
        lash_kernel_doc::scalar::float_number(op, a, b)
    }
    #[inline(always)]
    fn truth(op: lash_kernel_doc::scalar::ScalarOp, a: Self, b: Self) -> Option<bool> {
        lash_kernel_doc::scalar::float_truth(op, a, b)
    }
}

struct Inputs<N> {
    counter: N,
    previous: N,
    accumulator: Option<N>,
}

enum BoundInput<N> {
    Counter,
    Previous,
    Accumulator,
    Invariant(N),
}

impl Input {
    fn bind<N: Number>(self, frame: &Values<'_, '_>) -> Option<BoundInput<N>> {
        Some(match self {
            Self::Counter => BoundInput::Counter,
            Self::Previous => BoundInput::Previous,
            Self::Accumulator => BoundInput::Accumulator,
            Self::Literal(value) => BoundInput::Invariant(N::read(value)?),
            Self::Invariant(at) => BoundInput::Invariant(N::read(frame.value(at)?)?),
        })
    }
}

impl<N: Number> BoundInput<N> {
    #[inline(always)]
    fn read(&self, values: &Inputs<N>) -> Option<N> {
        match self {
            Self::Counter => Some(values.counter),
            Self::Previous => Some(values.previous),
            Self::Accumulator => values.accumulator,
            Self::Invariant(value) => Some(*value),
        }
    }
}

struct ScalarRun<N> {
    operation: lash_kernel_doc::scalar::ScalarOp,
    domain: std::marker::PhantomData<N>,
}

impl<N: Number> ScalarRun<N> {
    fn of(scalar: &Scalar, _exe: &Executable) -> Option<Self> {
        Some(Self {
            operation: N::operation(&scalar.pair?)?,
            domain: std::marker::PhantomData,
        })
    }

    #[inline(always)]
    fn number(&self, args: [N; 2]) -> Option<N> {
        N::number(self.operation, args[0], args[1])
    }

    #[inline(always)]
    fn truth(&self, args: [N; 2]) -> Option<bool> {
        N::truth(self.operation, args[0], args[1])
    }
}

impl Scalar {
    #[inline(always)]
    fn unary(&self, exe: &Executable, value: Value) -> Option<Value> {
        let answer = match &self.site {
            Some(site) => super::super::compiled::unary(site, value, exe)?.0,
            None => value,
        };
        matches!(
            (value, answer),
            (Value::Int(_), Value::Int(_)) | (Value::Float(_), Value::Float(_))
        )
        .then_some(answer)
    }
}

struct BinaryRun<N> {
    scalar: ScalarRun<N>,
    inputs: [BoundInput<N>; 2],
}

impl<N: Number> BinaryRun<N> {
    fn of(binary: &Binary, exe: &Executable, frame: &Values<'_, '_>) -> Option<Self> {
        Some(Self {
            scalar: ScalarRun::of(&binary.scalar, exe)?,
            inputs: [binary.inputs[0].bind(frame)?, binary.inputs[1].bind(frame)?],
        })
    }

    #[inline(always)]
    fn number(&self, inputs: &Inputs<N>) -> Option<N> {
        self.scalar
            .number([self.inputs[0].read(inputs)?, self.inputs[1].read(inputs)?])
    }

    #[inline(always)]
    fn truth(&self, inputs: &Inputs<N>) -> Option<bool> {
        self.scalar
            .truth([self.inputs[0].read(inputs)?, self.inputs[1].read(inputs)?])
    }
}

impl Copy {
    #[inline(always)]
    fn value<N: Number>(&self, exe: &Executable, value: N) -> Option<N> {
        self.unary.as_ref().map_or(Some(value), |unary| {
            N::read(unary.unary(exe, value.value())?)
        })
    }
}

pub(super) struct Run<'plan, 'ledger> {
    bound: BoundRecurrence<'plan, 'ledger>,
    writes: Vec<(u32, Value)>,
}

impl Repeat {
    #[expect(
        clippy::too_many_arguments,
        reason = "one loop owns its source activation and shared resources"
    )]
    pub(super) fn run<'plan, 'ledger>(
        &'plan self,
        exe: &Executable,
        frame: &Values<'_, '_>,
        window: Window,
        ledger: &'ledger Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        invocation: &Invocation,
        activation: &DenseActivation<'_>,
        resources: &mut RepeatResources<'_>,
        memory_bound: u64,
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<Option<Run<'plan, 'ledger>>, HeapFault> {
        match frame.value(self.update.at) {
            Some(Value::Int(counter)) => self.run_typed(
                frame,
                exe,
                counter,
                window,
                ledger,
                arena,
                heap,
                invocation,
                activation,
                resources,
                memory_bound,
                tape,
            ),
            Some(Value::Float(counter)) => self.run_typed(
                frame,
                exe,
                counter.get(),
                window,
                ledger,
                arena,
                heap,
                invocation,
                activation,
                resources,
                memory_bound,
                tape,
            ),
            _ => {
                witnessed!(Repeated, Guard);
                Ok(None)
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the typed loop retains its authenticated source entry"
    )]
    fn run_typed<'plan, 'ledger, N: Number>(
        &'plan self,
        frame: &Values<'_, '_>,
        exe: &Executable,
        mut counter: N,
        window: Window,
        ledger: &'ledger Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        invocation: &Invocation,
        activation: &DenseActivation<'_>,
        resources: &mut RepeatResources<'_>,
        memory_bound: u64,
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<Option<Run<'plan, 'ledger>>, HeapFault> {
        let Some(calls) =
            self.proof
                .scope(exe, ledger, invocation, activation, resources.capacity())
        else {
            return Ok(None);
        };
        let Some(mut bound) = self
            .proof
            .recurrence
            .bind(ledger, arena, heap, invocation, activation, calls)?
        else {
            return Ok(None);
        };
        for copy in [&self.before, &self.after].into_iter().flatten() {
            if copy.keep && frame.value(copy.at).is_none() {
                witnessed!(Repeated, Guard);
                return Ok(None);
            }
        }
        let test = BinaryRun::<N>::of(&self.test, exe, frame);
        let update = BinaryRun::<N>::of(&self.update.operation, exe, frame);
        let fold = self
            .reduction
            .as_ref()
            .and_then(|reduce| ScalarRun::<N>::of(&reduce.fold, exe));
        let scale = self
            .reduction
            .as_ref()
            .and_then(|reduce| reduce.scale.as_ref())
            .and_then(|scale| BinaryRun::<N>::of(scale, exe, frame));
        let term = self
            .reduction
            .as_ref()
            .and_then(|reduce| reduce.term.bind::<N>(frame));
        let mut accumulator_value = self
            .reduction
            .as_ref()
            .and_then(|reduce| frame.value(reduce.at))
            .and_then(N::read);
        let (mut saved_before, mut saved_after) = (None, None);
        let mut iterations = 0u64;
        loop {
            let mut inputs = Inputs {
                counter,
                previous: counter,
                accumulator: None,
            };
            let Some(test) = test.as_ref().and_then(|test| test.truth(&inputs)) else {
                stop!(Guard);
            };
            if !test {
                stop!(End);
            }
            let before = match &self.before {
                Some(copy) => {
                    let Some(value) = copy.value(exe, counter) else {
                        stop!(Guard);
                    };
                    inputs.previous = value;
                    Some(value)
                }
                None => None,
            };
            let reduction = match &self.reduction {
                Some(reduce) => {
                    let Some(accumulator) = accumulator_value else {
                        stop!(Guard);
                    };
                    inputs.accumulator = Some(accumulator);
                    let term = match &reduce.scale {
                        Some(_) => {
                            let Some(value) =
                                scale.as_ref().and_then(|scale| scale.number(&inputs))
                            else {
                                stop!(Guard);
                            };
                            value
                        }
                        None => {
                            let Some(term) = term.as_ref().and_then(|term| term.read(&inputs))
                            else {
                                stop!(Guard);
                            };
                            term
                        }
                    };
                    let args = if reduce.reverse {
                        [term, accumulator]
                    } else {
                        [accumulator, term]
                    };
                    let Some(value) = fold.as_ref().and_then(|fold| fold.number(args)) else {
                        stop!(Guard);
                    };
                    Some((value, term))
                }
                None => None,
            };
            let Some(next) = update.as_ref().and_then(|update| update.number(&inputs)) else {
                stop!(Guard);
            };
            let after = match &self.after {
                Some(copy) => {
                    let Some(value) = copy.value(exe, next) else {
                        stop!(Guard);
                    };
                    Some(value)
                }
                None => None,
            };
            let actual = IterationValues {
                test,
                before: before.map(Number::value),
                scale: reduction.map(|(_, term)| term.value()),
                reduced: reduction.map(|(value, _)| value.value()),
                next: Some(next.value()),
                after: after.map(Number::value),
            };
            #[cfg(test)]
            let actual =
                if super::super::compiled::SUBSTITUTE_LOOP_RESULT.with(|enabled| enabled.get()) {
                    IterationValues {
                        next: match actual.next {
                            Some(Value::Int(value)) => value.checked_add(1).map(Value::Int),
                            other => other,
                        },
                        ..actual
                    }
                } else {
                    actual
                };
            let Some(evaluated) = self.proof.evaluate(
                exe,
                arena,
                ledger,
                invocation,
                bound.calls.as_ref(),
                bound.entries(),
                &actual,
            )?
            else {
                stop!(Guard);
            };
            if let Some(ceiling) = window.iteration_max
                && evaluated
                    .effects
                    .debts()
                    .iter()
                    .try_fold(0u64, |sum, debt| sum.checked_add(debt.units()?))
                    .is_none_or(|units| units > ceiling)
            {
                stop!(Guard);
            }
            let Some(room) = bound.evaluate(&evaluated.effects, memory_bound) else {
                stop!(Prefix);
            };
            if !resources.admit(
                evaluated.effects.debts(),
                evaluated.effects.depth(),
                evaluated.effects.occurrences(),
            ) {
                stop!(Prefix);
            }
            if let Some(tape) = tape {
                for debt in evaluated.effects.debts() {
                    debt.record(tape);
                }
            }
            bound.advance(room, evaluated.candidates)?;
            saved_before = before;
            if let Some((value, _)) = reduction {
                accumulator_value = Some(value);
            }
            counter = next;
            saved_after = after;
            iterations += 1;
        }
        let mut writes = Vec::new();
        if iterations != 0 {
            if let (Some(copy), Some(value)) = (&self.before, saved_before)
                && copy.keep
            {
                writes.push((copy.at, value.value()));
            }
            if let (Some(reduce), Some(value)) = (&self.reduction, accumulator_value) {
                writes.push((reduce.at, value.value()));
            }
            writes.push((self.update.at, counter.value()));
            if let (Some(copy), Some(value)) = (&self.after, saved_after)
                && copy.keep
            {
                writes.push((copy.at, value.value()));
            }
        }
        Ok((bound.iterations() != 0).then_some(Run { bound, writes }))
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn repeat_scope(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &super::super::compiled::RepeatSite,
        cost: u64,
        metered: u64,
    ) -> Result<u64, Stop> {
        use crate::admission::SourceResources;
        use crate::compile::room::publication::{Occurrence, Position as SourcePosition};
        if OBSERVING || self.deferred_end.is_some() || self.arena.boundary_due() {
            return Ok(0);
        }
        self.logical
            .flush(self.arena, self.heap)
            .map_err(|error| self.halt(error.into()))?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let Some(invocation) = frame
            .state
            .source
            .as_ref()
            .map(|source| source.invocation.clone())
        else {
            return Ok(0);
        };
        let Some(activation) = site
            .run
            .proof
            .capture(
                frame.state.callee.plan.code,
                &invocation,
                &ledger,
                self.heap,
                frame.slots,
            )
            .map_err(|error| self.halt(error.into()))?
        else {
            return Ok(0);
        };
        let safe_point = self.charged;
        let header = cost.saturating_add(metered);
        // The ordinary header following the admitted prefix remains unpaid.
        // Reserve its existing cost, including statement metering, unchanged.
        let Some(mut resources) = (SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound.saturating_sub(header),
            slice: self.slice_end.saturating_sub(header),
            depth: (self.base + u64::from(self.depth)).saturating_sub(1),
            limit: self.call_depth,
            safe_point,
        })
        .repeat() else {
            return Ok(0);
        };
        let pending = self.tape.map(|_| crate::observe::ChargeTape::new());
        let run = match site.run.run(
            self.exe,
            frame,
            Window {
                iteration_max: site.iteration_max,
            },
            &ledger,
            self.arena,
            self.heap,
            &invocation,
            &activation,
            &mut resources,
            self.memory_bound,
            pending.as_ref(),
        ) {
            Ok(Some(run)) => run,
            Ok(None) => return Ok(0),
            Err(error) => {
                self.scratch.exit.halt = Some(error.into());
                return Err(Stop);
            }
        };
        drop(activation);
        let Run { bound, writes } = run;
        let iterations = bound.iterations();
        let callee = frame.state.callee;
        let source = &self.exe.code(callee.plan.code).source_room;
        let Some(document) = self.exe.instances.get(&callee.link) else {
            self.scratch.exit.halt = Some(fault("loop source has no defining document"));
            return Err(Stop);
        };
        let Some(kernel) = lash_kernel_doc::KernelVersion::of(document.kernel()) else {
            self.scratch.exit.halt = Some(fault("loop source kernel is unsupported"));
            return Err(Stop);
        };
        let mut loops: Vec<_> = frame
            .state
            .control
            .iter()
            .filter_map(|control| match control {
                Position::Loop { id, started, .. } => {
                    Some((self.exe.stmt(*id).site.clone(), started.saturating_sub(1)))
                }
                _ => None,
            })
            .collect();
        if let Some((_, occurrence)) = loops.last_mut() {
            let Some(Position::Loop { started, .. }) = frame.state.control.last() else {
                unreachable!();
            };
            let Some(next) = started.checked_add(iterations) else {
                self.scratch.exit.halt = Some(fault("loop occurrence overflow"));
                return Err(Stop);
            };
            *occurrence = next.saturating_sub(1);
        }
        let frontier = match source.frontier(
            &ledger,
            self.heap,
            &invocation,
            callee.link,
            kernel,
            SourcePosition {
                at: site.run.proof.through.clone(),
                unpaid: Some(site.run.proof.first.clone()),
                recovery: site.run.proof.recovery,
                occurrence: Occurrence {
                    task: self.task,
                    invocation: invocation.id(),
                    loops,
                    action: None,
                },
            },
        ) {
            Ok(frontier) => frontier,
            Err(_) => {
                self.scratch.exit.halt = Some(fault("loop exit failed source authentication"));
                return Err(Stop);
            }
        };
        let registration = match self.arena.registration() {
            Ok(registration) => registration,
            Err(error) => {
                self.scratch.exit.halt =
                    Some(crate::machine::source_publication::native_fault(error).into());
                return Err(Stop);
            }
        };
        let transaction = match bound.prepare(registration, self.heap) {
            Ok(Some(transaction)) => transaction,
            Ok(None) => return Ok(0),
            Err(error) => {
                self.scratch.exit.halt = Some(error.into());
                return Err(Stop);
            }
        };
        let accepted = match resources.accept(transaction) {
            Ok(accepted) => accepted,
            Err(error) => {
                self.scratch.exit.halt = Some(error.into());
                return Err(Stop);
            }
        };
        drop(ledger);
        let published = accepted.commit(|transaction| {
            let balance = transaction.balance()?;
            let accepted = transaction
                .admit(self.memory_bound)
                .map_err(|_| HeapFault::Malformed("loop boundary admission changed".into()))?;
            accepted.publish(&mut store.borrow_mut(), |_, edits| {
                assert!(edits.is_empty(), "a scalar loop cannot edit a member");
                for (at, value) in writes {
                    frame.assign(at, value);
                }
                frame
                    .state
                    .source
                    .as_mut()
                    .unwrap_or_else(|| unreachable!("authenticated loop activation"))
                    .frontier = Some(frontier);
            })?;
            Ok::<_, HeapFault>(balance)
        });
        let balance = published.map_err(|error| self.halt(error.into()))?;
        self.logical_memory
            .set(balance.total().map_err(|error| self.halt(error.into()))?);
        if let (Some(tape), Some(pending)) = (self.tape, pending) {
            for weight in pending.steps() {
                tape.push(weight);
            }
        }
        if let Some(recorder) = self.recorder {
            recorder.typed(iterations);
        }
        #[cfg(any(test, feature = "perf-witness"))]
        execution::statements_here(
            &self.scratch.census,
            iterations
                * (1 + u64::from(site.run.before.is_some())
                    + u64::from(site.run.reduction.is_some())
                    + u64::from(site.run.after.is_some())),
            self.scratch.counting,
        );
        #[cfg(any(test, feature = "testing"))]
        for _ in 0..iterations {
            witnessed!(Repeated, Commit);
        }
        #[cfg(test)]
        {
            super::REPEATED_TEMPLATE.with(|count| count.set(count.get() + iterations));
            super::super::compiled::LOOP_PUBLICATIONS.with(|count| count.set(count.get() + 1));
        }
        Ok(iterations)
    }
}
