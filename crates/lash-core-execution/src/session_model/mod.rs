pub use lash_core_store::session_policy::*;
pub mod context;
pub use lash_sansio::session_model::message;

use crate::llm::types::{LlmEventSender, LlmStreamEvent};
use crate::provider::{AttachmentCapabilitySnapshot, ProviderHandle, ReasoningSelection};
use crate::{LlmProfileConfig, LlmProfileKey, LlmProfileUnavailable, LlmProfiles};

pub use lash_sansio::format_tool_output_content;
pub use lash_sansio::session_model::{
    ConversationRecord, ErrorEnvelope, FailureCode, MaxToolCalls, Message, MessageRole, Namespace,
    NoProgressBudget, Part, PartKind, ProtocolEvent, SessionStreamEvent, StreamMessageKind,
    TokenUsage, TokenUsageOverflow, ToolCallLimitExceeded, ToolCallLimitScope, TurnBudget,
    TurnFailureCode, TurnFailureKind, make_error_envelope, make_error_event, reassign_part_ids,
    render_prompt, render_transcript_prompt, shared_parts,
};

pub type SessionHistoryRecord = lash_sansio::session_model::SessionHistoryRecord<ProtocolEvent>;

pub const PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID: &str = "lash.plugin_runtime";

pub use lash_core_llm::session_model::ChargeSafetyPolicy;

/// Version of runtime-plugin events nested in session history.
/// version_surface = "migrate"
/// format_manifest = "PluginRuntimeEvent"
/// version_guard(roots(PersistedPluginRuntimeEvent))
pub const PLUGIN_RUNTIME_EVENT_VERSION: u32 = 1;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedPluginRuntimeEvent {
    pub format: u32,
    pub plugin_id: String,
    pub event: crate::PluginRuntimeEvent,
}

pub fn plugin_runtime_protocol_event(
    plugin_id: impl Into<String>,
    event: crate::PluginRuntimeEvent,
    fleet: crate::FleetFormat,
) -> Result<ProtocolEvent, serde_json::Error> {
    ProtocolEvent::typed(
        PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID,
        PersistedPluginRuntimeEvent {
            format: fleet.writer_version(lash_core_store::surface_format!(
                PLUGIN_RUNTIME_EVENT_VERSION
            )),
            plugin_id: plugin_id.into(),
            event,
        },
    )
}

pub fn plugin_runtime_event_from_protocol(
    event: &ProtocolEvent,
) -> Result<Option<PersistedPluginRuntimeEvent>, crate::StoredDataCorruption> {
    if event.plugin_id != PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID {
        return Ok(None);
    }
    let corrupt = |message: String| crate::StoredDataCorruption {
        record_kind: "plugin runtime event".into(),
        message,
    };
    #[derive(serde::Deserialize)]
    struct Stamp {
        format: u32,
    }
    let stamp: Stamp = serde_json::from_value(event.payload.clone())
        .map_err(|error| corrupt(error.to_string()))?;
    if !lash_core_store::store::upcast_chain_covers(
        lash_core_store::surface_format!(PLUGIN_RUNTIME_EVENT_VERSION),
        stamp.format,
        PLUGIN_RUNTIME_EVENT_VERSION,
    ) {
        return Err(corrupt(format!(
            "unsupported plugin runtime event format {}",
            stamp.format
        )));
    }
    serde_json::from_value(event.payload.clone())
        .map(Some)
        .map_err(|error| corrupt(error.to_string()))
}

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
pub struct LlmProfileBinding {
    recorded: crate::RecordedLlmProfile,
    models: std::sync::Arc<dyn LlmProfiles>,
    clock: std::sync::Arc<dyn crate::Clock>,
    bound: std::sync::Arc<std::sync::OnceLock<ProviderHandle>>,
}

