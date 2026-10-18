//! Charge formulas compiled for the machine: each operand resolved to the
//! argument it names, and the formula a postfix program (`K-CHG-003`).

use lash_kernel_doc as doc;
use lash_kernel_doc::Value;

use crate::data::{holds_nothing, size_one};

/// What a formula's operand measures: an argument by its position, the
/// result, or a parameter the function does not have, which measures 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Arg(usize),
    Result,
    Nothing,
}

/// How many of a formula's distinct measurements an evaluation holds
/// before it allocates.
const KEPT_MEASUREMENTS: usize = 8;
/// How deep a formula's evaluation stack may be before it is allocated.
const STACK: usize = 16;

/// A formula with its operands resolved to the arguments they name
/// (`K-CHG-003`), as a postfix program. It computes what
/// [`doc::Formula::evaluate`] computes: every amount is a non-negative
/// saturating sum, product, maximum or minimum, which no grouping or order
/// of its terms changes, so nested sums and products are flattened and
/// their constants folded. Each distinct operand and measure is measured
/// once per evaluation.
pub(crate) struct Plan {
    ops: Vec<Op>,
    measurements: Vec<(Source, doc::Measure)>,
    depth: usize,
    /// A sum of measurements, optionally with one pair combined by a
    /// minimum: the folded constant and the pair spare the program.
    linear: Option<Linear>,
}

/// An instruction of a plan's program. Each combining one takes the two
/// amounts on the top of the stack: a sum of more terms is its terms
/// combined pairwise from the left, as the saturating fold computes it.
enum Linear {
    Sum(u64),
    /// A sum with one minimum of two adjacent measurements. Their
    /// indexes and the folded constant are derived from the program.
    Minimum {
        constant: u64,
        at: usize,
    },
}

#[derive(Clone, Copy)]
enum Op {
    Constant(u64),
    /// The measurement of that index.
    Measure(usize),
    Add,
    Multiply,
    Max,
    Min,
    CeilLog2,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    Sum,
    Product,
}

impl Plan {
    pub(crate) fn new(formula: &doc::Formula, params: &[doc::Param]) -> Self {
        Self::prepare(formula, params, false)
    }

    fn prepare(formula: &doc::Formula, params: &[doc::Param], shallow: bool) -> Self {
        let mut plan = Self {
            ops: Vec::new(),
            measurements: Vec::new(),
            depth: 0,
            linear: None,
        };
        plan.term(formula, params, shallow);
        plan.linear = plan.linear();
        let mut depth = 0usize;
        for op in &plan.ops {
            depth = match op {
                Op::Constant(_) | Op::Measure(_) => depth + 1,
                Op::CeilLog2 => depth,
                Op::Add | Op::Multiply | Op::Max | Op::Min => depth - 1,
            };
            plan.depth = plan.depth.max(depth);
        }
        plan
    }

    /// Recognizes a sum of measurements in order, optionally with one
    /// minimum pair, followed by its folded constant and additions.
    fn linear(&self) -> Option<Linear> {
        let mut measured = 0;
        let mut terms = 0;
        let mut minimum = None;
        let mut end = 0;
        for (position, op) in self.ops.iter().enumerate() {
            match op {
                Op::Measure(index) if *index == measured => {
                    measured += 1;
                    terms += 1;
                }
                Op::Min
                    if minimum.is_none()
                        && end >= 2
                        && matches!(self.ops[end - 1], Op::Measure(_))
                        && matches!(self.ops[end - 2], Op::Measure(_)) =>
                {
                    minimum = Some(measured - 2);
                    terms -= 1;
                }
                _ => break,
            }
            end = position + 1;
        }
        let (constant, rest) = match &self.ops[end..] {
            [Op::Constant(constant), rest @ ..] => {
                terms += 1;
                (*constant, rest)
            }
            rest => (0, rest),
        };
        if terms == 0
            || measured != self.measurements.len()
            || rest.len() + 1 != terms
            || !rest.iter().all(|op| matches!(op, Op::Add))
        {
            return None;
        }
        Some(match minimum {
            Some(at) => Linear::Minimum { constant, at },
            None => Linear::Sum(constant),
        })
    }

