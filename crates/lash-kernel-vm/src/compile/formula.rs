//! Charge formulas compiled for the machine: each operand resolved to the
//! argument it names, and the formula a postfix program (`K-CHG-003`).

use lash_kernel_doc as doc;
use lash_kernel_doc::Value;

use super::regions::Fresh;
use crate::data::{empty_measure, holds_nothing, immediate_measure, immediate_size, size_one};

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
    pub(crate) fn new(formula: &doc::Formula, params: &[lash_vocabulary::types::Param]) -> Self {
        Self::prepare(formula, params, false)
    }

    fn prepare(
        formula: &doc::Formula,
        params: &[lash_vocabulary::types::Param],
        shallow: bool,
    ) -> Self {
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
    fn term(
        &mut self,
        formula: &doc::Formula,
        params: &[lash_vocabulary::types::Param],
        shallow: bool,
    ) {
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
    fn fold(
        &mut self,
        fold: Fold,
        items: &[doc::Formula],
        params: &[lash_vocabulary::types::Param],
        shallow: bool,
    ) {
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
    fn extreme(
        &mut self,
        op: Op,
        items: &[doc::Formula],
        params: &[lash_vocabulary::types::Param],
        shallow: bool,
    ) {
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

    fn measure(
        &mut self,
        operand: &doc::Operand,
        measure: doc::Measure,
        params: &[lash_vocabulary::types::Param],
    ) {
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
    /// The formula over scalar sizes; for a formula that measures a
    /// magnitude or has more than three parameters, nothing.
    scalar: Option<ScalarTable>,
    /// The formula as it computes for values that hold nothing
    /// ([`doc::Formula::shallow`]).
    shallow: Plan,
    /// The values the formula measures by deep size: `shallow` holds when
    /// none of them holds another value.
    deep: Box<[Source]>,
    /// The values the formula measures by nested size: `shallow` holds when
    /// none of them is a tuple or an error.
    nested: Box<[Source]>,
}

/// The one scalar charge table (K-CHG-004): a formula's completion units
/// for every combination of scalar sizes 1 and 2 of its arguments and
/// result, a zero small integer having size 1 and a nonzero one size 2,
/// with deep sizes as sizes and nested sizes 0. A prepared guard's
/// certified sizes, the general call's classification of its values and a
/// region's charge of the same call all index it, so each charges what the
/// formula charges, by construction (`docs/design/kernel-regions.md`
/// §3.1).
#[derive(Clone)]
pub(crate) struct ScalarTable {
    /// The units by size mask: bit `i` for argument `i`, bit 3 for the
    /// result, set for size 2.
    units: Box<[u64; 16]>,
    /// The values the formula measures, which a classification reads.
    measured: Box<[Source]>,
}

/// A size mask's bit for `source`, which measures 0 when it is nothing.
fn size_bit(source: Source) -> Option<usize> {
    match source {
        Source::Arg(index) => Some(index),
        Source::Result => Some(3),
        Source::Nothing => None,
    }
}

impl ScalarTable {
    /// The table of `charge` for a function of `params`.
    fn of(charge: &Plan, params: &[lash_vocabulary::types::Param]) -> Option<Self> {
        if params.len() > 3
            || charge
                .measurements
                .iter()
                .any(|(_, kind)| *kind == doc::Measure::Magnitude)
        {
            return None;
        }
        let units = Box::new(std::array::from_fn(|mask| {
            charge.evaluate(|source, kind| {
                if kind == doc::Measure::NestedSize {
                    return 0;
                }
                match size_bit(source) {
                    Some(bit) => 1 + ((mask >> bit) & 1) as u64,
                    None => 0,
                }
            })
        }));
        let mut measured: Vec<Source> = charge
            .measurements
            .iter()
            .map(|(source, _)| *source)
            .filter(|source| *source != Source::Nothing)
            .collect();
        measured.dedup();
        Some(Self {
            units,
            measured: measured.into(),
        })
    }

    /// The units of the scalar sizes `mask` states.
    #[inline(always)]
    pub(crate) fn units(&self, mask: u8) -> Option<u64> {
        self.units.get(usize::from(mask)).copied()
    }

    /// The size mask of a call whose measured values `value` gives, or
    /// `None` when one of them is not a small integer or a value of size 1.
    #[inline(always)]
    pub(crate) fn classify<'v>(&self, value: impl Fn(&Source) -> Option<&'v Value>) -> Option<u8> {
        let mut mask = 0_u8;
        for source in &self.measured {
            match value(source) {
                Some(Value::Int(0)) => {}
                Some(Value::Int(_)) => mask |= 1 << size_bit(*source)?,
                Some(value) if size_one(value) => {}
                _ => return None,
            }
        }
        Some(mask)
    }

    /// The most any call charges.
    pub(crate) fn max(&self) -> u64 {
        self.units.iter().copied().max().unwrap_or(0)
    }
}

/// A prepared entry's immediate formula at one call site
/// ([`Fast::at_site`]): its constant with what the site's constant
/// operands measure folded in, the terms a call measures, and its minimum
/// pair. Every amount is a non-negative saturating sum, which no order of
/// its terms changes, so it computes what [`Fast::immediate`] computes.
pub(crate) struct SiteFormula {
    constant: u64,
    terms: Vec<Term>,
    least: Option<[Term; 2]>,
}

/// A text answer's linear size formula, with its operand positions
/// resolved once. Inline text sizes need the byte count, rather than the
/// binary scalar mask. The coefficients come from the same site formula;
/// no helper has a separately authored charge (K-CHG-003).
pub(crate) struct TextCharge {
    constant: u64,
    args: [u64; 3],
    result: u64,
}

impl TextCharge {
    #[inline(always)]
    pub(crate) fn units(&self, args: &[Value], result: &Value) -> Option<u64> {
        #[inline(always)]
        fn term(coefficient: u64, value: Option<&Value>) -> Option<u64> {
            if coefficient == 0 {
                Some(0)
            } else {
                Some(coefficient.saturating_mul(immediate_size(value?)?))
            }
        }
        Some(
            self.constant
                .saturating_add(term(self.args[0], args.first())?)
                .saturating_add(term(self.args[1], args.get(1))?)
                .saturating_add(term(self.args[2], args.get(2))?)
                .saturating_add(term(self.result, Some(result))?),
        )
    }
}

/// A measurement of a site's formula.
#[derive(Clone, Copy)]
enum Term {
    /// Known where the site was compiled.
    Known(u64),
    /// Argument `at`'s, or `fresh`'s where the operand is a construction of
    /// no members and no write is held that may change it.
    Arg {
        at: usize,
        measure: doc::Measure,
        fresh: Option<u64>,
    },
    Result(doc::Measure),
}

impl Term {
    #[inline(always)]
    fn measure(self, args: &[Value], result: &Value, held: bool) -> Option<u64> {
        match self {
            Self::Known(units) => Some(units),
            Self::Arg {
                fresh: Some(units), ..
            } if !held => Some(units),
            Self::Arg { at, measure, .. } => match args.get(at) {
                Some(value) => site_measure(value, measure),
                None => Some(0),
            },
            Self::Result(measure) => site_measure(result, measure),
        }
    }
}

/// [`immediate_measure`] of `value`, a magnitude measured out of line: a
/// call measures its sizes alone where the optimizer would otherwise
/// compute every value's magnitude ahead of the terms that need it.
#[inline(always)]
fn site_measure(value: &Value, measure: doc::Measure) -> Option<u64> {
    #[inline(never)]
    fn magnitude(value: &Value) -> Option<u64> {
        immediate_measure(value, doc::Measure::Magnitude)
    }
    match measure {
        doc::Measure::Magnitude => magnitude(value),
        _ => immediate_measure(value, measure),
    }
}

impl SiteFormula {
    /// A sum of immediate sizes, with no minimum or fresh collection
    /// operand. Every other formula keeps its general evaluator.
    pub(crate) fn text_charge(&self) -> Option<TextCharge> {
        if self.least.is_some() {
            return None;
        }
        let mut charge = TextCharge {
            constant: self.constant,
            args: [0; 3],
            result: 0,
        };
        for term in &self.terms {
            let coefficient = match term {
                Term::Arg {
                    at,
                    measure: doc::Measure::Size | doc::Measure::DeepSize,
                    fresh: None,
                } => charge.args.get_mut(*at)?,
                Term::Result(doc::Measure::Size | doc::Measure::DeepSize) => &mut charge.result,
                _ => return None,
            };
            *coefficient += 1;
        }
        Some(charge)
    }

    /// What [`Fast::immediate`] charges a call of `args` that gave
    /// `result`, the site's fresh operands measured as their values where
    /// a write is `held`; `None` where a value it measures is one the
    /// arena measures.
    #[inline(always)]
    pub(crate) fn units(&self, args: &[Value], result: &Value, held: bool) -> Option<u64> {
        let mut sum = self.constant;
        for term in &self.terms {
            sum = sum.saturating_add(term.measure(args, result, held)?);
        }
        if let Some([left, right]) = self.least {
            let least = left
                .measure(args, result, held)?
                .min(right.measure(args, result, held)?);
            sum = sum.saturating_add(least);
        }
        Some(sum)
    }
}

/// How a call the fast path answered is charged: an amount known when the
/// library was prepared, or a plan to compute over the call's values.
pub(crate) enum Charge<'p> {
    Units(u64),
    Plan(&'p Plan),
}

impl Fast {
    /// What charges an entry's answers; `scalar`, whether it answers
    /// immediates, which a scalar charge table serves.
    pub(crate) fn new(
        formula: &doc::Formula,
        params: &[lash_vocabulary::types::Param],
        charge: &Plan,
        scalar: bool,
    ) -> Self {
        let shallow = Plan::prepare(formula, params, true);
        let measured = |kind: doc::Measure| -> Box<[Source]> {
            charge
                .measurements
                .iter()
                .filter(|(_, measure)| *measure == kind)
                .map(|(source, _)| *source)
                .collect()
        };
        Self {
            scalar: if scalar {
                ScalarTable::of(charge, params)
            } else {
                None
            },
            deep: measured(doc::Measure::DeepSize),
            nested: measured(doc::Measure::NestedSize),
            shallow,
        }
    }

    /// The exact formula for scalar sizes already certified by the guard.
    /// Formulas measuring magnitude retain the general fallback.
    #[inline]
    pub(crate) fn scalar_units(&self, mask: u8) -> Option<u64> {
        self.scalar.as_ref()?.units(mask)
    }

    /// The scalar charge table, when the formula has one.
    pub(crate) fn scalar(&self) -> Option<&ScalarTable> {
        self.scalar.as_ref()
    }

    /// What the formula charges a call with `args` that returned `result`
    /// when it is a sum of measurements, optionally with one minimum pair
    /// (`same`'s), and every value it measures carries its measurements:
    /// one the arena holds nothing of ([`immediate_measure`]), or an
    /// argument `fresh` names, a construction of no members
    /// ([`empty_measure`]). Neither holds another value, so the shallow
    /// formula computes for them what the formula does, combined as
    /// [`Plan::evaluate`] combines it. `None` for another formula, or where
    /// a measured value is one the arena measures.
    #[inline(always)]
    pub(crate) fn immediate(&self, args: &[Value], fresh: Fresh, result: &Value) -> Option<u64> {
        let measured = |(source, measure): &(Source, doc::Measure)| -> Option<u64> {
            Some(match *source {
                Source::Arg(index) if fresh.empty(index) => empty_measure(*measure),
                Source::Arg(index) => match args.get(index) {
                    Some(value) => immediate_measure(value, *measure)?,
                    None => 0,
                },
                Source::Result => immediate_measure(result, *measure)?,
                Source::Nothing => 0,
            })
        };
        let measurements = &self.shallow.measurements;
        match self.shallow.linear {
            Some(Linear::Sum(constant)) => measurements.iter().try_fold(constant, |sum, item| {
                Some(sum.saturating_add(measured(item)?))
            }),
            Some(Linear::Minimum { constant, at }) => {
                let mut sum = constant;
                for item in &measurements[..at] {
                    sum = sum.saturating_add(measured(item)?);
                }
                let least = measured(&measurements[at])?.min(measured(&measurements[at + 1])?);
                sum = sum.saturating_add(least);
                for item in &measurements[at + 2..] {
                    sum = sum.saturating_add(measured(item)?);
                }
                Some(sum)
            }
            None => None,
        }
    }

    /// [`Self::immediate`] at one call site (FIG-6965), whose operands
    /// `known` names are constants and `fresh` names constructions of no
    /// members: the measurements those give folded once, where the site is
    /// compiled, so that a call measures only its other values. `None` for
    /// a formula that is not a sum of measurements, which
    /// [`Self::immediate`] computes nothing for.
    pub(crate) fn at_site(&self, known: &[Option<Value>], fresh: Fresh) -> Option<SiteFormula> {
        let term = |&(source, measure): &(Source, doc::Measure)| match source {
            Source::Arg(at) if fresh.empty(at) => Term::Arg {
                at,
                measure,
                fresh: Some(empty_measure(measure)),
            },
            Source::Arg(at) => match known
                .get(at)
                .copied()
                .flatten()
                .and_then(|value| immediate_measure(&value, measure))
            {
                Some(units) => Term::Known(units),
                None => Term::Arg {
                    at,
                    measure,
                    fresh: None,
                },
            },
            Source::Result => Term::Result(measure),
            Source::Nothing => Term::Known(0),
        };
        let measurements = &self.shallow.measurements;
        let (constant, pair) = match self.shallow.linear {
            Some(Linear::Sum(constant)) => (constant, None),
            Some(Linear::Minimum { constant, at }) => (constant, Some(at)),
            None => return None,
        };
        let mut formula = SiteFormula {
            constant,
            terms: Vec::new(),
            least: pair.map(|at| [term(&measurements[at]), term(&measurements[at + 1])]),
        };
        for (index, item) in measurements.iter().enumerate() {
            if pair.is_some_and(|at| index == at || index == at + 1) {
                continue;
            }
            match term(item) {
                Term::Known(units) => formula.constant = formula.constant.saturating_add(units),
                term => formula.terms.push(term),
            }
        }
        Some(formula)
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
        match self
            .scalar
            .as_ref()
            .and_then(|table| table.units(table.classify(value)?))
        {
            Some(units) => Charge::Units(units),
            None => Charge::Plan(&self.shallow),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "a refused formula fixture fails the law"
)]
mod text_laws {
    use super::*;
    use doc::arena::{ArenaHandle, ShortText};

    /// K-CHG-003: specializing a text formula retains byte sizes,
    /// constant folding, saturation and arena declines.
    #[test]
    fn prepared_text_charge_is_the_original_formula() {
        let params = ["a", "b", "c"].map(|name| lash_vocabulary::types::Param {
            name: lash_vocabulary::names::Name::new(name),
            ty: lash_vocabulary::types::Type::Any,
            optional: false,
        });
        let a = doc::Operand::Param(lash_vocabulary::names::Name::new("a"));
        let b = doc::Operand::Param(lash_vocabulary::names::Name::new("b"));
        let c = doc::Operand::Param(lash_vocabulary::names::Name::new("c"));
        let values = [
            Value::Int(0),
            Value::Int(-1),
            Value::Float(doc::Float::new(-0.0)),
            Value::Text(ShortText::new("").unwrap()),
            Value::Text(ShortText::new("é_123").unwrap()),
            Value::Text(ShortText::new("12345678901234").unwrap()),
            Value::List(ArenaHandle::new(0, 0)),
            Value::LongText(ArenaHandle::new(1, 0)),
        ];
        for constant in [0, 8, u64::MAX - 1] {
            let formula = doc::Formula::Sum(vec![
                doc::Formula::Constant(constant),
                doc::Formula::Size(a.clone()),
                doc::Formula::Size(b.clone()),
                doc::Formula::DeepSize(c.clone()),
                doc::Formula::Size(doc::Operand::Result),
            ]);
            let plan = Plan::new(&formula, &params);
            let fast = Fast::new(&formula, &params, &plan, true);
            for known in [[None; 3], [Some(values[4]), None, None]] {
                let site = fast.at_site(&known, Fresh::of(0)).unwrap();
                let text_charge = site.text_charge().unwrap();
                for a in &values {
                    if known[0].is_some_and(|known| !known.same(a)) {
                        continue;
                    }
                    for b in &values {
                        for c in &values {
                            for result in &values[3..6] {
                                let args = [*a, *b, *c];
                                assert_eq!(
                                    text_charge.units(&args, result),
                                    site.units(&args, result, false)
                                );
                                assert_eq!(
                                    text_charge.units(&args, result),
                                    site.units(&args, result, true)
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
