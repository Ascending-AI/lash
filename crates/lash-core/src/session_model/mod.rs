pub mod context;
mod session_policy_serde;
pub use lash_sansio::session_model::message;
pub use lash_sansio::session_model::prompt;

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::ModelSpec;
use crate::llm::types::{LlmEventSender, LlmStreamEvent};
use crate::provider::{ProviderBinding, ProviderHandle, ProviderResolutionError};
use lash_sansio::PluginMessage;

pub use lash_sansio::format_tool_output_content;
pub use lash_sansio::session_model::{
    ConversationRecord, ErrorEnvelope, MAIN_AGENT_INTRO, Message, MessageRole, NoProgressBudget,
    Part, PartKind, PromptBuiltin, PromptSlot, PromptTemplate, PromptTemplateEntry,
    PromptTemplateSection, ProtocolEvent, PruneState, SessionStreamEvent, TokenUsage,
    TokenUsageOverflow, TurnBudget, TurnTerminationPolicyState, default_prompt_template,
    make_error_envelope, make_error_event, reassign_part_ids, render_prompt,
    render_transcript_prompt, shared_parts,
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
        vec![Part::text(
            format!("{message_id}.p0"),
            plugin_message.content.clone(),
            None,
        )]
    } else {
        plugin_message.parts.clone()
    };
    parts.extend(plugin_message.attachments.iter().cloned().map(|source| {
        Part::attachment_part(
            String::new(),
            String::new(),
            Some(message::PartAttachment { source }),
        )
    }));
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPolicy {
    pub model: ModelSpec,
    pub provider_id: String,
    pub session_id: Option<String>,
    pub autonomous: bool,
    /// Required turn-budget decision. A host must choose either a non-zero
    /// bound or explicit unbounded execution; absence is never interpreted.
    pub turn_budget: TurnBudget,
    /// Bound on consecutive provider attempts within one turn that commit no
    /// successful execution.
    ///
    /// Host-owned live policy in the sense of ADR 0030: it is reconciled from
    /// the host's configuration on every reopen and is deliberately not
    /// persisted with the session head, so a host that changes the bound
    /// changes it for sessions already on disk. Its default is bounded, so a
    /// carrier that predates the field resolves to the bound rather than to a
    /// loop.
    pub no_progress_budget: NoProgressBudget,
    pub prompt: crate::PromptLayer,
    /// Caller-owned generation intent applied to every LLM call this session
    /// makes. It lives on the policy rather than on a single turn because it
    /// is session-wide truth: child sessions resolve their spec against this
    /// policy, and every carrier of a whole session policy carries it too.
    ///
    /// The durable session-head copy restores this intent on a cold load. Per
    /// ADR 0030, a live facade host may still reconcile its current spec over
    /// loaded state at open time, exactly as it does for the model and prompt.
    /// The process/remote policy carrier mirrors the same field.
    pub generation: crate::GenerationOptions,
}

impl SessionPolicy {
    /// Construct a policy with an explicit turn budget and otherwise neutral
    /// settings.
    pub fn new(turn_budget: TurnBudget) -> Self {
        Self {
            model: ModelSpec::default(),
            provider_id: String::new(),
            session_id: None,
            autonomous: false,
            turn_budget,
            no_progress_budget: NoProgressBudget::default(),
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        }
    }

    /// Exposes the provider ID captured in policy for protocol implementors restoring the same
    /// provider/model assignment on replay.
    pub fn recorded_provider_id(&self) -> &str {
        self.provider_id.trim()
    }

    /// Exposes model id to protocol and process-engine implementors while materializing
    /// protocol-specific session and turn state.
    pub fn model_id(&self) -> &str {
        &self.model.id
    }

    /// Exposes model variant to protocol and process-engine implementors while materializing
    /// protocol-specific session and turn state.
    pub fn model_variant(&self) -> &crate::ReasoningSelection {
        &self.model.variant
    }

    /// Exposes context window tokens to store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn context_window_tokens(&self) -> usize {
        self.model.context_window_tokens()
    }
}

