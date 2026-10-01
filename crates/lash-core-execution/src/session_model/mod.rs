pub use lash_core_store::session_policy::*;
pub mod context;
pub use lash_sansio::session_model::message;

use crate::llm::types::{LlmEventSender, LlmStreamEvent};
use crate::provider::{AttachmentCapabilitySnapshot, ProviderHandle, ReasoningSelection};
use crate::{ModelConfig, ModelKey, ModelUnavailable, RuntimeModels};

pub use lash_sansio::format_tool_output_content;
pub use lash_sansio::session_model::{
    ConversationRecord, ErrorEnvelope, FailureCode, Message, MessageRole, Namespace,
    NoProgressBudget, Part, PartKind, ProtocolEvent, SessionStreamEvent, StreamMessageKind,
    TokenUsage, TokenUsageOverflow, TurnBudget, TurnFailureCode, TurnFailureKind,
    make_error_envelope, make_error_event, reassign_part_ids, render_prompt,
    render_transcript_prompt, shared_parts,
};

pub type SessionHistoryRecord = lash_sansio::session_model::SessionHistoryRecord<ProtocolEvent>;

pub const PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID: &str = "lash.plugin_runtime";

pub use lash_core_llm::session_model::ChargeSafetyPolicy;

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

pub(crate) use lash_core_store::message_projection::plugin_message_to_message;

/// The lazy binding of one recorded model to the transport that executes it
/// on this worker (FIG-4404).
///
/// Nothing is resolved when the binding is made: [`Self::bind`] asks the
/// host's models only when the body of an unjournaled model call runs. A
/// fully journaled replay never runs such a body, so it never touches the
/// registry, and a key the deployment retired cannot block work that is
/// already recorded. The first bound transport is kept for the rest of the
/// attempt; clones share it.
#[derive(Clone)]
pub struct ModelBinding {
    recorded: crate::RecordedModel,
    models: std::sync::Arc<dyn RuntimeModels>,
    clock: std::sync::Arc<dyn crate::Clock>,
    bound: std::sync::Arc<std::sync::OnceLock<ProviderHandle>>,
}

impl ModelBinding {
    pub fn new(
        recorded: crate::RecordedModel,
        models: std::sync::Arc<dyn RuntimeModels>,
        clock: std::sync::Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            recorded,
            models,
            clock,
            bound: std::sync::Arc::default(),
        }
    }

    /// The recorded model this binding executes.
    pub fn recorded(&self) -> &crate::RecordedModel {
        &self.recorded
    }

    /// The transport that executes the recorded model. Only the body of an
    /// unjournaled model call, or an observation nothing records, calls
    /// this. A refusal is this deployment's fault and is never cached: the
    /// next attempt asks again.
    pub fn bind(&self) -> Result<ProviderHandle, ModelUnavailable> {
        if let Some(provider) = self.bound.get() {
            return Ok(provider.clone());
        }
        let provider = self
            .models
            .bind(&self.recorded)?
            .with_clock(std::sync::Arc::clone(&self.clock));
        Ok(self.bound.get_or_init(|| provider).clone())
    }

    /// [`Self::bind`] for the body of an unjournaled model call: a refusal
    /// is the attempt's typed fault, which leaves the step unsealed and is
    /// never the call's recorded result.
    pub fn bind_for_unjournaled_call(
        &self,
    ) -> Result<ProviderHandle, crate::RuntimeEffectControllerError> {
        self.bind()
            .map_err(|error| crate::RuntimeEffectControllerError::model_unavailable(&error))
    }
}

impl std::fmt::Debug for ModelBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelBinding")
            .field("key", self.recorded.key())
            .field("bound", &self.bound.get().is_some())
            .finish_non_exhaustive()
    }
}

/// Runtime-only policy: a session policy and the lazy binding of its recorded
/// model to the transport that executes it on this worker.
#[derive(Clone, Debug)]
pub struct RuntimeSessionPolicy {
    pub policy: SessionPolicy,
    model: ModelConfig,
    binding: ModelBinding,
}

impl RuntimeSessionPolicy {
    /// `policy` with its recorded model bound lazily through `models`;
    /// `None` when the policy selects no model, since there is nothing to
    /// bind.
    pub fn new(
        policy: SessionPolicy,
        models: std::sync::Arc<dyn RuntimeModels>,
        clock: std::sync::Arc<dyn crate::Clock>,
    ) -> Option<Self> {
        let model = policy.model.clone()?;
        let binding = ModelBinding::new(model.model.clone(), models, clock);
        Some(Self {
            policy,
            model,
            binding,
        })
    }

