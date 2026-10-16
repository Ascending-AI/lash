//! Model semantic projection. A logical chat owns GenAI usage; its sealed
//! attempts carry diagnostic usage under lash keys, so billing is counted once.
use super::*;
use crate::{TraceLlmResponse, TraceModelAttempt, TraceModelCall};
use lash_sansio::llm::types::{
    AttemptOutcome, AttemptRecord, GenerationOptionOutcome, NormalizedError, ProtocolPosition,
    TokenUsageOverflow,
};

pub(super) fn error_type(error: &NormalizedError) -> String {
    error
        .code
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| error.class.code().to_string())
}

pub(super) fn call_attributes(
    model: Option<&TraceModelCall>,
    response: Option<&TraceLlmResponse>,
    attempts: Option<&[TraceModelAttempt]>,
    out: &mut Vec<KeyValue>,
) {
    let last_success = attempts
        .iter()
        .flat_map(|attempts| attempts.iter())
        .rev()
        .find(|attempt| attempt.outcome == AttemptOutcome::Completed);
    if let Some(model) = model {
        out.push(A::RequestModel.value(model.request_model.clone()));
        out.push(A::RequestStream.value(model.stream));
        if let Some(provider) = &model.provider {
            out.push(
                A::GenAiProviderName.value(registry::gen_ai_provider_name(provider).to_string()),
            );
        }
        if let Some(reasoning) = &model.reasoning {
            if reasoning == "disabled" {
                out.push(A::ReasoningDisabled.value("disabled"));
            } else {
                out.push(A::ReasoningLevel.value(reasoning.clone()));
            }
        }
        if let Some(output) = &model.output_type {
            out.push(A::OutputType.value(output.clone()));
        }
        let receipt = response
            .and_then(|response| response.generation_disposition)
            .or_else(|| {
                attempts
                    .and_then(|attempts| attempts.last())
                    .and_then(|attempt| attempt.generation_disposition)
            });
        if let Some(receipt) = receipt {
            if matches!(receipt.output_token_cap, GenerationOptionOutcome::Applied)
                && let Some(cap) = model.generation.output_token_cap_u64()
            {
                out.push(A::RequestMaxTokens.value(i64::try_from(cap).unwrap_or(i64::MAX)));
            }
            if receipt.seed == GenerationOptionOutcome::Applied
                && let Some(seed) = model.generation.seed
            {
                out.push(A::RequestSeed.value(seed));
            }
            if receipt.temperature == GenerationOptionOutcome::Applied
                && let Some(value) = &model.generation.temperature
            {
                out.push(A::RequestTemperature.value(value.get()));
            }
            if receipt.stop_sequences == GenerationOptionOutcome::Applied {
                out.push(
                    A::RequestStopSequences.value(opentelemetry::Value::Array(
                        opentelemetry::Array::String(
                            model
                                .generation
                                .stop_sequences
                                .iter()
                                .cloned()
                                .map(Into::into)
                                .collect(),
                        ),
                    )),
                );
            }
        }
    } else if let Some(response) = response {
        out.push(A::RequestModel.value(response.request_model.clone()));
    }
    if let Some(evidence) = last_success.and_then(|attempt| attempt.evidence.as_ref()) {
        if let Some(model) = &evidence.served_model {
            out.push(A::ResponseModel.value(model.clone()));
        }
        if let Some(id) = &evidence.provider_response_id {
            out.push(A::ResponseId.value(id.clone()));
        }
        if let Some(reason) = &evidence.provider_finish_reason {
            out.push(A::ResponseFinishReasons.value(opentelemetry::Value::Array(
                opentelemetry::Array::String(vec![reason.clone().into()]),
            )));
        }
    }
    if let Some(attempts) = attempts {
        out.push(A::ModelAttempts.value(i64::try_from(attempts.len()).unwrap_or(i64::MAX)));
    }
}

/// The one fold behind a logical call's `gen_ai.usage.*` and
/// `SpanView::usage`: the sum of every attempt that reported usage, or the
/// call's own usage when it recorded no attempts. `Err` when the sum
/// overflows.
pub(super) fn logical_usage(event: &TraceEvent) -> Option<Result<LlmUsage, TokenUsageOverflow>> {
    match event {
        TraceEvent::LlmCallCompleted {
            attempts: Some(attempts),
            ..
        }
        | TraceEvent::LlmCallFailed {
            attempts: Some(attempts),
            ..
        } => {
            let mut usages = attempts
                .iter()
                .filter_map(|attempt| attempt.attempt.usage.as_ref());
            let first = usages.next()?.clone();
            Some(usages.try_fold(first, |usage, next| usage.checked_add(next)))
        }
        TraceEvent::LlmCallCompleted { usage, .. } => usage.clone().map(Ok),
        _ => None,
    }
}

