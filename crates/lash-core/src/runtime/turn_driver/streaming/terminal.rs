use super::*;

pub(super) fn synthesize_protocol_abort(
    stream_accumulator: &LlmStreamAccumulator,
    streamed_usage: LlmUsage,
    stream_evidence: &crate::LlmStreamEvidence,
    started_at: u64,
    duration: std::time::Duration,
    replay_drops: Vec<crate::ProviderReplayDrop>,
) -> (LlmResponse, crate::LlmCallRecord) {
    let mut execution_evidence = stream_evidence
        .execution_evidence
        .clone()
        .unwrap_or_default();
    execution_evidence.collection_interruption =
        Some(crate::ExecutionEvidenceCollectionInterruption::ProtocolAbort);
    let mut response = LlmResponse {
        full_text: stream_accumulator.full_text(),
        parts: Vec::new(),
        usage: streamed_usage,
        terminal_reason: crate::LlmTerminalReason::Stop,
        terminal_diagnostic: None,
        provider_usage: stream_evidence.provider_usage.clone(),
        request_body: stream_evidence.request_body.clone(),
        http_summary: stream_evidence.http_summary.clone(),
        execution_evidence: Some(execution_evidence.clone()),
        generation_disposition: stream_evidence.generation_disposition,
        response_metadata: stream_evidence.response_metadata.clone(),
    };
    stream_accumulator.apply_to_response(&mut response);
    let call_record = crate::LlmCallRecord {
        call_id: crate::LlmCallId(uuid::Uuid::new_v4().to_string()),
        label: None,
        replay_drops,
        attempts: vec![crate::AttemptRecord {
            ordinal: 1,
            started_at,
            duration,
            outcome: crate::AttemptOutcome::Aborted,
            protocol_position: crate::ProtocolPosition::OutputStarted,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: Some(execution_evidence),
            generation_disposition: response.generation_disposition,
            usage: (response.provider_usage.is_some() || response.usage != LlmUsage::default())
                .then(|| response.usage.clone()),
        }],
    };
    (response, call_record)
}

pub(super) fn observed_stream_protocol_position(
    text_streamed: bool,
    stream_accumulator: &LlmStreamAccumulator,
    stream_evidence: &crate::LlmStreamEvidence,
) -> crate::ProtocolPosition {
    if text_streamed || !stream_accumulator.is_empty() {
        return crate::ProtocolPosition::OutputStarted;
    }
    if stream_evidence.execution_evidence.is_some()
        || stream_evidence.provider_usage.is_some()
        || stream_evidence.http_summary.is_some()
        || !stream_evidence.response_metadata.is_empty()
    {
        return crate::ProtocolPosition::ResponseObserved;
    }
    crate::ProtocolPosition::NoResponse
}
