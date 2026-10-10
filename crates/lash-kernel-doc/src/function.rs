//! Library-function definitions (`K-LIB-001`).

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::Block;
use crate::canonical::{EncodeError, digest};
use crate::document::{DecodeError, from_versioned_json, to_json};
use crate::name::{FunctionId, FunctionName, Name};
use crate::types::Signature;

/// A library function: its name, its typed signature over kernel values,
/// its error kinds, what a call is charged, the guard that bounds its work,
/// and how it is implemented. Its identity is the hash of all of it, so a
/// change to any part is a new function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionDefinition {
    /// The kernel version the definition is written for.
    pub kernel: u32,
    pub name: FunctionName,
    pub signature: Signature,
    /// The kinds of error a call may raise, beyond the kernel's own.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub errors: BTreeSet<String>,
    /// What a call is charged, whichever implementation runs.
    pub charge: Formula,
    /// The work bound of a native implementation that can run away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<Guard>,
    pub implementation: Implementation,
    /// The version of the native implementation (`K-LIB-011`): its code is
    /// not part of the definition, so a native that answers otherwise is
    /// stated anew under the next version, which is a new function. Only a
    /// definition that states a native implementation states one other
    /// than the first.
    #[serde(
        default = "first_native_version",
        skip_serializing_if = "is_first_native_version"
    )]
    pub native_version: u32,
}

/// The version a native implementation states when it states none.
pub const FIRST_NATIVE_VERSION: u32 = 1;

fn first_native_version() -> u32 {
    FIRST_NATIVE_VERSION
}

fn is_first_native_version(version: &u32) -> bool {
    *version == FIRST_NATIVE_VERSION
}

/// How a library function is implemented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Implementation {
    /// Only a native implementation, registered by the embedder.
    Native,
    /// Only a kernel-code body. A call to it is a statement of its own.
    Body(FunctionBody),
    /// A native implementation and a kernel-code body that behave the same;
    /// an engine may run either.
    Both(FunctionBody),
}

/// A library function's kernel-code body. Its parameters are the
/// signature's, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionBody {
    /// Every library function the body calls, by identity, with the name
    /// its definition carries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub functions: BTreeMap<FunctionId, FunctionName>,
    pub block: Block,
}

/// A native implementation's work bound: the unit it counts and the most
/// units one call may spend. A call that would pass the limit ends with a
/// typed bound error (`K-LIB-008`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    /// What one unit of work is, in words a second implementation can
    /// count by: `"backtracking step"`.
    pub unit: String,
    /// The limit, over the arguments.
    pub limit: Formula,
}

/// An amount computed from the sizes of a call's arguments and result, in
/// saturating unsigned 64-bit arithmetic (`K-CHG-003`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Formula {
    Constant(u64),
    /// The operand's own size (`K-CHG-004`).
    Size(Operand),
    /// The operand's size with everything it holds (`K-CHG-005`).
    DeepSize(Operand),
    /// The sizes of the immutable values nested in the operand, down to
    /// the heap objects it holds (`K-CHG-005`).
    NestedSize(Operand),
    /// The operand's value, when it is a non-negative number (`K-CHG-006`).
    Magnitude(Operand),
    Sum(Vec<Formula>),
    Product(Vec<Formula>),
    /// The largest of one or more amounts.
    Max(Vec<Formula>),
    /// The smallest of one or more amounts.
    Min(Vec<Formula>),
}

/// What a [`Formula`] measures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Operand {
    /// The argument bound to this parameter.
    Param(Name),
    /// The value the call returns. A guard's limit may not name it.
    Result,
}

/// How a [`Formula`] measures an operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    Size,
    DeepSize,
    NestedSize,
    Magnitude,
}

impl Formula {
    /// Computes the amount, asking `measure` for each operand.
    pub fn evaluate(&self, measure: &mut dyn FnMut(&Operand, Measure) -> u64) -> u64 {
        match self {
            Self::Constant(amount) => *amount,
            Self::Size(operand) => measure(operand, Measure::Size),
            Self::DeepSize(operand) => measure(operand, Measure::DeepSize),
            Self::NestedSize(operand) => measure(operand, Measure::NestedSize),
            Self::Magnitude(operand) => measure(operand, Measure::Magnitude),
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
        }
    }
}

impl FunctionDefinition {
    /// The function's identity (`K-ID-002`).
    pub fn identity(&self) -> Result<FunctionId, EncodeError> {
        digest("lash-kernel-function", self).map(FunctionId::from_bytes)
    }

    /// Whether the definition states a native implementation: the property
    /// that lets a call sit inside an expression (`K-STMT-002`).
    pub fn has_native(&self) -> bool {
        matches!(
            self.implementation,
            Implementation::Native | Implementation::Both(_)
        )
    }

    pub fn body(&self) -> Option<&FunctionBody> {
        match &self.implementation {
            Implementation::Native => None,
            Implementation::Body(body) | Implementation::Both(body) => Some(body),
        }
    }

    pub fn to_json(&self) -> Result<String, EncodeError> {
        to_json(self)
    }

    pub fn from_json(text: &str) -> Result<Self, DecodeError> {
        from_versioned_json(text)
    }
}
