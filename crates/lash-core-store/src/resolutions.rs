//! Execution-owned immutable deferred resolution outcomes.
//! Absent means unresolved; infrastructure failures create no terminal row.
use crate::tool_surface::linked::OperationRoute;
use lash_vocabulary::{ToolDefinition, claims::*, names::*, operations::*, package::*};
use serde::{Deserialize, Serialize};

// resolutions: ExecutionId wraps the existing stable execution address encoding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// Stable domain-qualified cell/process execution address, reused on retry, cold resume and adoption.
pub struct ExecutionId(pub String);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One deferred OperationUse in one execution, independent of activation or invocation identity.
pub struct ResolutionKey {
    pub execution: ExecutionId,
    pub use_: OperationUse,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// One immutable terminal resolution. Infrastructure failures create no row; accepted targets must be Waited.
pub enum ResolutionEntry {
    Resolved {
        definition: Box<ToolDefinition>,
        contract: ContractId,
        route: Box<OperationRoute>,
        mode: OperationMode,
        requirements: ClaimRequirements,
    },
    Refused {
        note: String,
    },
    Unavailable {
        note: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Requested name and committed outcome. Concurrent first callers use the winning row under distinct call identities.
pub struct ResolutionRecord {
    pub key: ResolutionKey,
    pub requested: QualifiedName,
    pub entry: ResolutionEntry,
}

/// Trusted host policy output, validated before publishing a new execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSeed {
    pub use_: OperationUse,
    pub entry: ResolutionEntry,
}
