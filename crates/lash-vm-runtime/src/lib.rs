use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::collections::BTreeSet;
use std::sync::Arc;

mod admissions;
mod aggregate;
mod args;
mod worker_execution;
pub use admissions::{MemberAdmissions, RunAdmissions};
pub use aggregate::{
    AggregateAnswer, AggregateConsumer, LeafStanding, aggregate_answer,
    host_lifetime_failure_message, is_tool_call_limit_failure, timer_duration_ms,
    tool_call_limit_failure,
};
pub use worker_execution::{OperationAdmissions, Performing, PerformingGate, WorkerRun};
mod error;
pub use error::{
    LashVmHostError, LashVmProcessFailureCode, LashVmRuntimeError, ProcessHostOp, ToolBindingError,
};
mod host_identity;
pub use host_identity::LashVmHostIdentities;
mod cell_bindings;
pub use cell_bindings::{
    CellBindingDrift, CellBindingDriftKind, CellToolBindings, RecordedCellToolBindings,
    journal_cell_tool_bindings,
};
mod replay_commands;
pub use replay_commands::{CommandInFlight, ReplayCommands, retype_replay_mismatch};
mod replay_run;
pub use replay_run::{
    CommandShape, IssuedCommand, LASH_VM_CELL_JOURNAL_GRAMMAR_VERSION,
    LASH_VM_REPLAY_KEY_GRAMMAR_VERSION, LashVmReplayNamespace, LashVmReplayRun, SealAttribution,
    lash_vm_cell_generation,
};
mod language_trace_host;
pub use language_trace_host::{LanguageTraceHost, trace_failure};
mod process_create_tool;
mod trace_waits;
pub use process_create_tool::{
    ProcessCreateTools, process_create_tool_definition, process_create_tool_provider,
};
pub use trace_waits::TraceWaitBookkeeping;
mod language_runtime;
pub use language_runtime::{
    is_language_runtime_receiver, journaled_language_runtime_value, language_runtime_operation,
};

pub use lash_trace::{
    TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionGeneration, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceNodeAwaited,
    TraceNodeWaitKind, TraceNodeWaitResolution, WorkflowExecutionOverlay, WorkflowOverlayChildLink,
    WorkflowOverlayDocument, WorkflowOverlayOccurrence, WorkflowOverlaySite,
};
pub use lash_vm::{
    LASH_TYPE_KEY, LashVmArtifacts, LashVmHostCatalog, LashVmHostEnvironment,
    LashVmLanguageFeatures,
};

/// The dialect-agnostic tool binding type, its manifest key, and the one
/// host-facing setter are defined in `lash-core` beside [`lash_core::ToolManifest`]
/// and re-exported here so existing runtime-crate paths keep resolving.
pub use lash_core::{TOOL_BINDING_KEY, ToolBinding, ToolDefinitionBindingExt};
pub use lash_vm::LASH_VM_ENGINE_KIND;
pub const LASH_VM_SURFACE_EXTENSION_ID: &str = "lash_vm.surface";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LashVmSurfaceContribution {
    pub language_features: LashVmLanguageFeatures,
    pub resources: LashVmHostCatalog,
}

impl LashVmSurfaceContribution {
    pub fn new(language_features: LashVmLanguageFeatures, resources: LashVmHostCatalog) -> Self {
        Self {
            language_features,
            resources,
        }
    }

    pub fn from_surface(surface: LashVmSurface) -> Self {
        Self {
            language_features: surface.language_features,
            resources: surface.resources,
        }
    }
}

/// Wrap a [`LashVmSurfaceContribution`] as the plugin extension a
/// `SessionPlugin` returns from its `extension_contributions`, so a host can
/// grant surface vocabulary per process from the execution env spec's plugin
/// options rather than once per core.
pub fn lash_vm_surface_extension(
    contribution: &LashVmSurfaceContribution,
) -> Result<lash_core::plugin::PluginExtensionContribution, serde_json::Error> {
    lash_core::plugin::PluginExtensionContribution::new(LASH_VM_SURFACE_EXTENSION_ID, contribution)
}

/// Resolution over the dialect-agnostic [`ToolBinding`] relocated to
/// `lash-core`: validate the authored module path and operation and produce
/// the typed, front-end-neutral [`ResolvedToolBinding`] the runtime links and
/// each dialect spells in its own syntax (ADR 0096).
pub trait ToolBindingResolutionExt {
    fn executable_for(&self, tool_name: &str) -> Result<ResolvedToolBinding, ToolBindingError>;
}

