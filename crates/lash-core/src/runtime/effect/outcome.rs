use crate::SessionId;
use std::sync::Arc;

use crate::LlmResponse;
use crate::llm::transport::LlmTransportError;
use crate::runtime::session_manager::{CurrentSessionCapability, UsageCapability};
use crate::sansio::LlmCallError;
use crate::{LlmRequest as CoreLlmRequest, PluginError, session_model::TokenUsage};

use super::{CausalRef, RuntimeEffectOutcome};

// =============================================================================
// LLM trace helpers
// =============================================================================

pub(crate) fn token_usage_from_llm(usage: &crate::llm::types::LlmUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
    }
}

pub(crate) fn emit_llm_trace_started(
    trace_sink: &Option<Arc<dyn lash_trace::TraceSink>>,
    base_context: &lash_trace::TraceContext,
    context: lash_trace::TraceContext,
    request: &CoreLlmRequest,
    clock: &dyn crate::Clock,
) {
    crate::trace::emit_projected_trace(
        trace_sink,
        base_context,
        context,
        lash_trace::TraceEvent::LlmCallStarted {
            request: crate::trace::trace_llm_request(request),
        },
        clock,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "trace completion carries the explicit sink, scope, outcome, attempt record, and clock"
)]
pub(crate) fn emit_llm_trace_completed(
    trace_sink: &Option<Arc<dyn lash_trace::TraceSink>>,
    base_context: &lash_trace::TraceContext,
    context: lash_trace::TraceContext,
    response: &LlmResponse,
    request_model: &str,
    duration_ms: u64,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
    clock: &dyn crate::Clock,
) {
    super::emit_provider_replay_drops(trace_sink, base_context, &context, call_record, clock);
    crate::trace::emit_projected_trace(
        trace_sink,
        base_context,
        context,
        lash_trace::TraceEvent::LlmCallCompleted {
            response: crate::trace::trace_llm_response(
                response.full_text(),
                duration_ms,
                request_model.to_string(),
                Some(response.terminal_reason),
                crate::trace::trace_output_parts(&response.parts),
                response.generation_disposition,
            ),
            usage: Some(crate::trace::trace_usage_from_llm(&response.usage)),
            provider_usage: response.provider_usage.clone(),
            stream_summary,
            attempts: crate::trace::trace_llm_attempts(call_record),
        },
        clock,
    );
}

pub(crate) struct LlmTraceFailure {
    message: String,
    retryable: bool,
    terminal_reason: crate::LlmTerminalReason,
    code: Option<String>,
    raw: Option<String>,
}

impl LlmTraceFailure {
    pub(crate) fn invalid_structured_output(message: String) -> Self {
        Self {
            message,
            retryable: false,
            terminal_reason: crate::LlmTerminalReason::ProviderError,
            code: Some("invalid_structured_output".to_string()),
            raw: None,
        }
    }
}

impl From<&LlmTransportError> for LlmTraceFailure {
    fn from(err: &LlmTransportError) -> Self {
        Self {
            message: err.message.clone(),
            retryable: err.is_retryable(),
            terminal_reason: err.terminal_reason,
            code: err.code.clone(),
            raw: err.raw.as_deref().cloned(),
        }
    }
}

impl From<&LlmCallError> for LlmTraceFailure {
    fn from(err: &LlmCallError) -> Self {
        Self {
            message: err.message.clone(),
            retryable: err.retryable,
            terminal_reason: err.terminal_reason,
            code: err.code.clone(),
            raw: err.raw.clone(),
        }
    }
}

pub(crate) fn emit_llm_trace_failed(
    trace_sink: &Option<Arc<dyn lash_trace::TraceSink>>,
    base_context: &lash_trace::TraceContext,
    context: lash_trace::TraceContext,
    failure: LlmTraceFailure,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
    clock: &dyn crate::Clock,
) {
    super::emit_provider_replay_drops(trace_sink, base_context, &context, call_record, clock);
    crate::trace::emit_projected_trace(
        trace_sink,
        base_context,
        context,
        lash_trace::TraceEvent::LlmCallFailed {
            error: lash_trace::TraceError {
                message: failure.message,
                retryable: failure.retryable,
                terminal_reason: Some(failure.terminal_reason.code().to_string()),
                code: failure.code,
                raw: failure.raw,
            },
            stream_summary,
            attempts: crate::trace::trace_llm_attempts(call_record),
        },
        clock,
    );
}

