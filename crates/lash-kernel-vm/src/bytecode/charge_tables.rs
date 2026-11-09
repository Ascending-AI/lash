//! Completion formulas as the core bills them (`K-CHG-003`), and the one
//! point that selects what a library call bills.
//!
//! A formula is kept with its operands resolved to argument positions, so
//! billing reads the call's arguments and result without names. A formula
//! that is a constant is folded once.

use lash_kernel_doc::{Formula, FunctionDefinition, Measure, Operand};
use lash_kernel_value::names::Name;

/// An operand position: an argument index, or the result.
pub(crate) const RESULT: u8 = u8::MAX;

/// A completion formula or guard limit over argument positions.
#[derive(Clone, Debug)]
pub struct Charge {
    term: Term,
    constant: Option<u64>,
    /// The formula's closed form over two arguments and a result that
    /// hold nothing and measure 1 or 2 (a word, a float, a bool), indexed
    /// by [`Charge::pair`]: `None` when it reads anything else.
    pairs: Option<[u64; 8]>,
    /// Whether the formula measures an argument: a call keeps its
    /// arguments for the bill only then.
    reads_args: bool,
}

/// A formula with each operand resolved to its position once, so billing
/// compares no names.
#[derive(Clone, Debug)]
enum Term {
    Constant(u64),
    Measure(u8, Measure),
    Sum(Box<[Term]>),
    Product(Box<[Term]>),
    Max(Box<[Term]>),
    Min(Box<[Term]>),
    CeilLog2(Box<Term>),
}

impl Term {
    fn new(formula: &Formula, params: &[Name]) -> Self {
        let position = |operand: &Operand| match operand {
            Operand::Result => RESULT,
            Operand::Param(name) => params
                .iter()
                .position(|param| param == name)
                .map_or(RESULT - 1, |at| at as u8),
        };
        let all = |terms: &[Formula]| terms.iter().map(|term| Self::new(term, params)).collect();
        match formula {
            Formula::Constant(units) => Self::Constant(*units),
            Formula::Size(operand) => Self::Measure(position(operand), Measure::Size),
            Formula::DeepSize(operand) => Self::Measure(position(operand), Measure::DeepSize),
            Formula::NestedSize(operand) => Self::Measure(position(operand), Measure::NestedSize),
            Formula::Magnitude(operand) => Self::Measure(position(operand), Measure::Magnitude),
            Formula::Sum(terms) => Self::Sum(all(terms)),
            Formula::Product(terms) => Self::Product(all(terms)),
            Formula::Max(terms) => Self::Max(all(terms)),
            Formula::Min(terms) => Self::Min(all(terms)),
            Formula::CeilLog2(term) => Self::CeilLog2(Box::new(Self::new(term, params))),
        }
    }

    /// Whether the term measures an argument.
    fn reads_args(&self) -> bool {
        match self {
            Self::Constant(_) => false,
            Self::Measure(position, _) => *position != RESULT,
            Self::Sum(terms) | Self::Product(terms) | Self::Max(terms) | Self::Min(terms) => {
                terms.iter().any(Self::reads_args)
            }
            Self::CeilLog2(term) => term.reads_args(),
        }
    }

    /// Whether the term measures only the sizes of arguments 0 and 1 and
    /// of the result.
    fn pairwise(&self) -> bool {
        match self {
            Self::Constant(_) => true,
            Self::Measure(position, how) => {
                matches!(*position, 0 | 1 | RESULT) && *how != Measure::Magnitude
            }
            Self::Sum(terms) | Self::Product(terms) | Self::Max(terms) | Self::Min(terms) => {
                terms.iter().all(Self::pairwise)
            }
            Self::CeilLog2(term) => term.pairwise(),
        }
    }