impl ToolBindingResolutionExt for ToolBinding {
    fn executable_for(&self, tool_name: &str) -> Result<ResolvedToolBinding, ToolBindingError> {
        if self.module_path.is_empty() {
            return Err(ToolBindingError::MissingModulePath {
                tool: tool_name.to_string(),
            });
        }
        for segment in &self.module_path {
            validate_lash_vm_identifier(tool_name, "module path segment", segment)?;
        }
        let operation =
            self.operation
                .as_deref()
                .ok_or_else(|| ToolBindingError::MissingOperation {
                    tool: tool_name.to_string(),
                })?;
        validate_lash_vm_identifier(tool_name, "operation name", operation)?;
        let authority_type = self
            .authority_type
            .as_deref()
            .filter(|authority_type| !authority_type.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_authority_type(&self.module_path));
        Ok(ResolvedToolBinding {
            module_path: self.module_path.clone(),
            operation: operation.to_string(),
            authority_type,
            aliases: self.aliases.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedToolBinding {
    pub module_path: Vec<String>,
    pub operation: String,
    pub authority_type: String,
    pub aliases: Vec<String>,
}

impl ResolvedToolBinding {
    pub fn module_path_string(&self) -> String {
        self.module_path.join(".")
    }

    pub fn call_path(&self) -> String {
        format!("{}.{}", self.module_path_string(), self.operation)
    }
}

fn default_authority_type(module_path: &[String]) -> String {
    module_path
        .last()
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Tool".to_string(),
            }
        })
        .unwrap_or_else(|| "Tool".to_string())
}

fn validate_lash_vm_identifier(
    tool_name: &str,
    label: &'static str,
    value: &str,
) -> Result<(), ToolBindingError> {
    let value = value.trim();
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ToolBindingError::InvalidIdentifier {
            tool: tool_name.to_string(),
            part: label,
            value: "<empty>".to_string(),
        });
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(ToolBindingError::InvalidIdentifier {
            tool: tool_name.to_string(),
            part: label,
            value: value.to_string(),
        });
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return Err(ToolBindingError::InvalidIdentifier {
            tool: tool_name.to_string(),
            part: label,
            value: value.to_string(),
        });
    }
    Ok(())
}

pub fn required_tool_binding(
    manifest: &lash_core::ToolManifest,
) -> Result<ToolBinding, ToolBindingError> {
    ToolManifestBindingExt::tool_binding(manifest)?.ok_or_else(|| {
        ToolBindingError::MissingBinding {
            tool: manifest.name.clone(),
            binding_key: TOOL_BINDING_KEY,
        }
    })
}

pub fn required_tool_executable(
    manifest: &lash_core::ToolManifest,
) -> Result<ResolvedToolBinding, ToolBindingError> {
    required_tool_binding(manifest)?.executable_for(&manifest.name)
}

pub trait ToolManifestBindingExt {
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError>;
}

impl ToolManifestBindingExt for lash_core::ToolManifest {
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError> {
        self.bindings
            .get(TOOL_BINDING_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|source| ToolBindingError::MalformedPayload {
                tool: self.name.clone(),
                binding_key: TOOL_BINDING_KEY,
                source,
            })
    }
}

#[derive(Clone, Debug, Default)]
pub struct LashVmSurface {
    pub language_features: LashVmLanguageFeatures,
    pub resources: LashVmHostCatalog,
}

impl LashVmSurface {
    pub fn new(language_features: LashVmLanguageFeatures, resources: LashVmHostCatalog) -> Self {
        Self {
            language_features,
            resources,
        }
    }

    pub fn with_resources(
        mut self,
        resources: LashVmHostCatalog,
    ) -> Result<Self, LashVmRuntimeError> {
        self.resources.try_extend(resources)?;
        Ok(self)
    }

    pub fn with_plugin_extensions(
        mut self,
        extensions: &lash_core::PluginExtensions,
    ) -> Result<Self, LashVmRuntimeError> {
        for payload in extensions.payloads(LASH_VM_SURFACE_EXTENSION_ID) {
            let contribution: LashVmSurfaceContribution =
                serde_json::from_value(payload.clone())
                    .map_err(|source| LashVmRuntimeError::InvalidSurfaceExtension { source })?;

            self.language_features = self.language_features.union(contribution.language_features);
            self.resources.try_extend(contribution.resources)?;
        }
        Ok(self)
    }

    pub fn host_environment(
        &self,
        catalog: &lash_core::ToolCatalog,
    ) -> Result<LashVmHostEnvironment, ToolBindingError> {
        self.host_environment_masking(catalog, &BTreeSet::new())
    }

    /// Builds the link-time environment while excluding exact ambient call
    /// paths already decided by the deferred-resolution journal.
    ///
    /// Masking happens before the flat Tool Catalog and contributed surface
    /// resources are merged and validated. Thus a recorded authority can mask
    /// every later ambient claimant for its path, while unrelated collisions
    /// and malformed definitions retain their normal failures.
    pub fn host_environment_masking(
        &self,
        catalog: &lash_core::ToolCatalog,
        masked_call_paths: &BTreeSet<String>,
    ) -> Result<LashVmHostEnvironment, ToolBindingError> {
        let mut resources = self.resources.clone();
        mask_call_paths(&mut resources, masked_call_paths);
        lash_vm_host_environment_from_resources(
            masked_tool_catalog_resources(catalog, masked_call_paths)?,
            self.language_features,
            resources,
        )
    }
}

