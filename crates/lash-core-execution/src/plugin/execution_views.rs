use std::sync::Arc;

use super::AdmittedPluginConfig;

/// The recorded configuration of the owner whose contributions execute.
#[derive(Clone, Debug)]
pub struct OwnerConfigView(OwnerConfigSource);

#[derive(Clone, Debug)]
enum OwnerConfigSource {
    Session(AdmittedPluginConfig),
    /// A process's: each component's config entry in its environment.
    Components(Arc<crate::ProcessEnv>),
}

impl Default for OwnerConfigView {
    /// A session's view of no recorded configuration.
    fn default() -> Self {
        Self::session(AdmittedPluginConfig::default())
    }
}

impl OwnerConfigView {
    pub fn session(config: AdmittedPluginConfig) -> Self {
        Self(OwnerConfigSource::Session(config))
    }

    /// The config of the components `env` names.
    pub fn components(env: Arc<crate::ProcessEnv>) -> Self {
        Self(OwnerConfigSource::Components(env))
    }

    pub fn registry_kind(&self) -> super::RegistryKind {
        match &self.0 {
            OwnerConfigSource::Session(_) => super::RegistryKind::Session,
            OwnerConfigSource::Components(_) => super::RegistryKind::Process,
        }
    }
    /// Refuse an address from another composition before attempting local lookup.
    pub fn validate_provider(
        &self,
        provider: &super::ToolProviderHandle,
    ) -> Result<(), super::PluginError> {
        let expected = self.registry_kind();
        if provider.registry() == expected {
            Ok(())
        } else {
            Err(
                lash_execution_contracts::plugin_error::PluginError::ProviderRegistryMismatch {
                    expected,
                    provider: provider.clone(),
                },
            )
        }
    }

    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        owner: &str,
    ) -> Result<Option<T>, serde_json::Error> {
        match &self.0 {
            OwnerConfigSource::Session(config) => config.decode(owner),
            OwnerConfigSource::Components(env) => env
                .components()
                .iter()
                .find(|entry| entry.id().as_str() == owner)
                .map(|entry| entry.config().decode())
                .transpose(),
        }
    }

    /// Identity-bearing format and value of one declared config dependency.
    pub fn environment_config_slice(
        &self,
        owner: &str,
    ) -> Result<Option<serde_json::Value>, serde_json::Error> {
        match &self.0 {
            OwnerConfigSource::Session(config) => config
                .config
                .namespace(owner)
                .map(serde_json::to_value)
                .transpose(),
            OwnerConfigSource::Components(env) => env
                .components()
                .iter()
                .find(|entry| entry.id().as_str() == owner)
                .map(|entry| serde_json::to_value(entry.config()))
                .transpose(),
        }
    }

    /// The session config revision; a process has none.
    pub fn session_revision(&self) -> Option<u64> {
        match &self.0 {
            OwnerConfigSource::Session(config) => Some(config.revision),
            OwnerConfigSource::Components(_) => None,
        }
    }
}

/// Immutable inputs read by execution independently of their capture source.
#[derive(Clone, Debug)]
pub struct ExecutionFacts {
    pub model: Option<crate::LlmProfileConfig>,
    pub generation: crate::GenerationOptions,
    pub charge_safety: crate::ChargeSafetyPolicy,
    pub attachment_acceptance: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    pub max_tool_calls: crate::MaxToolCalls,
    pub render: Option<crate::RecordedRender>,
}

impl ExecutionFacts {
    /// What a process runs under: its environment's. A process has no
    /// render (D12).
    pub fn of_process(env: &crate::ProcessEnv) -> Self {
        Self {
            model: env.model().map(|model| model.profile().clone()),
            generation: env
                .model()
                .map(|model| model.generation().clone())
                .unwrap_or_default(),
            charge_safety: env
                .model()
                .map(|model| model.charge_safety().clone())
                .unwrap_or_default(),
            attachment_acceptance: Arc::clone(env.attachments()),
            max_tool_calls: env.max_tool_calls(),
            render: None,
        }
    }

    /// What a turn of a logical Run runs under: the run's recorded
    /// configuration and render (FIG-6021).
    pub fn of_run(run: &crate::ResolvedRun) -> Self {
        let config = run.config();
        Self {
            model: config.model.clone(),
            generation: config.generation.clone(),
            charge_safety: config.charge_safety.clone(),
            attachment_acceptance: Arc::clone(&config.attachment_acceptance),
            max_tool_calls: config.max_tool_calls,
            render: run.render.clone(),
        }
    }

    /// What an execution under `policy` runs under.
    pub fn of_policy(policy: &crate::SessionPolicy, render: Option<crate::RecordedRender>) -> Self {
        Self {
            model: policy.model.clone(),
            generation: policy.generation.clone(),
            charge_safety: policy.charge_safety.clone(),
            attachment_acceptance: Arc::clone(&policy.attachment_acceptance),
            max_tool_calls: policy.max_tool_calls,
            render,
        }
    }

    /// These facts as one call records them (§5.4).
    pub fn recorded_call(&self) -> RecordedCallFacts {
        RecordedCallFacts {
            render: self.render.clone(),
            attachment_acceptance: Arc::clone(&self.attachment_acceptance),
        }
    }

    /// These facts with what `call` recorded at its admission: what its
    /// redelivery presents under.
    #[must_use]
    pub fn with_recorded_call(mut self, call: &RecordedCallFacts) -> Self {
        self.render = call.render.clone();
        self.attachment_acceptance = Arc::clone(&call.attachment_acceptance);
        self
    }
}

/// The facts one tool call records inline at its admission (§5.4): the only
/// per-call readers, so its redelivery presents as its first delivery did.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedCallFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<crate::RecordedRender>,
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    pub attachment_acceptance: Arc<crate::provider::AttachmentCapabilitySnapshot>,
}
