//! The host's one telemetry content policy, and the omission it applies.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    TraceEffectEnvelopeDiffValue, TraceEvent, TraceLanguageExecutionFailure,
    TraceLanguageExecutionPayload, TraceProgramStepOutcome, TraceProviderBodyOmission,
    TraceRetryAttempt, TraceRetryAttemptDetail, TraceRuntimeStreamPayload, TraceToolAttemptOutcome,
    TraceToolCallOutcome, TraceToolSpec,
};
use lash_sansio::llm::types::StreamBlockEvent;

/// Whether built-in telemetry carries content.
///
/// Content is what a session says, as opposed to what it did: prompts and
/// model responses, rendered instructions and tool contracts, tool arguments
/// and results, executed code and its output, raw provider payloads, and
/// diagnostic or provider text. One host choice governs all of it on every
/// built-in telemetry path: the records every [`TraceSink`](crate::TraceSink)
/// receives (JSONL, stderr, a tee, a host's own sink) and the adapter's
/// projection of them.
///
/// The policy is consent, not detail: [`TraceLevel`](crate::TraceLevel) still
/// chooses which records exist, and byte bounds still cut what is captured.
/// It governs telemetry only. Durable requests and results, session history,
/// product observations and the app's own responses keep their contracts.
///
/// Opaque [`TraceEvent::Custom`] payloads are plugin-authored (or host-authored)
/// output. Lash does not inspect or classify them. A plugin must honour the
/// policy for content it includes in custom payloads; its factory reads the
/// current deployment policy through
/// `lash::plugins::PluginSessionContext::telemetry_content()`. This operational
/// privacy setting follows the receiving host on reopen, rather than a
/// session's recorded execution config.
///
/// A host has the final say over custom payloads at its exporter: install a
/// wrapping [`TraceSink`](crate::TraceSink) through
/// `LashCoreBuilder::trace_sink` that drops custom records or clones and
/// redacts their payloads before forwarding them to the export sink. The
/// wrapper should also forward [`TraceSink::flush`](crate::TraceSink::flush).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryContent {
    /// Records keep identities, statuses, counts, hashes and durations.
    /// Content fields are empty, and [`TraceRecord::content`] says so.
    ///
    /// [`TraceRecord::content`]: crate::TraceRecord::content
    #[default]
    Omitted,
    /// Records carry the original text within the documented bounds. Nothing
    /// is scrubbed.
    Captured,
}

impl TelemetryContent {
    /// Standard preset: omitted. A host opts in to content capture.
    pub const fn standard() -> Self {
        Self::Omitted
    }

    pub const fn is_captured(self) -> bool {
        matches!(self, Self::Captured)
    }

    /// Builds a content value under consent, and its empty value otherwise:
    /// an omitted payload is never cloned, rendered or serialized.
    pub fn capture<T: Default>(self, build: impl FnOnce() -> T) -> T {
        match self {
            Self::Captured => build(),
            Self::Omitted => T::default(),
        }
    }
}

/// The omission reason written beside a payload that has its own reason
/// field: the host's content policy withheld it.
pub const CONTENT_POLICY_OMISSION: &str = "content_policy";

impl TraceToolSpec {
    fn omit_content(&mut self) {
        self.description.clear();
        self.input_schema = Value::Null;
        self.output_schema = Value::Null;
    }
}

impl TraceEffectEnvelopeDiffValue {
    fn omit_content(&mut self) {
        match self {
            Self::Missing => {}
            Self::Present {
                value_json,
                value_json_omitted_reason,
                ..
            } => {
                if value_json.take().is_some() {
                    *value_json_omitted_reason = Some(CONTENT_POLICY_OMISSION.to_owned());
                }
            }
        }
    }
}

/// A failed or cancelled tool outcome is the runtime's failure or
/// cancellation record: its typed classification stays, and its message,
/// cause detail and raw tool value go.
fn omit_outcome_text(payload: &mut Value) {
    const CLASSIFICATION: [&str; 6] = [
        "class",
        "code",
        "source",
        "suggested_delay_ms",
        "origin",
        "forced",
    ];
    match payload {
        Value::Object(fields) => fields.retain(|name, _| CLASSIFICATION.contains(&name.as_str())),
        other => *other = Value::Null,
    }
}

fn omit_attempt_content(attempts: &mut Option<Vec<TraceRetryAttempt>>) {
    for attempt in attempts.iter_mut().flatten() {
        match &mut attempt.detail {
            TraceRetryAttemptDetail::Tool { outcome } => match outcome {
                TraceToolAttemptOutcome::Completed => {}
                TraceToolAttemptOutcome::Failed { message, .. }
                | TraceToolAttemptOutcome::Cancelled { message, .. } => message.clear(),
            },
        }
    }
}

