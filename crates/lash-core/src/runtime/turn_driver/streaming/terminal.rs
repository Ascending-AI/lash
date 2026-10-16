use super::*;

/// The response the turn derives an accepted cell from when the protocol
/// accepted it before the provider returned: what streamed up to the
/// acceptance. Its usage is unknown until the call's tail drains
/// (FIG-6433), never the zero of a usage nothing reported (ADR 0031).
pub(super) fn protocol_handoff_response(
    call_id: &crate::LlmCallId,
    stream_accumulator: &LlmStreamAccumulator,
    stream_evidence: &crate::LlmStreamEvidence,
) -> LlmResponse {
    let mut execution_evidence = stream_evidence
        .execution_evidence
        .clone()
        .unwrap_or_default();
    execution_evidence.collection_interruption =
        Some(crate::ExecutionEvidenceCollectionInterruption::ProtocolAbort);
    let mut response = LlmResponse {
        discarded_after_cell: None,
        parts: Vec::new(),
        usage: LlmUsage::unknown_call(),
        terminal_reason: crate::LlmTerminalReason::Stop,
        terminal_diagnostic: None,
        provider_usage: stream_evidence.provider_usage.clone(),
        request_body: stream_evidence.request_body.clone(),
        http_summary: stream_evidence.http_summary.clone(),
        execution_evidence: Some(execution_evidence),
        generation_disposition: stream_evidence.generation_disposition,
        response_metadata: stream_evidence.response_metadata.clone(),
        // Runtime-synthesized abort: the provider policy is unknowable here,
        // so leave the stamp unset — completed reasoning keeps the legacy
        // publish behaviour while streamed blocks reconcile by identity.
        expose_thinking: None,
    };
    stream_accumulator.apply_to_response_for_request(&mut response, &call_id.0);
    response
}

pub(super) fn observed_stream_protocol_position(
    text_streamed: bool,
    stream_accumulator: &LlmStreamAccumulator,
    stream_evidence: &crate::LlmStreamEvidence,
) -> crate::ProtocolPosition {
    if text_streamed || !stream_accumulator.is_empty() {
        return crate::ProtocolPosition::OutputStarted;
    }
    if stream_evidence.response_started
        || stream_evidence.execution_evidence.is_some()
        || stream_evidence.provider_usage.is_some()
        || !stream_evidence.response_metadata.is_empty()
    {
        return crate::ProtocolPosition::ResponseObserved;
    }
    crate::ProtocolPosition::NoResponse
}
