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

struct Inputs {
    counter: Value,
    previous: Value,
    accumulator: Value,
}

impl Input {
    fn bind(self, frame: &Values<'_, '_>) -> Option<Self> {
        match self {
            Self::Invariant(at) => frame.value(at).map(Self::Literal),
            _ => Some(self),
        }
    }

    #[inline(always)]
    fn read(self, values: &Inputs) -> Option<Value> {
        match self {
            Self::Counter => Some(values.counter),
            Self::Previous => Some(values.previous),
            Self::Accumulator => Some(values.accumulator),
            Self::Literal(value) => Some(value),
            Self::Invariant(_) => None,
        }
    }
}

struct ScalarRun<'e> {
    pair: lash_kernel_doc::scalar::Pair,
    table: Option<&'e crate::compile::ScalarTable>,
}

impl<'e> ScalarRun<'e> {
    fn of(scalar: &Scalar, exe: &'e Executable, charged: bool) -> Option<Self> {
        Some(Self {
            pair: scalar.site.pair(exe)?,
            table: charged.then(|| scalar.site.table(exe)),
        })
    }

    #[inline(always)]
    fn binary(&self, args: [Value; 2]) -> Option<(Value, u64)> {
        let (value, units) =
            super::super::compiled::typed_table(&self.pair, args[0], args[1], self.table)?;
        Some((value, units?))
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

struct BinaryRun<'e> {
    scalar: ScalarRun<'e>,
    inputs: [Input; 2],
}

impl<'e> BinaryRun<'e> {
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
    fn value(&self, inputs: &Inputs) -> Option<(Value, u64)> {
        self.scalar
            .binary([self.inputs[0].read(inputs)?, self.inputs[1].read(inputs)?])
    }
}

impl Copy {
    #[inline(always)]
    fn value(&self, exe: &Executable, value: Value, charged: bool) -> Option<(Value, u64)> {
        self.unary
            .as_ref()
            .map_or(Some((value, 0)), |unary| unary.unary(exe, value, charged))
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
        let test = BinaryRun::of(&self.test, exe, frame, self.charged);
        let update = BinaryRun::of(&self.update.operation, exe, frame, self.charged);
        let fold = self
            .reduction
            .as_ref()
            .and_then(|reduce| ScalarRun::of(&reduce.fold, exe, self.charged));
        let scale = self
            .reduction
            .as_ref()
            .and_then(|reduce| reduce.scale.as_ref())
            .and_then(|scale| BinaryRun::of(scale, exe, frame, self.charged));
        let term = self
            .reduction
            .as_ref()
            .and_then(|reduce| reduce.term.bind(frame));
        let mut counter_value = frame.value(self.update.at);
        let mut accumulator_value = self
            .reduction
            .as_ref()
            .and_then(|reduce| frame.value(reduce.at));
        let (mut saved_before, mut saved_after) = (None, None);
        let (mut iterations, mut units) = (0u64, 0u64);
        loop {
            if budget.admit(units).is_none() {
                stop!(Prefix);
            }
            let Some(counter @ (Value::Int(_) | Value::Float(_))) = counter_value else {
                stop!(Guard);
            };
            let mut inputs = Inputs {
                counter,
                previous: counter,
                accumulator: Value::Null,
            };
            let Some((test, test_fee)) = test.as_ref().and_then(|test| test.value(&inputs)) else {
                stop!(Guard);
            };
            match test {
                Value::Bool(true) => {}
                Value::Bool(false) => stop!(End),
                _ => stop!(Guard),
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
                    inputs.accumulator = accumulator;
                    let (term, scale_fee) = match &reduce.scale {
                        Some(_) => {
                            let Some(answer) =
                                scale.as_ref().and_then(|scale| scale.value(&inputs))
                            else {
                                stop!(Guard);
                            };
                            answer
                        }
                        None => {
                            let Some(term) = term.and_then(|term| term.read(&inputs)) else {
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
                    let Some((value, fold_fee)) = fold.as_ref().and_then(|fold| fold.binary(args))
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
            let Some((next @ (Value::Int(_) | Value::Float(_)), update_fee)) =
                update.as_ref().and_then(|update| update.value(&inputs))
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
            counter_value = Some(next);
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
                frame.assign(copy.at, value);
            }
            if let (Some(reduce), Some(value)) = (&self.reduction, accumulator_value) {
                frame.assign(reduce.at, value);
            }
            if let Some(value) = counter_value {
                frame.assign(self.update.at, value);
            }
            if let (Some(copy), Some(value)) = (&self.after, saved_after)
                && copy.keep
            {
                frame.assign(copy.at, value);
            }
        }
        (iterations, units)
    }
}
