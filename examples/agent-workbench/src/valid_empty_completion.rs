//! Dev-only, token-free provider for a Standard valid empty completion.

use std::sync::Arc;

use lash::provider::ProviderHandle;

const SCRIPT: &str = include_str!(
    "../../../crates/lash-sim/provider-scripts/runtime/openai-compatible.chat-valid-empty-stop.json"
);

pub(crate) fn provider() -> Result<ProviderHandle, lash::provider::LlmTransportError> {
    let transport = Arc::new(lash_sim::ScriptedLlmHttpTransport::from_json_str(SCRIPT)?);
    let (provider, _, _) = lash_sim::runtime_providers::runtime_provider_components(
        lash_sim::runtime_providers::OPENAI_COMPATIBLE,
        &transport,
    )
    .map_err(|error| lash::provider::LlmTransportError::new(error.to_string()))?;
    Ok(provider)
}