pub(crate) fn emit_provider_replay_drops(
    trace_sink: &Option<Arc<dyn lash_trace::TraceSink>>,
    base_context: &lash_trace::TraceContext,
    context: &lash_trace::TraceContext,
    call_record: Option<&crate::LlmCallRecord>,
    clock: &dyn crate::Clock,
) {
    let Some(call_record) = call_record else {
        return;
    };
    for drop in &call_record.replay_drops {
        let route = |route: &crate::ProviderRouteIdentity| lash_trace::TraceProviderRouteIdentity {
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
        crate::trace::emit_projected_trace(
            trace_sink,
            base_context,
            context.clone(),
            lash_trace::TraceEvent::ProviderReplayDropped { event },
            clock,
        );
    }
}

pub(crate) fn llm_call_error_from_transport(err: LlmTransportError) -> LlmCallError {
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

// =============================================================================
// Direct-completion outcome plumbing
// =============================================================================

/// Applies a recorded direct-effect outcome: records usage/trace against the
/// session and yields the raw provider response. Both the text-only
/// (`DirectCompletion`) and full-output (`DirectLlmCompletion`) client methods
/// project from this single result.
#[allow(private_interfaces)]
pub(crate) async fn apply_direct_outcome(
    current: &CurrentSessionCapability,
    usage_capability: &UsageCapability,
    request: &CoreLlmRequest,
    usage_source: &str,
    caused_by: Option<&CausalRef>,
    outcome: RuntimeEffectOutcome,
) -> Result<(LlmResponse, TokenUsage, crate::LlmCallRecord), PluginError> {
    let (result, call_record) = outcome
        .into_direct_response()
        .map_err(|err| PluginError::Session(err.to_string()))?;
    let (response, usage) = apply_direct_llm_result(
        current,
        usage_capability,
        request,
        usage_source,
        &request.model.clone(),
        caused_by,
        result,
        call_record.as_ref(),
    )
    .await?;
    let call_record = call_record.ok_or_else(|| {
        PluginError::Session(
            "direct LLM effect completed without a provider call record".to_string(),
        )
    })?;
    Ok((response, usage, call_record))
}

#[allow(
    clippy::too_many_arguments,
    reason = "direct effect application keeps usage, causal, outcome, and attempt capabilities explicit"
)]
async fn apply_direct_llm_result(
    current: &CurrentSessionCapability,
    usage_capability: &UsageCapability,
    request: &CoreLlmRequest,
    usage_source: &str,
    usage_model: &str,
    caused_by: Option<&CausalRef>,
    result: Result<LlmResponse, LlmCallError>,
    call_record: Option<&crate::LlmCallRecord>,
) -> Result<(LlmResponse, TokenUsage), PluginError> {
    let llm_call_id = emit_direct_llm_trace_started(current, request, caused_by);
    match result {
        Ok(response) => {
            emit_direct_llm_trace_completed(
                current,
                llm_call_id.as_deref(),
                caused_by,
                &response,
                &request.model,
                call_record,
            );
            let usage = token_usage_from_llm(&response.usage);
            // Record into the shared token ledger only. The ledger is the same
            // `Arc` the turn loop drains at turn-commit time (`turn_loop.rs`).
            // This usage is persisted exactly once by the final turn commit.
            // We deliberately do NOT commit here:
            //   * an out-of-band `commit_runtime_state` mid-turn races the
            //     owning turn's CAS and can bump the head from under it;
            //   * on effect-host replay this `apply` runs again with the cached
            //     outcome, and an incremental persist would double-merge the
            //     usage into the already-persisted state. Recording (without
            //     persisting) is replay-safe: it just rebuilds the in-memory
            //     ledger that the single turn-commit drain then persists.
            usage_capability.record_token_usage(usage_source, usage_model, &usage);
            Ok((response, usage))
        }
        Err(err) => {
            emit_direct_llm_trace_failed(
                current,
                llm_call_id.as_deref(),
                caused_by,
                &err,
                call_record,
            );
            Err(PluginError::Session(err.message))
        }
    }
}