    /// The recorded model selection this policy runs.
    pub fn model_config(&self) -> &ModelConfig {
        &self.model
    }

    /// The lazy binding of the recorded model.
    pub fn binding(&self) -> &ModelBinding {
        &self.binding
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
/// persisted [`SessionPolicy`] directly. It selects a model by key; resolving
/// the spec mints that key's binding through the host's models, once, and the
/// resulting policy records it. A spec that selects no model keeps the base
/// policy's recorded binding verbatim, never re-resolving its key.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSpec {
    inherit: bool,
    pub model: Option<ModelKey>,
    /// The reasoning the session runs its model with. `None` keeps the base
    /// policy's selection.
    pub reasoning: Option<ReasoningSelection>,
    /// The attachment-acceptance rules the session renders attachments
    /// against (ADR 0026). `None` keeps the base policy's.
    pub attachment_acceptance: Option<std::sync::Arc<AttachmentCapabilitySnapshot>>,
    pub turn_budget: Option<TurnBudget>,
    /// Whether the session's turns run autonomously. `None` keeps the base
    /// policy's.
    pub autonomous: Option<bool>,
    /// Bound on consecutive unproductive provider attempts. `None` keeps the
    /// bound the base policy already carries.
    pub no_progress_budget: Option<NoProgressBudget>,
    /// Duplicate-billing appetite. `None` keeps the base policy's.
    pub charge_safety: Option<ChargeSafetyPolicy>,
    /// Plugin-keyed, serializable creation options (FIG-4379), the protocol
    /// plugin's prompt config among them. Each installed plugin creates its
    /// recorded namespace from its key, and only its owner's typed config
    /// commands change it afterwards. A key stated here is laid over the same
    /// key of the spec beneath it ([`PluginOptions::over`]); a key no
    /// installed plugin owns, or a value its owner refuses, fails the
    /// creation typed as
    /// [`SessionConfigRefused`](crate::SessionError::SessionConfigRefused).
    pub plugin_options: crate::PluginOptions,
    /// Generation intent for every LLM call the session makes. `None` inherits
    /// the base policy's options unchanged; `Some` applies a
    /// [`GenerationOverlay`], which merges per option unless it explicitly
    /// replaces.
    pub generation: Option<GenerationOverlay>,
}

// `serde_json::Value` never holds NaN or an infinite number, so the plugin
// options' `PartialEq` is reflexive.
impl Eq for SessionSpec {}

impl SessionSpec {
    /// Unset fields resolve from the runtime's core defaults.
    pub fn new() -> Self {
        Self {
            inherit: false,
            model: None,
            reasoning: None,
            attachment_acceptance: None,
            turn_budget: None,
            autonomous: None,
            no_progress_budget: None,
            charge_safety: None,
            plugin_options: crate::PluginOptions::default(),
            generation: None,
        }
    }

    /// Unset fields inherit from the live parent policy at resolution time.
    pub fn inherit() -> Self {
        Self {
            inherit: true,
            ..Self::new()
        }
    }

    /// The model the session runs, by the host's key.
    pub fn model(mut self, key: impl Into<ModelKey>) -> Self {
        self.model = Some(key.into());
        self
    }

    /// The reasoning the session runs its model with.
    pub fn reasoning(mut self, reasoning: ReasoningSelection) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// The attachment-acceptance rules the session renders attachments
    /// against.
    pub fn attachment_acceptance(
        mut self,
        acceptance: std::sync::Arc<AttachmentCapabilitySnapshot>,
    ) -> Self {
        self.attachment_acceptance = Some(acceptance);
        self
    }

    pub fn turn_budget(mut self, turn_budget: TurnBudget) -> Self {
        self.turn_budget = Some(turn_budget);
        self
    }

    /// Whether the session's turns run autonomously.
    pub fn autonomous(mut self, autonomous: bool) -> Self {
        self.autonomous = Some(autonomous);
        self
    }

    /// Bound consecutive provider attempts that commit no successful
    /// execution, or opt the session out of that bound.
    pub fn no_progress_budget(mut self, no_progress_budget: NoProgressBudget) -> Self {
        self.no_progress_budget = Some(no_progress_budget);
        self
    }

    /// # Integrator class
    ///
    /// Host applications use this setting; protocol and provider implementors
    /// do not override it.
    pub fn charge_safety(mut self, charge_safety: ChargeSafetyPolicy) -> Self {
        self.charge_safety = Some(charge_safety);
        self
    }

