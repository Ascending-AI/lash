//! Trace projections for the LLM call surface.
//!
//! A model call's trace reports the provider-layer facts as they are: the
//! typed terminal reason, output parts, generation receipt and the sealed
//! attempt records. Only the observation envelope is built here.

use lash_trace::TraceLlmResponse;

use crate::llm::types::{AttemptRecord, LlmOutputPart};

pub fn trace_llm_response(
    text: String,
    duration_ms: u64,
    request_model: String,
    terminal_reason: Option<crate::LlmTerminalReason>,
    parts: &[LlmOutputPart],
    generation_disposition: Option<crate::GenerationReceipt>,
) -> TraceLlmResponse {
    TraceLlmResponse {
        text,
        duration_ms,
        request_model,
        terminal_reason,
        parts: (!parts.is_empty()).then(|| {
            parts
                .iter()
                .map(LlmOutputPart::without_replay_payloads)
                .collect()
        }),
        generation_disposition,
    }
}

/// The sealed attempts of a call, as its record keeps them.
pub fn trace_llm_attempts(record: Option<&crate::LlmCallRecord>) -> Option<Vec<AttemptRecord>> {
    record.map(|record| record.attempts.clone())
}