    /// Appends a formula's program.
    fn term(&mut self, formula: &doc::Formula, params: &[doc::Param], shallow: bool) {
        match formula {
            doc::Formula::Constant(amount) => self.ops.push(Op::Constant(*amount)),
            doc::Formula::CeilLog2(term) => {
                self.term(term, params, shallow);
                self.ops.push(Op::CeilLog2);
            }
            doc::Formula::Size(operand) => self.measure(operand, doc::Measure::Size, params),
            doc::Formula::DeepSize(operand) => self.measure(
                operand,
                if shallow {
                    doc::Measure::Size
                } else {
                    doc::Measure::DeepSize
                },
                params,
            ),
            doc::Formula::NestedSize(operand) => {
                if shallow {
                    self.ops.push(Op::Constant(0));
                } else {
                    self.measure(operand, doc::Measure::NestedSize, params);
                }
            }
            doc::Formula::Magnitude(operand) => {
                self.measure(operand, doc::Measure::Magnitude, params)
            }
            doc::Formula::Sum(items) => self.fold(Fold::Sum, items, params, shallow),
            doc::Formula::Product(items) => self.fold(Fold::Product, items, params, shallow),
            doc::Formula::Max(items) => self.extreme(Op::Max, items, params, shallow),
            doc::Formula::Min(items) => self.extreme(Op::Min, items, params, shallow),
        }
    }

    /// Appends a sum or a product, with the terms of the sums or products
    /// it holds as its own and its constants folded into one.
    fn fold(&mut self, fold: Fold, items: &[doc::Formula], params: &[doc::Param], shallow: bool) {
        let (identity, combine): (u64, fn(u64, u64) -> u64) = match fold {
            Fold::Sum => (0, u64::saturating_add),
            Fold::Product => (1, u64::saturating_mul),
        };
        let mut constant = identity;
        let mut count = 0;
        let mut pending: Vec<&doc::Formula> = items.iter().rev().collect();
        while let Some(item) = pending.pop() {
            match (fold, item) {
                (_, doc::Formula::Constant(amount)) => constant = combine(constant, *amount),
                (_, doc::Formula::NestedSize(_)) if shallow => constant = combine(constant, 0),
                (Fold::Sum, doc::Formula::Sum(inner))
                | (Fold::Product, doc::Formula::Product(inner)) => {
                    pending.extend(inner.iter().rev());
                }
                _ => {
                    self.term(item, params, shallow);
                    count += 1;
                }
            }
        }
        if constant != identity || count == 0 {
            self.ops.push(Op::Constant(constant));
            count += 1;
        }
        let combine = match fold {
            Fold::Sum => Op::Add,
            Fold::Product => Op::Multiply,
        };
        self.ops.extend(std::iter::repeat_n(combine, count - 1));
    }

    /// Appends the largest or the smallest of some amounts; of none, 0.
    fn extreme(&mut self, op: Op, items: &[doc::Formula], params: &[doc::Param], shallow: bool) {
        if items.is_empty() {
            self.ops.push(Op::Constant(0));
            return;
        }
        for (index, item) in items.iter().enumerate() {
            self.term(item, params, shallow);
            if index > 0 {
                self.ops.push(op);
            }
        }
    }

    fn measure(&mut self, operand: &doc::Operand, measure: doc::Measure, params: &[doc::Param]) {
        let source = match operand {
            doc::Operand::Result => Source::Result,
            doc::Operand::Param(name) => params
                .iter()
                .position(|param| param.name == *name)
                .map_or(Source::Nothing, Source::Arg),
        };
        if source == Source::Nothing {
            self.ops.push(Op::Constant(0));
            return;
        }
        let index = match self
            .measurements
            .iter()
            .position(|known| *known == (source, measure))
        {
            Some(index) => index,
            None => {
                self.measurements.push((source, measure));
                self.measurements.len() - 1
            }
        };
        self.ops.push(Op::Measure(index));
    }