    /// State `plugin_id`'s creation options, replacing what this spec stated
    /// for that plugin before.
    pub fn plugin<T: serde::Serialize>(
        mut self,
        plugin_id: impl Into<String>,
        options: T,
    ) -> Result<Self, serde_json::Error> {
        self.plugin_options.insert_typed(plugin_id, options)?;
        Ok(self)
    }

    /// State every plugin's creation options at once.
    pub fn plugin_options(mut self, plugin_options: crate::PluginOptions) -> Self {
        self.plugin_options = plugin_options;
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

    /// Resolve this spec over `base`. A selected key is minted through
    /// `models` now; with none, `base`'s recorded model is kept as recorded.
    /// A spec that selects a key or reasoning has the pair it records judged
    /// against the recorded capability, so an unsupported selection is
    /// refused here and nothing is created with it.
    pub fn resolve_against(
        &self,
        base: &SessionPolicy,
        models: &dyn RuntimeModels,
    ) -> Result<SessionPolicy, SpecResolveError> {
        let mut policy = base.clone();
        if let Some(key) = self.model.as_ref() {
            let recorded = models.snapshot(key).map_err(SpecResolveError::Model)?;
            let reasoning = base
                .model
                .as_ref()
                .map(|model| model.reasoning.clone())
                .unwrap_or_default();
            policy.model = Some(ModelConfig {
                model: recorded,
                reasoning,
            });
        }
        if let Some(reasoning) = self.reasoning.as_ref() {
            policy
                .model
                .as_mut()
                .ok_or(SpecResolveError::ReasoningWithoutModel)?
                .reasoning = reasoning.clone();
        }
        if (self.model.is_some() || self.reasoning.is_some())
            && let Some(model) = policy.model.as_ref()
        {
            model
                .validate_reasoning()
                .map_err(SpecResolveError::Reasoning)?;
        }
        if let Some(acceptance) = self.attachment_acceptance.as_ref() {
            policy.attachment_acceptance = acceptance.clone();
        }
        if let Some(turn_budget) = self.turn_budget {
            policy.turn_budget = turn_budget;
        }
        if let Some(autonomous) = self.autonomous {
            policy.autonomous = autonomous;
        }
        if let Some(no_progress_budget) = self.no_progress_budget {
            policy.no_progress_budget = no_progress_budget;
        }
        if let Some(charge_safety) = self.charge_safety.as_ref() {
            policy.charge_safety = charge_safety.clone();
        }
        if let Some(generation) = self.generation.as_ref() {
            policy.generation = generation.resolve(&policy.generation);
        }
        Ok(policy)
    }
}

/// Why a [`SessionSpec`] did not resolve to a policy.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpecResolveError {
    /// The spec's model key has no binding on this deployment.
    #[error(transparent)]
    Model(ModelUnavailable),
    /// The spec selects reasoning, but neither it nor its base selects a
    /// model.
    #[error("a reasoning selection needs a model, and none is selected")]
    ReasoningWithoutModel,
    /// The reasoning the spec records is one its model's recorded capability
    /// refuses.
    #[error(transparent)]
    Reasoning(crate::ReasoningRefused),
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self::new()
    }
}

/// The receiving half of [`llm_stream_channel`]: the provider's stream events
/// arriving at the journaled step body that forwards each one into the
/// journal. Declared here so the channel's mechanism is named at the seam
/// instead of inside scanned drive code (FIG-3672).
pub type LlmStreamEventRx = tokio::sync::mpsc::UnboundedReceiver<LlmStreamEvent>;

/// Open the provider stream-event pipe [`transport_stream_events`] consumes:
/// the sender goes into the request's `stream_events` so the provider's task
/// can report progress, and the receiver stays with the step body.
pub fn llm_stream_channel() -> (
    tokio::sync::mpsc::UnboundedSender<LlmStreamEvent>,
    LlmStreamEventRx,
) {
    tokio::sync::mpsc::unbounded_channel()
}

