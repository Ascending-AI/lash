use crate::LlmRequest as CoreLlmRequest;
use crate::LlmResponse;
use crate::llm::transport::LlmTransportError;
use crate::sansio::LlmCallError;

use super::CausalRef;

// =============================================================================
// LLM trace helpers
// =============================================================================

/// Opens one logical model call's `chat` leaf under `standing`'s
/// container: its id is drawn now, its attempts and provider events are its
/// events, and its completion or failure closes it.
pub fn emit_llm_trace_started(
    standing: &crate::trace::TraceStanding,
    context: lash_trace::TraceContext,
    request: &CoreLlmRequest,
    provider: &str,
) -> crate::trace::OpenSpan {
    let standing = standing.selected(lash_trace::TraceSpanKind::Model, &context.ids());
    let fingerprints = standing
        .runtime()
        .composition_tool_fingerprints(&request.tools);
    let fingerprint = crate::trace::trace_composition_key(request, &fingerprints)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let retained = standing
        .is_observed()
        .then(|| crate::trace::trace_llm_request(request, standing.content(), fingerprint.clone()));
    let mut call = standing.open(lash_trace::TraceSpanKind::Model, || {
        (
            context,
            lash_trace::TraceEvent::LlmCallStarted {
                request: crate::trace::trace_llm_request(
                    request,
                    lash_trace::ContentPolicy::omitted(),
                    fingerprint,
                ),
            },
        )
    });
    call.request = retained;
    call.with_model(|| lash_trace::TraceModelCall {
        provider: Some(provider.to_string()),
        request_model: request.model.wire_model().to_string(),
        stream: request.stream_events.is_some(),
        reasoning: match &request.model.reasoning {
            crate::ReasoningSelection::ProviderDefault => None,
            crate::ReasoningSelection::Disabled => Some("disabled".into()),
            crate::ReasoningSelection::Effort(effort) => Some(effort.clone()),
        },
        output_type: request.output_spec.as_ref().map(|_| "json".into()),
        generation: request.generation.clone(),
    })
}

/// Closes `call`, the model call [`emit_llm_trace_started`] opened, with its
/// response.
pub fn emit_llm_trace_completed(
    mut call: crate::trace::OpenSpan,
    context: lash_trace::TraceContext,
    response: &LlmResponse,
    request_model: &str,
    duration_ms: u64,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
) {
    super::emit_provider_replay_drops(call.standing(), &context, call_record);
    call.select_record(&context.ids());
    let content = call.standing().content();
    let request = call.request.take().unwrap_or_default();
    let parts = content.capture(lash_trace::ContentCategory::OutputMessages, || {
        response
            .parts
            .iter()
            .filter(|part| match part {
                crate::LlmOutputPart::Reasoning { replay, .. } => {
                    content.captures(lash_trace::ContentCategory::Reasoning)
                        && !replay.as_ref().is_some_and(|replay| {
                            replay.redacted || replay.encrypted_content.is_some()
                        })
                }
                _ => true,
            })
            .map(crate::LlmOutputPart::without_replay_payloads)
            .collect::<Vec<_>>()
    });
    call.close(|| {
        (
            context,
            lash_trace::TraceEvent::LlmCallCompleted {
                model: None,
                request,
                response_metadata: content
                    .capture(lash_trace::ContentCategory::ResponseMetadata, || {
                        response.response_metadata.clone()
                    }),
                response: crate::trace::trace_llm_response(
                    content.capture(lash_trace::ContentCategory::OutputMessages, || {
                        response.full_text()
                    }),
                    duration_ms,
                    request_model.to_string(),
                    Some(response.terminal_reason),
                    &parts,
                    response.generation_disposition,
                ),
                usage: Some(response.usage.clone()),
                provider_usage: response.provider_usage.clone(),
                stream_summary,
                attempts: crate::trace::trace_llm_attempts(call_record),
            },
        )
    });
}

pub struct LlmTraceFailure {
    retryable: bool,
    terminal_reason: crate::LlmTerminalReason,
    /// The transport's failure classification — what OTel `error.type`
    /// projects. `ProviderFailureKind::Unknown` projects as `_OTHER`.
    kind: crate::ProviderFailureKind,
    /// The namespaced failure code. OTel `lash.error.code` projects the
    /// spelling alone; the namespace travels beside it.
    code: Option<crate::FailureCode>,
}

