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
    fn read(self, frame: &Values<'_, '_>, values: &Inputs) -> Option<Value> {
        match self {
            Self::Counter => Some(values.counter),
            Self::Previous => Some(values.previous),
            Self::Accumulator => Some(values.accumulator),
            Self::Literal(value) => Some(value),
            Self::Invariant(at) => frame.value(at),
        }
    }
}

impl Scalar {
    fn arguments_fit(&self, exe: &Executable, args: &[Value]) -> bool {
        !self.site.checked
            || exe
                .lib(self.site.lib)
                .header
                .signature
                .params
                .iter()
                .zip(args)
                .all(|(param, value)| crate::compile::import_result_fits(&param.ty, *value))
    }

    fn result_fits(&self, exe: &Executable, value: Value) -> bool {
        !self.site.checked
            || crate::compile::import_result_fits(
                &exe.lib(self.site.lib).header.signature.result,
                value,
            )
    }

    fn binary(&self, exe: &Executable, args: [Value; 2], charged: bool) -> Option<(Value, u64)> {
        if !self.arguments_fit(exe, &args) {
            return None;
        }
        let (value, units) = super::super::compiled::typed(
            &self.site,
            &self.site.pair(exe)?,
            args[0],
            args[1],
            charged,
            exe,
        )?;
        self.result_fits(exe, value).then_some((value, units?))
    }

    fn unary(&self, exe: &Executable, value: Value, charged: bool) -> Option<(Value, u64)> {
        let (answer, units) = super::super::compiled::unary(&self.site, value, exe)?;
        matches!(
            (value, answer),
            (Value::Int(_), Value::Int(_)) | (Value::Float(_), Value::Float(_))
        )
        .then_some((answer, if charged { units? } else { 0 }))
    }
}

impl Binary {
    fn value(
        &self,
        exe: &Executable,
        frame: &Values<'_, '_>,
        inputs: &Inputs,
        charged: bool,
    ) -> Option<(Value, u64)> {
        self.scalar.binary(
            exe,
            [
                self.inputs[0].read(frame, inputs)?,
                self.inputs[1].read(frame, inputs)?,
            ],
            charged,
        )
    }
}

impl Copy {
    fn value(&self, exe: &Executable, value: Value, charged: bool) -> Option<(Value, u64)> {
        self.unary
            .as_ref()
            .map_or(Some((value, 0)), |unary| unary.unary(exe, value, charged))
    }
}

impl Charge {
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
        let (mut iterations, mut units) = (0u64, 0u64);
        loop {
            if budget.admit(units).is_none() {
                stop!(Prefix);
            }
            let Some(counter @ (Value::Int(_) | Value::Float(_))) = frame.value(self.update.at)
            else {
                stop!(Guard);
            };
            let mut inputs = Inputs {
                counter,
                previous: counter,
                accumulator: Value::Null,
            };
            let Some((test, test_fee)) = self.test.value(exe, frame, &inputs, self.charged) else {
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
                    let Some(accumulator) = frame.value(reduce.at) else {
                        stop!(Guard);
                    };
                    inputs.accumulator = accumulator;
                    let (term, scale_fee) = match &reduce.scale {
                        Some(scale) => {
                            let Some(answer) = scale.value(exe, frame, &inputs, self.charged)
                            else {
                                stop!(Guard);
                            };
                            answer
                        }
                        None => {
                            let Some(term) = reduce.term.read(frame, &inputs) else {
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
                    let Some((value, fold_fee)) = reduce.fold.binary(exe, args, self.charged)
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
            let Some((next @ (Value::Int(_) | Value::Float(_)), update_fee)) = self
                .update
                .operation
                .value(exe, frame, &inputs, self.charged)
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
            if let (Some(copy), Some((value, _))) = (&self.before, before)
                && copy.keep
            {
                frame.assign(copy.at, value);
            }
            if let (Some(reduce), Some((value, _, _))) = (&self.reduction, reduction) {
                frame.assign(reduce.at, value);
            }
            frame.assign(self.update.at, next);
            if let (Some(copy), Some((value, _))) = (&self.after, after)
                && copy.keep
            {
                frame.assign(copy.at, value);
            }
            if let Some(tape) = tape {
                self.test_charge.record(tape, &[test_fee]);
                if let (Some(copy), Some((_, fee))) = (&self.before, before) {
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
        (iterations, units)
    }
}
