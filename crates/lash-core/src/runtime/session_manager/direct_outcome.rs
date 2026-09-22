use super::{CurrentSessionCapability, UsageCapability};
use crate::runtime::effect::{
    LlmTraceFailure, direct_trace_context, emit_llm_trace_completed, emit_llm_trace_failed,
    emit_llm_trace_started, token_usage_from_llm,
};
use crate::sansio::LlmCallError;
use crate::{
    CausalRef, LlmRequest as CoreLlmRequest, LlmResponse, PluginError, RuntimeEffectOutcome,
    TokenUsage,
};

// =============================================================================
// Direct-completion outcome plumbing
// =============================================================================

/// Both the text-only (`DirectCompletion`) and full-output (`DirectLlmCompletion`) client
/// methods project from this single result.
#[allow(private_interfaces)]
pub(crate) async fn apply_direct_outcome(
    current: &CurrentSessionCapability,
    usage_capability: &UsageCapability,
    request: &CoreLlmRequest,
    usage_source: &str,
    caused_by: Option<&CausalRef>,
    outcome: RuntimeEffectOutcome,
    usage_sink: Option<&crate::runtime::effect::ToolUsageLedger>,
) -> Result<(LlmResponse, TokenUsage, crate::LlmCallRecord), PluginError> {
    let (result, call_record) = outcome
        .into_direct_response()
        .map_err(|err| PluginError::Session(err.to_string()))?;
    // The sealed record's known usage is captured *before* its outcome is
    // projected into a success or an error: a billed failed attempt and an
    // aborted call's recorded spend are usage facts too, and the sealed
    // record is the only place they exist — once the error path below
    // returns, nothing else carries them to the child's settlement.
    if let (Some(sink), Some(record)) = (usage_sink, call_record.as_ref()) {
        sink.record(record);
    }
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
    // durable-entropy: trace-sink correlation id; the durable call record's
    // id comes from the request scope, not this value
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