fn emit_direct_llm_trace_started(
    current: &CurrentSessionCapability,
    request: &CoreLlmRequest,
    caused_by: Option<&CausalRef>,
) -> Option<String> {
    current.host.core.tracing.trace_sink.as_ref()?;
    let llm_call_id = uuid::Uuid::new_v4().to_string();
    emit_llm_trace_started(
        &current.host.core.tracing.trace_sink,
        &current.host.core.tracing.trace_context,
        direct_trace_context(&current.session_id, Some(&llm_call_id), caused_by),
        request,
        current.host.core.clock.as_ref(),
    );
    Some(llm_call_id)
}

fn emit_direct_llm_trace_completed(
    current: &CurrentSessionCapability,
    llm_call_id: Option<&str>,
    caused_by: Option<&CausalRef>,
    response: &LlmResponse,
    request_model: &str,
    call_record: Option<&crate::LlmCallRecord>,
) {
    let Some(llm_call_id) = llm_call_id else {
        return;
    };
    emit_llm_trace_completed(
        &current.host.core.tracing.trace_sink,
        &current.host.core.tracing.trace_context,
        direct_trace_context(&current.session_id, Some(llm_call_id), caused_by),
        response,
        request_model,
        0,
        None,
        call_record,
        current.host.core.clock.as_ref(),
    );
}

fn emit_direct_llm_trace_failed(
    current: &CurrentSessionCapability,
    llm_call_id: Option<&str>,
    caused_by: Option<&CausalRef>,
    err: &LlmCallError,
    call_record: Option<&crate::LlmCallRecord>,
) {
    let Some(llm_call_id) = llm_call_id else {
        return;
    };
    emit_llm_trace_failed(
        &current.host.core.tracing.trace_sink,
        &current.host.core.tracing.trace_context,
        direct_trace_context(&current.session_id, Some(llm_call_id), caused_by),
        LlmTraceFailure::from(err),
        None,
        call_record,
        current.host.core.clock.as_ref(),
    );
}

fn direct_trace_context(
    session_id: &SessionId,
    llm_call_id: Option<&str>,
    caused_by: Option<&CausalRef>,
) -> lash_trace::TraceContext {
    let mut context = lash_trace::TraceContext::default().for_session(session_id.to_string());
    if let Some(llm_call_id) = llm_call_id {
        context = context.for_llm_call(llm_call_id.to_string());
    }
    if let Some(caused_by) = caused_by {
        context = crate::trace::trace_context_with_causal_ref(context, caused_by);
    }
    context
}

#[cfg(test)]
mod tests {
    use crate::SessionId;
    use crate::TurnId;
    use lash_sansio::sync::MutexExt;
    use std::sync::Arc;

    #[derive(Default)]
    struct RecordingTraceSink(std::sync::Mutex<Vec<lash_trace::TraceRecord>>);

