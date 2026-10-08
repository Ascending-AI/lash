use crate::LlmResponse;
use crate::llm::transport::LlmTransportError;
use crate::sansio::LlmCallError;
use crate::{LlmRequest as CoreLlmRequest, session_model::TokenUsage};

use super::CausalRef;

// =============================================================================
// LLM trace helpers
// =============================================================================

pub fn token_usage_from_llm(usage: &crate::llm::types::LlmUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
    }
}

pub fn emit_llm_trace_started(
    standing: &crate::trace::TraceStanding,
    context: lash_trace::TraceContext,
    request: &CoreLlmRequest,
) {
    standing.observe(|| {
        (
            context,
            lash_trace::TraceEvent::LlmCallStarted {
                request: crate::trace::trace_llm_request(request),
            },
        )
    });
}

pub fn emit_llm_trace_completed(
    standing: &crate::trace::TraceStanding,
    context: lash_trace::TraceContext,
    response: &LlmResponse,
    request_model: &str,
    duration_ms: u64,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
) {
    super::emit_provider_replay_drops(standing, &context, call_record);
    standing.observe(|| {
        (
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

pub fn emit_llm_trace_failed(
    standing: &crate::trace::TraceStanding,
    context: lash_trace::TraceContext,
    failure: LlmTraceFailure,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
) {
    super::emit_provider_replay_drops(standing, &context, call_record);
    standing.observe(|| {
        (
            context,
            lash_trace::TraceEvent::LlmCallFailed {
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
    if let Some(caused_by) = caused_by {
        context = crate::trace::trace_context_with_causal_ref(context, caused_by);
    }
    context
}

#[cfg(test)]
mod tests {
    use crate::SessionId;
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

    fn standing(
        sink: Arc<dyn lash_trace::TraceSink>,
        base: &lash_trace::TraceContext,
    ) -> crate::trace::TraceStanding {
        crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock))
            .with_trace_sink(sink)
            .with_base_context(base.clone())
            .unreplayed(None)
    }

    fn request() -> crate::LlmRequest {
        crate::LlmRequest {
            instructions: None,
            model: lash_sansio::llm_profile::LlmProfileConfig::new(
                lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                    lash_sansio::llm_profile::LlmProfileMetadata::builder("test/model".to_string())
                        .context_window_tokens(128_000)
                        .capability(Default::default())
                        .extra_body(Default::default())
                        .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                        .build()
                        .expect("valid profile"),
                ),
            )
            .with_reasoning(crate::ReasoningSelection::ProviderDefault),
            messages: Vec::new(),
            tools: Arc::new(Vec::new()),
            tool_choice: crate::llm::types::LlmToolChoice::Auto,
            attachment_acceptance: Default::default(),
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
                crate::ExecutionScope::process(crate::process_id_for_test("trace-process")),
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
                crate::ExecutionScope::process(crate::process_id_for_test("cause-process")),
                "cause-effect",
            )
            .expect("valid cause address"),
        };
        let context = projected_context(crate::RuntimeAttribution::none(), Some(cause));

        super::emit_llm_trace_started(
            &standing(Arc::clone(&sink_dyn), &base),
            context.clone(),
            &request(),
        );
        super::emit_llm_trace_completed(
            &standing(Arc::clone(&sink_dyn), &base),
            context.clone(),
            &crate::LlmResponse::default(),
            "test/model",
            1,
            None,
            None,
        );
        super::emit_llm_trace_failed(
            &standing(sink_dyn, &base),
            context,
            super::LlmTraceFailure::invalid_structured_output(),
            None,
            None,
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
            crate::ExecutionScope::process(crate::process_id_for_test("cause-process")),
            "cause-effect",
        )
        .expect("valid cause address");
        super::emit_llm_trace_started(
            &standing(Arc::clone(&sink_dyn), &lash_trace::TraceContext::default()),
            projected_context(
                crate::RuntimeAttribution::for_turn("actual-session", "actual-turn", 3, 1),
                Some(crate::CausalRef::Effect {
                    address: cause_address.clone(),
                }),
            ),
            &request(),
        );
        super::emit_llm_trace_started(
            &standing(sink_dyn, &lash_trace::TraceContext::default()),
            projected_context(
                crate::RuntimeAttribution::for_turn("actual-session", "actual-turn", 3, 1),
                None,
            ),
            &request(),
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
            crate::ExecutionScope::process(crate::process_id_for_test("direct-parent-process")),
            "direct-parent-effect",
        )
        .expect("valid direct parent address");
        let effect_cause = crate::CausalRef::Effect {
            address: effect_address.clone(),
        };
        let event_process = crate::process_id_for_test("direct-event-process");
        let event_cause = crate::CausalRef::ProcessEvent {
            process_id: event_process.clone(),
            sequence: 9,
        };

        super::emit_llm_trace_started(
            &standing(Arc::clone(&sink_dyn), &lash_trace::TraceContext::default()),
            super::direct_trace_context(
                &crate::RuntimeOwner::Session(session_id.clone()),
                Some("direct-start"),
                Some(&effect_cause),
            ),
            &request(),
        );
        super::emit_llm_trace_completed(
            &standing(Arc::clone(&sink_dyn), &lash_trace::TraceContext::default()),
            super::direct_trace_context(
                &crate::RuntimeOwner::Session(session_id.clone()),
                Some("direct-completed"),
                Some(&event_cause),
            ),
            &crate::LlmResponse::default(),
            "test/model",
            1,
            None,
            None,
        );
        let explicit_base = lash_trace::TraceContext {
            parent_graph_node_id: Some("host:explicit-parent".to_string()),
            ..Default::default()
        };
        super::emit_llm_trace_failed(
            &standing(sink_dyn, &explicit_base),
            super::direct_trace_context(
                &crate::RuntimeOwner::Session(session_id.clone()),
                Some("direct-failed"),
                Some(&effect_cause),
            ),
            super::LlmTraceFailure::invalid_structured_output(),
            None,
            None,
        );

        let records = sink.0.lock_recover();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0].context.parent_graph_node_id.as_deref(),
            Some(effect_address.graph_key().as_str())
        );
        assert_eq!(
            records[1].context.parent_graph_node_id.as_deref(),
            Some(format!("process:{event_process}:9").as_str())
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
}
