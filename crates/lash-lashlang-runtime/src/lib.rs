use std::sync::{Arc, Mutex};

mod error;
pub use error::{LashlangHostError, LashlangProcessFailureCode, LashlangRuntimeError};
mod process_identity;
pub use process_identity::deterministic_lashlang_process_id;
mod typescript_runtime;
pub use typescript_runtime::{is_typescript_runtime_receiver, journaled_typescript_runtime_value};

#[cfg(feature = "testing")]
pub mod testing;

pub use lash_trace::{
    TraceLanguageChildExecution, TraceLanguageExecutionEvent, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionMap, TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
    TraceLanguageExecutionStatus, TraceLashlangEdgeSelection, TraceLashlangGraph,
    TraceLashlangGraphChildLink, TraceLashlangGraphEdge, TraceLashlangGraphNode,
    TraceLashlangGraphStore, TraceLashlangNodeStatus,
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
pub struct LashlangToolBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl LashlangToolBinding {
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

    pub fn executable_for(
        &self,
        tool_name: &str,
    ) -> Result<ResolvedLashlangToolBinding, LashlangRuntimeError> {
        if self.module_path.is_empty() {
            return Err(LashlangRuntimeError::MissingToolModulePath {
                tool: tool_name.to_string(),
            });
        }
        for segment in &self.module_path {
            validate_lashlang_identifier(tool_name, "module path segment", segment)?;
        }
        let operation = self
            .operation
            .as_deref()
            .filter(|operation| !operation.trim().is_empty())
            .ok_or_else(|| LashlangRuntimeError::MissingToolOperation {
                tool: tool_name.to_string(),
            })?;
        validate_lashlang_identifier(tool_name, "operation name", operation)?;
        let authority_type = self
            .authority_type
            .as_deref()
            .filter(|authority_type| !authority_type.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_authority_type(&self.module_path));
        Ok(ResolvedLashlangToolBinding {
            module_path: self.module_path.clone(),
            operation: operation.to_string(),
            authority_type,
            aliases: self.aliases.clone(),
        })
    }

    pub fn required_for_remote(
        manifest: &lash_core::ToolManifest,
    ) -> Result<ResolvedLashlangToolBinding, LashlangRuntimeError> {
        required_tool_lashlang_executable(manifest)
    }

    pub fn required_executable_for_remote(
        &self,
        tool_name: &str,
    ) -> Result<ResolvedLashlangToolBinding, LashlangRuntimeError> {
        self.executable_for(tool_name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLashlangToolBinding {
    pub module_path: Vec<String>,
    pub operation: String,
    pub authority_type: String,
    pub aliases: Vec<String>,
}

impl ResolvedLashlangToolBinding {
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
    label: &str,
    value: &str,
) -> Result<(), LashlangRuntimeError> {
    let value = value.trim();
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(LashlangRuntimeError::EmptyIdentifier {
            tool: tool_name.to_string(),
            label: label.to_string(),
        });
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(LashlangRuntimeError::InvalidIdentifier {
            tool: tool_name.to_string(),
            label: label.to_string(),
            value: value.to_string(),
        });
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return Err(LashlangRuntimeError::InvalidIdentifier {
            tool: tool_name.to_string(),
            label: label.to_string(),
            value: value.to_string(),
        });
    }
    Ok(())
}

pub fn tool_lashlang_binding(
    manifest: &lash_core::ToolManifest,
) -> Result<Option<LashlangToolBinding>, LashlangRuntimeError> {
    manifest
        .bindings
        .get(LASHLANG_TOOL_BINDING_KEY)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|source| LashlangRuntimeError::InvalidToolBinding {
            tool: manifest.name.clone(),
            source,
        })
}

pub fn required_tool_lashlang_binding(
    manifest: &lash_core::ToolManifest,
) -> Result<LashlangToolBinding, LashlangRuntimeError> {
    tool_lashlang_binding(manifest)?.ok_or_else(|| LashlangRuntimeError::MissingToolBinding {
        tool: manifest.name.clone(),
    })
}

pub fn required_tool_lashlang_executable(
    manifest: &lash_core::ToolManifest,
) -> Result<ResolvedLashlangToolBinding, LashlangRuntimeError> {
    required_tool_lashlang_binding(manifest)?.executable_for(&manifest.name)
}

pub fn required_tool_typescript_executable(
    manifest: &lash_core::ToolManifest,
) -> Result<ResolvedLashlangToolBinding, LashlangRuntimeError> {
    let binding = manifest
        .bindings
        .get(TYPESCRIPT_TOOL_BINDING_KEY)
        .cloned()
        .map(serde_json::from_value::<LashlangToolBinding>)
        .transpose()
        .map_err(
            |source| LashlangRuntimeError::InvalidTypescriptToolBinding {
                tool: manifest.name.clone(),
                source,
            },
        )?
        .ok_or_else(|| LashlangRuntimeError::MissingTypescriptToolBinding {
            tool: manifest.name.clone(),
        })?;
    binding.executable_for(&manifest.name)
}

pub trait ToolManifestLashlangExt {
    fn lashlang_binding(&self) -> Result<Option<LashlangToolBinding>, serde_json::Error>;
}

impl ToolManifestLashlangExt for lash_core::ToolManifest {
    fn lashlang_binding(&self) -> Result<Option<LashlangToolBinding>, serde_json::Error> {
        self.bindings
            .get(LASHLANG_TOOL_BINDING_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }
}

pub trait ToolDefinitionLashlangExt {
    fn with_lashlang_binding(self, lashlang_binding: LashlangToolBinding) -> Self;
}

impl ToolDefinitionLashlangExt for lash_core::ToolDefinition {
    fn with_lashlang_binding(mut self, lashlang_binding: LashlangToolBinding) -> Self {
        let value = serde_json::to_value(&lashlang_binding)
            .expect("lashlang tool binding must serialize to JSON");
        self.manifest
            .bindings
            .insert(LASHLANG_TOOL_BINDING_KEY.to_string(), value);
        self.manifest.bindings.insert(
            TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(lashlang_binding)
                .expect("typescript tool binding must serialize to JSON"),
        );
        self
    }
}

pub trait RemoteToolGrantLashlangExt {
    fn with_lashlang_binding(self, lashlang_binding: LashlangToolBinding) -> Self;
    fn lashlang_binding(&self) -> Result<Option<LashlangToolBinding>, serde_json::Error>;
}

impl RemoteToolGrantLashlangExt for lash_remote_protocol::RemoteToolGrant {
    fn with_lashlang_binding(mut self, lashlang_binding: LashlangToolBinding) -> Self {
        let value = serde_json::to_value(&lashlang_binding)
            .expect("lashlang tool binding must serialize to JSON");
        self.bindings
            .insert(LASHLANG_TOOL_BINDING_KEY.to_string(), value);
        self.bindings.insert(
            TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
            serde_json::to_value(lashlang_binding)
                .expect("typescript tool binding must serialize to JSON"),
        );
        self
    }

    fn lashlang_binding(&self) -> Result<Option<LashlangToolBinding>, serde_json::Error> {
        self.bindings
            .get(LASHLANG_TOOL_BINDING_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
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

    pub fn with_resources(mut self, resources: LashlangHostCatalog) -> Self {
        self.resources.extend(resources);
        self
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
            self.resources.extend(contribution.resources);
        }
        Ok(self)
    }

    pub fn host_environment(
        &self,
        catalog: &lash_core::ToolCatalog,
    ) -> Result<LashlangHostEnvironment, LashlangRuntimeError> {
        lashlang_host_environment_from_tool_catalog(
            catalog,
            self.abilities,
            self.language_features,
            self.resources.clone(),
        )
    }
}

pub fn lashlang_host_environment_from_tool_catalog(
    catalog: &lash_core::ToolCatalog,
    abilities: LashlangAbilities,
    language_features: LashlangLanguageFeatures,
    host_resources: LashlangHostCatalog,
) -> Result<LashlangHostEnvironment, LashlangRuntimeError> {
    let mut resources = lashlang_resources_from_tool_catalog(catalog)?;
    resources.extend(host_resources);
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
        lashlang::add_trigger_resource_operations(&mut resources);
    }
    Ok(
        LashlangHostEnvironment::new(resources, abilities)
            .with_language_features(language_features),
    )
}

pub fn lashlang_resources_from_tool_catalog(
    catalog: &lash_core::ToolCatalog,
) -> Result<LashlangHostCatalog, LashlangRuntimeError> {
    let mut host_catalog = LashlangHostCatalog::new();
    // Every externally activated catalog member is callable. Internal members
    // remain registry-resolvable for runtime-owned process bodies only.
    for entry in catalog.tools.iter() {
        if entry.manifest.activation == lash_core::ToolActivation::Internal {
            continue;
        }
        let lashlang_binding = required_tool_lashlang_executable(&entry.manifest)?;
        let operation_binding = catalog
            .resolve_contract(&entry.manifest.name)
            .as_deref()
            .map(lashlang_tool_contract_types)
            .unwrap_or(lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Any,
                output_from_input: None,
            });
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
                Some(current_binding) if current_binding == required_binding => {}
                Some(current_binding) => {
                    return Err(LashlangRuntimeError::ModuleOperationMismatch {
                        module: module.alias.clone(),
                        operation: operation.clone(),
                        actual: current_binding.host_operation.clone(),
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
    pub registration: lash_core::ProcessRegistration,
    pub label: Option<String>,
}

pub async fn prepare_lashlang_process_start(
    artifact_store: Arc<dyn LashlangArtifactStore>,
    parent_start_seed: &str,
    start: lashlang::ProcessStart,
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
    if artifact.host_requirements_ref != start.host_requirements_ref {
        return Err(LashlangRuntimeError::ArtifactRequirementsMismatch {
            module_ref: start.module_ref.to_string(),
            requested: start.host_requirements_ref.to_string(),
            actual: artifact.host_requirements_ref.to_string(),
        });
    }
    if artifact.process_ref(&start.process_name) != Some(&start.process_ref) {
        return Err(LashlangRuntimeError::ArtifactProcessMismatch {
            module_ref: start.module_ref.to_string(),
            process: start.process_name.clone(),
            process_ref: format!("{:?}", start.process_ref),
        });
    }
    let args = match serde_json::to_value(lashlang::Value::Record(Arc::new(start.args)))
        .map_err(|source| LashlangRuntimeError::SerializeProcessArgs { source })?
    {
        serde_json::Value::Object(map) => map,
        _ => return Err(LashlangRuntimeError::ProcessArgsNotRecord),
    };
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
    let registration = lash_core::ProcessRegistration::new(
        process_id,
        process_input,
        // Lashlang engine rows are journaled and idempotent by process id, so
        // recovery may re-execute them (ADR 0019).
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_identity(identity)
    .with_extra_event_types(
        lashlang_process_event_types()
            .into_iter()
            .chain(signal_event_types),
    );
    Ok(PreparedLashlangProcessStart {
        registration,
        label: display_name,
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
        .map(|binding| binding.host_operation.clone())
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

    async fn validate_start(
        &self,
        context: lash_core::ProcessEngineValidationContext<'_>,
        payload: &serde_json::Value,
        _env_spec: Option<&lash_core::ProcessExecutionEnvSpec>,
    ) -> Result<(), lash_core::PluginError> {
        let input: LashlangProcessInput =
            serde_json::from_value(payload.clone()).map_err(|err| {
                lash_core::PluginError::Session(format!("invalid lashlang process payload: {err}"))
            })?;
        let artifact = self
            .artifact_store
            .get_module_artifact(&input.module_ref)
            .await
            .map_err(|err| lash_core::PluginError::Session(format!("load module artifact: {err}")))?
            .ok_or_else(|| {
                lash_core::PluginError::Session(format!(
                    "missing lashlang module artifact `{}`",
                    input.module_ref
                ))
            })?;
        if artifact.host_requirements_ref != input.host_requirements_ref {
            return Err(lash_core::PluginError::Session(format!(
                "lashlang process `{}` requested surface {}, artifact has {}",
                input.process_name, input.host_requirements_ref, artifact.host_requirements_ref
            )));
        }
        if artifact.process_ref(&input.process_name) != Some(&input.process_ref) {
            return Err(lash_core::PluginError::Session(format!(
                "lashlang module `{}` does not export process `{}` as requested ref {:?}",
                input.module_ref, input.process_name, input.process_ref
            )));
        }
        let surface = self
            .surface
            .clone()
            .for_process_registry(context.process_registry_available());
        let host_environment = surface
            .host_environment(context.tool_catalog())
            .map_err(|err| lash_core::PluginError::Session(err.to_string()))?;
        if let Err(err) = lashlang_host_environment_satisfies_requirements(
            &artifact.host_requirements,
            &host_environment,
        ) {
            return Err(lash_core::PluginError::Session(format!(
                "lashlang process `{}` is incompatible with this host surface: {err}",
                input.process_name
            )));
        }
        Ok(())
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

    fn identity(&self, payload: &serde_json::Value) -> lash_core::ProcessIdentity {
        match LashlangProcessInput::from_payload(payload.clone()) {
            Ok(input) => lashlang_process_identity(&input),
            Err(_) => lash_core::ProcessIdentity::new(LASHLANG_ENGINE_KIND),
        }
    }
}

mod bridge;
#[cfg(test)]
mod catalog_tests;
mod catalogue_preview;
mod deferred;
mod process;
mod typed_output;

pub use bridge::{
    lashlang_value_to_json, process_event_payload, protocol_tool_output_to_lashlang_value,
    protocol_tool_reply_to_lashlang_value, sleep_duration_ms,
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
    DeferredResolutionLinkKey, DeferredResolutionRecord, DeferredToolResolver, Resolution,
    SharedDeferredToolResolver, ToolGrant, link_with_deferred_resolution,
    resolve_and_fold_deferred,
};
pub use process::{
    LASHLANG_SEGMENT_STATE_VERSION, lashlang_process_event_types,
    lashlang_process_signal_event_types, lashlang_program_hash, lashlang_type_expr_schema,
    trace_lashlang_main_map,
};
pub use typed_output::parse_output_schema;

#[cfg(test)]
mod lib_tests;
