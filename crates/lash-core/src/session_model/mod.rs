pub mod context;
pub use lash_sansio::session_model::message;
pub use lash_sansio::session_model::prompt;

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::ModelSpec;
use crate::llm::types::{LlmEventSender, LlmStreamEvent};
use crate::plugin::PluginMessage;
use crate::provider::{ProviderBinding, ProviderHandle, ProviderResolutionError};

pub use lash_sansio::format_tool_output_content;
pub use lash_sansio::session_model::{
    ConversationRecord, ErrorEnvelope, MAIN_AGENT_INTRO, Message, MessageRole, Part, PartKind,
    PromptBuiltin, PromptSlot, PromptTemplate, PromptTemplateEntry, PromptTemplateSection,
    ProtocolEvent, PruneState, SessionStreamEvent, TokenUsage, TurnTerminationPolicyState,
    default_prompt_template, make_error_envelope, make_error_event, reassign_part_ids,
    render_prompt, render_transcript_prompt, shared_parts,
};

pub type SessionHistoryRecord = lash_sansio::session_model::SessionHistoryRecord<ProtocolEvent>;

pub const PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID: &str = "lash.plugin_runtime";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PersistedPluginRuntimeEvent {
    pub plugin_id: String,
    pub event: crate::PluginRuntimeEvent,
}

pub fn plugin_runtime_protocol_event(
    plugin_id: impl Into<String>,
    event: crate::PluginRuntimeEvent,
) -> Result<ProtocolEvent, serde_json::Error> {
    ProtocolEvent::typed(
        PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID,
        PersistedPluginRuntimeEvent {
            plugin_id: plugin_id.into(),
            event,
        },
    )
}

pub fn plugin_runtime_event_from_protocol(
    event: &ProtocolEvent,
) -> Result<Option<PersistedPluginRuntimeEvent>, serde_json::Error> {
    event.decode(PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID)
}

/// Send an event to the channel if it's still open.
pub(crate) async fn send_event(tx: &mpsc::Sender<SessionStreamEvent>, event: SessionStreamEvent) {
    if !tx.is_closed() {
        let _ = tx.send(event).await;
    }
}

pub(crate) fn plugin_message_to_message(
    plugin_message: &PluginMessage,
    fallback_id: &str,
) -> Message {
    let message_id = plugin_message
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_id.to_string());
    let mut parts = if plugin_message.parts.is_empty() && !plugin_message.content.is_empty() {
        vec![Part {
            id: format!("{message_id}.p0"),
            kind: PartKind::Text,
            content: plugin_message.content.clone(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]
    } else {
        plugin_message.parts.clone()
    };
    parts.extend(
        plugin_message
            .attachments
            .iter()
            .cloned()
            .map(|source| Part {
                id: String::new(),
                kind: PartKind::Attachment,
                content: String::new(),
                attachment: Some(message::PartAttachment { source }),
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }),
    );
    reassign_part_ids(&message_id, &mut parts);
    Message {
        id: message_id,
        role: plugin_message.role,
        parts: Arc::new(parts),
        origin: plugin_message.origin.clone().or_else(|| {
            Some(crate::MessageOrigin::Plugin {
                plugin_id: "plugin".to_string(),
                transient: false,
            })
        }),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPolicy {
    pub model: ModelSpec,
    pub provider_id: String,
    pub session_id: Option<String>,
    pub autonomous: bool,
    pub max_turns: Option<usize>,
    pub prompt: crate::PromptLayer,
    /// Caller-owned generation intent applied to every LLM call this session
    /// makes. It lives on the policy rather than on a single turn because it
    /// is session-wide truth: child sessions resolve their spec against this
    /// policy, and a resumed session restores it with the rest of the policy.
    pub generation: crate::GenerationOptions,
}

impl SessionPolicy {
    pub fn recorded_provider_id(&self) -> &str {
        self.provider_id.trim()
    }

    pub fn model_id(&self) -> &str {
        &self.model.id
    }

    pub fn model_variant(&self) -> &crate::ReasoningSelection {
        &self.model.variant
    }

    pub fn context_window_tokens(&self) -> usize {
        self.model.context_window_tokens()
    }
}

impl serde::Serialize for SessionPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut fields = 5;
        if !self.prompt.is_empty() {
            fields += 1;
        }
        if self.generation != crate::GenerationOptions::default() {
            fields += 1;
        }
        let mut state = serializer.serialize_struct("SessionPolicy", fields)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("provider_id", self.recorded_provider_id())?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("autonomous", &self.autonomous)?;
        state.serialize_field("max_turns", &self.max_turns)?;
        if !self.prompt.is_empty() {
            state.serialize_field("prompt", &self.prompt)?;
        }
        if self.generation != crate::GenerationOptions::default() {
            state.serialize_field("generation", &self.generation)?;
        }
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for SessionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            model: ModelSpec,
            #[serde(default)]
            provider_id: String,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            autonomous: bool,
            #[serde(default)]
            max_turns: Option<usize>,
            #[serde(default)]
            prompt: crate::PromptLayer,
            #[serde(default)]
            generation: crate::GenerationOptions,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        if value
            .as_object()
            .is_some_and(|object| object.contains_key("provider"))
        {
            return Err(serde::de::Error::custom(
                "legacy serialized provider config is not supported in session state; persist provider_id only",
            ));
        }
        let wire = Wire::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            model: wire.model,
            provider_id: wire.provider_id,
            session_id: wire.session_id,
            autonomous: wire.autonomous,
            max_turns: wire.max_turns,
            prompt: wire.prompt,
            generation: wire.generation,
        })
    }
}

