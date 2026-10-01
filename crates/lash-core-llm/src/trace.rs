//! Trace projections for the LLM call surface.
//!
//! These convert provider-layer records (`LlmCallRecord`, `LlmResponse`,
//! usage and charge-safety facts) into the `lash-trace` mirrors. They are pure
//! projections with no store or runtime dependency, so they live next to the
//! provider handle that produces the records; `lash-core`'s `trace` module
//! re-exports every one of them at its original path.

use lash_trace::{
    TraceAttemptUsageOutcome, TraceExecutionEvidence, TraceLlmResponse, TraceRetryAttempt,
    TraceRetryAttemptDetail, TraceRetryDecision, TraceTokenUsage,
};

use crate::llm::types::LlmUsage;

pub fn trace_llm_response(
    text: String,
    duration_ms: u64,
    request_model: String,
    terminal_reason: Option<crate::LlmTerminalReason>,
    parts: Option<serde_json::Value>,
    generation_disposition: Option<crate::GenerationReceipt>,
) -> TraceLlmResponse {
    TraceLlmResponse {
        text,
        duration_ms,
        request_model,
        terminal_reason: terminal_reason.map(|reason| reason.code().to_string()),
        parts,
        generation_disposition: generation_disposition
            .and_then(|disposition| serde_json::to_value(disposition).ok()),
    }
}

// `lash-trace` is a standalone leaf crate (no dependency on the runtime), so it
// carries its own `TraceTokenUsage` mirror of the usage counters. These two
// converters are the only bridge to it; each destructures its source
// exhaustively (no `..`) so adding a counter to the runtime's usage types is a
// compile error here until the trace mirror is extended too.
pub fn trace_usage_from_llm(usage: &LlmUsage) -> TraceTokenUsage {
    let LlmUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
    } = usage;
    TraceTokenUsage {
        input_tokens: *input_tokens,
        output_tokens: *output_tokens,
        cache_read_input_tokens: *cache_read_input_tokens,
        cache_write_input_tokens: *cache_write_input_tokens,
        reasoning_output_tokens: *reasoning_output_tokens,
    }
}

pub fn trace_llm_attempts(record: Option<&crate::LlmCallRecord>) -> Option<Vec<TraceRetryAttempt>> {
    let record = record?;
    Some(
        record
            .attempts
            .iter()
            .map(|attempt| TraceRetryAttempt {
                ordinal: attempt.ordinal,
                delay_ms: attempt
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.delay())
                    .map(|delay| delay.as_millis().try_into().unwrap_or(u64::MAX)),
                detail: TraceRetryAttemptDetail::Llm {
                    outcome: attempt.outcome,
                    error: attempt.error.clone(),
                    retry_decision: attempt.retry_decision.as_ref().map(
                        |decision| match decision {
                            lash_sansio::llm::types::RetryDecision::Scheduled {
                                wait,
                                class,
                                ..
                            } => TraceRetryDecision::Scheduled {
                                wait: *wait,
                                class: *class,
                            },
                            lash_sansio::llm::types::RetryDecision::Declined(cause) => {
                                TraceRetryDecision::Declined(*cause)
                            }
                        },
                    ),
                    execution_evidence: attempt.evidence.as_ref().map(|evidence| {
                        let crate::ExecutionEvidence {
                            served_model,
                            provider_response_id,
                            provider_request_id,
                            reasoning_output_tokens,
                            provider_finish_reason,
                            collection_interruption,
                        } = evidence;
                        Box::new(TraceExecutionEvidence {
                            served_model: served_model.clone(),
                            provider_response_id: provider_response_id.clone(),
                            provider_request_id: provider_request_id.clone(),
                            reasoning_output_tokens: *reasoning_output_tokens,
                            provider_finish_reason: provider_finish_reason.clone(),
                            collection_interruption: collection_interruption.map(|interruption| {
                                match interruption {
                                crate::ExecutionEvidenceCollectionInterruption::ProtocolAbort => {
                                    "protocol_abort".to_string()
                                }
                            }
                            }),
                        })
                    }),
                    generation_disposition: attempt.generation_disposition,
                    usage: attempt.usage.as_ref().map(trace_usage_from_llm),
                    usage_disposition: trace_attempt_usage_disposition(attempt.usage_disposition),
                },
            })
            .collect(),
    )
}

fn trace_attempt_usage_disposition(
    disposition: crate::AttemptUsageOutcome,
) -> TraceAttemptUsageOutcome {
    match disposition {
        crate::AttemptUsageOutcome::Reported => TraceAttemptUsageOutcome::Reported,
        crate::AttemptUsageOutcome::UnreportedByProvider => {
            TraceAttemptUsageOutcome::UnreportedByProvider
        }
        crate::AttemptUsageOutcome::UnreportedAfterAbort => {
            TraceAttemptUsageOutcome::UnreportedAfterAbort
        }
        crate::AttemptUsageOutcome::UnreportedAfterFailure => {
            TraceAttemptUsageOutcome::UnreportedAfterFailure
        }
    }
}