    /// The measurements the amount reads, each once.
    pub(crate) fn measurements(&self) -> &[(Source, doc::Measure)] {
        &self.measurements
    }

    /// Computes the amount, asking `measure` for each measurement. A
    /// measurement reads the heap as it is, which nothing changes while
    /// the formula is computed.
    pub(crate) fn evaluate(&self, mut measure: impl FnMut(Source, doc::Measure) -> u64) -> u64 {
        match self.linear {
            Some(Linear::Sum(constant)) => {
                return self
                    .measurements
                    .iter()
                    .fold(constant, |sum, (source, kind)| {
                        sum.saturating_add(measure(*source, *kind))
                    });
            }
            Some(Linear::Minimum { constant, at }) => {
                let mut sum = constant;
                for (source, kind) in &self.measurements[..at] {
                    sum = sum.saturating_add(measure(*source, *kind));
                }
                let (left, right) = (self.measurements[at], self.measurements[at + 1]);
                sum = sum.saturating_add(measure(left.0, left.1).min(measure(right.0, right.1)));
                for (source, kind) in &self.measurements[at + 2..] {
                    sum = sum.saturating_add(measure(*source, *kind));
                }
                return sum;
            }
            None => {}
        }
        let mut kept = [0; KEPT_MEASUREMENTS];
        let mut more = Vec::new();
        let measured = match kept.get_mut(..self.measurements.len()) {
            Some(kept) => kept,
            None => {
                more.resize(self.measurements.len(), 0);
                &mut more[..]
            }
        };
        for (amount, (source, kind)) in measured.iter_mut().zip(&self.measurements) {
            *amount = measure(*source, *kind);
        }
        if self.depth <= STACK {
            self.run(&mut [0; STACK], measured)
        } else {
            self.run(&mut vec![0; self.depth], measured)
        }
    }

    /// Runs the program over the measurements.
    fn run(&self, stack: &mut [u64], measured: &[u64]) -> u64 {
        let mut top = 0;
        for op in &self.ops {
            let amount = match *op {
                Op::Constant(amount) => amount,
                Op::Measure(index) => measured[index],
                Op::CeilLog2 => {
                    top -= 1;
                    u64::from(u64::BITS - stack[top].saturating_sub(1).leading_zeros())
                }
                Op::Add | Op::Multiply | Op::Max | Op::Min => {
                    top -= 1;
                    let (left, right) = (stack[top - 1], stack[top]);
                    top -= 1;
                    match *op {
                        Op::Add => left.saturating_add(right),
                        Op::Multiply => left.saturating_mul(right),
                        Op::Max => left.max(right),
                        _ => left.min(right),
                    }
                }
            };
            stack[top] = amount;
            top += 1;
        }
        stack.first().copied().unwrap_or(0)
    }
}

/// A native function's charge formula prepared for the calls its fast path
/// answers (`K-CHG-003`). It is derived from the formula when the library is
/// prepared, so it charges what the formula charges whatever the formula
/// says: for values that hold nothing a deep size is a size and a nested
/// size 0, and a formula that measures only scalar sizes is closed over
/// sizes 1 and 2, including zero and nonzero small integers.
pub(crate) struct Fast {
    /// Completion units indexed by the prepared guard's scalar size mask.
    scalar: Option<Box<[u64; 16]>>,
    /// The formula as it computes for values that hold nothing
    /// ([`doc::Formula::shallow`]).
    shallow: Plan,
    /// What `shallow` computes when every value it measures has size 1; for
    /// a formula that measures a magnitude, nothing.
    unit: Option<u64>,
    /// Every combination of measured sizes 1 and 2. Zero small integers
    /// have size 1; nonzero small integers have size 2 (K-CHG-004).
    small: Option<Box<[u64]>>,
    /// The values the formula measures by deep size: `shallow` holds when
    /// none of them holds another value.
    deep: Box<[Source]>,
    /// The values the formula measures by nested size: `shallow` holds when
    /// none of them is a tuple or an error.
    nested: Box<[Source]>,
}

