use std::sync::Arc;

use lash_trace::{
    TraceAttachment, TraceContentBlock, TraceContext, TraceEvent, TraceExecutionEvidence,
    TraceLlmMessage, TraceLlmRequest, TraceLlmResponse, TraceRecord, TraceRetryAttempt, TraceSink,
    TraceTokenUsage, TraceToolSpec, sha256_hex,
};

use crate::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmOutputPart, LlmOutputSpec, LlmRequest,
    LlmRole, LlmToolChoice, LlmToolSpec, LlmUsage,
};
use crate::session_model::TokenUsage;
use crate::{ToolCallOutcome, ToolCallOutput};
use sha2::{Digest as _, Sha256};

#[cfg(test)]
thread_local! {
    static COMPOSITION_SCHEMA_SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn composition_schema_serialization_count() -> usize {
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(std::cell::Cell::get)
}

pub(crate) fn emit_trace(
    sink: &Option<Arc<dyn TraceSink>>,
    base_context: &TraceContext,
    context: TraceContext,
    event: TraceEvent,
    clock: &dyn crate::Clock,
) {
    emit_trace_at(
        sink,
        base_context,
        context,
        event,
        clock.timestamp_datetime(),
    );
}

pub(crate) fn emit_trace_at(
    sink: &Option<Arc<dyn TraceSink>>,
    base_context: &TraceContext,
    context: TraceContext,
    event: TraceEvent,
    timestamp: chrono::DateTime<chrono::Utc>,
) {
    let Some(sink) = sink else {
        return;
    };
    let mut merged = base_context.clone();
    merge_context(&mut merged, context);
    assign_span_identity(&mut merged, &event);
    if let Err(err) = sink.append(&TraceRecord::new_with_timestamp(merged, event, timestamp)) {
        tracing::warn!(error = %err, "failed to append trace record");
    }
}

/// Emit evidence only for store failures whose typed class means persisted
/// state is corrupt or a monotonic durable identity cannot advance.
pub(crate) fn emit_store_error(
    sink: &Option<Arc<dyn TraceSink>>,
    base_context: &TraceContext,
    context: TraceContext,
    operation: &str,
    error: &crate::StoreError,
    clock: &dyn crate::Clock,
) {
    if !matches!(
        error,
        crate::StoreError::StoredDataCorrupt { .. }
            | crate::StoreError::MonotonicCounterOverflow { .. }
    ) {
        return;
    }
    emit_trace(
        sink,
        base_context,
        context,
        TraceEvent::StoreErrorObserved {
            operation: operation.to_string(),
            error_class: error.variant_name().to_string(),
            message: error.to_string(),
        },
        clock,
    );
}

fn merge_context(base: &mut TraceContext, overlay: TraceContext) {
    if overlay.run_id.is_some() {
        base.run_id = overlay.run_id;
    }
    if overlay.experiment_id.is_some() {
        base.experiment_id = overlay.experiment_id;
    }
    if overlay.candidate_id.is_some() {
        base.candidate_id = overlay.candidate_id;
    }
    if overlay.candidate_parent_id.is_some() {
        base.candidate_parent_id = overlay.candidate_parent_id;
    }
    if overlay.example_id.is_some() {
        base.example_id = overlay.example_id;
    }
    if overlay.split.is_some() {
        base.split = overlay.split;
    }
    if overlay.session_id.is_some() {
        base.session_id = overlay.session_id;
    }
    if overlay.turn_id.is_some() {
        base.turn_id = overlay.turn_id;
    }
    if overlay.graph_node_id.is_some() {
        base.graph_node_id = overlay.graph_node_id;
    }
    if overlay.parent_graph_node_id.is_some() {
        base.parent_graph_node_id = overlay.parent_graph_node_id;
    }
    if overlay.turn_index.is_some() {
        base.turn_index = overlay.turn_index;
    }
    if overlay.protocol_iteration.is_some() {
        base.protocol_iteration = overlay.protocol_iteration;
    }
    if overlay.effect_id.is_some() {
        base.effect_id = overlay.effect_id;
    }
    if overlay.llm_call_id.is_some() {
        base.llm_call_id = overlay.llm_call_id;
    }
    base.metadata.extend(overlay.metadata);
}

/// Stamp the span identity (`graph_node_id`) and parent link
/// (`parent_graph_node_id`) for the span this record represents, derived purely
/// from data lash already carries (session / turn / llm / tool ids and any
/// `caused_by` causal parent). This makes the trace stream self-describing: a
/// consumer builds a correctly-nested span tree from `(graph_node_id,
/// parent_graph_node_id)` with a single `id -> span` map, with no heuristic
/// hierarchy reconstruction.
///
/// The tree is `session -> turn -> { llm call, tool call, … }`. A turn's parent
/// is its causal origin (`caused_by` — e.g. the tool call in a parent session
/// that spawned this subagent) when one is already on the context, otherwise
/// the session root. Records that already carry their own node identity in the
/// payload, and host-defined custom events, are left untouched.
fn assign_span_identity(context: &mut TraceContext, event: &TraceEvent) {
    let session_node = context.session_id.as_deref().map(session_node_id);
    let turn_node = turn_node_id(context);

    match event {
        TraceEvent::SessionStarted { .. } => set_span(context, session_node, None),
        TraceEvent::TurnStarted { .. } | TraceEvent::TurnCompleted { .. } => {
            let parent = context.parent_graph_node_id.clone().or(session_node);
            set_span(context, turn_node, parent);
        }
        TraceEvent::LlmCallStarted { .. }
        | TraceEvent::LlmCallCompleted { .. }
        | TraceEvent::LlmCallFailed { .. } => {
            let self_id = context.llm_call_id.as_deref().map(llm_node_id);
            set_span(context, self_id, turn_node);
        }
        TraceEvent::ToolCallStarted { call_id, .. }
        | TraceEvent::ToolCallCompleted { call_id, .. } => {
            let self_id = call_id.as_deref().map(tool_node_id);
            set_span(context, self_id, turn_node);
        }
        TraceEvent::ProviderRequest { .. }
        | TraceEvent::ProviderReplayDropped { .. }
        | TraceEvent::ProviderStreamEvent { .. }
        | TraceEvent::RuntimeStreamEvent { .. } => {
            let parent = context
                .llm_call_id
                .as_deref()
                .map(llm_node_id)
                .or(turn_node);
            set_span(context, None, parent);
        }
        TraceEvent::PromptBuilt { .. }
        | TraceEvent::CompositionChanged { .. }
        | TraceEvent::RollingHistoryCompactionNeeded { .. }
        | TraceEvent::RollingHistoryPromptPruned { .. }
        | TraceEvent::EffectEnvelopeDiff { .. }
        | TraceEvent::ProtocolStep { .. }
        | TraceEvent::TokenUsage { .. }
        | TraceEvent::JournaledEffectStarted { .. }
        | TraceEvent::JournaledEffectSettled { .. }
        | TraceEvent::DurableWaitParked { .. }
        | TraceEvent::DurableWaitResolved { .. }
        | TraceEvent::DurableTimerStarted { .. }
        | TraceEvent::DurableTimerResolved { .. }
        | TraceEvent::DurableSegmentBoundary { .. }
        | TraceEvent::StoreErrorObserved { .. } => set_span(context, None, turn_node),
        TraceEvent::RollingHistoryCompactionStarted { .. }
        | TraceEvent::RollingHistoryCompactionCompleted { .. } => {
            set_span(context, None, turn_node.or(session_node));
        }
        // Events that already carry their own node identity in the payload, and
        // host-defined custom events, keep whatever the emitter set.
        _ => {}
    }
}

/// Apply a computed `(self_id, parent_id)` without clobbering identity an
/// emitter set explicitly, and never letting a span become its own parent.
fn set_span(context: &mut TraceContext, self_id: Option<String>, parent_id: Option<String>) {
    if context.graph_node_id.is_none() {
        context.graph_node_id = self_id;
    }
    if let Some(parent_id) = parent_id
        && context.graph_node_id.as_deref() != Some(parent_id.as_str())
    {
        context.parent_graph_node_id = Some(parent_id);
    }
}

fn session_node_id(session_id: &str) -> String {
    format!("session:{session_id}")
}

fn turn_node_id(context: &TraceContext) -> Option<String> {
    let session_id = context.session_id.as_deref()?;
    if let Some(turn_id) = context.turn_id.as_deref() {
        Some(format!("turn:{session_id}:{turn_id}"))
    } else {
        context
            .turn_index
            .map(|turn_index| format!("turn:{session_id}:idx{turn_index}"))
    }
}

fn llm_node_id(llm_call_id: &str) -> String {
    format!("llm:{llm_call_id}")
}

fn tool_node_id(call_id: &str) -> String {
    format!("tool:{call_id}")
}

/// Map a `caused_by` reference onto the node id its target span carries, so a
/// child session/turn nests under whatever spawned it. The `Turn` / `ToolCall`
/// arms intentionally mirror [`turn_node_id`] / [`tool_node_id`] so the
/// cross-session parent reference resolves to a real span.
fn causal_node_id(caused_by: &crate::CausalRef) -> String {
    match caused_by {
        crate::CausalRef::Turn {
            session_id,
            turn_id,
        } => format!("turn:{session_id}:{turn_id}"),
        crate::CausalRef::Effect { effect_id, .. } => format!("effect:{effect_id}"),
        crate::CausalRef::ToolCall { call_id, .. } => format!("tool:{call_id}"),
        crate::CausalRef::Process { process_id } => format!("process:{process_id}"),
        crate::CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => format!("process:{process_id}:{sequence}"),
        crate::CausalRef::TriggerOccurrence { occurrence_id, .. } => {
            format!("trigger:{occurrence_id}")
        }
        crate::CausalRef::SessionNode {
            session_id,
            node_id,
        } => format!("node:{session_id}:{node_id}"),
    }
}

pub(crate) fn trace_context_from_invocation(invocation: &crate::RuntimeInvocation) -> TraceContext {
    let mut context = TraceContext::default().for_session(invocation.scope.session_id.clone());
    if let Some(turn_id) = invocation.scope.turn_id.as_ref() {
        context = context.for_turn(turn_id.clone());
    }
    if let Some(turn_index) = invocation.scope.turn_index {
        context = context.for_turn_index(turn_index);
    }
    if let Some(protocol_iteration) = invocation.scope.protocol_iteration {
        context = context.for_protocol_iteration(protocol_iteration);
    }
    if let Some(effect_id) = invocation.effect_id() {
        context.effect_id = Some(effect_id.to_string());
    }
    if let Some(replay) = invocation.replay.as_ref() {
        context
            .metadata
            .insert("replay_key".to_string(), serde_json::json!(replay.key));
    }
    if let Some(caused_by) = invocation.caused_by.as_ref() {
        context = trace_context_with_causal_ref(context, caused_by);
        if context.parent_graph_node_id.is_none() {
            context.parent_graph_node_id = Some(causal_node_id(caused_by));
        }
    }
    context
}

pub(crate) fn trace_context_with_causal_ref(
    mut context: TraceContext,
    caused_by: &crate::CausalRef,
) -> TraceContext {
    if let Ok(value) = serde_json::to_value(caused_by) {
        context.metadata.insert("caused_by".to_string(), value);
    }
    context
}

pub(crate) fn trace_llm_request(req: &LlmRequest) -> TraceLlmRequest {
    TraceLlmRequest {
        model: req.model.clone(),
        model_variant: match &req.model_variant {
            crate::ReasoningSelection::ProviderDefault => None,
            crate::ReasoningSelection::Disabled => Some("disabled".to_string()),
            crate::ReasoningSelection::Effort(effort) => Some(effort.clone()),
        },
        messages: req.messages.iter().map(trace_llm_message).collect(),
        attachments: req.attachments.iter().map(trace_attachment).collect(),
        tools: req.tools.iter().map(trace_tool_spec).collect(),
        tool_choice: match req.tool_choice {
            LlmToolChoice::Auto => "auto",
            LlmToolChoice::None => "none",
            LlmToolChoice::Required => "required",
        }
        .to_string(),
        output_spec: req.output_spec.as_ref().map(trace_output_spec),
        stream: req.stream_events.is_some(),
    }
}

fn trace_tool_spec(tool: &LlmToolSpec) -> TraceToolSpec {
    TraceToolSpec {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: serde_json::to_value(&tool.input_schema)
            .expect("SchemaContract serialization is infallible"),
        output_schema: serde_json::to_value(&tool.output_schema)
            .expect("SchemaContract serialization is infallible"),
    }
}

struct Sha256Writer(Sha256);

impl std::io::Write for Sha256Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn composition_tool_fingerprint(tool: &LlmToolSpec) -> [u8; 32] {
    #[cfg(test)]
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    let mut writer = Sha256Writer(Sha256::new());
    serde_json::to_writer(&mut writer, tool)
        .expect("model-facing tool contract serialization is infallible");
    writer.0.finalize().into()
}

pub(crate) fn trace_composition_key(req: &LlmRequest, tool_fingerprints: &[[u8; 32]]) -> [u8; 32] {
    debug_assert_eq!(req.tools.len(), tool_fingerprints.len());
    let mut hash = Sha256::new();
    hash.update(b"lash:model-facing-composition:v1\0");
    if let Some(message) = req
        .messages
        .first()
        .filter(|message| matches!(message.role, LlmRole::System))
    {
        for block in message.blocks.iter() {
            if let LlmContentBlock::Text { text, .. } = block {
                hash.update(text.len().to_le_bytes());
                hash.update(text.as_bytes());
            }
        }
    }
    hash.update(tool_fingerprints.len().to_le_bytes());
    for fingerprint in tool_fingerprints {
        hash.update(fingerprint);
    }
    hash.finalize().into()
}

pub(crate) struct CompositionTraceSnapshot {
    pub(crate) fingerprint: String,
    pub(crate) rendered_system_prompt: String,
    pub(crate) tool_schemas: Vec<TraceToolSpec>,
}

pub(crate) fn trace_composition_snapshot(
    req: &LlmRequest,
    fingerprint: [u8; 32],
) -> CompositionTraceSnapshot {
    #[cfg(test)]
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    let rendered_system_prompt = req
        .messages
        .first()
        .filter(|message| matches!(message.role, LlmRole::System))
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default();
    let tool_schemas = req.tools.iter().map(trace_tool_spec).collect::<Vec<_>>();
    let fingerprint = fingerprint
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    CompositionTraceSnapshot {
        fingerprint,
        rendered_system_prompt,
        tool_schemas,
    }
}

pub(crate) fn trace_tool_call_output(output: &ToolCallOutput) -> lash_trace::TraceToolCallOutput {
    let outcome = match &output.outcome {
        ToolCallOutcome::Success(value) => {
            lash_trace::TraceToolCallOutcome::Success(value.to_json_value())
        }
        ToolCallOutcome::Failure(failure) => {
            lash_trace::TraceToolCallOutcome::Failure(failure.to_json_value())
        }
        ToolCallOutcome::Cancelled(cancellation) => {
            lash_trace::TraceToolCallOutcome::Cancelled(cancellation.to_json_value())
        }
    };
    lash_trace::TraceToolCallOutput {
        outcome,
        control: output
            .control
            .as_ref()
            .and_then(|control| serde_json::to_value(control).ok()),
    }
}

fn trace_llm_message(message: &LlmMessage) -> TraceLlmMessage {
    TraceLlmMessage {
        role: match message.role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "system",
        }
        .to_string(),
        blocks: message.blocks.iter().map(trace_content_block).collect(),
    }
}