/// Runtime-only policy resolved against host-owned live dependencies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeSessionPolicy {
    pub policy: SessionPolicy,
    pub binding: ProviderBinding,
}

impl RuntimeSessionPolicy {
    pub fn new(policy: SessionPolicy, binding: ProviderBinding) -> Self {
        Self { policy, binding }
    }

    pub fn from_provider(
        policy: SessionPolicy,
        provider: ProviderHandle,
    ) -> Result<Self, ProviderResolutionError> {
        let binding = ProviderBinding::new(policy.recorded_provider_id(), provider)?;
        Ok(Self { policy, binding })
    }

    pub fn provider(&self) -> &ProviderHandle {
        &self.binding.provider
    }

    pub fn into_policy(self) -> SessionPolicy {
        self.policy
    }
}

impl std::ops::Deref for RuntimeSessionPolicy {
    type Target = SessionPolicy;

    fn deref(&self) -> &Self::Target {
        &self.policy
    }
}

impl std::ops::DerefMut for RuntimeSessionPolicy {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.policy
    }
}

/// Reusable session configuration overlay.
///
/// `SessionSpec` is the public configuration shape for callers that want to
/// describe either a root session or a child session without constructing the
/// persisted [`SessionPolicy`] directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSpec {
    inherit: bool,
    pub provider_id: Option<String>,
    pub model: Option<ModelSpec>,
    pub max_turns: Option<Option<usize>>,
    pub prompt: Option<crate::PromptLayer>,
    /// Generation intent for every LLM call the session makes. `None` inherits
    /// the base policy's options; `Some` replaces them wholesale, so an
    /// explicitly empty [`crate::GenerationOptions`] clears an inherited one.
    pub generation: Option<crate::GenerationOptions>,
}

impl SessionSpec {
    /// Create an explicit root-style spec. Unset fields resolve from the
    /// runtime's core defaults.
    pub fn new() -> Self {
        Self {
            inherit: false,
            provider_id: None,
            model: None,
            max_turns: None,
            prompt: None,
            generation: None,
        }
    }

    /// Create a parent-relative spec. Unset fields inherit from the live
    /// parent policy at resolution time.
    pub fn inherit() -> Self {
        Self {
            inherit: true,
            ..Self::new()
        }
    }

    pub fn inherits(&self) -> bool {
        self.inherit
    }