pub fn transport_stream_events(
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

    /// Charge safety is recorded session config (FIG-4376): a chosen appetite
    /// survives the durable policy round trip, and the safe default stays
    /// absent so the default shape does not widen.
    #[test]
    fn charge_safety_round_trips_through_the_durable_policy_shape() {
        let default_value = serde_json::to_value(SessionPolicy::new(crate::TurnBudget::Unbounded))
            .expect("serialize default policy");
        assert!(
            default_value.get("charge_safety").is_none(),
            "the safe default must not widen the persisted shape: {default_value}"
        );

        let mut policy = SessionPolicy::new(crate::TurnBudget::Unbounded);
        policy.charge_safety = ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 2,
            max_duplicate_cost_tokens: Some(4_096),
        };
        let value = serde_json::to_value(&policy).expect("serialize policy");
        assert!(value.get("charge_safety").is_some(), "{value}");
        let decoded: SessionPolicy = serde_json::from_value(value).expect("decode policy");
        assert_eq!(decoded.charge_safety, policy.charge_safety);
    }

    #[test]
    fn session_spec_states_charge_safety_for_creation() {
        let base = SessionPolicy::new(crate::TurnBudget::Unbounded);
        let appetite = ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 3,
            max_duplicate_cost_tokens: Some(8_192),
        };

        assert_eq!(
            resolve(&SessionSpec::inherit(), &base).charge_safety,
            ChargeSafetyPolicy::RequireGuarantee
        );
        assert_eq!(
            resolve(
                &SessionSpec::inherit().charge_safety(appetite.clone()),
                &base
            )
            .charge_safety,
            appetite
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
    fn session_policy_serializes_the_recorded_model_and_no_transport() {
        let policy = SessionPolicy {
            model: Some(recorded("mock-model")),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        };

        let value = serde_json::to_value(&policy).expect("serialize policy");

        assert_eq!(value["model"]["model"]["key"], "mock-model");
        assert_eq!(
            value["model"]["model"]["metadata"]["wire_model"],
            "mock-model-wire"
        );
        assert!(value.get("provider").is_none());
        assert!(value.get("provider_id").is_none());
        let decoded: SessionPolicy = serde_json::from_value(value).expect("decode policy");
        assert_eq!(decoded.model, policy.model);
    }

    /// A catalog serving the listed keys, counting its mints; it is never
    /// asked to bind.
    struct CountingModels {
        keys: &'static [&'static str],
        snapshots: std::sync::atomic::AtomicUsize,
    }

    impl CountingModels {
        fn serving(keys: &'static [&'static str]) -> Self {
            Self {
                keys,
                snapshots: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn snapshots(&self) -> usize {
            self.snapshots.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RuntimeModels for CountingModels {
        fn snapshot(&self, key: &ModelKey) -> Result<crate::RecordedModel, ModelUnavailable> {
            self.snapshots
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.keys.contains(&key.as_str()) {
                Ok(recorded(key.as_str()).model)
            } else {
                Err(ModelUnavailable::new(
                    key.clone(),
                    crate::ModelUnavailableReason::UnknownKey,
                ))
            }
        }

        fn bind(
            &self,
            recorded: &crate::RecordedModel,
        ) -> Result<ProviderHandle, ModelUnavailable> {
            panic!(
                "resolving a spec never binds a transport: {}",
                recorded.key()
            )
        }
    }

    /// `key`'s binding with the `low`/`high` efforts, except `plain-model`,
    /// whose capability has no reasoning controls.
    fn recorded(key: &str) -> ModelConfig {
        let mut builder =
            crate::ModelMetadata::builder(format!("{key}-wire")).context_window_tokens(200_000);
        if key != "plain-model" {
            builder = builder.capability(crate::ModelCapability {
                reasoning: Some(crate::ReasoningCapability {
                    efforts: vec!["low".to_string(), "high".to_string()],
                    encoding: crate::ReasoningEncoding::Effort,
                    disable: false,
                    mandatory: false,
                }),
                ..crate::ModelCapability::default()
            });
        }
        ModelConfig::new(crate::RecordedModel::mint(
            ModelKey::new(key),
            builder.build().expect("valid test model"),
        ))
    }

    fn resolve(spec: &SessionSpec, base: &SessionPolicy) -> SessionPolicy {
        spec.resolve_against(base, &crate::EmptyModels)
            .expect("a spec naming no model resolves without a catalog")
    }

    #[test]
    fn an_inheriting_spec_copies_the_recorded_model_without_minting() {
        let base = SessionPolicy {
            model: Some(
                recorded("parent-model")
                    .with_reasoning(ReasoningSelection::Effort("high".to_string())),
            ),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        };
        let models = CountingModels::serving(&["parent-model"]);
        let child = SessionSpec::inherit()
            .resolve_against(&base, &models)
            .expect("inherit");
        assert_eq!(
            child.model, base.model,
            "the child copies the resolved fact"
        );
        assert_eq!(
            models.snapshots(),
            0,
            "inheritance never re-derives the model"
        );
    }

    #[test]
    fn a_spec_key_is_minted_once_and_keeps_the_base_reasoning() {
        let base = SessionPolicy {
            model: Some(
                recorded("parent-model")
                    .with_reasoning(ReasoningSelection::Effort("high".to_string())),
            ),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        };
        let models = CountingModels::serving(&["parent-model", "child-model"]);
        let child = SessionSpec::inherit()
            .model("child-model")
            .resolve_against(&base, &models)
            .expect("mint");
        assert_eq!(models.snapshots(), 1);
        assert_eq!(
            child.model,
            Some(
                recorded("child-model")
                    .with_reasoning(ReasoningSelection::Effort("high".to_string()))
            )
        );

        let low = SessionSpec::inherit()
            .model("child-model")
            .reasoning(ReasoningSelection::Effort("low".to_string()))
            .resolve_against(&base, &models)
            .expect("mint with reasoning");
        assert_eq!(
            low.model.expect("model").reasoning,
            ReasoningSelection::Effort("low".to_string())
        );
    }

    #[test]
    fn a_spec_naming_an_unserved_key_or_reasoning_without_a_model_is_refused() {
        let base = SessionPolicy::new(crate::TurnBudget::Unbounded);
        let models = CountingModels::serving(&["served"]);
        assert!(matches!(
            SessionSpec::inherit()
                .model("retired")
                .resolve_against(&base, &models),
            Err(SpecResolveError::Model(ModelUnavailable {
                reason: crate::ModelUnavailableReason::UnknownKey,
                ..
            }))
        ));
        assert!(matches!(
            SessionSpec::inherit()
                .reasoning(ReasoningSelection::Effort("high".to_string()))
                .resolve_against(&base, &models),
            Err(SpecResolveError::ReasoningWithoutModel)
        ));
    }

    /// FIG-4531: the pair a spec records is judged where it is stated. An
    /// effort its key's capability does not advertise, and a base's effort
    /// inherited onto a key with no reasoning controls, are refused typed; a
    /// spec that changes neither keeps the base as recorded, unjudged.
    #[test]
    fn a_spec_whose_reasoning_the_model_refuses_is_refused_typed() {
        let base = SessionPolicy {
            model: Some(
                recorded("parent-model")
                    .with_reasoning(ReasoningSelection::Effort("high".to_string())),
            ),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        };
        let models = CountingModels::serving(&["parent-model", "plain-model"]);
        match SessionSpec::inherit()
            .reasoning(ReasoningSelection::Effort("extreme".to_string()))
            .resolve_against(&base, &models)
        {
            Err(SpecResolveError::Reasoning(refused)) => {
                assert_eq!(refused.key, ModelKey::new("parent-model"));
                assert_eq!(
                    refused.reasoning,
                    ReasoningSelection::Effort("extreme".to_string())
                );
            }
            other => panic!("an unadvertised effort is refused, got {other:?}"),
        }
        match SessionSpec::inherit()
            .model("plain-model")
            .resolve_against(&base, &models)
        {
            Err(SpecResolveError::Reasoning(refused)) => {
                assert_eq!(refused.key, ModelKey::new("plain-model"));
            }
            other => panic!("an inherited effort the key cannot take is refused, got {other:?}"),
        }
        SessionSpec::inherit()
            .model("plain-model")
            .reasoning(ReasoningSelection::ProviderDefault)
            .resolve_against(&base, &models)
            .expect("the provider's default reasoning fits a model with no controls");
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
            parallel_tool_calls: None,
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

        let inherited = resolve(&SessionSpec::inherit(), &base);
        assert_eq!(inherited.generation, base.generation);

        let merged = resolve(
            &SessionSpec::inherit().generation(crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            }),
            &base,
        );
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

        let child = resolve(
            &SessionSpec::inherit().generation(crate::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(4096),
                ..Default::default()
            }),
            &base,
        );

        assert_eq!(
            child.generation,
            crate::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(4096),
                temperature: base.generation.temperature.clone(),
                seed: Some(42),
                stop_sequences: base.generation.stop_sequences.clone(),
                parallel_tool_calls: None,
                projection_provenance: Default::default(),
            }
        );
    }

    #[test]
    fn session_spec_generation_replaces_and_clears_only_when_asked() {
        let base = pinned_base_policy();

        let replaced = resolve(
            &SessionSpec::inherit().replace_generation(crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            }),
            &base,
        );
        assert_eq!(
            replaced.generation,
            crate::GenerationOptions {
                seed: Some(11),
                ..Default::default()
            },
            "an explicit replace discards every inherited option"
        );

        let cleared = resolve(&SessionSpec::inherit().clear_generation(), &base);
        assert_eq!(cleared.generation, crate::GenerationOptions::default());

        let merged_default = resolve(
            &SessionSpec::inherit().generation(crate::GenerationOptions::default()),
            &base,
        );
        assert_eq!(
            merged_default.generation, base.generation,
            "an empty merge overlay expresses nothing and so clears nothing"
        );
    }
}
