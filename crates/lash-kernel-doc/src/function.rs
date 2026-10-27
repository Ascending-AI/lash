//! Library authoring/linking seam: typed function definitions are editor and
//! registry data, not retained runnable host documents (`DocumentBytes`).
//!
//! Library-function definitions (`K-LIB-001`).

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::Block;
use crate::document::{DecodeError, EncodeError, from_versioned_json_with_limits, to_json};

use lash_kernel_value::types::Signature;
use {
    lash_kernel_value::names::FunctionId, lash_kernel_value::names::FunctionName,
    lash_kernel_value::names::Name,
};
use {lash_kernel_value::names::Import, lash_kernel_value::names::ImportSlot};
use {lash_kernel_value::tokens::encode_body, lash_kernel_value::tokens::logical_digest};

/// A library's declarations by content identity, without native implementations.
///
/// A runtime exports these with [`crate::FunctionRegistry::export_declarations`]
/// and ships their serde representation beside documents. The decoded map is
/// a [`crate::FunctionCatalog`], so a reader can link documents and derive
/// workflow views without a registry or engine. Definitions include signatures
/// and kernel bodies: document `use` lines contain only names and identities.
pub type FunctionDeclarations = BTreeMap<FunctionId, FunctionDefinition>;

/// A build helper's role in the authored workflow projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionRole {
    #[default]
    /// Calls remain authored steps.
    Authored,
    /// Copy-out inserted around an effect argument, with an optional forwarder.
    ArgumentCopy,
    /// Decode an effect reply into the authored language's value representation.
    ResultDecode,
    /// Validate and convert a sleep argument expressed in milliseconds.
    DurationMilliseconds,
    /// An authored wait whose argument is expressed in seconds.
    TimerSeconds,
    /// An authored function whose final return was inserted by lowering.
    ImplicitReturn,
}

impl ProjectionRole {
    pub fn is_authored(&self) -> bool {
        *self == Self::Authored
    }
    pub fn is_glue(self) -> bool {
        matches!(
            self,
            Self::ArgumentCopy | Self::ResultDecode | Self::DurationMilliseconds
        )
    }
}

/// A library function: its name, its typed signature over kernel values,
/// its error kinds, what a call is charged, the guard that bounds its work,
/// and how it is implemented. Its identity is the hash of all of it, so a
/// change to any part is a new function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionDefinition {
    #[serde(default, skip_serializing_if = "ProjectionRole::is_authored")]
    pub projection: ProjectionRole,
    /// The kernel version the definition is written for.
    pub kernel: u32,
    pub name: FunctionName,
    pub signature: Signature,
    /// The kinds of error a call may raise, beyond the kernel's own.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub errors: BTreeSet<String>,
    /// What a call is charged, whichever implementation runs.
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

/// What a first use of a library function reads of its definition: all of
/// it but its body (FIG-6931). Lowering, validation and a machine's
/// preparation read the header, so a body is decoded only to compile it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionHeader {
    pub projection: ProjectionRole,
    pub kernel: u32,
    pub name: FunctionName,
    pub signature: Signature,
    pub errors: BTreeSet<String>,
    pub metering: Metering,
    pub guard: Option<Guard>,
    pub implementation: ImplementationKind,
}

/// Which implementations a definition states, without its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImplementationKind {
    Native,
    Guest,
    Both,
}

impl FunctionHeader {
    /// `definition`'s header.
    #[must_use]
    pub fn of(definition: &FunctionDefinition) -> Self {
        Self {
            projection: definition.projection,
            kernel: definition.kernel,
            name: definition.name.clone(),
            signature: definition.signature.clone(),
            errors: definition.errors.clone(),
            metering: definition.metering(),
            guard: definition.guard.clone(),
            implementation: match definition.implementation {
                Implementation::Native { .. } => ImplementationKind::Native,
                Implementation::Guest { .. } => ImplementationKind::Guest,
                Implementation::Both { .. } => ImplementationKind::Both,
            },
        }
    }

    /// Whether the definition states a native implementation
    /// ([`FunctionDefinition::has_native`]).
    #[must_use]
    pub fn has_native(&self) -> bool {
        matches!(
            self.implementation,
            ImplementationKind::Native | ImplementationKind::Both
        )
    }

