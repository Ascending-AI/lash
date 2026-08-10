use thiserror::Error;

/// A failure while preparing or executing the Lashlang process runtime.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum LashlangRuntimeError {
    /// A projected tool or host resource conflicts with another Lashlang catalog entry.
    #[error("invalid Lashlang host catalog: {source}")]
    HostCatalog {
        #[from]
        source: lashlang::LashlangHostCatalogError,
    },
    /// A tool definition omits its Lashlang module path.
    #[error("tool `{tool}` is missing an explicit Lashlang module path")]
    MissingToolModulePath { tool: String },
    /// A tool definition omits its Lashlang operation name.
    #[error("tool `{tool}` is missing an explicit Lashlang operation name")]
    MissingToolOperation { tool: String },
    /// A tool definition contains an empty Lashlang identifier.
    #[error("tool `{tool}` has an empty Lashlang {label}")]
    EmptyIdentifier { tool: String, label: String },
    /// A tool definition contains an invalid Lashlang identifier.
    #[error("tool `{tool}` has invalid Lashlang {label} `{value}`")]
    InvalidIdentifier {
        tool: String,
        label: String,
        value: String,
    },
    /// A tool's `lashlang.tool` binding cannot be decoded.
    #[error("tool `{tool}` has invalid `lashlang.tool` binding: {source}")]
    InvalidToolBinding {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
    /// A tool definition omits its required `lashlang.tool` binding.
    #[error("tool `{tool}` is missing an explicit `lashlang.tool` binding")]
    MissingToolBinding { tool: String },
    /// The `lashlang.surface` extension payload cannot be decoded.
    #[error("invalid `lashlang.surface` extension payload: {source}")]
    InvalidSurfaceExtension {
        #[source]
        source: serde_json::Error,
    },
    /// The host does not provide process execution.
    #[error("processes are not available")]
    ProcessesUnavailable,
    /// The host does not provide sleeping.
    #[error("sleep is not available")]
    SleepUnavailable,
    /// The host does not provide process signals.
    #[error("process signals are not available")]
    ProcessSignalsUnavailable,
    /// The host does not provide triggers.
    #[error("triggers are not available")]
    TriggersUnavailable,
    /// The host does not provide label annotations.
    #[error("label annotations are not available")]
    LabelAnnotationsUnavailable,
    /// A referenced module is absent from the host catalogue.
    #[error("module `{module}` is not available")]
    ModuleUnavailable { module: String },
    /// A referenced module has a different type than required.
    #[error("module `{module}` has type `{actual}`, expected `{expected}`")]
    ModuleTypeMismatch {
        module: String,
        actual: String,
        expected: String,
    },
    /// A module operation resolves to a different host operation than required.
    #[error(
        "module `{module}` operation `{operation}` resolves to `{actual}`, expected `{expected}`"
    )]
    ModuleOperationMismatch {
        module: String,
        operation: String,
        actual: String,
        expected: String,
    },
    /// A module does not expose the requested operation.
    #[error("module `{module}` does not expose operation `{operation}`")]
    ModuleOperationUnavailable { module: String, operation: String },
    /// A referenced resource type is absent from the host catalogue.
    #[error("resource type `{resource_type}` is not available")]
    ResourceTypeUnavailable { resource_type: String },
    /// A resource type does not expose the requested operation.
    #[error("resource type `{resource_type}` does not expose operation `{operation}`")]
    ResourceOperationUnavailable {
        resource_type: String,
        operation: String,
    },
    /// A resource operation's input type is incompatible with Lashlang.
    #[error("resource type `{resource_type}` operation `{operation}` has incompatible input type")]
    ResourceInputMismatch {
        resource_type: String,
        operation: String,
    },
    /// A resource operation's output type is incompatible with Lashlang.
    #[error("resource type `{resource_type}` operation `{operation}` has incompatible output type")]
    ResourceOutputMismatch {
        resource_type: String,
        operation: String,
    },
    /// A required host data type is absent from the host catalogue.
    #[error("host data type `{name}` is not available")]
    HostDataTypeUnavailable { name: String },
    /// A host data type has a structure incompatible with Lashlang.
    #[error("host data type `{name}` has incompatible structure")]
    HostDataTypeMismatch { name: String },
    /// A required value constructor is absent from the host catalogue.
    #[error("value constructor `{path}` is not available")]
    ValueConstructorUnavailable { path: String },
    /// A value constructor's input type is incompatible with Lashlang.
    #[error("value constructor `{path}` has incompatible input type")]
    ValueConstructorInputMismatch { path: String },
    /// A value constructor's output type is incompatible with Lashlang.
    #[error("value constructor `{path}` has incompatible output type")]
    ValueConstructorOutputMismatch { path: String },
    /// A required trigger source type is absent from the host catalogue.
    #[error("trigger source type `{source_type}` is not available")]
    TriggerSourceUnavailable { source_type: String },
    /// A trigger source's event type is incompatible with Lashlang.
    #[error("trigger source type `{source_type}` has incompatible event type")]
    TriggerSourceMismatch { source_type: String },
    /// Loading the Lashlang module artifact from storage failed.
    #[error("failed to load lashlang module artifact: {source}")]
    LoadArtifact {
        #[source]
        source: lashlang::ArtifactStoreError,
    },
    /// The requested Lashlang module artifact is missing from storage.
    #[error("missing lashlang module artifact `{module_ref}` for process `{process}`")]
    MissingArtifact { module_ref: String, process: String },
    /// The module artifact's host requirements differ from the process request.
    #[error(
        "lashlang module artifact `{module_ref}` host requirements mismatch: process requested {requested}, artifact has {actual}"
    )]
    ArtifactRequirementsMismatch {
        module_ref: String,
        requested: String,
        actual: String,
    },
    /// The module artifact does not export the requested process reference.
    #[error(
        "lashlang module artifact `{module_ref}` does not export process `{process}` as requested ref {process_ref}"
    )]
    ArtifactProcessMismatch {
        module_ref: String,
        process: String,
        process_ref: String,
    },
    /// Serializing process arguments failed.
    #[error("failed to serialize process args: {source}")]
    SerializeProcessArgs {
        #[source]
        source: serde_json::Error,
    },
    /// Process arguments did not serialize to a record.
    #[error("process args must serialize as a record")]
    ProcessArgsNotRecord,
    /// Deriving the deterministic process identifier failed.
    #[error("failed to derive deterministic process id: {source}")]
    DeriveProcessId {
        #[source]
        source: serde_json::Error,
    },
    /// Encoding the process input failed.
    #[error("failed to encode process input: {source}")]
    EncodeProcessInput {
        #[source]
        source: serde_json::Error,
    },
    /// Parsing the typed-output schema witness failed.
    #[error(transparent)]
    OutputSchema(#[from] lashlang::OutputSchemaError),
}

