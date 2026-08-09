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
    crate::trace::emit_trace(
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
    duration_ms: u64,
    stream_summary: Option<serde_json::Value>,
    call_record: Option<&crate::LlmCallRecord>,
    clock: &dyn crate::Clock,
) {
    crate::trace::emit_trace(
        trace_sink,
        base_context,
        context,
        lash_trace::TraceEvent::LlmCallCompleted {
            response: crate::trace::trace_llm_response(
                response.full_text.clone(),
                duration_ms,
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

impl From<&LlmTransportError> for LlmTraceFailure {
    fn from(err: &LlmTransportError) -> Self {
        Self {
            message: err.message.clone(),
            retryable: err.retryable,
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
    crate::trace::emit_trace(
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

pub(crate) fn llm_call_error_from_transport(err: LlmTransportError) -> LlmCallError {
    LlmCallError {
        message: err.message,
        retryable: err.retryable,
        kind: err.kind,
        raw: err.raw.map(|raw| *raw),
        code: err.code,
        terminal_reason: err.terminal_reason,
        request_body: err.request_body,
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
    session_id: &str,
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
    use crate::RuntimeEffectKind;

    #[test]
    fn direct_effect_invocation_preserves_runtime_scope() {
        let invocation = crate::runtime::causal::direct_effect_invocation(
            "s",
            "tool",
            "request:k".to_string(),
            None,
            None,
        );

        assert_eq!(invocation.scope.session_id, "s");
        assert_eq!(invocation.effect_kind(), Some(RuntimeEffectKind::Direct));
        assert!(
            invocation
                .replay_key()
                .expect("replay key")
                .starts_with("direct:v2:sha256:")
        );
    }

    #[test]
    fn tool_retry_sleep_invocation_preserves_parent_replay_identity() {
        let parent = crate::runtime::causal::direct_effect_invocation(
            "s",
            "tool",
            "request:k".to_string(),
            Some("turn"),
            None,
        );

        let sleep = crate::runtime::causal::tool_retry_sleep_invocation(&parent, "probe", 2);

        assert_eq!(sleep.effect_kind(), Some(RuntimeEffectKind::Sleep));
        assert!(
            sleep
                .replay_key()
                .expect("replay key")
                .ends_with(":probe:attempt:2:sleep")
        );
    }
}
