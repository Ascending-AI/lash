use std::sync::Arc;

use super::AdmittedPluginConfig;

/// The recorded configuration of the owner whose contributions execute.
#[derive(Clone, Debug)]
pub struct OwnerConfigView(OwnerConfigSource);

#[derive(Clone, Debug)]
enum OwnerConfigSource {
    Session(AdmittedPluginConfig),
}

impl OwnerConfigView {
    pub fn session(config: AdmittedPluginConfig) -> Self {
        Self(OwnerConfigSource::Session(config))
    }

    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        owner: &str,
    ) -> Result<Option<T>, serde_json::Error> {
        match &self.0 {
            OwnerConfigSource::Session(config) => config.decode(owner),
        }
    }

    pub fn session_revision(&self) -> Option<u64> {
        match &self.0 {
            OwnerConfigSource::Session(config) => Some(config.revision),
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
    pub fn of_env_spec(spec: &crate::ProcessExecutionEnvSpec) -> Self {
        Self {
            model: spec.policy.model.clone(),
            generation: spec.policy.generation.clone(),
            charge_safety: spec.policy.charge_safety.clone(),
            attachment_acceptance: Arc::clone(&spec.policy.attachment_acceptance),
            max_tool_calls: spec.policy.max_tool_calls,
            render: spec.render.clone(),
        }
    }
}