/// A typed failure exposed through the Lashlang execution-host boundary.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum LashlangHostError {
    /// Serializing a Lashlang value for a host operation failed.
    #[error("failed to serialize value: {source}")]
    SerializeValue {
        #[source]
        source: serde_json::Error,
    },
    /// A tool rejected the call with its original message.
    #[error("{message}")]
    ToolRejected { message: String },
    /// `sleep for` received a value outside its duration vocabulary.
    #[error(
        "`sleep for` expects a non-negative millisecond number or duration string, got {actual}"
    )]
    InvalidSleepDuration { actual: String },
    /// Parsing an RFC3339 `sleep until` deadline failed.
    #[error("`sleep until` expects RFC3339 text or Unix epoch milliseconds: {source}")]
    InvalidSleepDeadline {
        #[source]
        source: chrono::ParseError,
    },
    /// `sleep until` received a value outside its deadline vocabulary.
    #[error("`sleep until` expects RFC3339 text or Unix epoch milliseconds, got {actual}")]
    InvalidSleepDeadlineValue { actual: String },
    /// Parsing the numeric component of a duration string failed.
    #[error("invalid duration `{value}`: {source}")]
    InvalidDurationNumber {
        value: String,
        #[source]
        source: std::num::ParseFloatError,
    },
    /// A duration string contained a negative or non-finite value.
    #[error("invalid duration `{value}`: expected a non-negative finite number")]
    InvalidDurationValue { value: String },
    /// A module operation received a payload that is not an object.
    #[error("module operation payload must be an object")]
    ModulePayloadNotObject,
    /// A module operation requiring authority did not receive an authority value.
    #[error("module operation `{operation}` requires a module authority receiver")]
    ModuleAuthorityRequired { operation: String },
    /// A module operation resolved to a host operation unavailable at runtime.
    #[error(
        "module operation `{operation}` resolved to unavailable host operation `{host_operation}`"
    )]
    ResolvedOperationUnavailable {
        operation: String,
        host_operation: String,
    },
    /// A module operation did not have a deterministic execution call site.
    #[error(
        "module operation `{operation}` resolved to host operation `{host_operation}` but has no deterministic lashlang execution call site"
    )]
    OperationCallSiteMissing {
        operation: String,
        host_operation: String,
    },
    /// Preparing a child-process start at the process host boundary failed.
    #[error("{message}")]
    PrepareProcessStart { message: String },
    /// Appending a process event at the process host boundary failed.
    #[error("{message}")]
    AppendProcessEvent { message: String },
    /// Sleeping a process at the process host boundary failed.
    #[error("{message}")]
    SleepProcess { message: String },
    /// Validating a signal name at the process host boundary failed.
    #[error("{message}")]
    ValidateSignalName { message: String },
    /// Reading durable signal-wait state at the process host boundary failed.
    #[error("{message}")]
    ReadSignalWait { message: String },
    /// Persisting signal-wait state at the process host boundary failed.
    #[error("{message}")]
    SetSignalWait { message: String },
    /// Awaiting a signal at the process host boundary failed.
    #[error("{message}")]
    AwaitSignal { message: String },
    /// Clearing signal-wait state at the process host boundary failed.
    #[error("{message}")]
    ClearSignalWait { message: String },
    /// Sending a signal at the process host boundary failed.
    #[error("{message}")]
    SignalProcess { message: String },
    /// `print` was invoked from a process body where it is unavailable.
    #[error("`print` is not available inside lashlang process bodies")]
    PrintUnavailable,
    /// `signal_run` received a value that is not a process handle.
    #[error("signal_run expects a process handle")]
    InvalidProcessHandle,
    /// A `signal_run` process handle omitted its identifier.
    #[error("signal_run process handle is missing `id`")]
    ProcessHandleMissingId,
    /// A module resource does not expose the requested operation.
    #[error("module `{module}` of type `{resource_type}` does not expose operation `{operation}`")]
    ModuleOperationUnavailable {
        module: String,
        resource_type: String,
        operation: String,
    },
}