impl LlmProfileBinding {
    pub fn new(
        recorded: crate::RecordedLlmProfile,
        models: std::sync::Arc<dyn LlmProfiles>,
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
    pub fn recorded(&self) -> &crate::RecordedLlmProfile {
        &self.recorded
    }

    /// The transport that executes the recorded model. Only the body of an
    /// unjournaled model call, or an observation nothing records, calls
    /// this. A refusal is this deployment's fault and is never cached: the
    /// next attempt asks again.
    pub fn bind(&self) -> Result<ProviderHandle, LlmProfileUnavailable> {
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
        self.bind().map_err(|error| {
            crate::RuntimeEffectControllerError::llm_profile_unavailable(
                &error.key,
                format!("the recorded model cannot be bound on this worker: {error}"),
            )
        })
    }
}

impl std::fmt::Debug for LlmProfileBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProfileBinding")
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
    model: LlmProfileConfig,
    binding: LlmProfileBinding,
}

impl RuntimeSessionPolicy {
    /// `policy` with its recorded model bound lazily through `models`;
    /// `None` when the policy selects no model, since there is nothing to
    /// bind.
    pub fn new(
        policy: SessionPolicy,
        models: std::sync::Arc<dyn LlmProfiles>,
        clock: std::sync::Arc<dyn crate::Clock>,
    ) -> Option<Self> {
        let model = policy.model.clone()?;
        let binding = LlmProfileBinding::new(model.model.clone(), models, clock);
        Some(Self {
            policy,
            model,
            binding,
        })
    }

    /// The recorded model selection this policy runs.
    pub fn llm_profile_config(&self) -> &LlmProfileConfig {
        &self.model
    }