    pub fn provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
    }

    pub fn model(mut self, model: ModelSpec) -> Self {
        self.model = Some(model);
        self
    }

    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(Some(max_turns));
        self
    }

    pub fn clear_max_turns(mut self) -> Self {
        self.max_turns = Some(None);
        self
    }

    pub fn prompt_layer(mut self, prompt: crate::PromptLayer) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Set the generation options every LLM call in the session carries.
    pub fn generation(mut self, generation: crate::GenerationOptions) -> Self {
        self.generation = Some(generation);
        self
    }

    pub fn resolve_against(&self, base: &SessionPolicy) -> SessionPolicy {
        let mut policy = base.clone();
        if let Some(provider_id) = self.provider_id.as_ref() {
            policy.provider_id = provider_id.clone();
        }
        if let Some(model) = self.model.as_ref() {
            policy.model = model.clone();
        }
        if let Some(max_turns) = self.max_turns {
            policy.max_turns = max_turns;
        }
        if let Some(prompt) = self.prompt.as_ref() {
            policy.prompt = prompt.clone();
        }
        if let Some(generation) = self.generation.as_ref() {
            policy.generation = generation.clone();
        }
        policy
    }
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn transport_stream_events(
    provider: &ProviderHandle,
    requested: Option<tokio::sync::mpsc::UnboundedSender<LlmStreamEvent>>,
) -> Option<LlmEventSender> {
    if let Some(requested) = requested {
        return Some(make_stream_event_sender(requested));
    }

    if provider.requires_streaming() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
        drop(rx);
        Some(make_stream_event_sender(tx))
    } else {
        None
    }
}

fn make_stream_event_sender(
    tx: tokio::sync::mpsc::UnboundedSender<LlmStreamEvent>,
) -> LlmEventSender {
    LlmEventSender::new(move |event| {
        let _ = tx.send(event);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_event_writes_tagged_payload() {
        let event = ProtocolEvent::typed("test_protocol", serde_json::json!({ "value": 42 }))
            .expect("typed event");
        let serialized = serde_json::to_value(event).expect("serialize");
        assert_eq!(serialized["plugin_id"], "test_protocol");
        assert!(serialized.get("payload").is_some());
    }

    #[test]
    fn session_policy_rejects_legacy_provider_config() {
        let err = serde_json::from_value::<SessionPolicy>(serde_json::json!({
            "model": {},
            "provider": {
                "type": "openai",
                "api_key": "must-not-load"
            }
        }))
        .expect_err("legacy provider config must fail");

        assert!(
            err.to_string()
                .contains("legacy serialized provider config is not supported")
        );
    }

    #[test]
    fn session_policy_serializes_provider_id_without_provider_handle() {
        let policy = SessionPolicy {
            provider_id: "mock-provider".to_string(),
            model: ModelSpec::from_token_limits("mock-model", Default::default(), 200_000, None)
                .expect("valid test model"),
            ..SessionPolicy::default()
        };

        let value = serde_json::to_value(&policy).expect("serialize policy");

        assert_eq!(value["provider_id"], "mock-provider");
        assert!(value.get("provider").is_none());
    }

    #[test]
    fn session_policy_persists_generation_options_only_when_set() {
        let mut policy = SessionPolicy::default();
        let value = serde_json::to_value(&policy).expect("serialize policy");
        assert!(
            value.get("generation").is_none(),
            "a policy expressing no generation intent must not write the key"
        );

        policy.generation = crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(4096),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
            seed: Some(1234),
        };
        let value = serde_json::to_value(&policy).expect("serialize policy");
        assert_eq!(
            value["generation"],
            serde_json::json!({
                "output_token_cap": 4096,
                "temperature": 0.0,
                "seed": 1234,
            })
        );

        let restored: SessionPolicy = serde_json::from_value(value).expect("deserialize policy");
        assert_eq!(restored.generation, policy.generation);
    }

    #[test]
    fn session_spec_generation_inherits_when_absent_and_replaces_when_present() {
        let base = SessionPolicy {
            generation: crate::GenerationOptions {
                temperature: Some(
                    crate::NonNegativeFiniteF64::new(0.7).expect("finite temperature"),
                ),
                seed: Some(9),
                ..Default::default()
            },
            ..SessionPolicy::default()
        };

        let inherited = SessionSpec::inherit().resolve_against(&base);
        assert_eq!(inherited.generation, base.generation);

        let replaced = SessionSpec::inherit()
            .generation(crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            })
            .resolve_against(&base);
        assert_eq!(replaced.generation.seed, Some(11));
        assert_eq!(
            replaced.generation.temperature, None,
            "an explicit spec replaces the inherited options rather than merging into them"
        );

        let cleared = SessionSpec::inherit()
            .generation(crate::GenerationOptions::default())
            .resolve_against(&base);
        assert_eq!(cleared.generation, crate::GenerationOptions::default());
    }
}