    /// Whether the definition has a kernel body.
    #[must_use]
    pub fn has_body(&self) -> bool {
        matches!(
            self.implementation,
            ImplementationKind::Guest | ImplementationKind::Both
        )
    }
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
    /// Authored code charges statements; shipped guest helpers charge a completion formula.
    Guest {
        body: FunctionBody,
        metering: Metering,
    },
    Native {
        charge: Formula,
    },
    /// Both implementations have the same formula charge.
    Both {
        body: FunctionBody,
        charge: Formula,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Metering {
    Statements,
    Formula(Formula),
}
impl Metering {
    pub fn formula(&self) -> Option<&Formula> {
        match self {
            Self::Statements => None,
            Self::Formula(formula) => Some(formula),
        }
    }
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
    /// Every operation the body performs itself, with what it requires of
    /// it (`K-LIB-004`). A body beside a native implementation performs
    /// nothing. What the functions it calls perform is theirs to list.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub imports: BTreeMap<ImportSlot, Import>,
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
    /// Ceiling of log2 of an amount; 0 for amounts 0 and 1.
    CeilLog2(Box<Formula>),
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
    /// The formula as it computes when the operands it measures hold no
    /// other value: a deep size is then the operand's size, and a nested
    /// size 0 (`K-CHG-005`). A machine derives it once, and charges a call
    /// whose measured values hold nothing by it without walking them.
    pub fn shallow(&self) -> Self {
        let all = |terms: &[Self]| terms.iter().map(Self::shallow).collect();
        match self {
            Self::DeepSize(operand) => Self::Size(operand.clone()),
            Self::NestedSize(_) => Self::Constant(0),
            Self::Sum(terms) => Self::Sum(all(terms)),
            Self::Product(terms) => Self::Product(all(terms)),
            Self::Max(terms) => Self::Max(all(terms)),
            Self::Min(terms) => Self::Min(all(terms)),
            Self::CeilLog2(term) => Self::CeilLog2(Box::new(term.shallow())),
            Self::Constant(_) | Self::Size(_) | Self::Magnitude(_) => self.clone(),
        }
    }

    /// Computes the amount, asking `measure` for each operand.
    pub fn evaluate(&self, measure: &mut dyn FnMut(&Operand, Measure) -> u64) -> u64 {
        match self {
            Self::Constant(amount) => *amount,
            Self::CeilLog2(term) => {
                u64::from(u64::BITS - term.evaluate(measure).saturating_sub(1).leading_zeros())
            }
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

#[cfg(test)]
std::thread_local! {
    pub(crate) static IDENTITY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl FunctionDefinition {
    /// The function's identity (`K-ID-002`): the digest of its canonical
    /// bytes, its logical token body (`K-ID-003`).
    pub fn identity(&self) -> FunctionId {
        #[cfg(test)]
        IDENTITY_CALLS.with(|calls| calls.set(calls.get() + 1));
        FunctionId::from_bytes(logical_digest(&encode_body(self)))
    }

    /// Whether the definition states a native implementation: the property
    /// that lets an expression call it by name (`K-LIB-002`).
    pub fn has_native(&self) -> bool {
        matches!(
            self.implementation,
            Implementation::Native { .. } | Implementation::Both { .. }
        )
    }

    pub fn body(&self) -> Option<&FunctionBody> {
        match &self.implementation {
            Implementation::Native { .. } => None,
            Implementation::Guest { body, .. } | Implementation::Both { body, .. } => Some(body),
        }
    }

    pub fn to_json(&self) -> Result<String, EncodeError> {
        to_json(self)
    }

    pub fn from_json(text: &str) -> Result<Self, DecodeError> {
        Self::from_json_with_limits(
            text,
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
        )
    }
    pub fn from_json_with_limits(
        text: &str,
        limits: lash_kernel_value::data_limits::KernelDataLimits,
    ) -> Result<Self, DecodeError> {
        from_versioned_json_with_limits(text, limits)
    }
}

impl FunctionDefinition {
    pub fn body_mut(&mut self) -> Option<&mut FunctionBody> {
        match &mut self.implementation {
            Implementation::Native { .. } => None,
            Implementation::Guest { body, .. } | Implementation::Both { body, .. } => Some(body),
        }
    }
}

impl FunctionDefinition {
    pub fn charge(&self) -> Option<&Formula> {
        match &self.implementation {
            Implementation::Native { charge } | Implementation::Both { charge, .. } => Some(charge),
            Implementation::Guest { metering, .. } => metering.formula(),
        }
    }
    pub fn charge_mut(&mut self) -> Option<&mut Formula> {
        match &mut self.implementation {
            Implementation::Native { charge } | Implementation::Both { charge, .. } => Some(charge),
            Implementation::Guest {
                metering: Metering::Formula(charge),
                ..
            } => Some(charge),
            Implementation::Guest {
                metering: Metering::Statements,
                ..
            } => None,
        }
    }
    pub fn metering(&self) -> Metering {
        match &self.implementation {
            Implementation::Guest { metering, .. } => metering.clone(),
            Implementation::Native { charge } | Implementation::Both { charge, .. } => {
                Metering::Formula(charge.clone())
            }
        }
    }
}
impl FunctionHeader {
    pub fn charge(&self) -> Option<&Formula> {
        self.metering.formula()
    }
}
