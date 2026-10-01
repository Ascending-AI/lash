//! Executable-artifact refusals shared by the language, storage and worker boundaries.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleArtifactRefusal {
    #[error("unsupported module artifact generation: {0}; recompile and republish the module")]
    Generation(#[source] ModuleArtifactGeneration),
    #[error("corrupt module artifact: {0}")]
    Corrupt(#[source] ModuleArtifactCorruption),
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleArtifactGeneration {
    #[error("obsolete anonymous process type shape")]
    ObsoleteProcessTypeShape,
    #[error("retired compilation dialect")]
    RetiredCompilationDialect,
    #[error("unsupported artifact shape {field}: {value}")]
    FutureShape { field: String, value: String },
    #[error("unsupported artifact family {family}, encoding {encoding}")]
    UnsupportedFamily { family: String, encoding: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleArtifactCorruption {
    #[error("invalid module program: {0:?}")]
    InvalidAst(ModuleArtifactAstRefusal),
    #[error("durable module contains source spans")]
    DurableSpans,
    #[error("process {process} has no output type")]
    IncompleteProcessSignature { process: String },
    #[error("module contains an unlifted process literal")]
    UnliftedProcessLiteral,
    #[error("invalid artifact encoding: {message}")]
    Codec { message: String },
    #[error("artifact {field} mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    #[error("artifact stored under {expected} names {actual}")]
    StorageKeyMismatch { expected: String, actual: String },
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

/// Owned validation evidence suitable for a worker response or a durable journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleArtifactAstRefusal {
    NestingTooDeep { limit: usize },
    LoopControlOutsideLoop { keyword: String },
    ReturnOutsideFunction,
    InvalidParameterName { name: String },
    DuplicateParameter { name: String },
    UnknownProcessSignature,
    MalformedRole { role: String, reason: String },
    DuplicateDeclaration { name: String },
    InvalidProcessOrigin { process: String, reason: String },
}
