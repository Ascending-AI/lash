//! Trace projections for the LLM call surface.
//!
//! These convert provider-layer records (`LlmCallRecord`, `LlmResponse`,
//! usage and charge-safety facts) into the `lash-trace` mirrors. They are pure
//! projections with no store or runtime dependency, so they live next to the
//! provider handle that produces the records; `lash-core`'s `trace` module
//! re-exports every one of them at its original path.

use lash_trace::{
    TraceAttemptUsageDisposition, TraceChargeSafetyDecision, TraceChargeSafetyDenialReason,
    TraceExecutionEvidence, TraceLlmResponse, TraceRetryAttempt, TraceRetryAttemptOutcome,
    TraceTokenUsage,
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
                outcome: match attempt.outcome {
                    crate::AttemptOutcome::Completed => TraceRetryAttemptOutcome::Completed,
                    crate::AttemptOutcome::Failed => TraceRetryAttemptOutcome::Failed,
                    crate::AttemptOutcome::Aborted => TraceRetryAttemptOutcome::Aborted,
                    crate::AttemptOutcome::Interrupted => TraceRetryAttemptOutcome::Interrupted,
                },
                reason: trace_llm_attempt_reason(attempt),
                delay_ms: attempt
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.delay)
                    .map(|delay| delay.as_millis().try_into().unwrap_or(u64::MAX)),
                execution_evidence: attempt.evidence.as_ref().map(|evidence| {
                    let crate::ExecutionEvidence {
                        served_model,
                        provider_response_id,
                        provider_request_id,
                        reasoning_output_tokens,
                        provider_finish_reason,
                        collection_interruption,
                    } = evidence;
                    TraceExecutionEvidence {
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
                    }
                }),
                charge_safety: attempt
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.charge_safety.as_ref())
                    .map(trace_charge_safety_decision),
                generation_disposition: attempt.generation_disposition,
                usage: attempt.usage.as_ref().map(trace_usage_from_llm),
                usage_disposition: Some(trace_attempt_usage_disposition(attempt.usage_disposition)),
            })
            .collect(),
    )
}

fn trace_attempt_usage_disposition(
    disposition: crate::AttemptUsageDisposition,
) -> TraceAttemptUsageDisposition {
    match disposition {
        crate::AttemptUsageDisposition::Reported => TraceAttemptUsageDisposition::Reported,
        crate::AttemptUsageDisposition::UnreportedByProvider => {
            TraceAttemptUsageDisposition::UnreportedByProvider
        }
        crate::AttemptUsageDisposition::UnreportedAfterAbort => {
            TraceAttemptUsageDisposition::UnreportedAfterAbort
        }
        crate::AttemptUsageDisposition::UnreportedAfterFailure => {
            TraceAttemptUsageDisposition::UnreportedAfterFailure
        }
    }
}

fn trace_charge_safety_decision(
    decision: &crate::ChargeSafetyDecision,
) -> TraceChargeSafetyDecision {
    let denial_reason = |reason| match reason {
        crate::ChargeSafetyDenialReason::GuaranteeRequired => {
            TraceChargeSafetyDenialReason::GuaranteeRequired
        }
        crate::ChargeSafetyDenialReason::UnsafeRetryLimitExceeded => {
            TraceChargeSafetyDenialReason::UnsafeRetryLimitExceeded
        }
        crate::ChargeSafetyDenialReason::DuplicateCostLimitExceeded => {
            TraceChargeSafetyDenialReason::DuplicateCostLimitExceeded
        }
        crate::ChargeSafetyDenialReason::RetryAfterExceedsCap => {
            TraceChargeSafetyDenialReason::RetryAfterExceedsCap
        }
    };
    match decision {
        crate::ChargeSafetyDecision::Authorized {
            tokens_at_stake,
            attempt_number,
        } => TraceChargeSafetyDecision::Authorized {
            tokens_at_stake: *tokens_at_stake,
            attempt_number: *attempt_number,
        },
        crate::ChargeSafetyDecision::Denied {
            tokens_at_stake,
            attempt_number,
            reason,
        } => TraceChargeSafetyDecision::Denied {
            tokens_at_stake: *tokens_at_stake,
            attempt_number: *attempt_number,
            reason: denial_reason(*reason),
        },
    }
}

fn trace_llm_attempt_reason(attempt: &crate::AttemptRecord) -> Option<String> {
    let mut reason = attempt.error.as_ref().map(|error| {
        let mut reason = error.class.clone();
        let mut qualifiers = Vec::new();
        if let Some(status) = error.http_status {
            qualifiers.push(format!("http {status}"));
        }
        if let Some(code) = &error.code {
            qualifiers.push(format!("code {code}"));
        }
        if !qualifiers.is_empty() {
            reason.push_str(&format!(" ({})", qualifiers.join(", ")));
        }
        reason
    });
    if let Some(retry_reason) = attempt
        .retry_decision
        .as_ref()
        .and_then(|decision| decision.reason.as_deref())
    {
        match reason.as_mut() {
            Some(reason) if reason != retry_reason => {
                reason.push_str("; retry: ");
                reason.push_str(retry_reason);
            }
            None => reason = Some(retry_reason.to_string()),
            Some(_) => {}
        }
    }
    reason
}