    /// As [`Formula::evaluate`], saturating.
    fn evaluate(&self, measure: &mut impl FnMut(u8, Measure) -> u64) -> u64 {
        match self {
            Self::Constant(units) => *units,
            Self::Measure(position, how) => measure(*position, *how),
            Self::Sum(terms) => terms
                .iter()
                .fold(0u64, |sum, term| sum.saturating_add(term.evaluate(measure))),
            Self::Product(terms) => terms.iter().fold(1u64, |product, term| {
                product.saturating_mul(term.evaluate(measure))
            }),
            Self::Max(terms) => terms
                .iter()
                .map(|term| term.evaluate(measure))
                .max()
                .unwrap_or(0),
            Self::Min(terms) => terms
                .iter()
                .map(|term| term.evaluate(measure))
                .min()
                .unwrap_or(0),
            Self::CeilLog2(term) => {
                u64::from(u64::BITS - term.evaluate(measure).saturating_sub(1).leading_zeros())
            }
        }
    }
}

impl Charge {
    pub(crate) fn new(formula: &Formula, params: Vec<Name>) -> Self {
        let term = Term::new(formula, &params);
        // A leaf measures its size, shallow or deep, and nests nothing
        // (`K-CHG-004`, `K-CHG-005`).
        let pairs = term.pairwise().then(|| {
            std::array::from_fn(|index| {
                term.evaluate(&mut |position, how| {
                    let bit = match position {
                        0 => index & 1,
                        1 => (index >> 1) & 1,
                        _ => (index >> 2) & 1,
                    };
                    match how {
                        Measure::NestedSize => 0,
                        _ => 1 + bit as u64,
                    }
                })
            })
        });
        let constant = constant(formula);
        Self {
            reads_args: constant.is_none() && term.reads_args(),
            constant,
            term,
            pairs,
        }
    }

    /// The formula over two arguments and a result that hold nothing,
    /// of sizes `sizes`, when each is 1 or 2 and the formula reads nothing
    /// else.
    #[inline]
    pub fn pair(&self, sizes: [u64; 3]) -> Option<u64> {
        let table = self.pairs.as_ref()?;
        let mut index = 0;
        for (bit, size) in sizes.into_iter().enumerate() {
            match size {
                1 => {}
                2 => index |= 1 << bit,
                _ => return None,
            }
        }
        Some(table[index])
    }

    /// Whether the formula measures an argument.
    #[inline]
    pub fn reads_args(&self) -> bool {
        self.reads_args
    }

    /// The formula's value when it reads no operand.
    #[inline]
    pub fn constant(&self) -> Option<u64> {
        self.constant
    }

    /// Evaluates the formula (saturating, `K-CHG-003`), measuring operand
    /// `position` (an argument index or [`RESULT`]) through `measure`.
    #[inline]
    pub fn evaluate(&self, mut measure: impl FnMut(u8, Measure) -> u64) -> u64 {
        match self.constant {
            Some(constant) => constant,
            None => self.term.evaluate(&mut measure),
        }
    }
}

fn constant(formula: &Formula) -> Option<u64> {
    match formula {
        Formula::Constant(units) => Some(*units),
        Formula::Sum(parts) => parts
            .iter()
            .try_fold(0u64, |sum, part| Some(sum.saturating_add(constant(part)?))),
        Formula::Product(parts) => parts.iter().try_fold(1u64, |product, part| {
            Some(product.saturating_mul(constant(part)?))
        }),
        _ => None,
    }
}

/// What a library call bills when it completes (`K-CHG-003`,
/// `K-CHG-007`): its definition's formula when the call is the outermost
/// uncovered one, nothing inside a covered extent. Loop-protocol hooks are
/// library calls like any other (`K-ITER-005`, R2 ruling A): an outer
/// uncovered hook bills its formula once, with a result of size 0 when it
/// raises, and adds no call-expression fee of its own.
pub(crate) fn completion(definition: &FunctionDefinition, covered: bool) -> Option<&Formula> {
    if covered {
        return None;
    }
    definition.charge()
}
