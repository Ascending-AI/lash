use std::sync::Arc;

use lash_trace::{
    TraceAttachment, TraceContentBlock, TraceContext, TraceEvent, TraceLlmMessage, TraceLlmRequest,
    TraceLlmResponse, TraceRecord, TraceSink, TraceTokenUsage, TraceToolSpec, sha256_hex,
};

use crate::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmOutputPart, LlmOutputSpec, LlmRequest,
    LlmRole, LlmToolChoice, LlmUsage,
};
use crate::session_model::TokenUsage;
use crate::{ToolCallOutcome, ToolCallOutput};

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
        | TraceEvent::EffectEnvelopeDiff { .. }
        | TraceEvent::ProtocolStep { .. }
        | TraceEvent::TokenUsage { .. } => set_span(context, None, turn_node),
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
        tools: req
            .tools
            .iter()
            .map(|tool| TraceToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: serde_json::to_value(&tool.input_schema)
                    .unwrap_or(serde_json::Value::Null),
                output_schema: serde_json::to_value(&tool.output_schema)
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect(),
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
    generation_disposition: Option<crate::GenerationDisposition>,
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
                status: "completed".to_string(),
                done_reason: "modelstop".to_string(),
                agent_frame_switch: None,
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
}
