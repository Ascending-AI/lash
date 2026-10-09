//! Why a document is refused, as typed data.

use std::fmt;

use lash_kernel_doc::{
    EffectName, FunctionId, FunctionName, Invalid, InvalidReason, Name, Signature, Site, Type,
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

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RefusalReason {
    /// The document, or the body of a library function it reaches, fails
    /// structural validation (`K-DOC-005`); the statement rule is one such
    /// check (`K-STMT-002`).
    #[error("{0}")]
    Invalid(InvalidReason),
    /// `K-ADM-001`: the environment provides no such effect.
    #[error("the environment provides no effect `{effect}`")]
    MissingEffect { effect: EffectName },
    /// `K-ADM-002`: the environment's effect does not serve the signature
    /// the document expects.
    #[error("effect `{effect}` is provided with a signature that does not serve the one expected")]
    EffectSignature {
        effect: EffectName,
        expected: Signature,
        provided: Signature,
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

pub(crate) fn refused(site: Option<&Site>, reason: RefusalReason) -> Refused {
    Refused {
        site: site.cloned(),
        reason,
    }
}