    /// The lazy binding of the recorded model.
    pub fn binding(&self) -> &LlmProfileBinding {
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

/// A session's configuration, as its creator states it.
///
/// A root creation states a whole spec: [`SessionSpec::new`] takes the model
/// key, turn budget and tool-call limit; the creator also states a stall
/// bound with [`SessionSpec::no_progress_budget`]. Other fields have neutral
/// values. Nothing stands beneath it: a
/// deployment keeps no default spec, so a host that wants one keeps its own
/// `SessionSpec` value and passes it (FIG-4594). Every creation states its own policy.
///
/// It selects a model by key; resolving the spec mints that key's binding
/// through the host's models, once, and the resulting policy records it. An
/// overlay that selects no model keeps its base policy's recorded binding
/// verbatim, never re-resolving its key.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSpec {
    pub model: Option<LlmProfileKey>,
    /// The reasoning the session runs its model with. `None` keeps the base
    /// policy's selection.
    pub reasoning: Option<ReasoningSelection>,
    /// The attachment-acceptance rules the session renders attachments
    /// against (ADR 0026). `None` keeps the base policy's.
    pub attachment_acceptance: Option<std::sync::Arc<AttachmentCapabilitySnapshot>>,
    pub turn_budget: Option<TurnBudget>,
    /// The tool-call limit: the total one cell may make, and the number a
    /// process may hold at once. `None` keeps the base policy's; a root
    /// session has no base to keep, so it must state one.
    pub max_tool_calls: Option<MaxToolCalls>,
    /// Bound on consecutive unproductive provider attempts. `None` keeps the
    /// bound the base policy already carries.
    pub no_progress_budget: Option<NoProgressBudget>,
    /// Duplicate-billing appetite. `None` keeps the base policy's.
    pub charge_safety: Option<ChargeSafetyPolicy>,
    /// Plugin-keyed, serializable creation options (FIG-4379), the protocol
    /// plugin's prompt config among them. Each installed plugin creates its
    /// recorded namespace from its key, and only its owner's typed config
    /// commands change it afterwards. A key no installed plugin owns, or a
    /// value its owner refuses, fails the creation typed as
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
    /// A root session's spec: the model it runs, by the host's key, its
    /// turn budget and its tool-call limit, the three parts nothing
    /// defaults. The root also requires [`Self::no_progress_budget`]. Other
    /// fields start at the neutral value [`SessionPolicy::new`] states.
    pub fn new(
        model: impl Into<LlmProfileKey>,
        turn_budget: TurnBudget,
        max_tool_calls: MaxToolCalls,
    ) -> Self {
        Self {
            model: Some(model.into()),
            turn_budget: Some(turn_budget),
            max_tool_calls: Some(max_tool_calls),
            ..Self::neutral()
        }
    }

    fn neutral() -> Self {
        Self {
            model: None,
            reasoning: None,
            attachment_acceptance: None,
            turn_budget: None,
            max_tool_calls: None,
            no_progress_budget: None,
            charge_safety: None,
            plugin_options: crate::PluginOptions::default(),
            generation: None,
        }
    }

    /// The policy a run records from this spec alone: its stated fields
    /// over the neutral [`SessionPolicy::new`] of its turn budget and
    /// tool-call limit and stall bound, its key minted through `models` now.
    /// A spec that lacks any of these required choices (an
    /// incomplete spec) is refused: a root has no base to
    /// take them from.
    pub fn resolve_root(
        &self,
        models: &dyn LlmProfiles,
    ) -> Result<SessionPolicy, SpecResolveError> {
        if self.model.is_none() {
            return Err(SpecResolveError::RootWithoutLlmProfile);
        }
        self.resolve_against(&self.root_base()?, models)
    }

    /// The policy fields of a root this spec states, with no model minted:
    /// what a host session-turn start carries beside its model key and
    /// reasoning, so the start states nothing a catalog derives.
    pub fn stated_root_policy(&self) -> Result<SessionPolicy, SpecResolveError> {
        let mut unminted = self.clone();
        unminted.model = None;
        unminted.reasoning = None;
        unminted.resolve_against(&self.root_base()?, &crate::EmptyLlmProfiles)
    }

    fn root_base(&self) -> Result<SessionPolicy, SpecResolveError> {
        let turn_budget = self
            .turn_budget
            .ok_or(SpecResolveError::RootWithoutTurnBudget)?;
        let max_tool_calls = self
            .max_tool_calls
            .ok_or(SpecResolveError::RootWithoutMaxToolCalls)?;
        let no_progress_budget = self
            .no_progress_budget
            .ok_or(SpecResolveError::RootWithoutNoProgressBudget)?;
        Ok(SessionPolicy::new(
            turn_budget,
            max_tool_calls,
            no_progress_budget,
        ))
    }

    /// The model the session runs, by the host's key.
    pub fn model(mut self, key: impl Into<LlmProfileKey>) -> Self {
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

    /// The tool-call limit: the total one cell may make, and the number a
    /// process may hold at once.
    pub fn max_tool_calls(mut self, max_tool_calls: MaxToolCalls) -> Self {
        self.max_tool_calls = Some(max_tool_calls);
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
    /// Options this call leaves unset keep their base value, so a spec that
    /// caps output tokens does not silently drop the temperature and seed
    /// its base pinned. Use
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
    fn resolve_against(
        &self,
        base: &SessionPolicy,
        models: &dyn LlmProfiles,
    ) -> Result<SessionPolicy, SpecResolveError> {
        let mut policy = base.clone();
        if let Some(key) = self.model.as_ref() {
            let recorded = models.snapshot(key).map_err(SpecResolveError::Model)?;
            let reasoning = base
                .model
                .as_ref()
                .map(|model| model.reasoning.clone())
                .unwrap_or_default();
            policy.model = Some(LlmProfileConfig {
                model: recorded,
                reasoning,
            });
        }
        if let Some(reasoning) = self.reasoning.as_ref() {
            policy
                .model
                .as_mut()
                .ok_or(SpecResolveError::ReasoningWithoutLlmProfile)?
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
        if let Some(max_tool_calls) = self.max_tool_calls {
            policy.max_tool_calls = max_tool_calls;
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
    Model(LlmProfileUnavailable),
    /// The spec selects reasoning, but neither it nor its base selects a
    /// model.
    #[error("a reasoning selection needs a model, and none is selected")]
    ReasoningWithoutLlmProfile,
    /// The reasoning the spec records is one its model's recorded capability
    /// refuses.
    #[error(transparent)]
    Reasoning(crate::ReasoningRefused),
    /// A root's spec states no model: nothing stands beneath it to supply
    /// one.
    #[error("a root session's spec states no model")]
    RootWithoutLlmProfile,
    /// A root's spec states no turn budget: nothing stands beneath it to
    /// supply one.
    #[error("a root session's spec states no turn budget")]
    RootWithoutTurnBudget,
    /// A root's spec states no tool-call limit: nothing stands beneath it to
    /// supply one.
    #[error("a root session's spec states no max_tool_calls")]
    RootWithoutMaxToolCalls,
    /// The host has not chosen a stall bound.
    #[error("a root session's spec states no no_progress_budget")]
    RootWithoutNoProgressBudget,
}

/// The receiving half of [`llm_stream_channel`]: the provider's stream events
/// arriving at the journaled step body that forwards each one into the
/// journal. Declared here so the channel's mechanism is named at the seam
/// instead of inside scanned shift code (FIG-3672).
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

    /// FIG-4546: `max_tool_calls` has no default. A recorded policy or
    /// session head that states none is refused at load, typed by the field
    /// it lacks, and zero is not a limit.
    #[test]
    fn recorded_config_without_max_tool_calls_is_refused() {
        let policy = SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(8),
            crate::NoProgressBudget::bounded(12),
        );
        let head = crate::PersistedSessionConfig::from_policy(
            &policy,
            crate::SessionToolAccess::ambient(),
        );
        for (name, complete) in [
            (
                "policy",
                serde_json::to_value(&policy).expect("serialize complete policy"),
            ),
            (
                "session head",
                serde_json::to_value(&head).expect("serialize complete head"),
            ),
        ] {
            assert_eq!(complete["max_tool_calls"], 8, "{name} records the limit");
            let decode = |value: serde_json::Value| -> Result<(), String> {
                if name == "policy" {
                    serde_json::from_value::<SessionPolicy>(value)
                        .map(drop)
                        .map_err(|error| error.to_string())
                } else {
                    serde_json::from_value::<crate::PersistedSessionConfig>(value)
                        .map(drop)
                        .map_err(|error| error.to_string())
                }
            };
            decode(complete.clone()).expect("the complete record decodes");

            let mut missing = complete.clone();
            missing
                .as_object_mut()
                .expect("the record is a JSON object")
                .remove("max_tool_calls");
            let error = decode(missing).expect_err("a missing max_tool_calls must fail");
            assert!(
                error.contains("missing field `max_tool_calls`"),
                "{name}: the refusal names max_tool_calls: {error}"
            );

            let mut zero = complete;
            zero["max_tool_calls"] = serde_json::json!(0);
            decode(zero).expect_err("a zero max_tool_calls must fail");
        }
    }

    #[test]
    fn session_policy_rejects_missing_turn_budget() {
        let mut value = serde_json::to_value(SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
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

    /// FIG-5431: stored policies and heads require the host's stall choice.
    #[test]
    fn recorded_config_without_no_progress_budget_is_refused() {
        let policy = SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(8),
            crate::NoProgressBudget::Unbounded,
        );
        for (name, mut record) in [
            ("policy", serde_json::to_value(&policy).expect("policy")),
            (
                "head",
                serde_json::to_value(crate::PersistedSessionConfig::from_policy(
                    &policy,
                    crate::SessionToolAccess::ambient(),
                ))
                .expect("head"),
            ),
        ] {
            assert_eq!(record["no_progress_budget"], "unbounded");
            record
                .as_object_mut()
                .expect("record")
                .remove("no_progress_budget");
            let error = if name == "policy" {
                serde_json::from_value::<SessionPolicy>(record)
                    .expect_err("missing stall choice")
                    .to_string()
            } else {
                serde_json::from_value::<crate::PersistedSessionConfig>(record)
                    .expect_err("missing stall choice")
                    .to_string()
            };
            assert!(
                error.contains("missing field `no_progress_budget`"),
                "{name}: {error}"
            );
        }
    }
}