fn mask_call_paths(resources: &mut LashVmHostCatalog, masked_call_paths: &BTreeSet<String>) {
    for path in masked_call_paths {
        if let Some((module_path, operation)) = path.rsplit_once('.') {
            resources.mask_module_operation(module_path, operation);
        }
    }
}

/// The catalog's imported resources, derived once per catalog generation: a
/// cell or prompt against an unchanged tool set reuses them rather than
/// re-importing every tool schema. `None` when the whole catalog does not
/// import.
struct ToolCatalogResources(Option<LashVmHostCatalog>);

/// The resources the catalog's members outside `masked_call_paths` import.
///
/// When every member imports, this is the memoized import with the masked
/// paths removed, which is exactly what importing the remaining members
/// builds: a tool-only import binds every operation to one module, so
/// masking a path takes out that member's binding and whatever only it held.
/// When the whole catalog does not import, a masked member may be what fails,
/// so the remaining members are imported afresh and their own error stands.
fn masked_tool_catalog_resources(
    catalog: &lash_core::ToolCatalog,
    masked_call_paths: &BTreeSet<String>,
) -> Result<LashVmHostCatalog, ToolBindingError> {
    let imported = catalog
        .derived(|catalog| ToolCatalogResources(lash_vm_resources_from_tool_catalog(catalog).ok()));
    let Some(resources) = &imported.0 else {
        return lash_vm_resources_from_tool_catalog(&filtered_tool_catalog(
            catalog,
            masked_call_paths,
        ));
    };
    let mut resources = resources.clone();
    mask_call_paths(&mut resources, masked_call_paths);
    Ok(resources)
}

fn filtered_tool_catalog(
    catalog: &lash_core::ToolCatalog,
    masked_call_paths: &BTreeSet<String>,
) -> lash_core::ToolCatalog {
    catalog.filtered(|entry| {
        let Ok(binding) = required_tool_executable(&entry.manifest) else {
            // Preserve ordinary validation for malformed unrelated entries.
            return true;
        };
        !masked_call_paths.contains(&binding.call_path())
    })
}

pub fn lash_vm_host_environment_from_tool_catalog(
    catalog: &lash_core::ToolCatalog,
    language_features: LashVmLanguageFeatures,
    host_resources: LashVmHostCatalog,
) -> Result<LashVmHostEnvironment, ToolBindingError> {
    lash_vm_host_environment_from_resources(
        masked_tool_catalog_resources(catalog, &BTreeSet::new())?,
        language_features,
        host_resources,
    )
}

fn lash_vm_host_environment_from_resources(
    tool_resources: LashVmHostCatalog,
    language_features: LashVmLanguageFeatures,
    host_resources: LashVmHostCatalog,
) -> Result<LashVmHostEnvironment, ToolBindingError> {
    let mut resources = tool_resources.try_merged(host_resources)?;
    for (operation, host_operation) in [
        (
            lash_vm::LANGUAGE_RUNTIME_NOW_OPERATION,
            "lash_vm.runtime.now",
        ),
        (
            lash_vm::LANGUAGE_RUNTIME_RANDOM_OPERATION,
            "lash_vm.runtime.random",
        ),
    ] {
        resources.add_module_operation_contract(
            [lash_vm::LANGUAGE_RUNTIME_MODULE_PATH],
            lash_vm::LANGUAGE_RUNTIME_RESOURCE_TYPE,
            operation,
            host_operation,
            &lash_vm::OperationContract::new(
                serde_json::json!({}),
                serde_json::json!({ "type": "number" }),
            ),
        )?;
    }
    Ok(LashVmHostEnvironment::new(resources).with_language_features(language_features))
}

pub fn lash_vm_resources_from_tool_catalog(
    catalog: &lash_core::ToolCatalog,
) -> Result<LashVmHostCatalog, ToolBindingError> {
    let mut host_catalog = LashVmHostCatalog::new();
    // Every catalog member is callable.
    for entry in catalog.tools.iter() {
        let binding = required_tool_executable(&entry.manifest)?;
        let contract = lash_vm_tool_operation_contract(&entry.contract);
        host_catalog.add_module_operation_contract(
            binding.module_path.iter().map(String::as_str),
            binding.authority_type.clone(),
            binding.operation.clone(),
            entry.manifest.id.to_string(),
            &contract,
        )?;
    }
    Ok(host_catalog)
}

