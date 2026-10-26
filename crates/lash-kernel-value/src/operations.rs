//! Neutral operation signatures and contract identities.
use crate::types::Signature;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
/// What pins an operation's meaning under its signature: opaque text,
/// compared for equality and never read for anything else.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ContractId(String);

impl ContractId {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An operation as code requires it: the signature it performs the
/// operation under and the contract it was written against, which the
/// operation's runner declares or derives. A manifest states one for each
/// operation the document performs or reaches, and a library body one for
/// each operation it performs itself. A changed contract under one
/// signature is a different requirement (OP-SAME-SIGNATURE).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRequirement {
    pub signature: Signature,
    pub contract: ContractId,
}

impl OperationRequirement {
    /// The requirement of an operation performed under `signature` and
    /// pinned to `contract`.
    pub fn new(signature: Signature, contract: ContractId) -> Self {
        Self {
            signature,
            contract,
        }
    }
}

impl fmt::Display for OperationRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::type_text::requirement_text(self))
    }
}
