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
    formula: Formula,
    params: Vec<Name>,
    constant: Option<u64>,
}

impl Charge {
    pub(crate) fn new(formula: &Formula, params: Vec<Name>) -> Self {
        Self {
            constant: constant(formula),
            formula: formula.clone(),
            params,
        }
    }

    /// The formula's value when it reads no operand.
    #[inline]
    pub fn constant(&self) -> Option<u64> {
        self.constant
    }

    /// Evaluates the formula (saturating, `K-CHG-003`), measuring operand
    /// `position` (an argument index or [`RESULT`]) through `measure`.
    pub fn evaluate(&self, measure: &mut dyn FnMut(u8, Measure) -> u64) -> u64 {
        if let Some(constant) = self.constant {
            return constant;
        }
        self.formula.evaluate(&mut |operand, how| {
            let position = match operand {
                Operand::Result => RESULT,
                Operand::Param(name) => self
                    .params
                    .iter()
                    .position(|param| param == name)
                    .map_or(RESULT - 1, |at| at as u8),
            };
            measure(position, how)
        })
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