/// Re-expresses a tool contract as the host-operation contract the catalog reads.
///
/// This is a pure re-shaping: the schemas the tool declares travel unchanged
/// into the catalog, which is what keeps a tool and a lash-owned host
/// operation subject to the same importer, `x-lash` included.
fn lash_vm_tool_operation_contract(
    contract: &lash_core::ToolContract,
) -> lash_vm::OperationContract {
    let input_schema = contract.input_schema.canonical().clone();
    match &contract.output_contract {
        lash_core::ToolOutputContract::Static => lash_vm::OperationContract::new(
            input_schema,
            contract.output_schema.canonical().clone(),
        ),
        lash_core::ToolOutputContract::FromInputSchema {
            input_field,
            default_schema,
        } => lash_vm::OperationContract::from_input_field(
            input_schema,
            input_field.clone(),
            default_schema
                .as_ref()
                .map(|schema| schema.as_value().clone()),
        ),
    }
}

pub fn lash_vm_host_environment_satisfies_requirements(
    required: &lash_vm::HostRequirements,
    current: &LashVmHostEnvironment,
) -> Result<(), LashVmRuntimeError> {
    if required.language_features.label_annotations && !current.language_features.label_annotations
    {
        return Err(LashVmRuntimeError::LabelAnnotationsUnavailable);
    }

    for (_, module) in required.resources.module_instances() {
        let current_module = current
            .resources
            .resolve_module_path(&module.path)
            .ok_or_else(|| LashVmRuntimeError::ModuleUnavailable {
                module: module.alias.clone(),
            })?;
        if current_module.resource_type != module.resource_type {
            return Err(LashVmRuntimeError::ModuleTypeMismatch {
                module: module.alias.clone(),
                actual: current_module.resource_type.to_string(),
                expected: module.resource_type.clone(),
            });
        }
        for (operation, required_binding) in &module.operations {
            match current.resources.resolve_module_operation(
                &module.resource_type,
                &module.alias,
                operation,
            ) {
                Some(current_binding)
                    if current_binding.host_operation == required_binding.host_operation => {}
                Some(current_binding) => {
                    return Err(LashVmRuntimeError::ModuleOperationMismatch {
                        module: module.alias.clone(),
                        operation: operation.clone(),
                        actual: current_binding.host_operation.to_string(),
                        expected: required_binding.host_operation.clone(),
                    });
                }
                None => {
                    return Err(LashVmRuntimeError::ModuleOperationUnavailable {
                        module: module.alias.clone(),
                        operation: operation.clone(),
                    });
                }
            }
        }
    }

    for (resource_type, required_type) in required.resources.resource_types() {
        if !current.resources.has_resource_type(resource_type) {
            return Err(LashVmRuntimeError::ResourceTypeUnavailable {
                resource_type: resource_type.to_string(),
            });
        }
        for (operation, required_binding) in &required_type.operations {
            let current_binding = current
                .resources
                .resolve_operation(resource_type, operation)
                .ok_or_else(|| LashVmRuntimeError::ResourceOperationUnavailable {
                    resource_type: resource_type.to_string(),
                    operation: operation.clone(),
                })?;
            if current_binding.input_ty != required_binding.input_ty {
                return Err(LashVmRuntimeError::ResourceInputMismatch {
                    resource_type: resource_type.to_string(),
                    operation: operation.clone(),
                });
            }
            if current_binding.output_ty != required_binding.output_ty {
                return Err(LashVmRuntimeError::ResourceOutputMismatch {
                    resource_type: resource_type.to_string(),
                    operation: operation.clone(),
                });
            }
        }
    }
    for (name, required_data_type) in required.resources.named_data_types() {
        let current_data_type =
            current
                .resources
                .resolve_named_data_type(name)
                .ok_or_else(|| LashVmRuntimeError::HostDataTypeUnavailable {
                    name: name.to_string(),
                })?;
        if current_data_type != required_data_type {
            return Err(LashVmRuntimeError::HostDataTypeMismatch {
                name: name.to_string(),
            });
        }
    }
    for (path, required_binding) in required.resources.value_constructors() {
        let current_binding = current
            .resources
            .resolve_value_constructor(&path.split('.').collect::<Vec<_>>())
            .ok_or_else(|| LashVmRuntimeError::ValueConstructorUnavailable {
                path: path.to_string(),
            })?;
        if current_binding.input_ty != required_binding.input_ty {
            return Err(LashVmRuntimeError::ValueConstructorInputMismatch {
                path: path.to_string(),
            });
        }
        if current_binding.output_ty != required_binding.output_ty {
            return Err(LashVmRuntimeError::ValueConstructorOutputMismatch {
                path: path.to_string(),
            });
        }
    }

    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LashVmProcessInput {
    pub module_ref: lash_vm::ModuleRef,
    pub process_ref: lash_vm::ProcessRef,
    pub host_requirements_ref: lash_vm::HostRequirementsRef,
    #[serde(skip)]
    pub process_name: String,
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
}

/// Whether a caller can and must check the live host environment.
pub enum LashVmHostEnvironmentCheck<'a> {
    /// Prepare validates immutable artifact claims and deliberately omits live-host checks.
    OmitHostEnvironment,
    CheckHostEnvironment(Result<&'a LashVmHostEnvironment, String>),
}

/// Typed refusal shared by prepare and the authoritative run-time recheck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LashVmProcessAdmissionRefusal {
    HostRequirementsMismatch {
        process: String,
        requested: String,
        actual: String,
    },
    ProcessRefMismatch {
        module_ref: String,
        process: String,
        process_ref: String,
    },
    HostEnvironmentInvalid {
        message: String,
    },
    HostEnvironmentIncompatible {
        process: String,
        message: String,
    },
}

