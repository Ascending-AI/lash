use lashlang::{ExecutionMode, LashlangHostEnvironment};

/// Explicit compiler/VM descriptions, containing no host handles or grants.
/// Encoded as MessagePack in a `ContextDescription` with kind `vm_run`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    pub environment: LashlangHostEnvironment,
    pub mode: ExecutionMode,
    #[serde(default)]
    pub projected: Vec<ProjectionDescription>,
    #[serde(default)]
    pub observe_execution: bool,
    /// Return a `CellCompletion` with the snapshot's metadata for resident cells.
    pub capture_state_view: bool,
    #[serde(default)]
    pub projection_namespace: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectionDescription {
    pub name: String,
    pub key: usize,
    pub type_name: String,
    #[serde(with = "lashlang::effect_value::optional")]
    pub scalar: Option<lashlang::Value>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectionRead {
    pub key: usize,
    pub request: lashlang::ProjectedReadRequest,
}
