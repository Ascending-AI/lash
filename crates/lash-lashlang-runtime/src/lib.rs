use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

mod error;
pub use error::{
    LashlangHostError, LashlangProcessFailureCode, LashlangRuntimeError, ToolBindingError,
};
mod process_identity;
pub use process_identity::deterministic_lashlang_process_id;
mod trigger_commands;
pub use trigger_commands::execute_trigger_operation;
mod typescript_runtime;
pub use typescript_runtime::{is_typescript_runtime_receiver, journaled_typescript_runtime_value};

pub use lash_trace::{
    TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionMap, TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceLashlangEdgeSelection,
    TraceLashlangGraph, TraceLashlangGraphChildLink, TraceLashlangGraphEdge,
    TraceLashlangGraphNode, TraceLashlangGraphStore, TraceLashlangNodeObservation,
};
pub use lashlang::{
    CompiledProcessCache, InMemoryLashlangArtifactStore, LASH_TYPE_KEY, LashlangAbilities,
    LashlangArtifactStore, LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures,
};

pub const LASHLANG_ENGINE_KIND: &str = "lashlang";
pub const LASHLANG_TOOL_BINDING_KEY: &str = "lashlang.tool";
pub const TYPESCRIPT_TOOL_BINDING_KEY: &str = "typescript.tool";
pub const LASHLANG_SURFACE_EXTENSION_ID: &str = "lashlang.surface";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LashlangSurfaceContribution {
    pub abilities: LashlangAbilities,
    pub language_features: LashlangLanguageFeatures,
    pub resources: LashlangHostCatalog,
}

impl LashlangSurfaceContribution {
    pub fn new(
        abilities: LashlangAbilities,
        language_features: LashlangLanguageFeatures,
        resources: LashlangHostCatalog,
    ) -> Self {
        Self {
            abilities,
            language_features,
            resources,
        }
    }

