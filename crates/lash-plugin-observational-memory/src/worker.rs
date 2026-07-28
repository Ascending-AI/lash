use lash_core::plugin::PluginError;
use lash_core::{DirectMessage, DirectOutputSpec, DirectPart, DirectRequest, DirectRole};

use crate::ObservationalMemoryConfig;
use crate::host::OmRuntimeHost;
use crate::model::{ActiveMemoryState, ObservedMessageNode, ParsedMemoryOutput};
use crate::prompts::{
    build_observer_prompt, build_reflector_prompt, observer_system_prompt, parse_memory_output,
    reflector_system_prompt, truncate_observation_tail,
};

pub(crate) async fn run_observer_batch(
    config: &ObservationalMemoryConfig,
    om_host: &OmRuntimeHost<'_>,
    policy: lash_core::SessionPolicy,
    active: Option<&ActiveMemoryState>,
    batch: &[impl ObservedMessageNode],
) -> Result<ParsedMemoryOutput, PluginError> {
    let existing_observations = active
        .map(|state| {
            truncate_observation_tail(&state.observations, config.previous_observer_tokens)
        })
        .filter(|text| !text.trim().is_empty());
    let prior_current_task = active.and_then(|state| state.current_task.clone());
    let prior_suggested_response = active.and_then(|state| state.suggested_response.clone());
    let prompt = build_observer_prompt(
        existing_observations.as_deref(),
        batch,
        prior_current_task.as_deref(),
        prior_suggested_response.as_deref(),
    );
    run_worker_turn(
        om_host,
        policy,
        "observer",
        &observer_system_prompt(),
        &prompt,
    )
    .await
}

pub(crate) async fn run_reflector(
    om_host: &OmRuntimeHost<'_>,
    policy: lash_core::SessionPolicy,
    observations: &str,
) -> Result<ParsedMemoryOutput, PluginError> {
    let prompt = build_reflector_prompt(observations);
    run_worker_turn(
        om_host,
        policy,
        "reflector",
        &reflector_system_prompt(),
        &prompt,
    )
    .await
}

async fn run_worker_turn(
    om_host: &OmRuntimeHost<'_>,
    policy: lash_core::SessionPolicy,
    worker_kind: &str,
    system_prompt: &str,
    prompt: &str,
) -> Result<ParsedMemoryOutput, PluginError> {
    let completion = om_host
        .direct_completion(
            DirectRequest {
                model: policy.model.id,
                model_variant: policy.model.variant,
                model_capability: policy.model.capability,
                messages: vec![
                    DirectMessage {
                        role: DirectRole::System,
                        parts: vec![DirectPart::Text(system_prompt.to_string())],
                    },
                    DirectMessage {
                        role: DirectRole::User,
                        parts: vec![DirectPart::Text(prompt.to_string())],
                    },
                ],
                attachments: Vec::new(),
                output: DirectOutputSpec::Text,
                stream_events: None,
                generation: policy.generation,
                session_id: Some(format!("{}-om-{worker_kind}", om_host.session_id())),
                caused_by: None,
                replay: None,
            },
            worker_kind,
        )
        .await?;
    Ok(parse_memory_output(&completion.text))
}
