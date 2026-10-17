//! Refusals of a stored module artifact (an admitted kernel document), shared
//! by the storage and runtime boundaries.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleArtifactRefusal {
    #[error("corrupt module artifact: {0}")]
    Corrupt(#[source] ModuleArtifactCorruption),
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleArtifactCorruption {
    #[error("invalid {record_kind} reference {reference}")]
    InvalidReference {
        record_kind: String,
        reference: String,
    },
    #[error("stored {record_kind} data is corrupt: {message}")]
    Storage {
        record_kind: String,
        message: String,
    },
}