    impl lash_trace::TraceSink for RecordingTraceSink {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            self.0.lock_recover().push(record.clone());
            Ok(())
        }
    }

    fn request() -> crate::LlmRequest {
        crate::LlmRequest {
            instructions: None,
            model: "test/model".to_string(),
            messages: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: crate::llm::types::LlmToolChoice::Auto,
            model_variant: crate::ReasoningSelection::ProviderDefault,
            model_capability: Default::default(),
            generation: Default::default(),
            scope: crate::LlmRequestScope::new("request-session", "frame", "request"),
            output_spec: None,
            stream_events: None,
            provider_trace: None,
        }
    }

    fn projected_context(
        attribution: crate::RuntimeAttribution,
        caused_by: Option<crate::CausalRef>,
    ) -> lash_trace::TraceContext {
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                crate::ExecutionScope::process("trace-process"),
                "trace-effect",
            )
            .expect("valid trace address"),
            attribution,
            "trace-effect",
        )
        .with_caused_by(caused_by);
        crate::trace::trace_context_from_effect_invocation(&invocation).for_llm_call("llm-witness")
    }

    #[test]
    fn emitted_llm_records_keep_authoritative_projection_and_parent_precedence() {
        let sink = Arc::new(RecordingTraceSink::default());
        let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
        let mut base = lash_trace::TraceContext::default()
            .for_session("ambient-session")
            .for_turn("ambient-turn")
            .for_turn_index(99)
            .for_protocol_iteration(42);
        base.run_id = Some("host-run".to_string());
        base.parent_graph_node_id = Some("host:explicit-parent".to_string());
        base.metadata
            .insert("host_key".to_string(), serde_json::json!("kept"));
        let cause = crate::CausalRef::Effect {
            address: crate::EffectAddress::new(
                crate::ExecutionScope::process("cause-process"),
                "cause-effect",
            )
            .expect("valid cause address"),
        };
        let context = projected_context(crate::RuntimeAttribution::none(), Some(cause));

        super::emit_llm_trace_started(
            &Some(Arc::clone(&sink_dyn)),
            &base,
            context.clone(),
            &request(),
            &crate::SystemClock,
        );
        super::emit_llm_trace_completed(
            &Some(Arc::clone(&sink_dyn)),
            &base,
            context.clone(),
            &crate::LlmResponse::default(),
            "test/model",
            1,
            None,
            None,
            &crate::SystemClock,
        );
        super::emit_llm_trace_failed(
            &Some(sink_dyn),
            &base,
            context,
            super::LlmTraceFailure::invalid_structured_output("invalid".to_string()),
            None,
            None,
            &crate::SystemClock,
        );

        let records = sink.0.lock_recover();
        assert_eq!(records.len(), 3);
        assert!(
            records
                .iter()
                .all(|record| record.context.session_id.is_none())
        );
        assert!(
            records
                .iter()
                .all(|record| record.context.turn_id.is_none())
        );
        assert!(
            records
                .iter()
                .all(|record| record.context.turn_index.is_none())
        );
        assert!(
            records
                .iter()
                .all(|record| record.context.protocol_iteration.is_none())
        );
        assert!(records.iter().all(|record| {
            record.context.parent_graph_node_id.as_deref() == Some("host:explicit-parent")
        }));
        assert!(records.iter().all(|record| {
            record.context.run_id.as_deref() == Some("host-run")
                && record.context.metadata.get("host_key") == Some(&serde_json::json!("kept"))
        }));
    }

    #[test]
    fn emitted_llm_parent_falls_back_from_full_cause_to_actual_turn() {
        let sink = Arc::new(RecordingTraceSink::default());
        let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
        let cause_address = crate::EffectAddress::new(
            crate::ExecutionScope::process("cause-process"),
            "cause-effect",
        )
        .expect("valid cause address");
        super::emit_llm_trace_started(
            &Some(Arc::clone(&sink_dyn)),
            &lash_trace::TraceContext::default(),
            projected_context(
                crate::RuntimeAttribution::for_turn("actual-session", "actual-turn", 3, 1),
                Some(crate::CausalRef::Effect {
                    address: cause_address.clone(),
                }),
            ),
            &request(),
            &crate::SystemClock,
        );
        super::emit_llm_trace_started(
            &Some(sink_dyn),
            &lash_trace::TraceContext::default(),
            projected_context(
                crate::RuntimeAttribution::for_turn("actual-session", "actual-turn", 3, 1),
                None,
            ),
            &request(),
            &crate::SystemClock,
        );

        let records = sink.0.lock_recover();
        assert_eq!(
            records[0].context.parent_graph_node_id.as_deref(),
            Some(cause_address.graph_key().as_str())
        );
        assert_eq!(
            records[1].context.parent_graph_node_id.as_deref(),
            Some("turn:actual-session:actual-turn")
        );
    }

    #[test]
    fn emitted_direct_llm_records_preserve_full_cause_and_explicit_parent() {
        let sink = Arc::new(RecordingTraceSink::default());
        let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
        let session_id = SessionId::from("direct-session");
        let effect_address = crate::EffectAddress::new(
            crate::ExecutionScope::process("direct-parent-process"),
            "direct-parent-effect",
        )
        .expect("valid direct parent address");
        let effect_cause = crate::CausalRef::Effect {
            address: effect_address.clone(),
        };
        let trigger_cause = crate::CausalRef::TriggerOccurrence {
            occurrence_id: "direct-occurrence".to_string(),
            subscription_id: Some("direct-subscription".to_string()),
            subscription_incarnation: Some("direct-incarnation".to_string()),
            subscription_revision: Some(9),
        };

        super::emit_llm_trace_started(
            &Some(Arc::clone(&sink_dyn)),
            &lash_trace::TraceContext::default(),
            super::direct_trace_context(&session_id, Some("direct-start"), Some(&effect_cause)),
            &request(),
            &crate::SystemClock,
        );
        super::emit_llm_trace_completed(
            &Some(Arc::clone(&sink_dyn)),
            &lash_trace::TraceContext::default(),
            super::direct_trace_context(
                &session_id,
                Some("direct-completed"),
                Some(&trigger_cause),
            ),
            &crate::LlmResponse::default(),
            "test/model",
            1,
            None,
            None,
            &crate::SystemClock,
        );
        let explicit_base = lash_trace::TraceContext {
            parent_graph_node_id: Some("host:explicit-parent".to_string()),
            ..Default::default()
        };
        super::emit_llm_trace_failed(
            &Some(sink_dyn),
            &explicit_base,
            super::direct_trace_context(&session_id, Some("direct-failed"), Some(&effect_cause)),
            super::LlmTraceFailure::invalid_structured_output("invalid".to_string()),
            None,
            None,
            &crate::SystemClock,
        );

        let records = sink.0.lock_recover();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0].context.parent_graph_node_id.as_deref(),
            Some(effect_address.graph_key().as_str())
        );
        assert_eq!(
            records[1].context.parent_graph_node_id.as_deref(),
            Some(
                format!(
                    "trigger:{}",
                    serde_json::to_string(&trigger_cause).expect("trigger cause serializes")
                )
                .as_str()
            )
        );
        assert_eq!(
            records[2].context.parent_graph_node_id.as_deref(),
            Some("host:explicit-parent")
        );
        assert!(records.iter().all(|record| {
            record.context.session_id.as_deref() == Some("direct-session")
                && record.context.turn_id.is_none()
                && record.context.turn_index.is_none()
                && record.context.protocol_iteration.is_none()
        }));
    }

    #[test]
    fn direct_effect_invocation_preserves_runtime_scope() {
        let invocation = crate::runtime::causal::direct_effect_invocation(
            &crate::ExecutionScope::runtime_operation("direct-test"),
            &SessionId::from("s"),
            "tool",
            "request:k".to_string(),
            None,
            None,
        );

        assert_eq!(invocation.attribution.session_id.as_deref(), Some("s"));
        assert!(invocation.replay_key().starts_with("direct:v2:blake3:"));
    }

    #[test]
    fn tool_retry_sleep_invocation_preserves_parent_replay_identity() {
        let parent = crate::runtime::causal::direct_effect_invocation(
            &crate::ExecutionScope::turn("s", "turn"),
            &SessionId::from("s"),
            "tool",
            "request:k".to_string(),
            Some(&TurnId::from("turn")),
            None,
        );

        let sleep = crate::runtime::causal::tool_retry_sleep_invocation(
            &crate::ExecutionScope::turn("s", "turn"),
            &parent.into_runtime_invocation(),
            "probe",
            2,
        );

        assert!(sleep.replay_key().ends_with(":probe:attempt:2:sleep"));
    }
}
