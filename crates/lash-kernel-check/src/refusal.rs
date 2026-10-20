//! Why a document is refused, as typed data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

use lash_kernel_doc::{
    EffectName, FunctionId, FunctionName, Invalid, InvalidReason, Name, OperationRequirement,
    Signature, Site, Type,
};

/// Everything that keeps a document from being admitted. Admission does not
/// stop at the first fault of the environment: every missing effect, every
/// mismatched signature and every missing function identity is named.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", list(.errors))]
pub struct Refusal {
    pub errors: Vec<Refused>,
}

fn list(errors: &[Refused]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// One reason, and the node at fault when the fault is in a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub site: Option<Site>,
    pub reason: RefusalReason,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)?;
        match &self.site {
            Some(site) => write!(f, " (at {site})"),
            None => Ok(()),
        }
    }
}

impl From<Invalid> for Refused {
    fn from(invalid: Invalid) -> Self {
        Self {
            site: invalid.site,
            reason: RefusalReason::Invalid(*invalid.reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RefusalReason {
    /// The document, or the body of a library function it reaches, fails
    /// structural validation (`K-DOC-005`), a call by name of a function
    /// with no native implementation among them (`K-LIB-002`).
    #[error("{0}")]
    Invalid(InvalidReason),
    /// `K-ADM-001`: the environment provides no such effect.
    #[error("the environment provides no effect `{effect}`")]
    MissingEffect { effect: EffectName },
    /// OP-INTERNAL: only the declaring package's bodies may perform it.
    #[error("internal operation `{effect}` may only be performed by its package bodies")]
    InternalOperation { effect: EffectName },
    /// `K-ADM-002`: the environment's effect does not serve the signature
    /// the document expects.
    #[error(
        "effect `{effect}` is provided with a signature that does not serve the one expected: {mismatch}"
    )]
    EffectSignature {
        effect: EffectName,
        expected: Signature,
        provided: Signature,
        mismatch: Box<SignatureMismatch>,
    },
    /// `K-ADM-003`: the environment holds no function of this identity.
    #[error("the environment holds no library function `{name}` ({function})")]
    MissingFunction {
        function: FunctionId,
        name: FunctionName,
    },
    /// `K-FORM-003`: a variable is read or assigned where no enclosing scope
    /// has declared it.
    #[error("variable `{name}` is not bound here")]
    UnboundVariable { name: Name },
    /// `K-ADM-005`: an argument can never be a value of its parameter's
    /// type.
    #[error("argument `{param}` of `{callee}` takes {expected:?}; this is {found:?}")]
    ArgumentType {
        callee: String,
        param: Name,
        expected: Type,
        found: Type,
    },
    /// `K-ADM-006`: a `perform` states a result type no result of the
    /// effect's signature can have.
    #[error("effect `{effect}` returns {declared:?}; this `perform` expects {stated:?}")]
    PerformResult {
        effect: EffectName,
        declared: Type,
        stated: Type,
    },
    /// `K-ADM-007`: the manifest lists an effect nothing performs.
    #[error("the manifest lists effect `{effect}`, which nothing performs")]
    EffectNotPerformed { effect: EffectName },
    /// `K-ADM-007`: the manifest lists a function nothing reaches.
    #[error("the manifest lists library function `{name}` ({function}), which nothing reaches")]
    FunctionNotReached {
        function: FunctionId,
        name: FunctionName,
    },
    /// `K-ADM-007`: a function reached through another function's body is
    /// not in the manifest.
    #[error(
        "library function `{name}` ({function}) is reached through {through} but not listed in \
         the manifest"
    )]
    FunctionNotListed {
        function: FunctionId,
        name: FunctionName,
        through: FunctionId,
    },
    /// `K-ADM-007`: a library body the document reaches performs an
    /// operation the manifest does not list.
    #[error(
        "effect `{effect}` is performed by the body of {through}, which requires {requirement}, \
         but not listed in the manifest"
    )]
    RequirementNotListed {
        effect: EffectName,
        through: FunctionId,
        requirement: OperationRequirement,
    },
    /// `ADM-REQUIREMENT`: the document's code and the bodies it reaches
    /// require one effect name under two requirements.
    #[error(
        "effect `{effect}` is required as {first} and as {second}; one name is one requirement"
    )]
    RequirementConflict {
        effect: EffectName,
        first: Box<RequiredBy>,
        second: Box<RequiredBy>,
    },
    /// `K-ADM-007`: a function is listed under a name its definition does
    /// not carry.
    #[error(
        "library function {function} is listed as `{listed}`; its definition names it `{defined}`"
    )]
    FunctionName {
        function: FunctionId,
        listed: FunctionName,
        defined: FunctionName,
    },
}

impl RefusalReason {
    /// A stable local snake_case name for this top-level alternative.
    /// Existing names keep their meaning; new alternatives add names. These
    /// are not HTTP statuses, and old clients may not recognize future names.
    /// The code names the reason independently of its payload or display text.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::InternalOperation { .. } => "internal_operation",
            Self::MissingEffect { .. } => "missing_effect",
            Self::EffectSignature { .. } => "effect_signature",
            Self::MissingFunction { .. } => "missing_function",
            Self::UnboundVariable { .. } => "unbound_variable",
            Self::ArgumentType { .. } => "argument_type",
            Self::PerformResult { .. } => "perform_result",
            Self::EffectNotPerformed { .. } => "effect_not_performed",
            Self::FunctionNotReached { .. } => "function_not_reached",
            Self::FunctionNotListed { .. } => "function_not_listed",
            Self::FunctionName { .. } => "function_name",
            Self::RequirementNotListed { .. } => "requirement_not_listed",
            Self::RequirementConflict { .. } => "requirement_conflict",
        }
    }
}

/// A requirement of an effect, and the code that states it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequiredBy {
    /// The library function whose body states it; `None` for the
    /// document's manifest, which states its own code's.
    pub through: Option<FunctionId>,
    pub requirement: OperationRequirement,
}

impl fmt::Display for RequiredBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.through {
            Some(function) => write!(f, "{} by the body of {function}", self.requirement),
            None => write!(f, "{} by the manifest", self.requirement),
        }
    }
}

pub(crate) fn refused(site: Option<&Site>, reason: RefusalReason) -> Refused {
    Refused {
        site: site.cloned(),
        reason,
    }
}

/// The side of an effect signature that cannot serve its declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignatureSide {
    Input,
    Output,
}

impl fmt::Display for SignatureSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Input => "input",
            Self::Output => "output",
        })
    }
}

/// The first incompatible part found by the signature's serving relation.
/// `path` uses JSON pointer escaping in the projected type structure:
/// `/input`, `/output`, and their properties, items or additional properties.
/// The types are the kernel's projections, not the original JSON schemas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignatureMismatch {
    pub side: SignatureSide,
    pub path: String,
    pub expected: Type,
    pub provided: Type,
}

impl fmt::Display for SignatureMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}: expected {}, provided {}",
            self.side,
            self.path,
            describe_type(&self.expected),
            describe_type(&self.provided)
        )
    }
}

fn describe_type(ty: &Type) -> String {
    match ty {
        Type::Any => "open/unconstrained (Any)".into(),
        Type::Json => "JSON data, holding no function (Json)".into(),
        Type::Record(record) if record.rest.is_none() => format!("closed object {ty:?}"),
        Type::Record(_) => format!("object with declared fields and open rest {ty:?}"),
        other => format!("{other:?}"),
    }
}