impl LlmTraceFailure {
    pub(crate) fn invalid_structured_output() -> Self {
        Self {
            retryable: false,
            terminal_reason: crate::LlmTerminalReason::ProviderError,
            kind: crate::ProviderFailureKind::Unknown,
            code: Some(crate::FailureCode::lash(
                crate::TurnFailureCode::InvalidStructuredOutput,
            )),
        }
    }
}

impl From<&LlmTransportError> for LlmTraceFailure {
    fn from(err: &LlmTransportError) -> Self {
        Self {
            retryable: err.is_retryable(),
            terminal_reason: err.terminal_reason,
            kind: err.kind,
            code: err.code.clone(),
        }
    }
}

impl From<&LlmCallError> for LlmTraceFailure {
    fn from(err: &LlmCallError) -> Self {
        Self {
            retryable: err.retryable,
            terminal_reason: err.terminal_reason,
            kind: err.kind,
            code: err.code.clone(),
        }
    }
}

/// Closes `call`, the model call [`emit_llm_trace_started`] opened, with its
/// failure.
pub fn emit_llm_trace_failed(
    mut call: crate::trace::OpenSpan,
    context: lash_trace::TraceContext,
    failure: LlmTraceFailure,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
) {
    super::emit_provider_replay_drops(call.standing(), &context, call_record);
    call.select_record(&context.ids());
    let request = call.request.take().unwrap_or_default();
    call.close(|| {
        (
            context,
            lash_trace::TraceEvent::LlmCallFailed {
                model: None,
                request,
                error: lash_trace::TraceError {
                    retryable: failure.retryable,
                    terminal_reason: failure.terminal_reason,
                    failure_kind: failure.kind,
                    code: failure.code,
                },
                stream_summary,
                attempts: crate::trace::trace_llm_attempts(call_record),
            },
        )
    });
}

pub fn emit_provider_replay_drops(
    standing: &crate::trace::TraceStanding,
    context: &lash_trace::TraceContext,
    call_record: Option<&crate::LlmCallRecord>,
) {
    let Some(call_record) = call_record else {
        return;
    };
    for drop in &call_record.replay_drops {
        standing.observe(|| {
            let route =
                |route: &crate::ProviderRouteIdentity| lash_trace::TraceProviderRouteIdentity {
                    provider: route.provider.to_string(),
                    endpoint: route.endpoint.to_string(),
                    model: route.model.to_string(),
                };
            let event = lash_trace::TraceProviderReplayDropEvent {
                replay_kind: match drop.kind {
                    crate::llm::types::ProviderReplayKind::ResponseText => {
                        lash_trace::TraceProviderReplayKind::ResponseText
                    }
                    crate::llm::types::ProviderReplayKind::Reasoning => {
                        lash_trace::TraceProviderReplayKind::Reasoning
                    }
                    crate::llm::types::ProviderReplayKind::ToolCall => {
                        lash_trace::TraceProviderReplayKind::ToolCall
                    }
                },
                reason: match drop.reason {
                    crate::llm::types::ProviderReplayDropReason::Unstamped => {
                        lash_trace::TraceProviderReplayDropReason::Unstamped
                    }
                    crate::llm::types::ProviderReplayDropReason::ForeignRoute => {
                        lash_trace::TraceProviderReplayDropReason::ForeignRoute
                    }
                },
                minting_route: drop.minting_route.as_ref().map(route),
                serving_route: route(&drop.serving_route),
            };
            (
                context.clone(),
                lash_trace::TraceEvent::ProviderReplayDropped { event },
            )
        });
    }
}

pub fn llm_call_error_from_transport(err: LlmTransportError) -> LlmCallError {
    let retryable = err.is_retryable();
    LlmCallError {
        message: err.message,
        retryable,
        kind: err.kind,
        raw: err.raw.map(|raw| *raw),
        code: err.code,
        terminal_reason: err.terminal_reason,
        request_body: err.request_body.map(|body| *body),
        partial_response: err.partial_response,
    }
}

pub fn direct_trace_context(
    owner: &crate::RuntimeOwner,
    llm_call_id: Option<&str>,
    caused_by: Option<&CausalRef>,
) -> lash_trace::TraceContext {
    let mut context = crate::plugin::owner_trace_context(owner);
    if let Some(llm_call_id) = llm_call_id {
        context = context.for_llm_call(llm_call_id.to_string());
    }
    context.caused_by = caused_by.cloned();
    context
}
