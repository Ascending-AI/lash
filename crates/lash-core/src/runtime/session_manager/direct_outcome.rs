use super::CurrentOwnerCapability;
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
    current: &CurrentOwnerCapability,
    request: &CoreLlmRequest,
    caused_by: Option<&CausalRef>,
    outcome: RuntimeEffectOutcome,
) -> Result<(LlmResponse, TokenUsage, crate::LlmCallRecord), PluginError> {
    let (result, call_record) = outcome
        .into_direct_response()
        .map_err(|err| PluginError::Session(err.to_string()))?;
    let (response, usage) =
        apply_direct_llm_result(current, request, caused_by, result, call_record.as_ref()).await?;
    let call_record = call_record.ok_or_else(|| {
        PluginError::Session(
            "direct LLM effect completed without a provider call record".to_string(),
        )
    })?;
    Ok((response, usage, call_record))
}

/// The completion's projection and trace. The usage it reports is the
/// response's, for the caller; the ledger was written by the effect's usage
/// run when the effect was recorded (ADR 0125).
async fn apply_direct_llm_result(
    current: &CurrentOwnerCapability,
    request: &CoreLlmRequest,
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
    current: &CurrentOwnerCapability,
    request: &CoreLlmRequest,
    caused_by: Option<&CausalRef>,
) -> Option<String> {
    current.host.core.tracing.trace_sink.as_ref()?;
    let llm_call_id = uuid::Uuid::new_v4().to_string();
    emit_llm_trace_started(
        &current.host.core.tracing.trace_sink,
        &current.host.core.tracing.trace_context,
        direct_trace_context(&current.runtime_owner(), Some(&llm_call_id), caused_by),
        request,
        current.host.core.clock.as_ref(),
    );
    Some(llm_call_id)
}

fn emit_direct_llm_trace_completed(
    current: &CurrentOwnerCapability,
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
        direct_trace_context(&current.runtime_owner(), Some(llm_call_id), caused_by),
        response,
        request_model,
        0,
        None,
        call_record,
        current.host.core.clock.as_ref(),
    );
}

fn emit_direct_llm_trace_failed(
    current: &CurrentOwnerCapability,
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
        direct_trace_context(&current.runtime_owner(), Some(llm_call_id), caused_by),
        LlmTraceFailure::from(err),
        None,
        call_record,
        current.host.core.clock.as_ref(),
    );
}