impl From<LashlangHostError> for lashlang::ExecutionHostError {
    fn from(error: LashlangHostError) -> Self {
        Self::new(error.to_string())
    }
}

/// Stable wire codes for Lashlang process failures.
///
/// This enum intentionally remains exhaustive: consumers must handle every
/// member of this closed wire vocabulary when matching failure codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LashlangProcessFailureCode {
    /// A Restate segment resumed with a different program hash.
    RestateSegmentProgramHashMismatch,
    /// A process payload could not be decoded.
    ProcessPayloadInvalid,
    /// The process's module artifact is missing.
    ProcessModuleArtifactMissing,
    /// The process and artifact host requirements differ.
    ProcessHostRequirementsMismatch,
    /// The process reference does not match the artifact export.
    ProcessRefMismatch,
    /// The process host environment is invalid.
    ProcessHostEnvironmentInvalid,
    /// The process host environment does not satisfy artifact requirements.
    ProcessHostEnvironmentIncompatible,
    /// Compiling the process failed.
    ProcessCompileFailed,
    /// A segment handover payload is invalid.
    ProcessSegmentHandoverInvalid,
    /// Resuming a process segment failed.
    ProcessSegmentResumeFailed,
    /// The process reported an explicit failure.
    ProcessFailed,
    /// Lashlang execution raised a runtime error.
    ProcessRuntimeError,
    /// Lashlang exhausted its configured instruction or active-time bound.
    ProcessExecutionBoundExhausted,
}

