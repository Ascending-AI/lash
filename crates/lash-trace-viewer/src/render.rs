use super::*;

/// Title, summary, and failure flag for a typed event.
///
/// The match is exhaustive on `TraceEvent` (no wildcard): a new variant will not compile until it is given a rendering here.
pub(super) fn interpret_typed(event: &TraceEvent, raw: &Value) -> (String, String, bool) {
    match event {
        TraceEvent::LlmCallStarted { request } => (
            llm_request_title(request),
            summarize_request(request),
            false,
        ),
        TraceEvent::LlmCallCompleted {
            response,
            usage,
            attempts,
            ..
        } => {
            let usage_line = match usage {
                Some(usage) => usage_text(usage, None),
                None => "usage unavailable".to_string(),
            };
            (
                format!("completed in {} ms", response.duration_ms),
                with_retry_ladder(
                    format!("{usage_line}\n{}", response.text),
                    attempts.as_deref(),
                ),
                false,
            )
        }
        TraceEvent::LlmCallFailed {
            error, attempts, ..
        } => (
            error.message.clone(),
            with_retry_ladder(failure_detail(error), attempts.as_deref()),
            true,
        ),
        TraceEvent::ProviderRequest { event } => (
            format!("{}: {}", event.provider, event.endpoint),
            format!(
                "seq {}, {} ms, body {} bytes, sha {}",
                event.sequence, event.elapsed_ms, event.body_len, event.body_sha256
            ),
            false,
        ),
        TraceEvent::ToolCallStarted { name, args, .. } => (name.clone(), json_compact(args), false),
        TraceEvent::ToolCallCompleted {
            name,
            output,
            duration_ms,
            attempts,
            ..
        } => {
            let ok = output.is_success();
            let summary = with_retry_ladder(
                format!(
                    "{} in {duration_ms} ms\n{}",
                    if ok { "ok" } else { "error" },
                    json_compact(&output.value_for_projection())
                ),
                attempts.as_deref(),
            );
            (name.clone(), summary, !ok)
        }
        TraceEvent::JournaledEffectStarted {
            effect_name,
            effect_kind,
        } => (
            effect_name.clone(),
            format!("journaled {effect_kind} effect started"),
            false,
        ),
        TraceEvent::JournaledEffectSettled {
            effect_name,
            effect_kind,
            status,
        } => (
            effect_name.clone(),
            format!("journaled {effect_kind} effect {status}"),
            status == "failed",
        ),
        TraceEvent::DurableWaitParked { wait_kind } => {
            ("durable wait parked".to_string(), wait_kind.clone(), false)
        }
        TraceEvent::DurableWaitResolved {
            wait_kind,
            resolution,
        } => (
            "durable wait resolved".to_string(),
            format!("{wait_kind}: {resolution}"),
            resolution == "failed",
        ),
        TraceEvent::DurableTimerStarted { duration_ms } => (
            "durable timer started".to_string(),
            format!("{duration_ms} ms"),
            false,
        ),
        TraceEvent::DurableTimerResolved {
            duration_ms,
            status,
        } => (
            "durable timer resolved".to_string(),
            format!("{duration_ms} ms: {status}"),
            status == "failed",
        ),
        TraceEvent::DurableSegmentBoundary {
            reason,
            effects_executed,
            journaled_bytes_estimate,
        } => (
            "durable segment boundary".to_string(),
            match journaled_bytes_estimate {
                Some(bytes) => {
                    format!("{reason}: {effects_executed} effects, ~{bytes} journal bytes")
                }
                None => format!("{reason}: {effects_executed} effects"),
            },
            false,
        ),
        TraceEvent::StoreErrorObserved {
            operation,
            error_class,
            message,
        } => (error_class.clone(), format!("{operation}\n{message}"), true),
        TraceEvent::ProviderStreamEvent { event } => (
            format!("{}: {}", event.provider, event.event_name),
            format!(
                "seq {}, {} ms, raw {} chars, sha {}",
                event.sequence, event.elapsed_ms, event.raw_len, event.raw_sha256
            ),
            false,
        ),
        TraceEvent::RuntimeStreamEvent { event } => {
            let summary = event
                .visible_text
                .clone()
                .or_else(|| event.raw_text.clone())
                .unwrap_or_else(|| json_compact(event));
            (event.event_name.clone(), summary, false)
        }
        TraceEvent::ProtocolStep { plugin_id, payload } => (
            "protocol step".to_string(),
            format!("{plugin_id}\n{}", json_compact(payload)),
            false,
        ),
        TraceEvent::TokenUsage { usage, cumulative } => (
            "token usage".to_string(),
            usage_text(usage, cumulative.as_ref()),
            false,
        ),
        TraceEvent::LashlangExecution { event } => (
            lashlang_title(event),
            lashlang_summary(event),
            lashlang_failed(event),
        ),
        TraceEvent::TurnCompleted {
            status,
            done_reason,
            ..
        } => (
            format!("{status}: {done_reason}"),
            default_summary(raw),
            // `status` is a free-form string in the schema, not an enum; this
            // is the one place the viewer compares it as a string.
            status == "failed",
        ),
        TraceEvent::EffectEnvelopeDiff { event } => (
            "effect envelope mismatch".to_string(),
            format!(
                "{} divergent paths\n{}",
                event.divergent_paths.len(),
                json_compact(&event.divergent_paths)
            ),
            true,
        ),
        TraceEvent::Custom { name, payload } => (name.clone(), json_compact(payload), false),
        TraceEvent::PromptBuilt {
            prompt_chars,
            components,
            ..
        } => {
            let summary = components
                .iter()
                .map(|component| {
                    let chars = component
                        .chars
                        .map(|chars| chars.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    format!("{}:{} {chars} chars", component.kind, component.id)
                })
                .collect::<Vec<_>>()
                .join("\n");
            (format!("{prompt_chars} prompt chars"), summary, false)
        }
        TraceEvent::RollingHistoryCompactionNeeded {
            context_budget_tokens,
            max_context_tokens,
            threshold_tokens,
        } => (
            "rolling-history compaction needed".to_string(),
            format!(
                "budget {context_budget_tokens}/{max_context_tokens}; threshold {threshold_tokens}"
            ),
            false,
        ),
        TraceEvent::RollingHistoryPromptPruned {
            context_budget_tokens,
            max_context_tokens,
            dropped_prefix_messages,
            retained_messages,
        } => (
            "rolling-history prompt pruned".to_string(),
            format!(
                "context budget {context_budget_tokens}, max context {max_context_tokens}, dropped {dropped_prefix_messages} prefix messages, retained {retained_messages} messages"
            ),
            false,
        ),
        TraceEvent::RollingHistoryCompactionStarted {
            source_messages,
            instructions_present,
        } => (
            "rolling-history compaction started".to_string(),
            format!("{source_messages} messages; instructions: {instructions_present}"),
            false,
        ),
        TraceEvent::RollingHistoryCompactionCompleted { summary_nodes } => (
            "rolling-history compaction completed".to_string(),
            format!("produced {summary_nodes} summary nodes"),
            false,
        ),
        TraceEvent::SessionStarted { .. } | TraceEvent::TurnStarted { .. } => {
            (kind_title(event.kind()), default_summary(raw), false)
        }
    }
}

fn with_retry_ladder(
    summary: String,
    attempts: Option<&[lash_trace::TraceRetryAttempt]>,
) -> String {
    let Some(attempts) = attempts else {
        return summary;
    };
    let mut lines = Vec::with_capacity(attempts.len() + 1);
    lines.push(format!("attempts: {}", attempts.len()));
    lines.extend(attempts.iter().map(|attempt| {
        let mut line = format!(
            "#{} {} ({} ms)",
            attempt.ordinal, attempt.outcome, attempt.duration_ms
        );
        if let Some(reason) = &attempt.reason {
            line.push_str(&format!(": {reason}"));
        }
        if let Some(delay_ms) = attempt.delay_ms {
            line.push_str(&format!("; retry after {delay_ms} ms"));
        }
        line
    }));
    format!("{summary}\n{}", lines.join("\n"))
}
