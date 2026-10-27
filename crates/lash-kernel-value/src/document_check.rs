//! Why a document is refused, as typed data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{Invalid, InvalidReason};

use crate::names::FunctionId;
use crate::names::FunctionName;
use crate::names::Name;
use crate::sites::Site;
use crate::types::Type;

/// Everything that keeps a document from being admitted. Admission does not
/// stop at the first fault of the environment: every missing effect, every
/// mismatched signature and every missing function identity is named.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[error("{}", list(.errors))]
#[serde(deny_unknown_fields)]
pub struct CheckRefusal {
    pub errors: Vec<CheckFailure>,
}

fn list(errors: &[CheckFailure]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// One reason, and the node at fault when the fault is in a node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckFailure {
    pub site: Option<Site>,
    pub reason: CheckReason,
}

impl fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)?;
        match &self.site {
            Some(site) => write!(f, " (at {site})"),
            None => Ok(()),
        }
    }
}

impl From<Invalid> for CheckFailure {
    fn from(invalid: Invalid) -> Self {
        Self {
            site: invalid.site,
            reason: CheckReason::Invalid(*invalid.reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
#[schemars(rename = "RefusalReason")]
pub enum CheckReason {
    /// The document, or the body of a library function it reaches, fails
    /// structural validation (`K-DOC-005`), a call by name of a function
    /// with no native implementation among them (`K-LIB-002`).
    #[error("{0}")]
    Invalid(InvalidReason),
    /// `K-ADM-001`: the environment provides no such effect.
    #[error("the environment provides no effect `{slot:?}`")]
    MissingEffect { slot: crate::names::ImportSlot },
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
        slot: crate::names::ImportSlot,
        declared: Type,
        stated: Type,
    },
}

impl CheckReason {
    /// A stable local snake_case name for this top-level alternative.
    /// Existing names keep their meaning; new alternatives add names. These
    /// are not HTTP statuses, and old clients may not recognize future names.
    /// The code names the reason independently of its payload or display text.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::MissingEffect { .. } => "missing_effect",
            Self::MissingFunction { .. } => "missing_function",
            Self::UnboundVariable { .. } => "unbound_variable",
            Self::ArgumentType { .. } => "argument_type",
            Self::PerformResult { .. } => "perform_result",
        }
    }
}
