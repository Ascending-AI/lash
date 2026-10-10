//! Charge formulas compiled for the machine: each operand resolved to the
//! argument it names, and the formula a postfix program (`K-CHG-003`).

use lash_kernel_doc as doc;

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
    /// A formula that is a constant plus each of its measurements once:
    /// that constant, which spares the program.
    linear: Option<u64>,
}

/// An instruction of a plan's program. Each combining one takes the two
/// amounts on the top of the stack: a sum of more terms is its terms
/// combined pairwise from the left, as the saturating fold computes it.
#[derive(Clone, Copy)]
enum Op {
    Constant(u64),
    /// The measurement of that index.
    Measure(usize),
    Add,
    Multiply,
    Max,
    Min,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    Sum,
    Product,
}

impl Plan {
    pub(crate) fn new(formula: &doc::Formula, params: &[doc::Param]) -> Self {
        let mut plan = Self {
            ops: Vec::new(),
            measurements: Vec::new(),
            depth: 0,
            linear: None,
        };
        plan.term(formula, params);
        plan.linear = plan.linear();
        let mut depth = 0usize;
        for op in &plan.ops {
            depth = match op {
                Op::Constant(_) | Op::Measure(_) => depth + 1,
                Op::Add | Op::Multiply | Op::Max | Op::Min => depth - 1,
            };
            plan.depth = plan.depth.max(depth);
        }
        plan
    }

    /// The constant of a program that adds a constant to measurements it
    /// makes once each, in their order.
    fn linear(&self) -> Option<u64> {
        // A sum's program is its terms, its constant, then one `Add` fewer
        // than its terms and constant.
        let terms = self
            .ops
            .iter()
            .take_while(|op| matches!(op, Op::Measure(_)))
            .count();
        let (constant, rest) = match &self.ops[terms..] {
            [Op::Constant(constant), rest @ ..] => (*constant, rest),
            rest => (0, rest),
        };
        let operands = terms + usize::from(self.ops.len() > terms + rest.len());
        let in_order = self.ops[..terms]
            .iter()
            .enumerate()
            .all(|(position, op)| matches!(op, Op::Measure(index) if *index == position));
        let adds = rest.len() + 1 == operands && rest.iter().all(|op| matches!(op, Op::Add));
        (operands > 0 && in_order && adds && terms == self.measurements.len()).then_some(constant)
    }

    /// Appends a formula's program.
    fn term(&mut self, formula: &doc::Formula, params: &[doc::Param]) {
        match formula {
            doc::Formula::Constant(amount) => self.ops.push(Op::Constant(*amount)),
            doc::Formula::Size(operand) => self.measure(operand, doc::Measure::Size, params),
            doc::Formula::DeepSize(operand) => {
                self.measure(operand, doc::Measure::DeepSize, params)
            }
            doc::Formula::NestedSize(operand) => {
                self.measure(operand, doc::Measure::NestedSize, params)
            }
            doc::Formula::Magnitude(operand) => {
                self.measure(operand, doc::Measure::Magnitude, params)
            }
            doc::Formula::Sum(items) => self.fold(Fold::Sum, items, params),
            doc::Formula::Product(items) => self.fold(Fold::Product, items, params),
            doc::Formula::Max(items) => self.extreme(Op::Max, items, params),
            doc::Formula::Min(items) => self.extreme(Op::Min, items, params),
        }
    }

    /// Appends a sum or a product, with the terms of the sums or products
    /// it holds as its own and its constants folded into one.
    fn fold(&mut self, fold: Fold, items: &[doc::Formula], params: &[doc::Param]) {
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
                (Fold::Sum, doc::Formula::Sum(inner))
                | (Fold::Product, doc::Formula::Product(inner)) => {
                    pending.extend(inner.iter().rev());
                }
                _ => {
                    self.term(item, params);
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
    fn extreme(&mut self, op: Op, items: &[doc::Formula], params: &[doc::Param]) {
        if items.is_empty() {
            self.ops.push(Op::Constant(0));
            return;
        }
        for (index, item) in items.iter().enumerate() {
            self.term(item, params);
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

    /// Computes the amount, asking `measure` for each measurement. A
    /// measurement reads the heap as it is, which nothing changes while
    /// the formula is computed.
    pub(crate) fn evaluate(&self, mut measure: impl FnMut(Source, doc::Measure) -> u64) -> u64 {
        if let Some(constant) = self.linear {
            return self
                .measurements
                .iter()
                .fold(constant, |sum, (source, kind)| {
                    sum.saturating_add(measure(*source, *kind))
                });
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