impl LashVmProcessAdmissionRefusal {
    pub const fn failure_code(&self) -> LashVmProcessFailureCode {
        match self {
            Self::HostRequirementsMismatch { .. } => {
                LashVmProcessFailureCode::ProcessHostRequirementsMismatch
            }
            Self::ProcessRefMismatch { .. } => LashVmProcessFailureCode::ProcessRefMismatch,
            Self::HostEnvironmentInvalid { .. } => {
                LashVmProcessFailureCode::ProcessHostEnvironmentInvalid
            }
            Self::HostEnvironmentIncompatible { .. } => {
                LashVmProcessFailureCode::ProcessHostEnvironmentIncompatible
            }
        }
    }
}

impl std::fmt::Display for LashVmProcessAdmissionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostRequirementsMismatch {
                process,
                requested,
                actual,
            } => write!(
                formatter,
                "lash_vm process `{process}` requested surface {requested}, artifact has {actual}"
            ),
            Self::ProcessRefMismatch {
                module_ref,
                process,
                process_ref,
            } => write!(
                formatter,
                "lash_vm module `{module_ref}` does not export process `{process}` as requested ref {process_ref}"
            ),
            Self::HostEnvironmentInvalid { message } => formatter.write_str(message),
            Self::HostEnvironmentIncompatible { process, message } => write!(
                formatter,
                "lash_vm process `{process}` is incompatible with this host surface: {message}"
            ),
        }
    }
}

impl std::error::Error for LashVmProcessAdmissionRefusal {}

pub fn validate_lash_vm_process_admission(
    artifact: &lash_vm_client::InspectedArtifact,
    input: &LashVmProcessInput,
    host: LashVmHostEnvironmentCheck<'_>,
) -> Result<(), LashVmProcessAdmissionRefusal> {
    if artifact.host_requirements_ref() != &input.host_requirements_ref {
        return Err(LashVmProcessAdmissionRefusal::HostRequirementsMismatch {
            process: input.process_name.clone(),
            requested: input.host_requirements_ref.to_string(),
            actual: artifact.host_requirements_ref().to_string(),
        });
    }
    if artifact.process_name_for_ref(&input.process_ref).is_none() {
        return Err(LashVmProcessAdmissionRefusal::ProcessRefMismatch {
            module_ref: input.module_ref.to_string(),
            process: input.process_name.clone(),
            process_ref: format!("{:?}", input.process_ref),
        });
    }
    if let LashVmHostEnvironmentCheck::CheckHostEnvironment(host) = host {
        let host = host
            .map_err(|message| LashVmProcessAdmissionRefusal::HostEnvironmentInvalid { message })?;
        lash_vm_host_environment_satisfies_requirements(artifact.host_requirements(), host)
            .map_err(
                |error| LashVmProcessAdmissionRefusal::HostEnvironmentIncompatible {
                    process: input.process_name.clone(),
                    message: error.to_string(),
                },
            )?;
    }
    Ok(())
}

impl LashVmProcessInput {
    pub fn process_identity(&self) -> lash_core::ProcessIdentity {
        lash_vm_process_identity(self)
    }

    /// The executable generation a run of this input runs as (FIG-3571):
    /// the program identity its incarnation's start record is stamped with.
    pub fn executable_generation(&self) -> lash_core::ExecutableGeneration {
        lash_core::ExecutableGeneration::new(process::lash_vm_program_hash(self))
    }

    pub fn to_process_input(&self) -> Result<lash_core::ProcessInput, serde_json::Error> {
        Ok(lash_core::ProcessInput::Engine {
            kind: LASH_VM_ENGINE_KIND.to_string(),
            payload: serde_json::to_value(self)?,
        })
    }

    pub fn into_process_input(self) -> Result<lash_core::ProcessInput, serde_json::Error> {
        self.to_process_input()
    }

    pub fn from_payload(payload: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(payload)
    }

    /// Projects the immutable definition this input names.
    ///
    /// Everything that persists or compares a Lash VM definition goes through
    /// [`lash_vm::ProcessDefinitionIdentity::to_process_value`] from here, so
    /// the stored `ProcessIdentity.definition` is byte-identical to the value a
    /// cell holds for the same process.
    pub fn definition_identity(&self) -> lash_vm::ProcessDefinitionIdentity {
        lash_vm::ProcessDefinitionIdentity::new(
            self.module_ref.clone(),
            self.host_requirements_ref.clone(),
            self.process_ref.clone(),
            self.process_name.clone(),
        )
    }
}