fn trace_content_block(block: &LlmContentBlock) -> TraceContentBlock {
    match block {
        LlmContentBlock::Text {
            text,
            cache_breakpoint,
            ..
        } => TraceContentBlock::Text {
            text: text.to_string(),
            cache_breakpoint: *cache_breakpoint,
        },
        LlmContentBlock::Attachment { attachment_idx } => TraceContentBlock::Attachment {
            attachment_idx: *attachment_idx,
        },
        LlmContentBlock::ToolCall {
            call_id,
            tool_name,
            input_json,
            replay,
        } => TraceContentBlock::ToolCall {
            call_id: Some(call_id.clone()),
            tool_name: tool_name.clone(),
            input_json: serde_json::from_str(input_json)
                .unwrap_or_else(|_| serde_json::Value::String(input_json.clone())),
            item_id: replay.as_ref().and_then(|meta| meta.item_id.clone()),
            has_signature: replay.as_ref().is_some_and(|meta| meta.opaque.is_some()),
        },
        LlmContentBlock::ToolResult {
            call_id,
            content,
            tool_name,
        } => TraceContentBlock::ToolResult {
            call_id: Some(call_id.clone()),
            tool_name: tool_name.clone(),
            content: content.clone(),
        },
        LlmContentBlock::Reasoning { text, replay } => TraceContentBlock::Reasoning {
            text: text.clone(),
            item_id: replay.as_ref().and_then(|meta| meta.item_id.clone()),
            summary: replay
                .as_ref()
                .map(|meta| meta.summary.clone())
                .unwrap_or_default(),
            has_encrypted: replay
                .as_ref()
                .is_some_and(|meta| meta.encrypted_content.is_some() || meta.signature.is_some()),
            redacted: replay.as_ref().is_some_and(|meta| meta.redacted),
        },
    }
}