    pub fn from_surface(surface: LashlangSurface) -> Self {
        Self {
            abilities: surface.abilities,
            language_features: surface.language_features,
            resources: surface.resources,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl ToolBinding {
    pub fn new(
        module_path: impl IntoIterator<Item = impl Into<String>>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            module_path: module_path.into_iter().map(Into::into).collect(),
            operation: Some(operation.into()),
            authority_type: None,
            aliases: Vec::new(),
        }
    }

    pub fn with_authority_type(mut self, authority_type: impl Into<String>) -> Self {
        self.authority_type = Some(authority_type.into());
        self
    }

    pub fn with_aliases(mut self, aliases: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    pub fn executable_for(&self, tool_name: &str) -> Result<ResolvedToolBinding, ToolBindingError> {
        if self.module_path.is_empty() {
            return Err(ToolBindingError::MissingModulePath {
                tool: tool_name.to_string(),
            });
        }
        for segment in &self.module_path {
            validate_lashlang_identifier(tool_name, "module path segment", segment)?;
        }
        let operation =
            self.operation
                .as_deref()
                .ok_or_else(|| ToolBindingError::MissingOperation {
                    tool: tool_name.to_string(),
                })?;
        validate_lashlang_identifier(tool_name, "operation name", operation)?;
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

    pub fn required_for_remote(
        manifest: &lash_core::ToolManifest,
    ) -> Result<ResolvedToolBinding, ToolBindingError> {
        required_tool_lashlang_executable(manifest)
    }

    pub fn required_executable_for_remote(
        &self,
        tool_name: &str,
    ) -> Result<ResolvedToolBinding, ToolBindingError> {
        self.executable_for(tool_name)
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

fn validate_lashlang_identifier(
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

pub fn required_tool_lashlang_binding(
    manifest: &lash_core::ToolManifest,
) -> Result<ToolBinding, ToolBindingError> {
    ToolManifestBindingExt::tool_binding(manifest)?.ok_or_else(|| {
        ToolBindingError::MissingBinding {
            tool: manifest.name.clone(),
            binding_key: LASHLANG_TOOL_BINDING_KEY,
        }
    })
}

pub fn required_tool_lashlang_executable(
    manifest: &lash_core::ToolManifest,
) -> Result<ResolvedToolBinding, ToolBindingError> {
    required_tool_lashlang_binding(manifest)?.executable_for(&manifest.name)
}

pub fn required_tool_typescript_executable(
    manifest: &lash_core::ToolManifest,
) -> Result<ResolvedToolBinding, ToolBindingError> {
    let binding = manifest
        .bindings
        .get(TYPESCRIPT_TOOL_BINDING_KEY)
        .cloned()
        .map(serde_json::from_value::<ToolBinding>)
        .transpose()
        .map_err(|source| ToolBindingError::MalformedPayload {
            tool: manifest.name.clone(),
            binding_key: TYPESCRIPT_TOOL_BINDING_KEY,
            source,
        })?
        .ok_or_else(|| ToolBindingError::MissingBinding {
            tool: manifest.name.clone(),
            binding_key: TYPESCRIPT_TOOL_BINDING_KEY,
        })?;
    binding.executable_for(&manifest.name)
}

pub trait ToolManifestBindingExt {
    /// Read the binding stored under the `lashlang.tool` wire key.
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError>;
}

impl ToolManifestBindingExt for lash_core::ToolManifest {
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError> {
        self.bindings
            .get(LASHLANG_TOOL_BINDING_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|source| ToolBindingError::MalformedPayload {
                tool: self.name.clone(),
                binding_key: LASHLANG_TOOL_BINDING_KEY,
                source,
            })
    }
}

pub trait ToolDefinitionBindingExt {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self;
}

impl ToolDefinitionBindingExt for lash_core::ToolDefinition {
    fn with_tool_binding(mut self, tool_binding: ToolBinding) -> Self {
        let value =
            serde_json::to_value(&tool_binding).expect("tool binding must serialize to JSON");
        self.manifest
            .bindings
            .insert(LASHLANG_TOOL_BINDING_KEY.to_string(), value);
        self.manifest.bindings.insert(
            TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(tool_binding)
                .expect("typescript tool binding must serialize to JSON"),
        );
        self
    }
}

pub trait RemoteToolGrantBindingExt {
    fn with_tool_binding(self, tool_binding: ToolBinding) -> Self;
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError>;
}

impl RemoteToolGrantBindingExt for lash_remote_protocol::RemoteToolGrant {
    fn with_tool_binding(mut self, tool_binding: ToolBinding) -> Self {
        let value =
            serde_json::to_value(&tool_binding).expect("tool binding must serialize to JSON");
        self.bindings
            .insert(LASHLANG_TOOL_BINDING_KEY.to_string(), value);
        self.bindings.insert(
            TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(tool_binding)
                .expect("typescript tool binding must serialize to JSON"),
        );
        self
    }

    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError> {
        self.bindings
            .get(LASHLANG_TOOL_BINDING_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|source| ToolBindingError::MalformedPayload {
                tool: self.name.clone(),
                binding_key: LASHLANG_TOOL_BINDING_KEY,
                source,
            })
    }
}

#[derive(Clone, Debug)]
pub struct LashlangSurface {
    pub abilities: LashlangAbilities,
    pub language_features: LashlangLanguageFeatures,
    pub resources: LashlangHostCatalog,
}

impl Default for LashlangSurface {
    fn default() -> Self {
        Self {
            abilities: LashlangAbilities::default().with_sleep(),
            language_features: LashlangLanguageFeatures::default(),
            resources: LashlangHostCatalog::new(),
        }
    }
}

impl LashlangSurface {
    pub fn new(
        abilities: LashlangAbilities,
        language_features: LashlangLanguageFeatures,
        resources: LashlangHostCatalog,
    ) -> Self {
        Self {
            abilities,
            language_features,
            resources,
        }
    }

    pub fn for_process_registry(mut self, process_registry_available: bool) -> Self {
        self.abilities = self.abilities.with_sleep();
        if process_registry_available {
            self.abilities = self.abilities.with_processes().with_process_signals();
        } else {
            self.abilities.processes = false;
            self.abilities.process_signals = false;
        }
        self
    }

    pub fn with_resources(
        mut self,
        resources: LashlangHostCatalog,
    ) -> Result<Self, LashlangRuntimeError> {
        self.resources.try_extend(resources)?;
        Ok(self)
    }

    pub fn with_plugin_extensions(
        mut self,
        extensions: &lash_core::PluginExtensions,
    ) -> Result<Self, LashlangRuntimeError> {
        for payload in extensions.payloads(LASHLANG_SURFACE_EXTENSION_ID) {
            let contribution: LashlangSurfaceContribution = serde_json::from_value(payload.clone())
                .map_err(|source| LashlangRuntimeError::InvalidSurfaceExtension { source })?;
            self.abilities = self.abilities.union(contribution.abilities);
            self.language_features = self.language_features.union(contribution.language_features);
            self.resources.try_extend(contribution.resources)?;
        }
        Ok(self)
    }

    pub fn host_environment(
        &self,
        catalog: &lash_core::ToolCatalog,
    ) -> Result<LashlangHostEnvironment, ToolBindingError> {
        self.host_environment_masking(catalog, &BTreeSet::new())
    }

    /// Builds the link-time environment while excluding exact ambient call
    /// paths already decided by the deferred-resolution journal.
    ///
    /// Filtering happens before the flat Tool Catalog and contributed surface
    /// resources are merged and validated. Thus a recorded authority can mask
    /// every later ambient claimant for its path, while unrelated collisions
    /// and malformed definitions retain their normal failures.
    pub fn host_environment_masking(
        &self,
        catalog: &lash_core::ToolCatalog,
        masked_call_paths: &BTreeSet<String>,
    ) -> Result<LashlangHostEnvironment, ToolBindingError> {
        let mut resources = self.resources.clone();
        for path in masked_call_paths {
            if let Some((module_path, operation)) = path.rsplit_once('.') {
                resources.mask_module_operation(module_path, operation);
            }
        }
        lashlang_host_environment_from_tool_catalog(
            &filtered_tool_catalog(catalog, masked_call_paths),
            self.abilities,
            self.language_features,
            resources,
        )
    }
}

fn filtered_tool_catalog(
    catalog: &lash_core::ToolCatalog,
    masked_call_paths: &BTreeSet<String>,
) -> lash_core::ToolCatalog {
    if masked_call_paths.is_empty() {
        return catalog.clone();
    }
    let mut filtered = catalog.clone();
    filtered.tools.retain(|entry| {
        let Ok(binding) = required_tool_lashlang_executable(&entry.manifest) else {
            // Preserve ordinary validation for malformed unrelated entries.
            return true;
        };
        !masked_call_paths.contains(&format!(
            "{}.{}",
            binding.module_path.join("."),
            binding.operation
        ))
    });
    filtered
}

pub fn lashlang_host_environment_from_tool_catalog(
    catalog: &lash_core::ToolCatalog,
    abilities: LashlangAbilities,
    language_features: LashlangLanguageFeatures,
    host_resources: LashlangHostCatalog,
) -> Result<LashlangHostEnvironment, ToolBindingError> {
    let mut resources = lashlang_resources_from_tool_catalog(catalog)?;
    resources.try_extend(host_resources)?;
    for (operation, host_operation) in [
        ("now", "typescript.runtime.now"),
        ("random", "typescript.runtime.random"),
    ] {
        resources.add_module_operation_binding(
            ["__typescript_runtime"],
            "typescript.Runtime",
            operation,
            host_operation,
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Float,
                output_from_input: None,
            },
        )?;
    }
    if abilities.triggers {
        lashlang::add_trigger_resource_operations(&mut resources)?;
    }
    Ok(
        LashlangHostEnvironment::new(resources, abilities)
            .with_language_features(language_features),
    )
}

pub fn lashlang_resources_from_tool_catalog(
    catalog: &lash_core::ToolCatalog,
) -> Result<LashlangHostCatalog, ToolBindingError> {
    let mut host_catalog = LashlangHostCatalog::new();
    // Every externally activated catalog member is callable. Internal members
    // remain registry-resolvable for runtime-owned process bodies only.
    for entry in catalog.tools.iter() {
        if entry.manifest.activation == lash_core::ToolActivation::Internal {
            continue;
        }
        let lashlang_binding = required_tool_lashlang_executable(&entry.manifest)?;
        let operation_binding = lashlang_tool_contract_types(&entry.contract);
        host_catalog.add_module_operation_binding(
            lashlang_binding.module_path.iter().map(String::as_str),
            lashlang_binding.authority_type.clone(),
            lashlang_binding.operation.clone(),
            entry.manifest.id.to_string(),
            operation_binding,
        )?;
    }
    Ok(host_catalog)
}

fn lashlang_tool_contract_types(
    contract: &lash_core::ToolContract,
) -> lashlang::ResourceOperationBinding {
    let input_ty = lashlang::json_schema_to_type_expr(contract.input_schema.canonical());
    let (output_ty, output_from_input) = match &contract.output_contract {
        lash_core::ToolOutputContract::Static => (
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
            None,
        ),
        lash_core::ToolOutputContract::FromInputSchema {
            input_field,
            default_schema,
        } => (
            lashlang::TypeExpr::Any,
            Some(lashlang::OutputFromInputBinding {
                input_field: input_field.clone(),
                default_schema: default_schema
                    .as_ref()
                    .map(lashlang::json_schema_to_type_expr),
            }),
        ),
    };
    lashlang::ResourceOperationBinding {
        input_ty,
        output_ty,
        output_from_input,
    }
}

pub fn lashlang_host_environment_satisfies_requirements(
    required: &lashlang::HostRequirements,
    current: &LashlangHostEnvironment,
) -> Result<(), LashlangRuntimeError> {
    let abilities = required.abilities;
    let current_abilities = current.abilities;
    if abilities.processes && !current_abilities.processes {
        return Err(LashlangRuntimeError::ProcessesUnavailable);
    }
    if abilities.sleep && !current_abilities.sleep {
        return Err(LashlangRuntimeError::SleepUnavailable);
    }
    if abilities.process_signals && !current_abilities.process_signals {
        return Err(LashlangRuntimeError::ProcessSignalsUnavailable);
    }
    if abilities.triggers && !current_abilities.triggers {
        return Err(LashlangRuntimeError::TriggersUnavailable);
    }
    if required.language_features.label_annotations && !current.language_features.label_annotations
    {
        return Err(LashlangRuntimeError::LabelAnnotationsUnavailable);
    }

    for (_, module) in required.resources.module_instances() {
        let current_module = current
            .resources
            .resolve_module_path(&module.path)
            .ok_or_else(|| LashlangRuntimeError::ModuleUnavailable {
                module: module.alias.clone(),
            })?;
        if current_module.resource_type != module.resource_type {
            return Err(LashlangRuntimeError::ModuleTypeMismatch {
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
                    return Err(LashlangRuntimeError::ModuleOperationMismatch {
                        module: module.alias.clone(),
                        operation: operation.clone(),
                        actual: current_binding.host_operation.to_string(),
                        expected: required_binding.host_operation.clone(),
                    });
                }
                None => {
                    return Err(LashlangRuntimeError::ModuleOperationUnavailable {
                        module: module.alias.clone(),
                        operation: operation.clone(),
                    });
                }
            }
        }
    }

    for (resource_type, required_type) in required.resources.resource_types() {
        if !current.resources.has_resource_type(resource_type) {
            return Err(LashlangRuntimeError::ResourceTypeUnavailable {
                resource_type: resource_type.to_string(),
            });
        }
        for (operation, required_binding) in &required_type.operations {
            let current_binding = current
                .resources
                .resolve_operation(resource_type, operation)
                .ok_or_else(|| LashlangRuntimeError::ResourceOperationUnavailable {
                    resource_type: resource_type.to_string(),
                    operation: operation.clone(),
                })?;
            if current_binding.input_ty != required_binding.input_ty {
                return Err(LashlangRuntimeError::ResourceInputMismatch {
                    resource_type: resource_type.to_string(),
                    operation: operation.clone(),
                });
            }
            if current_binding.output_ty != required_binding.output_ty {
                return Err(LashlangRuntimeError::ResourceOutputMismatch {
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
                .ok_or_else(|| LashlangRuntimeError::HostDataTypeUnavailable {
                    name: name.to_string(),
                })?;
        if current_data_type != required_data_type {
            return Err(LashlangRuntimeError::HostDataTypeMismatch {
                name: name.to_string(),
            });
        }
    }
    for (path, required_binding) in required.resources.value_constructors() {
        let current_binding = current
            .resources
            .resolve_value_constructor(&path.split('.').collect::<Vec<_>>())
            .ok_or_else(|| LashlangRuntimeError::ValueConstructorUnavailable {
                path: path.to_string(),
            })?;
        if current_binding.input_ty != required_binding.input_ty {
            return Err(LashlangRuntimeError::ValueConstructorInputMismatch {
                path: path.to_string(),
            });
        }
        if current_binding.output_ty != required_binding.output_ty {
            return Err(LashlangRuntimeError::ValueConstructorOutputMismatch {
                path: path.to_string(),
            });
        }
    }
    for (source_ty, required_binding) in required.resources.trigger_sources() {
        let current_binding = current
            .resources
            .resolve_trigger_source(source_ty)
            .ok_or_else(|| LashlangRuntimeError::TriggerSourceUnavailable {
                source_type: source_ty.to_string(),
            })?;
        if current_binding != required_binding {
            return Err(LashlangRuntimeError::TriggerSourceMismatch {
                source_type: source_ty.to_string(),
            });
        }
    }

    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LashlangProcessInput {
    pub module_ref: lashlang::ModuleRef,
    pub process_ref: lashlang::ProcessRef,
    pub host_requirements_ref: lashlang::HostRequirementsRef,
    pub process_name: String,
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
}

/// Whether a caller can and must check the live host environment.
pub enum LashlangHostEnvironmentCheck<'a> {
    /// Prepare validates immutable artifact claims and deliberately omits live-host checks.
    OmitHostEnvironment,
    /// Run validates against the environment it resolved for this attempt.
    CheckHostEnvironment(Result<&'a LashlangHostEnvironment, String>),
}

/// Typed refusal shared by prepare and the authoritative run-time recheck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LashlangProcessAdmissionRefusal {
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

impl LashlangProcessAdmissionRefusal {
    pub const fn failure_code(&self) -> LashlangProcessFailureCode {
        match self {
            Self::HostRequirementsMismatch { .. } => {
                LashlangProcessFailureCode::ProcessHostRequirementsMismatch
            }
            Self::ProcessRefMismatch { .. } => LashlangProcessFailureCode::ProcessRefMismatch,
            Self::HostEnvironmentInvalid { .. } => {
                LashlangProcessFailureCode::ProcessHostEnvironmentInvalid
            }
            Self::HostEnvironmentIncompatible { .. } => {
                LashlangProcessFailureCode::ProcessHostEnvironmentIncompatible
            }
        }
    }
}

impl std::fmt::Display for LashlangProcessAdmissionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostRequirementsMismatch {
                process,
                requested,
                actual,
            } => write!(
                formatter,
                "lashlang process `{process}` requested surface {requested}, artifact has {actual}"
            ),
            Self::ProcessRefMismatch {
                module_ref,
                process,
                process_ref,
            } => write!(
                formatter,
                "lashlang module `{module_ref}` does not export process `{process}` as requested ref {process_ref}"
            ),
            Self::HostEnvironmentInvalid { message } => formatter.write_str(message),
            Self::HostEnvironmentIncompatible { process, message } => write!(
                formatter,
                "lashlang process `{process}` is incompatible with this host surface: {message}"
            ),
        }
    }
}

impl std::error::Error for LashlangProcessAdmissionRefusal {}

pub fn validate_lashlang_process_admission(
    artifact: &lashlang::ModuleArtifact,
    input: &LashlangProcessInput,
    host: LashlangHostEnvironmentCheck<'_>,
) -> Result<(), LashlangProcessAdmissionRefusal> {
    if artifact.host_requirements_ref != input.host_requirements_ref {
        return Err(LashlangProcessAdmissionRefusal::HostRequirementsMismatch {
            process: input.process_name.clone(),
            requested: input.host_requirements_ref.to_string(),
            actual: artifact.host_requirements_ref.to_string(),
        });
    }
    if artifact.process_ref(&input.process_name) != Some(&input.process_ref) {
        return Err(LashlangProcessAdmissionRefusal::ProcessRefMismatch {
            module_ref: input.module_ref.to_string(),
            process: input.process_name.clone(),
            process_ref: format!("{:?}", input.process_ref),
        });
    }
    if let LashlangHostEnvironmentCheck::CheckHostEnvironment(host) = host {
        let host =
            host.map_err(
                |message| LashlangProcessAdmissionRefusal::HostEnvironmentInvalid { message },
            )?;
        lashlang_host_environment_satisfies_requirements(&artifact.host_requirements, host)
            .map_err(
                |error| LashlangProcessAdmissionRefusal::HostEnvironmentIncompatible {
                    process: input.process_name.clone(),
                    message: error.to_string(),
                },
            )?;
    }
    Ok(())
}

impl LashlangProcessInput {
    pub fn process_identity(&self) -> lash_core::ProcessIdentity {
        lashlang_process_identity(self)
    }

    pub fn remote_identity(&self) -> lash_remote_protocol::RemoteProcessIdentity {
        lash_remote_protocol::RemoteProcessIdentity {
            kind: LASHLANG_ENGINE_KIND.to_string(),
            label: Some(self.process_name.clone()),
            definition: Some(lash_remote_protocol::RemoteProcessDefinitionIdentity {
                value: self.definition(),
            }),
        }
    }

    pub fn to_process_input(&self) -> Result<lash_core::ProcessInput, serde_json::Error> {
        Ok(lash_core::ProcessInput::Engine {
            kind: LASHLANG_ENGINE_KIND.to_string(),
            payload: serde_json::to_value(self)?,
        })
    }

    pub fn into_process_input(self) -> Result<lash_core::ProcessInput, serde_json::Error> {
        self.to_process_input()
    }

    pub fn remote_trigger_subscription_draft(
        &self,
        subscription_key: impl Into<String>,
        env_ref: lash_remote_protocol::RemoteProcessExecutionEnvRef,
        source_type: impl Into<String>,
        source_key: impl Into<String>,
    ) -> Result<lash_remote_protocol::RemoteTriggerSubscriptionDraft, serde_json::Error> {
        Ok(
            lash_remote_protocol::RemoteTriggerSubscriptionDraft::for_process(
                subscription_key,
                env_ref,
                source_type,
                source_key,
                self.clone().try_into()?,
                self.remote_identity(),
            ),
        )
    }

    pub fn from_payload(payload: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(payload)
    }

    pub fn definition(&self) -> serde_json::Value {
        serde_json::json!({
            "module_ref": self.module_ref,
            "process_ref": self.process_ref,
            "host_requirements_ref": self.host_requirements_ref,
            "process_name": self.process_name,
        })
    }
}

impl TryFrom<LashlangProcessInput> for lash_remote_protocol::RemoteProcessInput {
    type Error = serde_json::Error;

    fn try_from(value: LashlangProcessInput) -> Result<Self, Self::Error> {
        Ok(Self::Engine {
            kind: LASHLANG_ENGINE_KIND.to_string(),
            payload: serde_json::to_value(value)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PreparedLashlangProcessStart {
    pub request: lash_core::ProcessStartRequest,
    pub label: Option<String>,
}

pub async fn prepare_lashlang_process_start(
    artifact_store: Arc<dyn LashlangArtifactStore>,
    parent_start_seed: &str,
    start: lashlang::ProcessStart,
    originator: lash_core::ProcessOriginator,
    lifecycle: lash_core::ProcessLifecyclePolicy,
    disposition: lash_core::RecoveryContract,
) -> Result<PreparedLashlangProcessStart, LashlangRuntimeError> {
    let display_name = Some(start.process_name.clone());
    let artifact = artifact_store
        .get_module_artifact(&start.module_ref)
        .await
        .map_err(|source| LashlangRuntimeError::LoadArtifact { source })?
        .ok_or_else(|| LashlangRuntimeError::MissingArtifact {
            module_ref: start.module_ref.to_string(),
            process: start.process_name.clone(),
        })?;
    artifact
        .verify()
        .map_err(|source| LashlangRuntimeError::InvalidArtifact {
            module_ref: start.module_ref.to_string(),
            message: source.to_string(),
        })?;
    let admission_input = LashlangProcessInput {
        module_ref: start.module_ref.clone(),
        process_ref: start.process_ref.clone(),
        host_requirements_ref: start.host_requirements_ref.clone(),
        process_name: start.process_name.clone(),
        args: serde_json::Map::new(),
    };
    validate_lashlang_process_admission(
        &artifact,
        &admission_input,
        LashlangHostEnvironmentCheck::OmitHostEnvironment,
    )?;
    let process = artifact
        .canonical_ir
        .process(&start.process_name)
        .ok_or_else(|| LashlangRuntimeError::ArtifactProcessMismatch {
            module_ref: start.module_ref.to_string(),
            process: start.process_name.clone(),
            process_ref: format!("{:?}", start.process_ref),
        })?;
    let args = match serde_json::to_value(lashlang::Value::Record(Arc::new(start.args)))
        .map_err(|source| LashlangRuntimeError::SerializeProcessArgs { source })?
    {
        serde_json::Value::Object(map) => map,
        _ => return Err(LashlangRuntimeError::ProcessArgsNotRecord),
    };
    for name in args.keys() {
        if !process
            .params
            .iter()
            .any(|param| param.name.as_str() == name)
        {
            return Err(LashlangRuntimeError::InvalidProcessArgument {
                path: name.clone(),
                message: "argument is not declared by the target process".to_string(),
            });
        }
    }
    for param in &process.params {
        let value = args.get(param.name.as_str()).ok_or_else(|| {
            LashlangRuntimeError::InvalidProcessArgument {
                path: param.name.to_string(),
                message: "required argument is missing".to_string(),
            }
        })?;
        let expected = artifact.resolve_type(&param.ty);
        if type_contains_process(&expected) {
            validate_process_claims(
                artifact_store.as_ref(),
                value,
                &expected,
                param.name.to_string(),
            )
            .await?;
        }
    }
    let signal_event_types = artifact
        .canonical_ir
        .process(&start.process_name)
        .map(lashlang_process_signal_event_types)
        .unwrap_or_default();
    let process_input = LashlangProcessInput {
        module_ref: start.module_ref,
        process_ref: start.process_ref,
        host_requirements_ref: start.host_requirements_ref,
        process_name: start.process_name,
        args,
    };
    let identity = lashlang_process_identity(&process_input);
    let process_id =
        deterministic_lashlang_process_id(parent_start_seed, &start.start_site, &process_input)
            .map_err(|source| LashlangRuntimeError::DeriveProcessId { source })?;
    let process_input = process_input
        .into_process_input()
        .map_err(|source| LashlangRuntimeError::EncodeProcessInput { source })?;
    let request = lash_core::ProcessStartRequest::new(
        process_id,
        process_input,
        disposition,
        originator,
        lifecycle,
    )
    .with_identity(identity)
    .with_extra_event_types(
        lashlang_process_event_types()
            .into_iter()
            .chain(signal_event_types),
    );
    Ok(PreparedLashlangProcessStart {
        request,
        label: display_name,
    })
}

fn type_contains_process(ty: &lashlang::TypeExpr) -> bool {
    match ty {
        lashlang::TypeExpr::Process(_) => true,
        lashlang::TypeExpr::List(item) | lashlang::TypeExpr::TriggerHandle(item) => {
            type_contains_process(item)
        }
        lashlang::TypeExpr::Object(fields) => {
            fields.iter().any(|field| type_contains_process(&field.ty))
        }
        lashlang::TypeExpr::Union(items) => items.iter().any(type_contains_process),
        lashlang::TypeExpr::Any
        | lashlang::TypeExpr::Str
        | lashlang::TypeExpr::Int
        | lashlang::TypeExpr::Float
        | lashlang::TypeExpr::Bool
        | lashlang::TypeExpr::Dict
        | lashlang::TypeExpr::Null
        | lashlang::TypeExpr::Enum(_)
        | lashlang::TypeExpr::Ref(_) => false,
    }
}

fn validate_process_claims<'a>(
    artifact_store: &'a dyn LashlangArtifactStore,
    value: &'a serde_json::Value,
    expected: &'a lashlang::TypeExpr,
    path: String,
) -> Pin<Box<dyn Future<Output = Result<(), LashlangRuntimeError>> + Send + 'a>> {
    Box::pin(async move {
        let invalid = |message: String| LashlangRuntimeError::InvalidProcessArgument {
            path: path.clone(),
            message,
        };
        match expected {
            lashlang::TypeExpr::Any => Ok(()),
            lashlang::TypeExpr::Str => value
                .is_string()
                .then_some(())
                .ok_or_else(|| invalid("expected string".to_string())),
            lashlang::TypeExpr::Int => value
                .as_i64()
                .is_some()
                .then_some(())
                .ok_or_else(|| invalid("expected integer".to_string())),
            lashlang::TypeExpr::Float => value
                .is_number()
                .then_some(())
                .ok_or_else(|| invalid("expected number".to_string())),
            lashlang::TypeExpr::Bool => value
                .is_boolean()
                .then_some(())
                .ok_or_else(|| invalid("expected boolean".to_string())),
            lashlang::TypeExpr::Null => value
                .is_null()
                .then_some(())
                .ok_or_else(|| invalid("expected null".to_string())),
            lashlang::TypeExpr::Enum(values) => value
                .as_str()
                .is_some_and(|value| values.iter().any(|item| item.as_str() == value))
                .then_some(())
                .ok_or_else(|| invalid("expected enum value".to_string())),
            lashlang::TypeExpr::Dict => value
                .is_object()
                .then_some(())
                .ok_or_else(|| invalid("expected object".to_string())),
            lashlang::TypeExpr::List(item) => {
                let items = value
                    .as_array()
                    .ok_or_else(|| invalid("expected list".to_string()))?;
                for (index, item_value) in items.iter().enumerate() {
                    validate_process_claims(
                        artifact_store,
                        item_value,
                        item,
                        format!("{path}[{index}]"),
                    )
                    .await?;
                }
                Ok(())
            }
            lashlang::TypeExpr::Object(fields) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("expected object".to_string()))?;
                for field in fields {
                    match object.get(field.name.as_str()) {
                        Some(field_value) => {
                            validate_process_claims(
                                artifact_store,
                                field_value,
                                &field.ty,
                                format!("{path}.{}", field.name),
                            )
                            .await?;
                        }
                        None if field.optional => {}
                        None => {
                            return Err(invalid(format!(
                                "required field `{}` is missing",
                                field.name
                            )));
                        }
                    }
                }
                Ok(())
            }
            lashlang::TypeExpr::Union(items) => {
                let mut errors = Vec::new();
                for item in items {
                    match validate_process_claims(artifact_store, value, item, path.clone()).await {
                        Ok(()) => return Ok(()),
                        Err(error) => errors.push(error.to_string()),
                    }
                }
                Err(invalid(format!(
                    "value matches no union variant ({})",
                    errors.join("; ")
                )))
            }
            lashlang::TypeExpr::Process(expected_process) => {
                let expected_signature = expected_process.as_signature().ok_or_else(|| {
                    invalid("program signature is unexpectedly unknown".to_string())
                })?;
                let identity = lashlang::ProcessDefinitionIdentity::from_process_value(value)
                    .map_err(|error| invalid(error.to_string()))?;
                let actual_artifact = artifact_store
                    .get_module_artifact(&identity.module_ref)
                    .await
                    .map_err(|error| invalid(format!("failed to load process artifact: {error}")))?
                    .ok_or_else(|| {
                        invalid(format!(
                            "missing process artifact `{}`",
                            identity.module_ref
                        ))
                    })?;
                let actual = identity
                    .resolve_process_type(actual_artifact.as_ref())
                    .map_err(|error| invalid(error.to_string()))?;
                let expected = lashlang::TypeExpr::Process(lashlang::ProcessType::known(
                    expected_signature.clone(),
                ));
                if lashlang::is_resolved_type_assignable(&actual, &expected) {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "immutable process signature `{actual}` is not assignable to `{expected}`"
                    )))
                }
            }
            lashlang::TypeExpr::TriggerHandle(_) | lashlang::TypeExpr::Ref(_) => Ok(()),
        }
    })
}

pub fn resolve_lashlang_module_operation(
    host_environment: &lashlang::LashlangHostEnvironment,
    receiver: &lashlang::ResourceHandle,
    operation: &str,
) -> Result<String, lashlang::ExecutionHostError> {
    host_environment
        .resources
        .resolve_module_operation(&receiver.resource_type, &receiver.alias, operation)
        .map(|binding| binding.host_operation.to_string())
        .ok_or_else(|| {
            LashlangHostError::ModuleOperationUnavailable {
                module: receiver.alias.to_string(),
                resource_type: receiver.resource_type.to_string(),
                operation: operation.to_string(),
            }
            .into()
        })
}

fn lashlang_process_identity(input: &LashlangProcessInput) -> lash_core::ProcessIdentity {
    lash_core::ProcessIdentity::new(LASHLANG_ENGINE_KIND)
        .with_label(Some(input.process_name.clone()))
        .with_definition(Some(input.definition()))
}

#[derive(Clone)]
pub struct LashlangProcessEngine {
    artifact_store: Arc<dyn LashlangArtifactStore>,
    process_cache: Arc<Mutex<CompiledProcessCache>>,
    surface: LashlangSurface,
    execution_sink: Option<Arc<dyn lash_trace::TraceSink>>,
    trace_context: lash_trace::TraceContext,
    execution_bounds: lashlang::ExecutionBounds,
}

impl LashlangProcessEngine {
    pub fn new(artifact_store: Arc<dyn LashlangArtifactStore>, surface: LashlangSurface) -> Self {
        Self {
            artifact_store,
            process_cache: Arc::new(Mutex::new(CompiledProcessCache::new())),
            surface,
            execution_sink: None,
            trace_context: lash_trace::TraceContext::default(),
            execution_bounds: lashlang::ExecutionBounds::unbounded(),
        }
    }

    pub fn in_memory(surface: LashlangSurface) -> Self {
        Self::new(
            lashlang::global_in_memory_lashlang_artifact_store(),
            surface,
        )
    }

    pub fn with_execution_trace(
        mut self,
        sink: Option<Arc<dyn lash_trace::TraceSink>>,
        trace_context: lash_trace::TraceContext,
    ) -> Self {
        self.execution_sink = sink;
        self.trace_context = trace_context;
        self
    }

    pub fn with_execution_bounds(mut self, execution_bounds: lashlang::ExecutionBounds) -> Self {
        self.execution_bounds = execution_bounds;
        self
    }

    pub fn artifact_store(&self) -> Arc<dyn LashlangArtifactStore> {
        Arc::clone(&self.artifact_store)
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for LashlangProcessEngine {
    fn kind(&self) -> &'static str {
        LASHLANG_ENGINE_KIND
    }

    async fn run(
        &self,
        context: lash_core::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        Box::pin(process::run_lashlang_process(
            self.clone(),
            context,
            payload,
        ))
        .await
    }

    async fn protect_start_artifacts(
        &self,
        owner: &lash_core::ArtifactOwner,
        payload: &serde_json::Value,
    ) -> Result<(), lash_core::PluginError> {
        let input = LashlangProcessInput::from_payload(payload.clone()).map_err(|error| {
            lash_core::PluginError::Session(format!("invalid lashlang process payload: {error}"))
        })?;
        self.artifact_store
            .retain_module_artifact(owner, &input.module_ref)
            .await
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))
    }

    async fn transfer_start_artifacts(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        payload: &serde_json::Value,
    ) -> Result<(), lash_core::PluginError> {
        let input = LashlangProcessInput::from_payload(payload.clone()).map_err(|error| {
            lash_core::PluginError::Session(format!("invalid lashlang process payload: {error}"))
        })?;
        self.artifact_store
            .transfer_module_artifact(from, to, &input.module_ref)
            .await
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))
    }

    async fn release_artifacts(
        &self,
        owner: &lash_core::ArtifactOwner,
        payload: &serde_json::Value,
    ) -> Result<(), lash_core::PluginError> {
        let input = LashlangProcessInput::from_payload(payload.clone()).map_err(|error| {
            lash_core::PluginError::Session(format!("invalid lashlang process payload: {error}"))
        })?;
        self.artifact_store
            .release_module_artifact(owner, &input.module_ref)
            .await
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))
    }