#[derive(Clone, Debug)]
pub struct PreparedLashVmProcessStart {
    pub request: lash_core::ProcessStartRequest,
    pub label: Option<String>,
}

pub async fn prepare_lash_vm_process_start(
    workers: &lash_vm_client::service::Service,
    artifact_store: LashVmArtifacts,
    host_start_key: Option<&str>,
    start: lash_vm::ProcessStart,
    originator: lash_core::ProcessOriginator,
    lifetime: lash_core::LifetimeDecision,
) -> Result<PreparedLashVmProcessStart, LashVmRuntimeError> {
    let display_name = Some(start.process_name.clone());
    let artifact = workers
        .inspect_artifact(&artifact_store, &start.module_ref)
        .await
        .map_err(|source| LashVmRuntimeError::LoadArtifact { source })?
        .ok_or_else(|| LashVmRuntimeError::MissingArtifact {
            module_ref: start.module_ref.to_string(),
            process: start.process_name.clone(),
        })?;
    let admission_input = LashVmProcessInput {
        module_ref: start.module_ref.clone(),
        process_ref: start.process_ref.clone(),
        host_requirements_ref: start.host_requirements_ref.clone(),
        process_name: start.process_name.clone(),
        args: serde_json::Map::new(),
    };
    validate_lash_vm_process_admission(
        &artifact,
        &admission_input,
        LashVmHostEnvironmentCheck::OmitHostEnvironment,
    )?;
    let process = artifact.process(&start.process_name).ok_or_else(|| {
        LashVmRuntimeError::ArtifactProcessMismatch {
            module_ref: start.module_ref.to_string(),
            process: start.process_name.clone(),
            process_ref: format!("{:?}", start.process_ref),
        }
    })?;
    let args = match serde_json::to_value(lash_vm::Value::Record(Arc::new(start.args)))
        .map_err(|source| LashVmRuntimeError::SerializeProcessArgs { source })?
    {
        serde_json::Value::Object(map) => map,
        _ => return Err(LashVmRuntimeError::ProcessArgsNotRecord),
    };
    args::check_args(
        workers,
        &artifact_store,
        &process.params,
        &args,
        lash_core::ArgsMode::Complete,
    )
    .await
    .map_err(|error| match error {
        lash_core::ArgsMismatch::Argument { path, message } => {
            LashVmRuntimeError::InvalidProcessArgument { path, message }
        }
        source => LashVmRuntimeError::CheckProcessArgs { source },
    })?;
    let process_input = LashVmProcessInput {
        module_ref: start.module_ref,
        process_ref: start.process_ref,
        host_requirements_ref: start.host_requirements_ref,
        process_name: start.process_name,
        args,
    };
    let process_input = process_input
        .into_process_input()
        .map_err(|source| LashVmRuntimeError::EncodeProcessInput { source })?;
    let mut request = lash_core::ProcessStartRequest::new(process_input, originator, lifetime);
    if let Some(host_start_key) = host_start_key {
        request = request.with_host_start_key(host_start_key);
    }
    Ok(PreparedLashVmProcessStart {
        request,
        label: display_name,
    })
}

pub fn resolve_lash_vm_module_operation(
    host_environment: &lash_vm::LashVmHostEnvironment,
    receiver: &lash_vm::ResourceHandle,
    operation: &str,
) -> Result<String, lash_vm::ExecutionHostError> {
    host_environment
        .resources
        .resolve_module_operation(&receiver.resource_type, &receiver.alias, operation)
        .map(|binding| binding.host_operation.to_string())
        .ok_or_else(|| {
            LashVmHostError::ModuleOperationUnavailable {
                module: receiver.alias.to_string(),
                resource_type: receiver.resource_type.to_string(),
                operation: operation.to_string(),
            }
            .into()
        })
}

#[expect(
    clippy::expect_used,
    reason = "validated engine references form a canonical descriptor"
)]
fn lash_vm_process_identity(input: &LashVmProcessInput) -> lash_core::ProcessIdentity {
    let mut identity = lash_core::ProcessIdentity::labelled(
        LASH_VM_ENGINE_KIND,
        (!input.process_name.is_empty()).then(|| input.process_name.clone()),
    );
    identity.definition_id = Some(
        input
            .definition_identity()
            .draft()
            .expect("descriptor")
            .id(),
    );
    identity
}

/// The engine-owned settings recorded with every Lash VM process at creation.
/// Runs, redrives and replays decode only this record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LashVmRecordedSettings {
    pub language_features: LashVmLanguageFeatures,
    pub resources: LashVmHostCatalog,
    pub execution_bounds: lash_vm::ExecutionBounds,
}

