//! Completion formulas as the core bills them (`K-CHG-003`), and the one
//! point that selects what a library call bills.
//!
//! A formula is kept with its operands resolved to argument positions, so
//! billing reads the call's arguments and result without names. A formula
//! that is a constant is folded once.

use lash_kernel_doc::{Formula, FunctionDefinition, Measure, Operand, Value};
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
    /// The formula as a constant plus the sizes of at most three operands,
    /// when it is a sum of constants and sizes, shallow or deep.
    linear: Option<Linear>,
    /// Whether the formula measures an argument: a call keeps its
    /// arguments for the bill only then.
    reads_args: bool,
    /// The function whose completion formula it is; `None` for a guard
    /// limit. A saved bill names its formula by it.
    pub(crate) function: Option<lash_kernel_value::names::FunctionId>,
}

/// A formula that adds a constant to the sizes, shallow or deep, of the
/// operands at `positions[..len]` (`Charge::linear`). Over values the
/// arena holds nothing of, a deep size is the size, so it is the
/// formula's sum without its tree.
#[derive(Clone, Copy, Debug)]
pub struct Linear {
    pub constant: u64,
    pub positions: [u8; 3],
    pub len: u8,
}

impl Linear {
    /// The formula's value where `size` gives each operand position's
    /// size, `Some(0)` for a position with no operand: `None` where it
    /// gives none, a value the arena holds something of.
    #[inline]
    pub fn units(self, mut size: impl FnMut(u8) -> Option<u64>) -> Option<u64> {
        let mut units = self.constant;
        for &position in &self.positions[..usize::from(self.len)] {
            units = units.saturating_add(size(position)?);
        }
        Some(units)
    }

    fn of(term: &Term) -> Option<Self> {
        fn add(term: &Term, linear: &mut Linear) -> Option<()> {
            match term {
                Term::Constant(units) => linear.constant = linear.constant.saturating_add(*units),
                Term::Measure(position, Measure::Size | Measure::DeepSize) => {
                    *linear.positions.get_mut(usize::from(linear.len))? = *position;
                    linear.len += 1;
                }
                Term::Sum(terms) => {
                    for term in terms {
                        add(term, linear)?;
                    }
                }
                _ => return None,
            }
            Some(())
        }
        let mut linear = Self {
            constant: 0,
            positions: [0; 3],
            len: 0,
        };
        add(term, &mut linear)?;
        Some(linear)
    }
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
            linear: Linear::of(&term),
            term,
            pairs,
            function: None,
        }
    }

    /// The formula over two arguments and a result that hold nothing,
    /// `values`, when each is of size 1 or 2 and the formula reads nothing
    /// else.
    #[inline(always)]
    pub fn pair(&self, values: [&Value; 3]) -> Option<u64> {
        let table = self.pairs.as_ref()?;
        let mut index = 0;
        for (bit, value) in values.into_iter().enumerate() {
            if crate::data::size_two(value)? {
                index |= 1 << bit;
            }
        }
        Some(table[index])
    }

    /// Whether the formula measures an argument.
    #[inline]
    pub fn reads_args(&self) -> bool {
        self.reads_args
    }

    /// The formula as a constant plus operand sizes, when it is one.
    #[inline]
    pub fn linear(&self) -> Option<Linear> {
        self.linear
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

#[cfg(test)]
mod tests {
    use lash_kernel_doc::{Measure, Value, parse_definition};

    use super::{Charge, RESULT};
    use crate::data::immediate_measure;

    /// `K-CHG-003`, `K-CHG-005`: a formula's linear form bills what its
    /// tree bills over every value the arena holds nothing of, each
    /// operand's deep size its size, an absent operand 0, and saturating
    /// at the top.
    #[test]
    fn a_linear_formula_bills_what_its_tree_bills_over_immediate_values() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Int(0),
            Value::Int(-7),
            Value::Int(i64::MAX),
            Value::Text(lash_kernel_doc::arena::ShortText::new("").unwrap()),
            Value::Text(lash_kernel_doc::arena::ShortText::new("fourteen bytes").unwrap()),
        ];
        let mut linear = 0;
        for formula in [
            "sum(6, deep(b))",
            "sum(8, deep(a), size(result))",
            "sum(2, size(result))",
            "sum(1, sum(size(a), 2), deep(b), size(result))",
            "sum(18446744073709551615, size(a))",
            "sum(size(a), size(b), size(result), 1)",
            "sum(size(a), size(b), size(a), size(result))",
            "sum(4, nested(a))",
            "product(2, size(a))",
        ] {
            let definition = parse_definition(&format!(
                "function law.f(a: Any, b: Any) -> Any\nkernel 1\ncharge {formula}\nnative\n"
            ))
            .unwrap();
            let params = definition.signature.params.iter();
            let charge = Charge::new(
                definition.charge().unwrap(),
                params.map(|param| param.name.clone()).collect(),
            );
            let Some(form) = charge.linear() else {
                continue;
            };
            linear += 1;
            for a in values {
                for b in values {
                    for result in values {
                        let at = |position: u8| match position {
                            RESULT => Some(result),
                            0 => Some(a),
                            1 => Some(b),
                            _ => None,
                        };
                        let tree = charge.evaluate(|position, how| {
                            at(position).map_or(0, |value| immediate_measure(&value, how).unwrap())
                        });
                        let sized = form.units(|position| {
                            Some(at(position).map_or(0, |value| {
                                immediate_measure(&value, Measure::Size).unwrap()
                            }))
                        });
                        assert_eq!(sized, Some(tree), "{formula} over {a:?}, {b:?}, {result:?}");
                    }
                }
            }
        }
        assert_eq!(
            linear, 6,
            "the sums of sizes have a linear form, the rest none"
        );
    }
}
