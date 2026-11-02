//! The fixed graph executes in source order and publishes only a complete
//! admitted iteration. A miss leaves that iteration to ordinary packets.

use super::super::code::repeat::{Binary, Charge, Copy, Input, Repeat, Scalar};
use super::*;
use crate::compile::Event;

macro_rules! stop {
    ($exit:ident) => {{
        witnessed!(Repeated, $exit);
        break;
    }};
}

pub(super) struct Window {
    pub(super) charged: u64,
    pub(super) bound: u64,
    pub(super) slice_end: u64,
    pub(super) iteration_max: u64,
    pub(super) loop_test: u64,
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
    fn fee(table: &crate::compile::ScalarTable, a: Self, b: Self, result: Value) -> Option<u64>;
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
    #[inline(always)]
    fn fee(table: &crate::compile::ScalarTable, a: Self, b: Self, result: Value) -> Option<u64> {
        table.units(super::super::sizes(&[a.value(), b.value()], &result))
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
    #[inline(always)]
    fn fee(table: &crate::compile::ScalarTable, _: Self, _: Self, _: Value) -> Option<u64> {
        table.units(0)
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

struct ScalarRun<'e, N> {
    operation: lash_kernel_doc::scalar::ScalarOp,
    table: Option<&'e crate::compile::ScalarTable>,
    domain: std::marker::PhantomData<N>,
}

impl<'e, N: Number> ScalarRun<'e, N> {
    fn of(scalar: &Scalar, exe: &'e Executable, charged: bool) -> Option<Self> {
        Some(Self {
            operation: N::operation(&scalar.site.pair(exe)?)?,
            table: charged.then(|| scalar.site.table(exe)),
            domain: std::marker::PhantomData,
        })
    }

    #[inline(always)]
    fn number(&self, args: [N; 2]) -> Option<(N, u64)> {
        let value = N::number(self.operation, args[0], args[1])?;
        let units = self.table.map_or(Some(0), |table| {
            N::fee(table, args[0], args[1], value.value())
        })?;
        Some((value, units))
    }

    #[inline(always)]
    fn truth(&self, args: [N; 2]) -> Option<(bool, u64)> {
        let value = N::truth(self.operation, args[0], args[1])?;
        let units = self.table.map_or(Some(0), |table| {
            N::fee(table, args[0], args[1], Value::Bool(value))
        })?;
        Some((value, units))
    }
}

impl Scalar {
    #[inline(always)]
    fn unary(&self, exe: &Executable, value: Value, charged: bool) -> Option<(Value, u64)> {
        let (answer, units) = super::super::compiled::unary(&self.site, value, exe)?;
        matches!(
            (value, answer),
            (Value::Int(_), Value::Int(_)) | (Value::Float(_), Value::Float(_))
        )
        .then_some((answer, if charged { units? } else { 0 }))
    }
}

struct BinaryRun<'e, N> {
    scalar: ScalarRun<'e, N>,
    inputs: [BoundInput<N>; 2],
}

impl<'e, N: Number> BinaryRun<'e, N> {
    fn of(
        binary: &Binary,
        exe: &'e Executable,
        frame: &Values<'_, '_>,
        charged: bool,
    ) -> Option<Self> {
        Some(Self {
            scalar: ScalarRun::of(&binary.scalar, exe, charged)?,
            inputs: [binary.inputs[0].bind(frame)?, binary.inputs[1].bind(frame)?],
        })
    }

    #[inline(always)]
    fn number(&self, inputs: &Inputs<N>) -> Option<(N, u64)> {
        self.scalar
            .number([self.inputs[0].read(inputs)?, self.inputs[1].read(inputs)?])
    }

    #[inline(always)]
    fn truth(&self, inputs: &Inputs<N>) -> Option<(bool, u64)> {
        self.scalar
            .truth([self.inputs[0].read(inputs)?, self.inputs[1].read(inputs)?])
    }
}

impl Copy {
    #[inline(always)]
    fn value<N: Number>(&self, exe: &Executable, value: N, charged: bool) -> Option<(N, u64)> {
        self.unary.as_ref().map_or(Some((value, 0)), |unary| {
            let (value, fee) = unary.unary(exe, value.value(), charged)?;
            Some((N::read(value)?, fee))
        })
    }
}

impl Charge {
    #[inline(always)]
    fn units(&self, formulas: &[u64]) -> Option<u64> {
        formulas
            .iter()
            .try_fold(self.fixed, |sum, units| sum.checked_add(*units))
    }