impl LashVmRecordedSettings {
    pub fn new(surface: LashVmSurface, execution_bounds: lash_vm::ExecutionBounds) -> Self {
        Self {
            language_features: surface.language_features,
            resources: surface.resources,
            execution_bounds,
        }
    }

    fn into_surface(self) -> LashVmSurface {
        LashVmSurface::new(self.language_features, self.resources)
    }
}

/// Maps a protocol's captured configuration into the engine-owned record once,
/// during the journaled creation step. Execution has no protocol-specific reader.
pub trait LashVmRunSettingsRecorder: Send + Sync {
    fn record(
        &self,
        environment: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<LashVmRecordedSettings, lash_core::PluginError>;
}

#[derive(Clone)]
pub struct LashVmProcessEngine {
    artifact_store: LashVmArtifacts,
    workers: lash_vm_client::service::Service,
    surface: LashVmSurface,
    execution_bounds: lash_vm::ExecutionBounds,
    segment_policy: engine::VmSegmentPolicy,
    run_settings_recorder: Option<Arc<dyn LashVmRunSettingsRecorder>>,
    trace_runtime: Option<lash_core::trace::TraceRuntime>,
}

impl LashVmProcessEngine {
    pub fn new(artifact_store: LashVmArtifacts, surface: LashVmSurface) -> Self {
        Self {
            artifact_store,
            workers: lash_vm_client::service::Service::default(),
            surface,
            execution_bounds: lash_vm::ExecutionBounds::unbounded(),
            segment_policy: engine::VmSegmentPolicy::standard(),
            run_settings_recorder: None,
            trace_runtime: None,
        }
    }

    /// Set the segment policy used by the registered VM step bodies.
    pub fn with_segment_policy(mut self, policy: engine::VmSegmentPolicy) -> Self {
        self.segment_policy = policy;
        self
    }

    pub fn with_worker_service(mut self, workers: lash_vm_client::service::Service) -> Self {
        self.workers = workers;
        self
    }

    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

    /// Sets bounds for newly created processes. Existing processes keep
    /// their recorded bounds on every run.
    pub fn with_execution_bounds(mut self, execution_bounds: lash_vm::ExecutionBounds) -> Self {
        self.execution_bounds = execution_bounds;
        self
    }

    /// Select the creation-time mapping into the engine-owned record.
    pub fn with_run_settings_recorder(
        mut self,
        recorder: Arc<dyn LashVmRunSettingsRecorder>,
    ) -> Self {
        self.run_settings_recorder = Some(recorder);
        self
    }

    pub fn artifact_store(&self) -> LashVmArtifacts {
        self.artifact_store.clone()
    }