impl TraceEvent {
    /// Empties every content field of this event, leaving its identities,
    /// statuses, counts, hashes and durations.
    ///
    /// Every variant is named, so a new one cannot be added without deciding
    /// what in it is content.
    pub fn omit_content(&mut self) {
        match self {
            Self::AttachmentDegraded { label, .. } => *label = None,
            Self::CompositionChanged {
                rendered_system_prompt,
                tool_schemas,
                ..
            } => {
                rendered_system_prompt.clear();
                tool_schemas.iter_mut().for_each(TraceToolSpec::omit_content);
            }
            Self::LlmCallStarted { request } => {
                request.messages.clear();
                request
                    .tools
                    .iter_mut()
                    .for_each(TraceToolSpec::omit_content);
                request.output_spec = None;
            }
            Self::LlmCallCompleted { response, .. } => {
                response.text.clear();
                response.parts = None;
            }
            Self::ProviderEvent { event } => {
                if event.raw_json.take().is_some() {
                    event.raw_json_omitted_reason = Some(TraceProviderBodyOmission::ContentPolicy);
                }
            }
            Self::EffectEnvelopeDiff { event } => {
                for entry in &mut event.divergent_paths {
                    entry.recorded.omit_content();
                    entry.reconstructed.omit_content();
                }
            }
            Self::RuntimeStreamEvent { event } => match &mut event.payload {
                TraceRuntimeStreamPayload::Block { event, raw_text } => {
                    *raw_text = None;
                    match event {
                        StreamBlockEvent::Started { .. } => {}
                        StreamBlockEvent::Delta { text, .. }
                        | StreamBlockEvent::Completed { text, .. } => text.clear(),
                    }
                }
                TraceRuntimeStreamPayload::TextPart { text, .. }
                | TraceRuntimeStreamPayload::ReasoningPart { text, .. } => text.clear(),
                TraceRuntimeStreamPayload::ToolCallPart { input_json, .. } => {
                    *input_json = Value::Null;
                }
                TraceRuntimeStreamPayload::Usage { .. } => {}
            },
            Self::ToolCallStarted { args, .. } => *args = Value::Null,
            Self::ToolCallCompleted {
                args,
                output,
                attempts,
                ..
            } => {
                *args = Value::Null;
                match &mut output.outcome {
                    TraceToolCallOutcome::Success(payload) => *payload = Value::Null,
                    TraceToolCallOutcome::Failure(payload)
                    | TraceToolCallOutcome::Cancelled(payload) => omit_outcome_text(payload),
                }
                output.control = None;
                omit_attempt_content(attempts);
            }
            Self::ExecCodeStarted { code, .. } => code.clear(),
            Self::ExecCodeCompleted {
                output,
                error,
                terminal_finish,
                ..
            } => {
                output.clear();
                if let Some(failure) = error {
                    failure.message.clear();
                    // Validator text quotes the value or schema it refused.
                    if let Some(mismatch) = &mut failure.value_mismatch {
                        mismatch.message.clear();
                    }
                    if let Some(lash_sansio::SchemaAdmissionError::Compilation { message, .. }) =
                        failure.schema_admission.as_deref_mut()
                    {
                        message.clear();
                    }
                }
                *terminal_finish = None;
            }
            Self::ExecCodeFailed { error, .. } => error.clear(),
            Self::StoreErrorObserved { message, .. } => message.clear(),
            Self::ProgramStep { outcome, .. } => match outcome {
                TraceProgramStepOutcome::Ok => {}
                TraceProgramStepOutcome::Failure { diagnostic } => diagnostic.clear(),
            },
            // A protocol step projects a session history record.
            Self::ProtocolStep { payload, .. } => *payload = Value::Null,
            Self::LanguageExecution { event, .. } => match &mut event.payload {
                TraceLanguageExecutionPayload::ExecutionFinished { error, .. } => *error = None,
                TraceLanguageExecutionPayload::NodeFailed { failure, .. } => match failure {
                    TraceLanguageExecutionFailure::Effect { message, .. }
                    | TraceLanguageExecutionFailure::Runtime { message, .. } => message.clear(),
                },
                TraceLanguageExecutionPayload::ExecutionStarted { .. }
                | TraceLanguageExecutionPayload::NodeStarted { .. }
                | TraceLanguageExecutionPayload::NodeWaiting { .. }
                | TraceLanguageExecutionPayload::NodeResumed { .. }
                | TraceLanguageExecutionPayload::NodeCancelled { .. }
                | TraceLanguageExecutionPayload::NodeCompleted { .. }
                | TraceLanguageExecutionPayload::BranchSelected { .. }
                | TraceLanguageExecutionPayload::ChildStarted { .. } => {}
            },
            // Typed identities, counts, hashes, statuses and classes only: a
            // model attempt is its sealed record, whose error is a class, a
            // code and a status.
            Self::TurnStarted { .. }
            | Self::PromptBuilt { .. }
            | Self::PromptCompositionFailed { .. }
            | Self::CompactionNeeded { .. }
            | Self::CompactionStarted { .. }
            | Self::CompactionCompleted { .. }
            | Self::PromptViewAttachmentsPruned { .. }
            | Self::LlmCallFailed { .. }
            | Self::LlmAttemptCompleted { .. }
            | Self::DomainCompleted { .. }
            | Self::ProviderReplayDropped { .. }
            | Self::ToolReceipt { .. }
            | Self::StepBodyStarted { .. }
            | Self::ToolCheckConflict { .. }
            | Self::ObservationProjection { .. }
            | Self::JournaledEffectStarted { .. }
            | Self::JournaledEffectSettled { .. }
            | Self::DurableWaitParked { .. }
            | Self::DurableWaitResolved { .. }
            | Self::DurableTimerStarted { .. }
            | Self::DurableTimerResolved { .. }
            | Self::TurnCompleted { .. }
            // The producer's own structured evidence (`TelemetryContent`).
            | Self::Custom { .. } => {}
        }
    }
}
