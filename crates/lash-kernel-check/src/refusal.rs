//! Why a document is refused, as typed data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

use lash_kernel_doc::{Invalid, InvalidReason};

use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::FunctionName;
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;

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
    #[error("the environment provides no effect `{slot:?}`")]
    MissingEffect {
        slot: lash_vocabulary::names::ImportSlot,
    },
    /// `K-ADM-002`: the environment's effect does not serve the signature
    /// the document expects.
    #[error(
        "effect `{slot:?}` is provided with a signature that does not serve the one expected: {mismatch}"
    )]
    EffectSignature {
        slot: lash_vocabulary::names::ImportSlot,
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
    #[error("effect `{slot:?}` returns {declared:?}; this `perform` expects {stated:?}")]
    PerformResult {
        slot: lash_vocabulary::names::ImportSlot,
        declared: Type,
        stated: Type,
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
            Self::MissingEffect { .. } => "missing_effect",
            Self::EffectSignature { .. } => "effect_signature",
            Self::MissingFunction { .. } => "missing_function",
            Self::UnboundVariable { .. } => "unbound_variable",
            Self::ArgumentType { .. } => "argument_type",
            Self::PerformResult { .. } => "perform_result",
        }
    }
}

pub use lash_vocabulary::types::{SignatureMismatch, SignatureSide};
pub(crate) fn refused(site: Option<&Site>, reason: RefusalReason) -> Refused {
    Refused {
        site: site.cloned(),
        reason,
    }
}