/// Whether a logical call's usage is unknown (ADR 0031): a completed call
/// whose usage never arrived, or one whose ending attempt was cut short after
/// a response was observed, before its provider usage arrived.
pub(super) fn usage_unknown(event: &TraceEvent) -> bool {
    match event {
        TraceEvent::LlmCallCompleted { usage: None, .. } => true,
        TraceEvent::LlmCallCompleted {
            attempts: Some(attempts),
            ..
        }
        | TraceEvent::LlmCallFailed {
            attempts: Some(attempts),
            ..
        } => attempts.last().is_some_and(|attempt| {
            attempt
                .attempt
                .usage_disposition()
                .is_unreported_after_interruption()
                && attempt.attempt.protocol_position != ProtocolPosition::NoResponse
        }),
        _ => false,
    }
}

pub(super) fn attempt_attributes(attempt: &AttemptRecord, out: &mut Vec<KeyValue>) {
    out.push(A::ModelAttemptOrdinal.value(i64::from(attempt.ordinal)));
    out.push(A::Outcome.value(match attempt.outcome {
        AttemptOutcome::Completed => "completed",
        AttemptOutcome::Failed => "failed",
        AttemptOutcome::Aborted => "cancelled",
        AttemptOutcome::Interrupted => "interrupted",
    }));
    if let Some(error) = &attempt.error {
        if attempt.outcome != AttemptOutcome::Aborted {
            out.push(A::ErrorType.value(error_type(error)));
        }
        if let Some(status) = error.http_status {
            out.push(A::HttpResponseStatusCode.value(i64::from(status)));
        }
    }
    if let Some(id) = attempt
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.provider_request_id.as_ref())
        .or_else(|| {
            attempt
                .error
                .as_ref()
                .and_then(|error| error.provider_request_id.as_ref())
        })
    {
        out.push(A::ProviderRequestId.value(id.clone()));
    }
    if let Some(usage) = &attempt.usage {
        out.extend([
            A::AttemptInputTokens.value(usage.input_tokens),
            A::AttemptOutputTokens.value(usage.output_tokens),
            A::AttemptCacheReadTokens.value(usage.cache_read_input_tokens),
            A::AttemptCacheWriteTokens.value(usage.cache_write_input_tokens),
            A::AttemptReasoningTokens.value(usage.reasoning_output_tokens),
        ]);
    }
}

impl OtelTelemetry {
    /// Ledger-only publication: live attempt events never enter this path.
    pub(super) fn export_model_attempts(&self, record: &TraceRecord, parent: &TracePosition) {
        if parent.kind != TraceSpanKind::Model
            || !self.policy.detail.builds(TraceSpanKind::ModelAttempt)
        {
            return;
        }
        let attempts = match &record.event {
            TraceEvent::LlmCallCompleted { attempts, .. }
            | TraceEvent::LlmCallFailed { attempts, .. } => attempts,
            _ => return,
        };
        for attempt in attempts.iter().flatten() {
            let Some(span) = &attempt.span else {
                continue;
            };
            let position = TracePosition {
                span: TraceCarrier::new(
                    parent.span.trace_id(),
                    span.span_id,
                    parent.span.flags(),
                    parent.span.tracestate().clone(),
                ),
                parent: Some(parent.span.clone()),
                links: crate::TraceLinks::new(),
                role: SpanRole::Whole,
                kind: TraceSpanKind::ModelAttempt,
                started_at: TraceTimestamp::from_unix_ms(span.started_at_ms),
                ended_at: Some(TraceTimestamp::from_unix_ms(span.ended_at_ms)),
                scope: parent.scope.clone(),
                attempt: parent.attempt.clone(),
            };
            self.export_projection(
                record,
                &position,
                None,
                Projection {
                    span: DomainSpan::ModelAttempt,
                    model: None,
                    tool: None,
                    process: None,
                    attempt: Some(attempt),
                    usage: None,
                },
            );
        }
    }
}