/// Runtime-only policy resolved against host-owned live dependencies.
#[derive(Clone, Debug)]
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
        let binding = ProviderBinding::from_provider(provider);
        Ok(Self { policy, binding })
    }

    pub fn provider(&self) -> &ProviderHandle {
        &self.binding.provider
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

/// How a [`SessionSpec`] layers generation intent over the policy it resolves
/// against.
///
/// [`crate::GenerationOptions`] is a set of independently optional controls, not
/// one value, so the default overlay is per-field: a child that caps output
/// tokens keeps the temperature and seed its parent pinned. Discarding
/// inherited intent stays available, but it has to be asked for.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", content = "generation", rename_all = "snake_case")]
pub enum GenerationOverlay {
    /// Layer the set options over the inherited ones. Options this overlay
    /// leaves unset keep the value they inherit.
    Merge(crate::GenerationOptions),
    /// Use exactly these options, discarding every inherited one. A default
    /// [`crate::GenerationOptions`] therefore clears the inherited intent.
    Replace(crate::GenerationOptions),
}

impl GenerationOverlay {
    /// Resolve this overlay against the options it inherits.
    pub fn resolve(&self, inherited: &crate::GenerationOptions) -> crate::GenerationOptions {
        match self {
            Self::Merge(generation) => generation.merged_over(inherited),
            Self::Replace(generation) => generation.clone(),
        }
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
    pub turn_budget: Option<TurnBudget>,
    /// Bound on consecutive unproductive provider attempts. `None` keeps the
    /// bound the base policy already carries.
    pub no_progress_budget: Option<NoProgressBudget>,
    pub prompt: Option<crate::PromptLayer>,
    /// Generation intent for every LLM call the session makes. `None` inherits
    /// the base policy's options unchanged; `Some` applies a
    /// [`GenerationOverlay`], which merges per option unless it explicitly
    /// replaces.
    pub generation: Option<GenerationOverlay>,
}

impl SessionSpec {
    /// Create an explicit root-style spec. Unset fields resolve from the
    /// runtime's core defaults.
    pub fn new() -> Self {
        Self {
            inherit: false,
            provider_id: None,
            model: None,
            turn_budget: None,
            no_progress_budget: None,
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

    pub fn provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
    }

    pub fn model(mut self, model: ModelSpec) -> Self {
        self.model = Some(model);
        self
    }

    pub fn turn_budget(mut self, turn_budget: TurnBudget) -> Self {
        self.turn_budget = Some(turn_budget);
        self
    }

    /// Bound consecutive provider attempts that commit no successful
    /// execution, or opt the session out of that bound.
    pub fn no_progress_budget(mut self, no_progress_budget: NoProgressBudget) -> Self {
        self.no_progress_budget = Some(no_progress_budget);
        self
    }

    pub fn prompt_layer(mut self, prompt: crate::PromptLayer) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Layer generation options over the ones the session inherits.
    ///
    /// Options this call leaves unset keep their inherited value, so a
    /// subagent spec that caps output tokens does not silently drop the
    /// temperature and seed its parent pinned. Use
    /// [`replace_generation`](Self::replace_generation) or
    /// [`clear_generation`](Self::clear_generation) to discard what is
    /// inherited.
    pub fn generation(mut self, generation: crate::GenerationOptions) -> Self {
        self.generation = Some(GenerationOverlay::Merge(generation));
        self
    }

    /// Use exactly these generation options, discarding every inherited one.
    pub fn replace_generation(mut self, generation: crate::GenerationOptions) -> Self {
        self.generation = Some(GenerationOverlay::Replace(generation));
        self
    }

    /// Drop the inherited generation options and express none of your own.
    pub fn clear_generation(mut self) -> Self {
        self.generation = Some(GenerationOverlay::Replace(
            crate::GenerationOptions::default(),
        ));
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
        if let Some(turn_budget) = self.turn_budget {
            policy.turn_budget = turn_budget;
        }
        if let Some(no_progress_budget) = self.no_progress_budget {
            policy.no_progress_budget = no_progress_budget;
        }
        if let Some(prompt) = self.prompt.as_ref() {
            policy.prompt = prompt.clone();
        }
        if let Some(generation) = self.generation.as_ref() {
            policy.generation = generation.resolve(&policy.generation);
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
    fn session_policy_rejects_missing_turn_budget() {
        let mut value = serde_json::to_value(SessionPolicy::new(crate::TurnBudget::Unbounded))
            .expect("serialize complete policy");
        value
            .as_object_mut()
            .expect("policy is a JSON object")
            .remove("turn_budget");
        let err = serde_json::from_value::<SessionPolicy>(value)
            .expect_err("missing turn_budget must fail");

        assert!(
            err.to_string().contains("missing field `turn_budget`"),
            "missing policy field should name turn_budget: {err}"
        );
    }

    /// FIG-1407: the no-progress budget is an additive policy field, and a
    /// policy that never mentions it must serialize byte-for-byte as before —
    /// the persisted policy is part of the process-execution identity
    /// preimage, so a silently widened default shape would re-key every
    /// process.
    #[test]
    fn a_default_no_progress_budget_is_absent_from_the_serialized_policy() {
        let value = serde_json::to_value(SessionPolicy::new(crate::TurnBudget::Unbounded))
            .expect("serialize policy");
        assert!(
            value.get("no_progress_budget").is_none(),
            "the default bound must not widen the persisted shape: {value}"
        );

        let decoded: SessionPolicy = serde_json::from_value(value).expect("decode policy");
        assert_eq!(
            decoded.no_progress_budget,
            NoProgressBudget::default(),
            "a carrier that predates the field resolves to the bound"
        );
    }

    /// An explicit host choice — including the opt-out — survives the round
    /// trip, which is what makes it a policy rather than a constant.
    #[test]
    fn an_explicit_no_progress_budget_round_trips() {
        for budget in [NoProgressBudget::bounded(3), NoProgressBudget::Unbounded] {
            let policy = SessionPolicy {
                no_progress_budget: budget,
                ..SessionPolicy::new(crate::TurnBudget::Unbounded)
            };
            let value = serde_json::to_value(&policy).expect("serialize policy");
            assert!(value.get("no_progress_budget").is_some(), "{value}");
            let decoded: SessionPolicy = serde_json::from_value(value).expect("decode policy");
            assert_eq!(decoded.no_progress_budget, budget);
        }
    }

    #[test]
    fn session_policy_serializes_provider_id_without_provider_handle() {
        let policy = SessionPolicy {
            provider_id: "mock-provider".to_string(),
            model: ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("valid test model"),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        };

        let value = serde_json::to_value(&policy).expect("serialize policy");

        assert_eq!(value["provider_id"], "mock-provider");
        assert!(value.get("provider").is_none());
    }

    #[test]
    fn session_policy_persists_generation_options_only_when_set() {
        let mut policy = SessionPolicy::new(crate::TurnBudget::Unbounded);
        let value = serde_json::to_value(&policy).expect("serialize policy");
        assert!(
            value.get("generation").is_none(),
            "a policy expressing no generation intent must not write the key"
        );

        policy.generation = crate::GenerationOptions {
            output_token_cap: std::num::NonZeroUsize::new(4096),
            temperature: Some(crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
            seed: Some(1234),
            stop_sequences: Vec::new(),
            projection_provenance: Default::default(),
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

    fn pinned_base_policy() -> SessionPolicy {
        SessionPolicy {
            generation: crate::GenerationOptions {
                temperature: Some(
                    crate::NonNegativeFiniteF64::new(0.7).expect("finite temperature"),
                ),
                seed: Some(9),
                ..Default::default()
            },
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        }
    }

    #[test]
    fn session_spec_generation_inherits_when_absent_and_merges_per_option_when_present() {
        let base = pinned_base_policy();

        let inherited = SessionSpec::inherit().resolve_against(&base);
        assert_eq!(inherited.generation, base.generation);

        let merged = SessionSpec::inherit()
            .generation(crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            })
            .resolve_against(&base);
        assert_eq!(merged.generation.seed, Some(11));
        assert_eq!(
            merged.generation.temperature, base.generation.temperature,
            "an option the spec leaves unset keeps the value it inherits"
        );
    }

    #[test]
    fn session_spec_generation_merge_keeps_a_parent_pin_a_child_never_mentioned() {
        // A parent pins sampling for a repeatable benchmark; a subagent
        // capability only bounds its own output length. The subagent must not
        // silently fall back to provider-default sampling — nothing reports
        // an option the child never requested.
        let base = SessionPolicy {
            generation: crate::GenerationOptions {
                temperature: Some(
                    crate::NonNegativeFiniteF64::new(0.0).expect("finite temperature"),
                ),
                seed: Some(42),
                stop_sequences: Vec::new(),
                ..Default::default()
            },
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        };

        let child = SessionSpec::inherit()
            .generation(crate::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(4096),
                ..Default::default()
            })
            .resolve_against(&base);

        assert_eq!(
            child.generation,
            crate::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(4096),
                temperature: base.generation.temperature.clone(),
                seed: Some(42),
                stop_sequences: base.generation.stop_sequences.clone(),
                projection_provenance: Default::default(),
            }
        );
    }

    #[test]
    fn session_spec_generation_replaces_and_clears_only_when_asked() {
        let base = pinned_base_policy();

        let replaced = SessionSpec::inherit()
            .replace_generation(crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            })
            .resolve_against(&base);
        assert_eq!(
            replaced.generation,
            crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            },
            "an explicit replace discards every inherited option"
        );

        let cleared = SessionSpec::inherit()
            .clear_generation()
            .resolve_against(&base);
        assert_eq!(cleared.generation, crate::GenerationOptions::default());

        let merged_default = SessionSpec::inherit()
            .generation(crate::GenerationOptions::default())
            .resolve_against(&base);
        assert_eq!(
            merged_default.generation, base.generation,
            "an empty merge overlay expresses nothing and so clears nothing"
        );
    }
}
