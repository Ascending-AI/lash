use lash_vm::{ExecutionMode, LashVmHostEnvironment};

/// Explicit compiler/VM descriptions, containing no host handles or grants.
/// Encoded as MessagePack in a `ContextDescription` with kind `vm_run`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    pub environment: LashVmHostEnvironment,
    pub mode: ExecutionMode,
    #[serde(default)]
    pub projected: Vec<ProjectionDescription>,
    #[serde(default)]
    pub observe_execution: bool,
    /// Return a `CellCompletion` with the snapshot's metadata for resident cells.
    pub capture_state_view: bool,
}

/// One projected binding as the worker installs it: plain data, a scalar
/// with its value or a resource projection with its `ResourceRef`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectionDescription {
    pub name: String,
    /// The binding's `Value::Projected`.
    #[serde(with = "lash_vm::effect_value")]
    pub value: lash_vm::Value,
}

/// A worker's read of one projection resource: one request is a `read`, more
/// are one `read_range`, and either is one IPC frame (ADR 0132 §9).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectionRead {
    pub resource: lash_vm::ResourceRef,
    pub requests: Vec<lash_vm::ProjectedReadRequest>,
}

/// The parent's answer to a [`ProjectionRead`], one response per request in
/// order, or the typed reason there is none.
pub type ProjectionAnswer =
    Result<Vec<Option<lash_vm::ProjectedReadResponse>>, lash_vm::ProjectionReadError>;