    fn record(&self, tape: &crate::observe::ChargeTape, formulas: &[u64]) {
        let mut formulas = formulas.iter();
        for event in &self.events {
            match event {
                Event::Fixed(weight) => tape.push(*weight),
                Event::Formula => {
                    if let Some(units) = formulas.next()
                        && *units != 0
                    {
                        tape.push(Weight::Atomic(*units));
                    }
                }
                _ => unreachable!("a fixed template has no dynamic charge arm or call"),
            }
        }
    }
}

impl Repeat {
    pub(super) fn run(
        &self,
        exe: &Executable,
        frame: &mut Values<'_, '_>,
        window: Window,
        tape: Option<&crate::observe::ChargeTape>,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
        counted: bool,
    ) -> (u64, u64) {
        match frame.value(self.update.at) {
            Some(Value::Int(counter)) => self.run_typed(
                frame,
                exe,
                counter,
                window,
                tape,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
                counted,
            ),
            Some(Value::Float(counter)) => self.run_typed(
                frame,
                exe,
                counter.get(),
                window,
                tape,
                #[cfg(any(test, feature = "perf-witness"))]
                census,
                counted,
            ),
            _ => {
                witnessed!(Repeated, Guard);
                (0, 0)
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the typed loop retains its admitted source window"
    )]
    fn run_typed<N: Number>(
        &self,
        frame: &mut Values<'_, '_>,
        exe: &Executable,
        mut counter: N,
        window: Window,
        tape: Option<&crate::observe::ChargeTape>,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
        counted: bool,
    ) -> (u64, u64) {
        let Some(budget) =
            crate::admission::Window::new(window.charged, window.bound, window.slice_end)
                .repeating(window.iteration_max)
        else {
            witnessed!(Repeated, Prefix);
            return (0, 0);
        };
        for copy in [&self.before, &self.after].into_iter().flatten() {
            if copy.keep && frame.value(copy.at).is_none() {
                witnessed!(Repeated, Guard);
                return (0, 0);
            }
        }
        let test = BinaryRun::<N>::of(&self.test, exe, frame, self.charged);
        let update = BinaryRun::<N>::of(&self.update.operation, exe, frame, self.charged);
        let fold = self
            .reduction
            .as_ref()
            .and_then(|reduce| ScalarRun::<N>::of(&reduce.fold, exe, self.charged));
        let scale = self
            .reduction
            .as_ref()
            .and_then(|reduce| reduce.scale.as_ref())
            .and_then(|scale| BinaryRun::<N>::of(scale, exe, frame, self.charged));
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
        let (mut iterations, mut units) = (0u64, 0u64);
        loop {
            if budget.admit(units).is_none() {
                stop!(Prefix);
            }
            let mut inputs = Inputs {
                counter,
                previous: counter,
                accumulator: None,
            };
            let Some((test, test_fee)) = test.as_ref().and_then(|test| test.truth(&inputs)) else {
                stop!(Guard);
            };
            if !test {
                stop!(End);
            }
            let Some(mut iteration) = self.test_charge.units(&[test_fee]) else {
                stop!(Guard);
            };
            let before = match &self.before {
                Some(copy) => {
                    let Some((value, fee)) = copy.value(exe, counter, self.charged) else {
                        stop!(Guard);
                    };
                    let Some(sum) = copy
                        .charge
                        .units(&[fee])
                        .and_then(|n| iteration.checked_add(n))
                    else {
                        stop!(Guard);
                    };
                    iteration = sum;
                    inputs.previous = value;
                    Some((value, fee))
                }
                None => None,
            };
            let reduction = match &self.reduction {
                Some(reduce) => {
                    let Some(accumulator) = accumulator_value else {
                        stop!(Guard);
                    };
                    inputs.accumulator = Some(accumulator);
                    let (term, scale_fee) = match &reduce.scale {
                        Some(_) => {
                            let Some((value, fee)) =
                                scale.as_ref().and_then(|scale| scale.number(&inputs))
                            else {
                                stop!(Guard);
                            };
                            (value, fee)
                        }
                        None => {
                            let Some(term) = term.as_ref().and_then(|term| term.read(&inputs))
                            else {
                                stop!(Guard);
                            };
                            (term, 0)
                        }
                    };
                    let args = if reduce.reverse {
                        [term, accumulator]
                    } else {
                        [accumulator, term]
                    };
                    let Some((value, fold_fee)) = fold.as_ref().and_then(|fold| fold.number(args))
                    else {
                        stop!(Guard);
                    };
                    let fees = if reduce.scale.is_some() {
                        &[scale_fee, fold_fee][..]
                    } else {
                        &[fold_fee][..]
                    };
                    let Some(sum) = reduce
                        .charge
                        .units(fees)
                        .and_then(|n| iteration.checked_add(n))
                    else {
                        stop!(Guard);
                    };
                    iteration = sum;
                    Some((value, scale_fee, fold_fee))
                }
                None => None,
            };
            let Some((next, update_fee)) =
                update.as_ref().and_then(|update| update.number(&inputs))
            else {
                stop!(Guard);
            };
            let Some(sum) = self
                .update
                .charge
                .units(&[update_fee])
                .and_then(|n| iteration.checked_add(n))
            else {
                stop!(Guard);
            };
            iteration = sum;
            let after = match &self.after {
                Some(copy) => {
                    let Some((value, fee)) = copy.value(exe, next, self.charged) else {
                        stop!(Guard);
                    };
                    let Some(sum) = copy
                        .charge
                        .units(&[fee])
                        .and_then(|n| iteration.checked_add(n))
                    else {
                        stop!(Guard);
                    };
                    iteration = sum;
                    Some((value, fee))
                }
                None => None,
            };
            let Some(iteration) = iteration.checked_add(window.loop_test) else {
                stop!(Guard);
            };
            // The independently verified source maximum is the admission
            // authority. A changed projection cannot spend beyond that proof.
            if iteration > window.iteration_max {
                stop!(Guard);
            }
            let Some(total) = units.checked_add(iteration) else {
                stop!(Guard);
            };
            saved_before = before.map(|(value, _)| value);
            if let Some((value, _, _)) = reduction {
                accumulator_value = Some(value);
            }
            counter = next;
            saved_after = after.map(|(value, _)| value);
            if let Some(tape) = tape {
                self.test_charge.record(tape, &[test_fee]);
                if !self.copy_after_reduction
                    && let (Some(copy), Some((_, fee))) = (&self.before, before)
                {
                    copy.charge.record(
                        tape,
                        copy.unary
                            .as_ref()
                            .map_or(&[][..], |_| std::slice::from_ref(&fee)),
                    );
                }
                if let (Some(reduce), Some((_, scale, fold))) = (&self.reduction, reduction) {
                    let fees = [scale, fold];
                    reduce
                        .charge
                        .record(tape, &fees[usize::from(reduce.scale.is_none())..]);
                }
                if self.copy_after_reduction
                    && let (Some(copy), Some((_, fee))) = (&self.before, before)
                {
                    copy.charge.record(
                        tape,
                        copy.unary
                            .as_ref()
                            .map_or(&[][..], |_| std::slice::from_ref(&fee)),
                    );
                }
                self.update.charge.record(tape, &[update_fee]);
                if let (Some(copy), Some((_, fee))) = (&self.after, after) {
                    copy.charge.record(
                        tape,
                        copy.unary
                            .as_ref()
                            .map_or(&[][..], |_| std::slice::from_ref(&fee)),
                    );
                }
                if window.loop_test != 0 {
                    tape.push(Weight::Atomic(window.loop_test));
                }
            }
            #[cfg(any(test, feature = "perf-witness"))]
            if counted {
                execution::statements_here(
                    census,
                    1 + u64::from(self.before.is_some())
                        + u64::from(self.reduction.is_some())
                        + u64::from(self.after.is_some()),
                    true,
                );
            }
            #[cfg(not(any(test, feature = "perf-witness")))]
            let _ = counted;
            witnessed!(Repeated, Commit);
            units = total;
            iterations += 1;
        }
        if iterations != 0 {
            if let (Some(copy), Some(value)) = (&self.before, saved_before)
                && copy.keep
            {
                frame.assign(copy.at, value.value());
            }
            if let (Some(reduce), Some(value)) = (&self.reduction, accumulator_value) {
                frame.assign(reduce.at, value.value());
            }
            frame.assign(self.update.at, counter.value());
            if let (Some(copy), Some(value)) = (&self.after, saved_after)
                && copy.keep
            {
                frame.assign(copy.at, value.value());
            }
        }
        (iterations, units)
    }
}
