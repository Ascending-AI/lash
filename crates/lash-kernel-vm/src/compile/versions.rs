//! Numeric block versions share their semantic blocks, register windows and
//! resume entries with the generic version. Only native-call dispatch differs:
//! the scalar version uses prepared entries and a formula constant derived for
//! its operand kinds. Every call guards its operands again, so mutation or a
//! native declining simply returns to the original instruction.

use lash_kernel_doc::{Formula, Measure, PreparedNative, Value};

use super::{Layout, Plan, Source};

/// The parameter-kind vectors for which scalar blocks are compiled. A packed
/// call is selected by its list kind. Scalar operands are guarded after binding,
/// so selecting a version never adds a speculative heap read.
pub(crate) enum BlockVersions {
    Generic,
    Entry,
    Float,
    Packed,
}

impl BlockVersions {
    pub(super) fn new(arity: usize, layout: Layout) -> Self {
        if !layout.0.is_multiple_of(4) {
            return Self::Generic;
        }
        match arity {
            0 => Self::Entry,
            1 => Self::Float,
            2 => Self::Packed,
            _ => Self::Generic,
        }
    }

    pub(crate) fn select(&self, args: &[Value]) -> bool {
        matches!(
            (self, args),
            (Self::Entry, [])
                | (Self::Float, [Value::Float(_)])
                | (Self::Packed, [_, Value::List(_)])
        )
    }
}

/// A prepared call version for Float/Bool operands and a Float/Bool result.
/// Those kinds have size and deep size 1 and nested size 0; magnitude is
/// value-dependent and therefore never enters this version.
#[derive(Clone, Copy)]
pub(crate) struct ScalarCall {
    pub(crate) entry: fn(&[Value]) -> Option<Value>,
    pub(crate) units: u64,
}

impl ScalarCall {
    pub(crate) fn new(entry: &PreparedNative, formula: &Formula, charge: &Plan) -> Option<Self> {
        fn fixed(formula: &Formula) -> bool {
            match formula {
                Formula::Magnitude(_) => false,
                Formula::CeilLog2(term) => fixed(term),
                Formula::Sum(terms)
                | Formula::Product(terms)
                | Formula::Max(terms)
                | Formula::Min(terms) => terms.iter().all(fixed),
                _ => true,
            }
        }
        fixed(formula).then(|| Self {
            entry: entry.entry,
            units: charge.evaluate(|source, measure| {
                if matches!(source, Source::Nothing) {
                    0
                } else {
                    match measure {
                        Measure::Size | Measure::DeepSize => 1,
                        Measure::NestedSize => 0,
                        Measure::Magnitude => {
                            unreachable!("magnitude has no constant scalar version")
                        }
                    }
                }
            }),
        })
    }

    pub(crate) fn answer(self, args: &[Value]) -> Option<Value> {
        if !args
            .iter()
            .all(|arg| matches!(arg, Value::Float(_) | Value::Bool(_)))
        {
            return None;
        }
        (self.entry)(args).filter(|value| matches!(value, Value::Float(_) | Value::Bool(_)))
    }
}
