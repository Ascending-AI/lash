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
            response, usage, ..
        } => {
            let usage_line = match usage {
                Some(usage) => usage_text(usage, None),
                None => "usage unavailable".to_string(),
            };
            (
                format!("completed in {} ms", response.duration_ms),
                format!("{usage_line}\n{}", response.text),
                false,
            )
        }
        TraceEvent::LlmCallFailed { error, .. } => {
            (error.message.clone(), failure_detail(error), true)
        }
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
            ..
        } => {
            let ok = output.is_success();
            let summary = format!(
                "{} in {duration_ms} ms\n{}",
                if ok { "ok" } else { "error" },
                json_compact(&output.value_for_projection())
            );
            (name.clone(), summary, !ok)
        }
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
        TraceEvent::SessionExecutionLeaseFrameHandoffTransferred {
            owner_id,
            incarnation_id,
            previous_fencing_token,
            transferred_fencing_token,
            trigger,
        } => (
            "session execution lease handoff".to_string(),
            format!(
                "{trigger}: {owner_id}/{incarnation_id}, fencing token {previous_fencing_token} -> {transferred_fencing_token}"
            ),
            false,
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
