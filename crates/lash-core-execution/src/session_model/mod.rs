pub use lash_core_store::session_policy::*;
pub mod context;
pub use lash_protocol_types::session_model::message;

use crate::llm::types::{LlmEventSender, LlmStreamEvent};

use lash_core_llm::provider::ProviderHandle;

use crate::LlmProfileConfig;

use lash_core_llm::provider::LlmProfileUnavailable;
use lash_core_llm::provider::LlmProfiles;

/// Formats a tool output for host display and transcripts only.
/// Model prompts use the mode's presenter and the provider's error marking.
pub fn format_tool_output_content(output: &crate::ToolCallOutput) -> String {
    match &output.outcome {
        lash_protocol_types::ToolCallOutcome::Success(_) => match output.value_for_projection() {
            serde_json::Value::String(text) => text,
            other => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()),
        },
        lash_protocol_types::ToolCallOutcome::Failure(failure) => {
            let message = failure.model_text();
            if message.is_empty() {
                "[Tool execution failed]".into()
            } else {
                format!("[Tool execution failed]\n{message}")
            }
        }
        lash_protocol_types::ToolCallOutcome::Cancelled(cancellation) => {
            if cancellation.message.is_empty() {
                "[Tool execution cancelled]".into()
            } else {
                format!("[Tool execution cancelled]\n{}", cancellation.message)
            }
        }
    }
}
pub use lash_protocol_types::{
    session_model::ConversationRecord, session_model::ErrorEnvelope, session_model::FailureCode,
    session_model::LlmUsage, session_model::MaxToolCalls, session_model::Message,
    session_model::MessageRole, session_model::Namespace, session_model::NoProgressBudget,
    session_model::Part, session_model::PartKind, session_model::ProtocolEvent,
    session_model::ProtocolPayload, session_model::RuntimeOutputCuts,
    session_model::SessionStreamEvent, session_model::StreamMessageKind,
    session_model::TokenUsageOverflow, session_model::ToolCallLimitExceeded,
    session_model::ToolCallLimitScope, session_model::TurnBudget, session_model::TurnFailureCode,
    session_model::TurnFailureKind, session_model::make_error_envelope,
    session_model::make_error_event, session_model::reassign_part_ids,
    session_model::render_prompt, session_model::shared_parts,
};

pub type SessionHistoryRecord =
    lash_protocol_types::session_model::SessionHistoryRecord<ProtocolEvent>;

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
    let stamp = <Stamp as serde::Deserialize>::deserialize(event.payload.value())
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
    <PersistedPluginRuntimeEvent as serde::Deserialize>::deserialize(event.payload.value())
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
            lash_core_store::runtime_error::RuntimeEffectControllerError::llm_profile_unavailable(
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
        let head = crate::PersistedSessionConfig::from_policy(&policy);
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
                serde_json::to_value(crate::PersistedSessionConfig::from_policy(&policy))
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