fn trace_attachment(attachment: &AttachmentSource) -> TraceAttachment {
    let bytes = match attachment {
        AttachmentSource::Inline { bytes, .. } => Some(bytes.as_slice()),
        AttachmentSource::Stored { .. }
        | AttachmentSource::ExternalUrl { .. }
        | AttachmentSource::ProviderFile { .. } => None,
    };
    TraceAttachment {
        source: crate::llm::transport::source_kind(attachment).to_string(),
        mime: attachment.media_type().map(ToString::to_string),
        filename: None,
        bytes_sha256: bytes.map(sha256_hex),
        bytes_len: bytes.map(<[u8]>::len),
    }
}

fn trace_output_spec(spec: &LlmOutputSpec) -> serde_json::Value {
    match spec {
        LlmOutputSpec::JsonObject => serde_json::json!({ "type": "json_object" }),
        LlmOutputSpec::JsonSchema(schema) => serde_json::json!({
            "type": "json_schema",
            "name": schema.name,
            "schema": schema.schema,
            "strict": schema.strict,
        }),
    }
}

pub(crate) fn trace_llm_response(
    text: String,
    duration_ms: u64,
    terminal_reason: Option<crate::LlmTerminalReason>,
    parts: Option<serde_json::Value>,
    generation_disposition: Option<crate::GenerationReceipt>,
) -> TraceLlmResponse {
    TraceLlmResponse {
        text,
        duration_ms,
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
pub(crate) fn trace_usage_from_llm(usage: &LlmUsage) -> TraceTokenUsage {
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

pub(crate) fn trace_usage_from_session(usage: &TokenUsage) -> TraceTokenUsage {
    let TokenUsage {
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

pub(crate) fn trace_llm_attempts(
    record: Option<&crate::LlmCallRecord>,
) -> Option<Vec<TraceRetryAttempt>> {
    let record = record?;
    Some(
        record
            .attempts
            .iter()
            .map(|attempt| TraceRetryAttempt {
                ordinal: attempt.ordinal,
                outcome: match attempt.outcome {
                    crate::AttemptOutcome::Completed => "completed",
                    crate::AttemptOutcome::Failed => "failed",
                    crate::AttemptOutcome::Aborted => "aborted",
                    crate::AttemptOutcome::Interrupted => "interrupted",
                }
                .to_string(),
                duration_ms: attempt.duration.as_millis().try_into().unwrap_or(u64::MAX),
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
            })
            .collect(),
    )
}

pub(crate) fn trace_tool_attempt(
    ordinal: u32,
    record: &crate::ToolCallRecord,
    delay_ms: Option<u64>,
) -> TraceRetryAttempt {
    let (outcome, reason) = match &record.output.outcome {
        crate::ToolCallOutcome::Success(_) => ("completed", None),
        crate::ToolCallOutcome::Failure(failure) => (
            "failed",
            Some(format!("{}: {}", failure.code, failure.message)),
        ),
        crate::ToolCallOutcome::Cancelled(cancellation) => {
            ("cancelled", Some(cancellation.message.clone()))
        }
    };
    TraceRetryAttempt {
        ordinal,
        outcome: outcome.to_string(),
        duration_ms: record.duration_ms,
        reason,
        delay_ms,
        execution_evidence: None,
    }
}

fn trace_llm_attempt_reason(attempt: &crate::AttemptRecord) -> Option<String> {
    let mut reason = attempt.error.as_ref().map(|error| {
        let mut reason = error.class.clone();
        let mut qualifiers = Vec::new();
        if let Some(status) = error.http_status {
            qualifiers.push(format!("http {status}"));
        }
        if let Some(code) = error.provider_code.as_deref() {
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

pub(crate) fn trace_output_parts(parts: &[LlmOutputPart]) -> Option<serde_json::Value> {
    let parts = parts
        .iter()
        .map(|part| match part {
            LlmOutputPart::Text { text, .. } => serde_json::json!({
                "type": "text",
                "text": text,
            }),
            LlmOutputPart::Reasoning { text, replay } => serde_json::json!({
                "type": "reasoning",
                "id": replay.as_ref().and_then(|meta| meta.item_id.as_ref()),
                "summary": replay.as_ref().map(|meta| &meta.summary),
                "text": text,
                "has_encrypted": replay.as_ref().is_some_and(|meta| meta.encrypted_content.is_some() || meta.signature.is_some()),
                "redacted": replay.as_ref().is_some_and(|meta| meta.redacted),
            }),
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => serde_json::json!({
                "type": "tool_call",
                "call_id": call_id,
                "tool_name": tool_name,
                "input_json": input_json,
                "id": replay.as_ref().and_then(|meta| meta.item_id.as_ref()),
                "has_opaque": replay.as_ref().is_some_and(|meta| meta.opaque.is_some()),
            }),
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then_some(serde_json::Value::Array(parts))
}

#[cfg(test)]
mod span_identity_tests {
    use super::*;

    fn turn_context() -> TraceContext {
        TraceContext::default()
            .for_session("sess")
            .for_turn_index(0)
            .for_turn("turn-1")
    }

    fn sample_request() -> TraceLlmRequest {
        TraceLlmRequest {
            model: "openai/test".to_string(),
            model_variant: Default::default(),
            messages: Vec::new(),
            attachments: Vec::new(),
            tools: Vec::new(),
            tool_choice: "auto".to_string(),
            output_spec: None,
            stream: false,
        }
    }

    #[test]
    fn turn_span_parents_under_session() {
        let mut context = turn_context();
        assign_span_identity(
            &mut context,
            &TraceEvent::TurnStarted {
                metadata: Default::default(),
            },
        );
        assert_eq!(context.graph_node_id.as_deref(), Some("turn:sess:turn-1"));
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("session:sess")
        );
    }

    #[test]
    fn llm_span_parents_under_turn() {
        let mut context = turn_context().for_llm_call("sess:0:0:0");
        assign_span_identity(
            &mut context,
            &TraceEvent::LlmCallStarted {
                request: sample_request(),
            },
        );
        assert_eq!(context.graph_node_id.as_deref(), Some("llm:sess:0:0:0"));
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("turn:sess:turn-1")
        );
    }

    #[test]
    fn tool_span_parents_under_turn_and_matches_causal_tool_ref() {
        let mut context = turn_context();
        assign_span_identity(
            &mut context,
            &TraceEvent::ToolCallStarted {
                call_id: Some("call_abc".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({}),
            },
        );
        assert_eq!(context.graph_node_id.as_deref(), Some("tool:call_abc"));
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("turn:sess:turn-1")
        );
        // A subagent caused_by this tool call must resolve to the same node id.
        assert_eq!(
            causal_node_id(&crate::CausalRef::ToolCall {
                session_id: "sess".to_string(),
                call_id: "call_abc".to_string(),
            }),
            "tool:call_abc"
        );
    }

    #[test]
    fn turn_keeps_causal_parent_when_present() {
        let mut context = turn_context();
        context.parent_graph_node_id = Some("tool:call_parent".to_string());
        assign_span_identity(
            &mut context,
            &TraceEvent::TurnCompleted {
                outcome: lash_trace::TraceTurnOutcome::Completed {
                    done_reason: lash_trace::TraceTurnCompletionReason::AssistantMessage,
                },
            },
        );
        assert_eq!(context.graph_node_id.as_deref(), Some("turn:sess:turn-1"));
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("tool:call_parent")
        );
    }

    #[test]
    fn tool_call_without_id_has_no_self_node_but_still_nests() {
        let mut context = turn_context();
        assign_span_identity(
            &mut context,
            &TraceEvent::ToolCallStarted {
                call_id: None,
                name: "read_file".to_string(),
                args: serde_json::json!({}),
            },
        );
        assert_eq!(context.graph_node_id, None);
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("turn:sess:turn-1")
        );
    }

    #[test]
    fn turnless_rolling_history_record_parents_under_session() {
        let mut context = TraceContext::default().for_session("compact-session");
        assign_span_identity(
            &mut context,
            &TraceEvent::RollingHistoryCompactionStarted {
                source_messages: 3,
                instructions_present: false,
            },
        );

        assert_eq!(context.graph_node_id, None);
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("session:compact-session")
        );
    }

    #[test]
    fn multi_attempt_llm_record_projects_the_trace_retry_ladder() {
        let record = crate::LlmCallRecord {
            call_id: crate::LlmCallId("llm-ladder".to_string()),
            label: None,
            replay_drops: Vec::new(),
            attempts: vec![
                crate::AttemptRecord {
                    ordinal: 1,
                    started_at: 1_000,
                    duration: std::time::Duration::from_millis(12),
                    outcome: crate::AttemptOutcome::Failed,
                    protocol_position: crate::ProtocolPosition::ResponseObserved,
                    retry_budget_consumed: true,
                    retry_decision: Some(crate::RetryDecision {
                        scheduled: true,
                        delay: Some(std::time::Duration::from_millis(250)),
                        reason: Some("provider_retry_after".to_string()),
                    }),
                    error: Some(crate::NormalizedError {
                        class: "rate_limited".to_string(),
                        provider_code: Some("rate_limit_exceeded".to_string()),
                        http_status: Some(429),
                        provider_request_id: None,
                        retry_after: Some(std::time::Duration::from_millis(250)),
                        diagnostic: None,
                    }),
                    evidence: None,
                    generation_disposition: None,
                    usage: None,
                },
                crate::AttemptRecord {
                    ordinal: 2,
                    started_at: 1_262,
                    duration: std::time::Duration::from_millis(20),
                    outcome: crate::AttemptOutcome::Completed,
                    protocol_position: crate::ProtocolPosition::TerminalObserved,
                    retry_budget_consumed: true,
                    retry_decision: None,
                    error: None,
                    evidence: None,
                    generation_disposition: None,
                    usage: None,
                },
            ],
        };

        let directory = tempfile::tempdir().expect("trace tempdir");
        let path = directory.path().join("llm-retry.trace.jsonl");
        let sink: Arc<dyn TraceSink> = Arc::new(lash_trace::JsonlTraceSink::new(&path));
        let sink = Some(sink);
        let error = crate::LlmCallError {
            message: "provider attempts exhausted".to_string(),
            retryable: true,
            kind: crate::ProviderFailureKind::Http,
            raw: None,
            code: Some("rate_limit_exceeded".to_string()),
            terminal_reason: crate::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: None,
        };
        crate::runtime::effect::emit_llm_trace_failed(
            &sink,
            &TraceContext::default(),
            TraceContext::default().for_session("llm-retry-session"),
            crate::runtime::effect::LlmTraceFailure::from(&error),
            None,
            Some(&record),
            &crate::facade_support::SystemClock,
        );
        let emitted: TraceRecord = serde_json::from_str(
            std::fs::read_to_string(path)
                .expect("read LLM trace")
                .trim(),
        )
        .expect("parse emitted LLM trace");
        let TraceEvent::LlmCallFailed { attempts, .. } = emitted.event else {
            panic!("expected emitted LLM failure");
        };
        let ladder = attempts.expect("emitted attempt ladder");
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].ordinal, 1);
        assert_eq!(ladder[0].outcome, "failed");
        assert!(ladder[0].reason.as_deref().is_some_and(|reason| {
            reason.contains("rate_limited")
                && reason.contains("http 429")
                && reason.contains("rate_limit_exceeded")
                && reason.contains("provider_retry_after")
        }));
        assert_eq!(ladder[0].delay_ms, Some(250));
        assert_eq!(ladder[1].ordinal, 2);
        assert_eq!(ladder[1].outcome, "completed");
        assert_eq!(ladder[1].delay_ms, None);
    }

    #[test]
    fn store_integrity_classes_emit_at_the_runtime_boundary_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("store.trace.jsonl");
        let sink: Arc<dyn TraceSink> = Arc::new(lash_trace::JsonlTraceSink::new(&path));
        let sink = Some(sink);
        let clock = crate::facade_support::SystemClock;
        let context = TraceContext::default().for_session("corrupt-session");

        emit_store_error(
            &sink,
            &TraceContext::default(),
            context.clone(),
            "session_restore",
            &crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: "invalid json".to_string(),
            },
            &clock,
        );
        emit_store_error(
            &sink,
            &TraceContext::default(),
            context.clone(),
            "turn_commit",
            &crate::StoreError::MonotonicCounterOverflow {
                counter: "head_revision",
                current: i64::MAX as u64,
            },
            &clock,
        );
        emit_store_error(
            &sink,
            &TraceContext::default(),
            context,
            "turn_commit",
            &crate::StoreError::Backend("transient".to_string()),
            &clock,
        );

        let lines = std::fs::read_to_string(path).expect("trace file");
        let records = lines
            .lines()
            .map(|line| serde_json::from_str::<TraceRecord>(line).expect("trace record"))
            .collect::<Vec<_>>();
        assert_eq!(
            records.len(),
            2,
            "ordinary backend failures stay out of this evidence class"
        );
        assert!(matches!(
            &records[0].event,
            TraceEvent::StoreErrorObserved { error_class, .. }
                if error_class == "StoredDataCorrupt"
        ));
        assert!(matches!(
            &records[1].event,
            TraceEvent::StoreErrorObserved { error_class, .. }
                if error_class == "MonotonicCounterOverflow"
        ));
    }
}