    /// The settings a process created under `env_spec` records: the surface
    /// its runs link against and their bounds.
    pub(crate) fn recorded_settings(
        &self,
        env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<LashVmRecordedSettings, lash_core::PluginError> {
        match &self.run_settings_recorder {
            Some(recorder) => recorder.record(env_spec),
            None => Ok(LashVmRecordedSettings::new(
                self.surface.clone(),
                self.execution_bounds,
            )),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for LashVmProcessEngine {
    fn kind(&self) -> &'static str {
        LASH_VM_ENGINE_KIND
    }

    fn program_identity(
        &self,
        payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        LashVmProcessInput::from_payload(payload.clone())
            .ok()
            .map(|input| input.executable_generation())
    }

    fn creation_config(
        &self,
        env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, lash_core::PluginError> {
        serde_json::to_value(self.recorded_settings(env_spec)?)
            .map(Some)
            .map_err(|error| lash_core::PluginError::Registration(error.to_string()))
    }

    async fn check_args(
        &self,
        signature: &lash_core::ProcessSignature,
        supplied: &serde_json::Map<String, serde_json::Value>,
        mode: lash_core::ArgsMode,
    ) -> Result<(), lash_core::ArgsMismatch> {
        let unsupported = || lash_core::ArgsMismatch::UnsupportedSignature {
            engine_kind: LASH_VM_ENGINE_KIND.into(),
        };
        let encoding = signature.encoding().ok_or_else(unsupported)?;
        let ty = lash_vm::json_schema_to_type_expr(encoding).map_err(|_| unsupported())?;
        let lash_vm::TypeExpr::Process(process) = ty else {
            return Err(unsupported());
        };
        let signature = process.as_signature().ok_or_else(unsupported)?;
        let params = signature
            .params()
            .iter()
            .map(|param| (param.name.to_string(), param.ty.clone()))
            .collect();
        args::check_args(&self.workers, &self.artifact_store, &params, supplied, mode).await
    }

    async fn resolve(
        &self,
        reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionRefusal> {
        let engine_kind = reference.engine_kind.clone();
        let unresolvable =
            |message: String| lash_core::ProcessDefinitionRefusal::UnresolvableDefinition {
                engine_kind: engine_kind.clone(),
                message,
            };
        let identity =
            lash_vm::ProcessDefinitionIdentity::from_process_value(reference.definition.as_json())
                .map_err(|error| unresolvable(error.to_string()))?;
        let artifact = self
            .workers
            .inspect_artifact(&self.artifact_store, &identity.module_ref)
            .await
            .map_err(|error| match error {
                lash_core::ArtifactStoreError::WorkerCheckoutTimedOut => {
                    lash_core::ProcessDefinitionRefusal::WorkerCheckoutTimedOut {
                        engine_kind: engine_kind.clone(),
                    }
                }
                error => unresolvable(error.to_string()),
            })?
            .ok_or_else(|| {
                unresolvable(format!(
                    "module artifact `{}` is not published",
                    identity.module_ref
                ))
            })?;
        let process_type = artifact
            .process_type(&identity)
            .map_err(|error| unresolvable(error.to_string()))?;
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::known(lash_vm_type_expr_schema(&process_type)),
        ))
    }

    /// A lash_vm start names one artifact: its module, in the store set's
    /// module port. The payload is a start input or a definition value, so a
    /// definition revision holds the same module its starts will
    /// (ADR 0113 §3.6).
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        let module_ref = document::payload_definition_identity(payload)?.module_ref;
        Ok(vec![lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::VmModule,
            artifact_ref: module_ref.as_str().to_owned(),
        }])
    }

    /// Lash VM keeps no engine store: its modules live in the module port,
    /// which ends its own referrers.
    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    /// Never called: [`Self::start_artifacts`] names nothing under this
    /// engine's own store.
    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Invoke(format!(
            "the lash_vm engine keeps no engine artifact store; `{artifact_ref}` belongs to the \
             module port"
        )))
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        engine::advance::state_format()
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    /// See [`engine`]: the VM runs only in the `vm_run` step, never here.
    fn advance(
        &self,
        state: lash_core::EngineState,
        event: lash_core::EngineEvent,
    ) -> Result<(lash_core::EngineState, lash_core::EngineAction), lash_core::ProcessInfraError>
    {
        engine::advance::advance(state, event)
    }
}

pub fn admit_lash_vm_process(
    _kind: &'static str,
    payload: &serde_json::Value,
    _env_spec: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    let input = LashVmProcessInput::from_payload(payload.clone()).map_err(|err| {
        lash_core::PluginError::Session(format!("invalid lash_vm process payload: {err}"))
    })?;
    Ok(lash_vm_process_identity(&input))
}

#[expect(
    clippy::expect_used,
    reason = "the engine and the admission descriptor are constructed from the same LASH_VM_ENGINE_KIND constant a few lines below, so registration cannot refuse"
)]
pub fn lash_vm_process_engine_registration(
    engine: LashVmProcessEngine,
) -> lash_core::ProcessEngineRegistration {
    let engine = Arc::new(engine);
    lash_core::ProcessEngineRegistration::new(
        engine.clone(),
        lash_core::ProcessEngineAdmission::new(LASH_VM_ENGINE_KIND, admit_lash_vm_process),
    )
    .expect("lash_vm engine and admission share a fixed kind")
    .with_document_provider(Arc::new(document::LashVmDocumentProvider {
        engine: Arc::clone(&engine),
    }))
    .with_engine_steps(Arc::new(LashVmEngineSteps::new(Arc::clone(&engine))))
}

mod bridge;
#[cfg(test)]
mod catalog_tests;
pub mod engine;
pub use engine::{LashVmEngineSteps, VmSegmentPolicy};
mod catalogue_preview;
mod deferred;
mod document;
mod process;

pub use bridge::{
    ExecutionCancellation, lash_vm_value_to_json, process_sleep,
    protocol_tool_output_to_lash_vm_value, protocol_tool_reply_to_lash_vm_value,
};
pub use catalogue_preview::{
    CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
    DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, catalogue_preview,
    catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
    catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
};
pub use deferred::{
    DeferredLink, DeferredLinkError, DeferredResolutionError, DeferredResolutionLinkKey,
    DeferredResolveContext, DeferredToolResolver, RecordedGrantInstallError, Resolution,
    SharedDeferredToolResolver, ToolGrant, compile_with_deferred_resolution,
    resolve_and_build_deferred_environment, resolve_and_build_deferred_environment_from_references,
    resolve_and_fold_deferred,
};
pub use document::{
    AdmittedWorkflow, WorkflowAdmissionOutcome, WorkflowAdmissionRequest, WorkflowDocument,
    WorkflowEntry, WorkflowExecutionDocument,
};
pub use engine::LASH_VM_SEGMENT_STATE_VERSION;
pub use process::{lash_vm_program_hash, lash_vm_type_expr_schema};

#[cfg(test)]
mod lib_tests;

#[cfg(any(test, feature = "testing"))]
#[path = "argument_admission_testing.rs"]
pub mod testing;