/// How a call the fast path answered is charged: an amount known when the
/// library was prepared, or a plan to compute over the call's values.
pub(crate) enum Charge<'p> {
    Units(u64),
    Plan(&'p Plan),
}

impl Fast {
    pub(crate) fn new(formula: &doc::Formula, params: &[doc::Param], charge: &Plan) -> Self {
        let shallow = Plan::prepare(formula, params, true);
        let unit = shallow
            .measurements
            .iter()
            .all(|(_, measure)| *measure == doc::Measure::Size)
            .then(|| shallow.evaluate(|_, _| 1));
        let small =
            (unit.is_some() && shallow.measurements.len() <= KEPT_MEASUREMENTS).then(|| {
                (0..(1usize << shallow.measurements.len()))
                    .map(|mask| {
                        shallow.evaluate(|source, _| {
                            let index = shallow
                                .measurements
                                .iter()
                                .position(|(held, _)| *held == source)
                                .unwrap_or(0);
                            1 + ((mask >> index) & 1) as u64
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            });
        let measured = |kind: doc::Measure| -> Box<[Source]> {
            charge
                .measurements
                .iter()
                .filter(|(_, measure)| *measure == kind)
                .map(|(source, _)| *source)
                .collect()
        };
        let scalar = (params.len() <= 3
            && charge
                .measurements
                .iter()
                .all(|(_, kind)| *kind != doc::Measure::Magnitude))
        .then(|| {
            Box::new(std::array::from_fn(|mask| {
                charge.evaluate(|source, kind| {
                    if kind == doc::Measure::NestedSize {
                        return 0;
                    }
                    let bit = match source {
                        Source::Arg(index) => index,
                        Source::Result => 3,
                        Source::Nothing => return 0,
                    };
                    1 + ((mask >> bit) & 1) as u64
                })
            }))
        });
        Self {
            scalar,
            unit,
            small,
            deep: measured(doc::Measure::DeepSize),
            nested: measured(doc::Measure::NestedSize),
            shallow,
        }
    }

    /// The exact formula for scalar sizes already certified by the guard.
    /// Formulas measuring magnitude retain the general fallback.
    #[inline]
    pub(crate) fn scalar_units(&self, mask: u8) -> Option<u64> {
        self.scalar.as_ref()?.get(usize::from(mask)).copied()
    }

    /// How to charge a call with `args` that returned `result`: what
    /// `charge`, the plan this was derived from, computes for them.
    #[inline(always)]
    pub(crate) fn charge<'p>(
        &'p self,
        charge: &'p Plan,
        args: &[Value],
        result: &Value,
    ) -> Charge<'p> {
        let value = |source: &Source| match *source {
            Source::Arg(index) => args.get(index),
            Source::Result => Some(result),
            Source::Nothing => None,
        };
        if self
            .deep
            .iter()
            .filter_map(value)
            .any(|value| !holds_nothing(value))
            || self
                .nested
                .iter()
                .filter_map(value)
                .any(|value| matches!(value, Value::Tuple(_) | Value::Error(_)))
        {
            return Charge::Plan(charge);
        }
        if let Some(small) = &self.small {
            let mut mask = 0usize;
            for (index, (source, _)) in self.shallow.measurements.iter().enumerate() {
                match value(source) {
                    Some(Value::Int(0)) => {}
                    Some(Value::Int(_)) => mask |= 1 << index,
                    Some(value) if size_one(value) => {}
                    _ => return Charge::Plan(&self.shallow),
                }
            }
            return Charge::Units(small[mask]);
        }
        match self.unit {
            Some(unit)
                if self
                    .shallow
                    .measurements
                    .iter()
                    .all(|(source, _)| value(source).is_some_and(size_one)) =>
            {
                Charge::Units(unit)
            }
            _ => Charge::Plan(&self.shallow),
        }
    }
}