impl LashlangProcessFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RestateSegmentProgramHashMismatch => "restate_segment_program_hash_mismatch",
            Self::ProcessPayloadInvalid => "process_payload_invalid",
            Self::ProcessModuleArtifactMissing => "process_module_artifact_missing",
            Self::ProcessHostRequirementsMismatch => "process_host_requirements_mismatch",
            Self::ProcessRefMismatch => "process_ref_mismatch",
            Self::ProcessHostEnvironmentInvalid => "process_host_environment_invalid",
            Self::ProcessHostEnvironmentIncompatible => "process_host_environment_incompatible",
            Self::ProcessCompileFailed => "process_compile_failed",
            Self::ProcessSegmentHandoverInvalid => "process_segment_handover_invalid",
            Self::ProcessSegmentResumeFailed => "process_segment_resume_failed",
            Self::ProcessFailed => "process_failed",
            Self::ProcessRuntimeError => "process_runtime_error",
            Self::ProcessExecutionBoundExhausted => "process_execution_bound_exhausted",
        }
    }
}

impl std::fmt::Display for LashlangProcessFailureCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! failure_code_test {
        ($name:ident, $variant:ident, $wire:literal) => {
            #[test]
            fn $name() {
                let code = LashlangProcessFailureCode::$variant;
                assert_eq!(code.as_str(), $wire);
                assert_eq!(code.to_string(), $wire);
                assert_eq!(
                    serde_json::to_string(&code).expect("serialize code"),
                    concat!("\"", $wire, "\"")
                );
                assert_eq!(
                    serde_json::from_str::<LashlangProcessFailureCode>(concat!("\"", $wire, "\""))
                        .expect("deserialize code"),
                    code
                );
            }
        };
    }

    failure_code_test!(
        restate_segment_program_hash_mismatch,
        RestateSegmentProgramHashMismatch,
        "restate_segment_program_hash_mismatch"
    );
    failure_code_test!(
        process_payload_invalid,
        ProcessPayloadInvalid,
        "process_payload_invalid"
    );
    failure_code_test!(
        process_module_artifact_missing,
        ProcessModuleArtifactMissing,
        "process_module_artifact_missing"
    );
    failure_code_test!(
        process_host_requirements_mismatch,
        ProcessHostRequirementsMismatch,
        "process_host_requirements_mismatch"
    );
    failure_code_test!(
        process_ref_mismatch,
        ProcessRefMismatch,
        "process_ref_mismatch"
    );
    failure_code_test!(
        process_host_environment_invalid,
        ProcessHostEnvironmentInvalid,
        "process_host_environment_invalid"
    );
    failure_code_test!(
        process_host_environment_incompatible,
        ProcessHostEnvironmentIncompatible,
        "process_host_environment_incompatible"
    );
    failure_code_test!(
        process_compile_failed,
        ProcessCompileFailed,
        "process_compile_failed"
    );
    failure_code_test!(
        process_segment_handover_invalid,
        ProcessSegmentHandoverInvalid,
        "process_segment_handover_invalid"
    );
    failure_code_test!(
        process_segment_resume_failed,
        ProcessSegmentResumeFailed,
        "process_segment_resume_failed"
    );
    failure_code_test!(process_failed, ProcessFailed, "process_failed");
    failure_code_test!(
        process_runtime_error,
        ProcessRuntimeError,
        "process_runtime_error"
    );
    failure_code_test!(
        process_execution_bound_exhausted,
        ProcessExecutionBoundExhausted,
        "process_execution_bound_exhausted"
    );
}