    async fn retire_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lash_core::PluginError> {
        self.artifact_store
            .retire_module_artifact_owner(owner)
            .await
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))
    }
}

pub fn admit_lashlang_process(
    _kind: &'static str,
    payload: &serde_json::Value,
    _env_spec: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    let input = LashlangProcessInput::from_payload(payload.clone()).map_err(|err| {
        lash_core::PluginError::Session(format!("invalid lashlang process payload: {err}"))
    })?;
    Ok(lashlang_process_identity(&input))
}

pub fn lashlang_process_engine_registration(
    engine: LashlangProcessEngine,
) -> lash_core::ProcessEngineRegistration {
    lash_core::ProcessEngineRegistration::new(
        Arc::new(engine),
        lash_core::ProcessEngineAdmission::new(LASHLANG_ENGINE_KIND, admit_lashlang_process),
    )
    .expect("lashlang engine and admission share a fixed kind")
}

mod bridge;
#[cfg(test)]
mod catalog_tests;
mod catalogue_preview;
mod deferred;
mod deferred_triggers;
mod process;
mod typed_output;

pub use bridge::{
    lashlang_value_to_json, process_event_payload, process_sleep,
    protocol_tool_output_to_lashlang_value, protocol_tool_reply_to_lashlang_value,
};
pub use catalogue_preview::{
    CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
    DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, catalogue_preview_contribution,
    catalogue_preview_contribution_for_entries,
    catalogue_preview_contribution_for_entries_with_options,
    catalogue_preview_contribution_for_manifests, catalogue_preview_contribution_with_options,
    catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
    catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
};
pub use deferred::{
    DeferredLinkError, DeferredResolutionError, DeferredResolutionLinkKey,
    DeferredResolutionRecord, DeferredToolResolver, RecordedGrantInstallError, Resolution,
    SharedDeferredToolResolver, ToolGrant, link_with_deferred_resolution,
    resolve_and_build_deferred_environment, resolve_and_build_deferred_environment_from_references,
    resolve_and_fold_deferred,
};
pub use deferred_triggers::{
    DeferredTriggerProvider, DeferredTriggerProviderRegistry, DeferredTriggerResolutionError,
    DeferredTriggerResolutionRecord, DeferredTriggerResolver, SharedDeferredTriggerResolver,
    TriggerGrant, TriggerResolution, resolve_and_fold_deferred_triggers,
};
pub use process::{
    LASHLANG_SEGMENT_STATE_VERSION, lashlang_process_event_types,
    lashlang_process_signal_event_types, lashlang_program_hash, lashlang_type_expr_schema,
    trace_lashlang_main_map,
};
pub use typed_output::parse_output_schema;

#[cfg(test)]
mod lib_tests;
