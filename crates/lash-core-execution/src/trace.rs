/// version_surface = "coexist"
/// version_guard(items(LASH_COMPOSITION_TOOL_DOMAIN_VERSION, composition_tool_fingerprint))
const LASH_COMPOSITION_TOOL_DOMAIN_VERSION: &str = "lash-composition-tool/v2";

/// version_surface = "coexist"
/// version_guard(items(LASH_MODEL_FACING_COMPOSITION_DOMAIN_VERSION, trace_composition_key))
const LASH_MODEL_FACING_COMPOSITION_DOMAIN_VERSION: &str = "lash-model-facing-composition/v3";

use lash_trace::{
    TraceAttachment, TraceContentBlock, TraceContext, TraceEvent, TraceLlmMessage, TraceLlmRequest,
    TraceRetryAttempt, TraceRetryAttemptDetail, TraceTokenUsage, TraceToolAttemptOutcome,
    TraceToolResultBlock, TraceToolSpec, llm_node_id, session_node_id, sha256_hex, tool_node_id,
    turn_node_id,
};

use crate::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmOutputPart, LlmOutputSpec, LlmRequest,
    LlmRole, LlmToolChoice, LlmToolSpec,
};
use crate::session_model::TokenUsage;
use crate::{ToolCallOutcome, ToolCallOutput};
use lash_sansio::core_support::Blake3DomainHasher;

/// LLM-call trace projections, re-exported at their original path from
/// `lash-core-llm`, which owns the provider records they project.
pub use lash_core_llm::trace::{trace_llm_attempts, trace_llm_response, trace_usage_from_llm};

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static COMPOSITION_SCHEMA_SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "testing"))]
pub fn composition_schema_serialization_count() -> usize {
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(std::cell::Cell::get)
}

mod boundary;
pub(crate) mod wait_receipts;
pub use boundary::TraceBoundaryReceipt;
pub(crate) mod run_receipts;
mod runtime;
pub use run_receipts::RunRecordObserver;
pub use runtime::{
    JournalFrontier, LiveStep, StepIssue, TraceEmitter, TraceRuntime, TraceStanding,
    tool_trace_scope,
};

/// Invocation-owned identity is authoritative, including absent fields; host-owned run
/// metadata and an explicit host parent remain intact.
fn merge_runtime_projection(base: &TraceContext, projection: TraceContext) -> TraceContext {
    let explicit_parent = base.parent_graph_node_id.clone();
    let projected_parent = projection.parent_graph_node_id.clone();
    let projected_session = projection.session_id.clone();
    let projected_turn = projection.turn_id.clone();
    let projected_graph_node = projection.graph_node_id.clone();
    let projected_turn_index = projection.turn_index;
    let projected_protocol_iteration = projection.protocol_iteration;
    let projected_effect = projection.effect_id.clone();
    let projected_llm_call = projection.llm_call_id.clone();

    let mut merged = base.clone();
    merged.metadata.remove("replay_key");
    merged.metadata.remove("caused_by");
    merge_context(&mut merged, projection);
    merged.session_id = projected_session;
    merged.turn_id = projected_turn;
    merged.graph_node_id = projected_graph_node;
    merged.parent_graph_node_id = explicit_parent.or(projected_parent);
    merged.turn_index = projected_turn_index;
    merged.protocol_iteration = projected_protocol_iteration;
    merged.effect_id = projected_effect;
    merged.llm_call_id = projected_llm_call;
    merged
}

/// Emit evidence only for store failures whose typed class means persisted
/// state is corrupt or a monotonic durable identity cannot advance.
pub fn emit_store_error(
    standing: &TraceStanding,
    context: TraceContext,
    operation: &str,
    error: &crate::StoreError,
) {
    let error_class = if let crate::StoreError::StoredDataCorrupt { .. } = error {
        lash_trace::TraceStoreErrorClass::StoredDataCorrupt
    } else if let crate::StoreError::MonotonicCounterOverflow { .. } = error {
        lash_trace::TraceStoreErrorClass::MonotonicCounterOverflow
    } else {
        return;
    };
    standing.observe(|| {
        (
            context,
            TraceEvent::StoreErrorObserved {
                operation: operation.to_string(),
                error_class,
                message: error.to_string(),
            },
        )
    });
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
    let session_node = context.session_id.as_ref().map(session_node_id);
    let turn_node = turn_node_id(context);

    match event {
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
        | TraceEvent::ToolCallCompleted { call_id, .. }
        | TraceEvent::ToolReceipt { call_id, .. } => {
            set_span(context, Some(tool_node_id(call_id.as_str())), turn_node);
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
        | TraceEvent::AttachmentDegraded { .. }
        | TraceEvent::CompositionChanged { .. }
        | TraceEvent::CompactionNeeded { .. }
        | TraceEvent::PromptViewAttachmentsPruned { .. }
        | TraceEvent::EffectEnvelopeDiff { .. }
        | TraceEvent::ProtocolStep { .. }
        | TraceEvent::ExecCodeStarted { .. }
        | TraceEvent::ExecCodeCompleted { .. }
        | TraceEvent::ExecCodeFailed { .. }
        | TraceEvent::ObservationProjection { .. }
        | TraceEvent::JournaledEffectStarted { .. }
        | TraceEvent::JournaledEffectSettled { .. }
        | TraceEvent::DurableWaitParked { .. }
        | TraceEvent::DurableWaitResolved { .. }
        | TraceEvent::DurableTimerStarted { .. }
        | TraceEvent::DurableTimerResolved { .. }
        | TraceEvent::DurableSegmentBoundary { .. }
        | TraceEvent::StoreErrorObserved { .. } => set_span(context, None, turn_node),
        TraceEvent::CompactionStarted { .. } | TraceEvent::CompactionCompleted { .. } => {
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
    if context.parent_graph_node_id.is_none()
        && let Some(parent_id) = parent_id
        && context.graph_node_id.as_deref() != Some(parent_id.as_str())
    {
        context.parent_graph_node_id = Some(parent_id);
    }
}

/// The `Turn` / `ToolCall` arms intentionally mirror [`turn_node_id`] / [`tool_node_id`] so
/// the cross-session parent reference resolves to a real span.
#[expect(
    clippy::expect_used,
    reason = "a `CausalRef` is an enum of validated string identities, whose serialization has no failing case"
)]
fn causal_node_id(caused_by: &crate::CausalRef) -> String {
    match caused_by {
        crate::CausalRef::Turn {
            session_id,
            turn_id,
        } => format!("turn:{session_id}:{turn_id}"),
        crate::CausalRef::Effect { address } => address.graph_key(),
        crate::CausalRef::ToolCall { call_id, .. } => format!("tool:{call_id}"),
        crate::CausalRef::Process { process_id } => format!("process:{process_id}"),
        crate::CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => format!("process:{process_id}:{sequence}"),
        crate::CausalRef::TriggerOccurrence { .. } => format!(
            "trigger:{}",
            serde_json::to_string(caused_by).expect("causal references serialize")
        ),
        crate::CausalRef::SessionNode {
            session_id,
            node_id,
        } => format!("node:{session_id}:{node_id}"),
    }
}

pub fn trace_context_from_invocation(invocation: &crate::RuntimeInvocation) -> TraceContext {
    trace_context_for_invocation(TraceContext::default(), invocation)
}

pub fn trace_context_for_invocation(
    context: TraceContext,
    invocation: &crate::RuntimeInvocation,
) -> TraceContext {
    trace_context_for_invocation_parts(
        context,
        &invocation.attribution,
        invocation.effect_id(),
        invocation.effect_replay_key(),
        invocation.caused_by.as_ref(),
    )
}

pub fn trace_context_from_effect_invocation(
    invocation: &crate::RuntimeEffectInvocation,
) -> TraceContext {
    trace_context_for_effect_invocation(TraceContext::default(), invocation)
}

pub fn trace_context_for_effect_invocation(
    context: TraceContext,
    invocation: &crate::RuntimeEffectInvocation,
) -> TraceContext {
    trace_context_for_invocation_parts(
        context,
        &invocation.attribution,
        Some(invocation.effect_id()),
        Some(invocation.effect_replay_key()),
        invocation.caused_by.as_ref(),
    )
}

fn trace_context_for_invocation_parts(
    mut context: TraceContext,
    attribution: &crate::RuntimeAttribution,
    effect_id: Option<&str>,
    replay_key: Option<&str>,
    caused_by: Option<&crate::CausalRef>,
) -> TraceContext {
    // Invocation identity replaces any ambient identity on the host context.
    context.session_id = attribution.session_id.clone();
    context.turn_id = attribution.turn_id.clone();
    context.turn_index = attribution.turn_index;
    context.protocol_iteration = attribution.protocol_iteration;
    context.effect_id = effect_id.map(str::to_string);
    context.metadata.remove("replay_key");
    context.metadata.remove("caused_by");
    if let Some(replay_key) = replay_key {
        context
            .metadata
            .insert("replay_key".to_string(), serde_json::json!(replay_key));
    }
    if let Some(caused_by) = caused_by {
        context = trace_context_with_causal_ref(context, caused_by);
    }
    if context.parent_graph_node_id.is_none()
        && let (Some(session_id), Some(turn_id)) = (
            attribution.session_id.as_ref(),
            attribution.turn_id.as_ref(),
        )
    {
        context.parent_graph_node_id = Some(format!("turn:{session_id}:{turn_id}"));
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
    if context.parent_graph_node_id.is_none() {
        context.parent_graph_node_id = Some(causal_node_id(caused_by));
    }
    context
}

pub(crate) fn trace_llm_request(req: &LlmRequest) -> TraceLlmRequest {
    TraceLlmRequest {
        model: req.model.wire_model().to_string(),
        model_variant: match &req.model.reasoning {
            crate::ReasoningSelection::ProviderDefault => None,
            crate::ReasoningSelection::Disabled => Some("disabled".to_string()),
            crate::ReasoningSelection::Effort(effort) => Some(effort.clone()),
        },
        messages: req.messages.iter().map(trace_llm_message).collect(),
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

#[expect(
    clippy::expect_used,
    reason = "a `SchemaContract` is a JSON document already held in memory, whose re-serialization has no failing case"
)]
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

struct CompositionHashWriter(Blake3DomainHasher);

impl std::io::Write for CompositionHashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[expect(
    clippy::expect_used,
    reason = "the tool contract serializes into an in-memory hasher, so neither the value nor the writer can fail"
)]
pub(crate) fn composition_tool_fingerprint(tool: &LlmToolSpec) -> [u8; 32] {
    #[cfg(any(test, feature = "testing"))]
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    let mut writer = CompositionHashWriter(Blake3DomainHasher::new(
        LASH_COMPOSITION_TOOL_DOMAIN_VERSION,
    ));
    serde_json::to_writer(&mut writer, tool)
        .expect("model-facing tool contract serialization is infallible");
    writer.0.finalize()
}

pub fn trace_composition_key(req: &LlmRequest, tool_fingerprints: &[[u8; 32]]) -> [u8; 32] {
    debug_assert_eq!(req.tools.len(), tool_fingerprints.len());
    let mut hash = Blake3DomainHasher::new(LASH_MODEL_FACING_COMPOSITION_DOMAIN_VERSION);
    hash.update([u8::from(req.instructions.is_some())]);
    hash.update(
        req.model
            .metadata()
            .capability
            .instruction_role
            .as_str()
            .as_bytes(),
    );
    if let Some(text) = &req.instructions {
        hash.update(text.len().to_le_bytes());
        hash.update(text.as_bytes());
    }
    hash.update(tool_fingerprints.len().to_le_bytes());
    for fingerprint in tool_fingerprints {
        hash.update(fingerprint);
    }
    hash.finalize()
}

pub struct CompositionTraceSnapshot {
    pub fingerprint: String,
    pub rendered_system_prompt: String,
    pub tool_schemas: Vec<TraceToolSpec>,
}

pub fn trace_composition_snapshot(
    req: &LlmRequest,
    fingerprint: [u8; 32],
) -> CompositionTraceSnapshot {
    #[cfg(any(test, feature = "testing"))]
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    let rendered_system_prompt = req.instructions.as_deref().unwrap_or_default().to_owned();
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
        LlmContentBlock::Attachment { source } => TraceContentBlock::Attachment {
            source: Box::new(trace_attachment(source)),
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
            content: content
                .iter()
                .map(|part| match part {
                    crate::ModelToolReturnPart::Text { text } => {
                        TraceToolResultBlock::Text { text: text.clone() }
                    }
                    // A trace shows what the model was shown: the witness.
                    crate::ModelToolReturnPart::Retained(retained) => TraceToolResultBlock::Text {
                        text: retained.witness.clone(),
                    },
                    crate::ModelToolReturnPart::Attachment(source) => {
                        TraceToolResultBlock::Attachment {
                            source: Box::new(trace_attachment(source)),
                        }
                    }
                })
                .collect(),
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

pub fn trace_usage_from_session(usage: &TokenUsage) -> TraceTokenUsage {
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

pub(crate) fn trace_tool_attempt(
    ordinal: u32,
    record: &crate::ToolCallRecord,
    delay_ms: Option<u64>,
) -> TraceRetryAttempt {
    let outcome = match &record.output.outcome {
        crate::ToolCallOutcome::Success(_) => TraceToolAttemptOutcome::Completed,
        crate::ToolCallOutcome::Failure(failure) => TraceToolAttemptOutcome::Failed {
            class: failure.class.clone(),
            code: failure.code.clone(),
            message: failure.message.clone(),
            source: failure.source.clone(),
            retry: failure.retry.clone(),
        },
        crate::ToolCallOutcome::Cancelled(cancellation) => TraceToolAttemptOutcome::Cancelled {
            message: cancellation.message.clone(),
            source: cancellation.source.clone(),
            origin: cancellation.origin,
        },
    };
    TraceRetryAttempt {
        ordinal,
        delay_ms,
        detail: TraceRetryAttemptDetail::Tool { outcome },
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
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod span_identity_tests {
    use super::*;
    use std::sync::Arc;

    fn turn_scope(started_at_ms: u64) -> lash_trace::DurableTraceScope {
        lash_trace::DurableTraceScope {
            scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                session_id: "session".into(),
                turn_id: "turn".into(),
            }),
            cause: lash_trace::TraceCause::Root,
            anchor: lash_trace::TraceAnchor::Untraced,
            started_at_ms,
        }
    }

    #[test]
    fn a_live_body_cannot_authorize_a_logical_terminal() {
        let directory = tempfile::tempdir().expect("trace directory");
        let path = directory.path().join("terminal.jsonl");
        let runtime = TraceRuntime::default()
            .with_trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(&path)));
        let standing = runtime.unreplayed(Some(turn_scope(100)));
        standing.transition(
            standing.body_permit(),
            200,
            lash_trace::TraceTransitionKind::Terminal,
            0,
            || {
                (
                    TraceContext::default(),
                    TraceEvent::TurnCompleted {
                        outcome: lash_trace::TraceTurnOutcome::Completed {
                            done_reason: lash_trace::TraceTurnCompletionReason::AssistantMessage,
                        },
                    },
                )
            },
        );
        runtime.flush().expect("flush");
        assert!(
            !path.exists(),
            "a live attempt is not a committed terminal receipt"
        );
    }

    #[test]
    fn replay_only_conclusion_builds_no_records_and_reads_no_clock() {
        #[derive(Debug)]
        struct SpyClock(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl crate::Clock for SpyClock {
            fn now(&self) -> std::time::Instant {
                panic!("denied observation read the clock")
            }
            fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                chrono::DateTime::UNIX_EPOCH
            }
            async fn sleep(&self, _: std::time::Duration) {
                panic!("unexpected sleep")
            }
            async fn sleep_until(&self, _: std::time::Instant) {
                panic!("unexpected sleep")
            }
        }
        let clock_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let constructions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let directory = tempfile::tempdir().expect("trace directory");
        let path = directory.path().join("replayed.jsonl");
        let runtime = TraceRuntime::new(Arc::new(SpyClock(clock_reads.clone())))
            .with_trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(&path)));
        let controller = crate::testing::UnavailableEffectController;
        let scoped = crate::ScopedEffectController::borrowed(
            &controller,
            crate::AdmittedScope::turn("session", "turn"),
        )
        .expect("scope");
        let standing = runtime.shift(Some(turn_scope(1)), &scoped);
        standing.observe(|| {
            constructions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                TraceContext::default(),
                TraceEvent::DurableWaitParked {
                    wait_kind: "event".into(),
                },
            )
        });
        let held = constructions.clone();
        standing.observe_deferred(move || {
            held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                TraceContext::default(),
                TraceEvent::DurableWaitParked {
                    wait_kind: "event".into(),
                },
            )
        });
        standing.conclude();
        assert!(!scoped.frontier().is_crossed());
        assert_eq!(constructions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(clock_reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!path.exists());
    }

    fn turn_context() -> TraceContext {
        TraceContext::default()
            .for_session("sess")
            .for_turn_index(0)
            .for_turn("turn-1")
    }

    #[test]
    fn tool_span_parents_under_turn_and_matches_causal_tool_ref() {
        let mut context = turn_context();
        assign_span_identity(
            &mut context,
            &TraceEvent::ToolCallStarted {
                call_id: crate::ToolCallId::fixture("call_abc"),
                provider_call_id: Some("call_abc".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({}),
                issuing_node_id: None,
            },
        );
        let node = format!("tool:{}", crate::ToolCallId::fixture("call_abc"));
        assert_eq!(context.graph_node_id.as_deref(), Some(node.as_str()));
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("turn:sess:turn-1")
        );
        // A subagent caused_by this tool call must resolve to the same node id.
        assert_eq!(
            causal_node_id(&crate::CausalRef::ToolCall {
                session_id: crate::SessionId::from("sess"),
                call_id: crate::ToolCallId::fixture("call_abc"),
            }),
            node
        );
    }

    #[test]
    fn effect_projection_replaces_ambient_identity_but_keeps_host_metadata_and_parent() {
        let mut base = TraceContext::default()
            .for_session("ambient-session")
            .for_turn("ambient-turn")
            .for_turn_index(99)
            .for_protocol_iteration(42);
        base.run_id = Some("host-run".to_string());
        base.parent_graph_node_id = Some("host:explicit-parent".to_string());
        base.metadata
            .insert("host_key".to_string(), serde_json::json!("kept"));
        base.metadata
            .insert("caused_by".to_string(), serde_json::json!({"stale": true}));
        base.metadata
            .insert("replay_key".to_string(), serde_json::json!("stale"));

        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                crate::ExecutionScope::process(crate::process_id_for_test("host-process")),
                "process-step",
            )
            .expect("valid process effect address"),
            crate::RuntimeAttribution::none(),
            "descriptive-label",
        )
        .with_caused_by(Some(crate::CausalRef::Process {
            process_id: crate::process_id_for_test("causal-process"),
        }));

        let context = trace_context_for_effect_invocation(base, &invocation);
        assert_eq!(context.session_id, None);
        assert_eq!(context.turn_id, None);
        assert_eq!(context.turn_index, None);
        assert_eq!(context.protocol_iteration, None);
        assert_eq!(context.run_id.as_deref(), Some("host-run"));
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("host:explicit-parent")
        );
        assert_eq!(
            context.metadata.get("host_key"),
            Some(&serde_json::json!("kept"))
        );
        assert_eq!(
            context.metadata.get("replay_key"),
            Some(&serde_json::json!("process-step"))
        );
        assert_eq!(
            context.metadata.get("caused_by"),
            Some(&serde_json::to_value(invocation.caused_by.as_ref().unwrap()).unwrap())
        );
    }

    #[test]
    fn effect_projection_uses_full_scoped_cause_before_real_turn_fallback() {
        let parent_address = crate::EffectAddress::new(
            crate::ExecutionScope::process(crate::process_id_for_test("parent-process")),
            "shared-replay-key",
        )
        .expect("valid causal effect address");
        let cause = crate::CausalRef::Effect {
            address: parent_address.clone(),
        };
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                crate::ExecutionScope::turn("child-session", "child-turn"),
                "child-key",
            )
            .expect("valid child effect address"),
            crate::RuntimeAttribution::for_turn("child-session", "child-turn", 3, 1),
            "child-effect",
        )
        .with_caused_by(Some(cause));

        let context = trace_context_from_effect_invocation(&invocation);
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some(parent_address.graph_key().as_str())
        );

        let turn_only = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                crate::ExecutionScope::turn("fallback-session", "fallback-turn"),
                "fallback-key",
            )
            .expect("valid fallback effect address"),
            crate::RuntimeAttribution::for_turn("fallback-session", "fallback-turn", 0, 0),
            "fallback-effect",
        );
        let context = trace_context_from_effect_invocation(&turn_only);
        assert_eq!(
            context.parent_graph_node_id.as_deref(),
            Some("turn:fallback-session:fallback-turn")
        );
    }

    #[test]
    fn trigger_trace_parent_distinguishes_equal_occurrence_ids_by_full_cause() {
        let parent = |subscription_id: &str| crate::CausalRef::TriggerOccurrence {
            occurrence_id: "shared-occurrence".to_string(),
            subscription_id: Some(subscription_id.to_string()),
            subscription_incarnation: Some("incarnation".to_string()),
            subscription_revision: Some(7),
        };

        assert_ne!(
            causal_node_id(&parent("subscription-a")),
            causal_node_id(&parent("subscription-b"))
        );
    }

    #[test]
    fn turnless_compaction_record_parents_under_session() {
        let mut context = TraceContext::default().for_session("compact-session");
        assign_span_identity(
            &mut context,
            &TraceEvent::CompactionStarted {
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
                    outcome: crate::AttemptOutcome::Failed,
                    protocol_position: crate::ProtocolPosition::ResponseObserved,
                    retry_budget_consumed: true,
                    retry_decision: Some(crate::RetryDecision::Scheduled {
                        delay: std::time::Duration::from_millis(250),
                        wait: lash_sansio::llm::types::RetryWait::Throttle,
                        class: lash_sansio::llm::types::RetryClass::RejectedHttpResponse,
                    }),
                    error: Some(crate::NormalizedError {
                        class: lash_sansio::llm::types::ProviderFailureKind::Http,
                        code: Some(crate::FailureCode::provider("rate_limit_exceeded")),
                        http_status: Some(429),
                        provider_request_id: None,
                        retry_after: Some(std::time::Duration::from_millis(250)),
                    }),
                    evidence: None,
                    generation_disposition: None,
                    usage: None,
                },
                crate::AttemptRecord {
                    ordinal: 2,
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
        let standing = TraceRuntime::default()
            .with_trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(&path)))
            .unreplayed(None);
        let error = crate::LlmCallError {
            message: "provider attempts exhausted".to_string(),
            retryable: true,
            kind: crate::ProviderFailureKind::Http,
            raw: None,
            code: Some(crate::FailureCode::provider("rate_limit_exceeded")),
            terminal_reason: crate::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: None,
        };
        crate::runtime::effect::emit_llm_trace_failed(
            &standing,
            TraceContext::default().for_session("llm-retry-session"),
            crate::runtime::effect::LlmTraceFailure::from(&error),
            None,
            Some(&record),
        );
        let emitted: lash_trace::TraceRecord = lash_trace::parse_jsonl_records(
            &std::fs::read_to_string(path).expect("read LLM trace"),
        )
        .expect("parse emitted LLM trace")
        .into_iter()
        .next()
        .expect("one emitted LLM trace record");
        let TraceEvent::LlmCallFailed { attempts, .. } = emitted.event else {
            panic!("expected emitted LLM failure");
        };
        let ladder = attempts.expect("emitted attempt ladder");
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].ordinal, 1);
        let TraceRetryAttemptDetail::Llm {
            outcome,
            error: Some(error),
            retry_decision: Some(lash_trace::TraceRetryDecision::Scheduled { wait, class }),
            ..
        } = &ladder[0].detail
        else {
            panic!("typed failed LLM attempt")
        };
        assert_eq!(*outcome, lash_trace::TraceLlmAttemptOutcome::Failed);
        assert_eq!(error.class, crate::ProviderFailureKind::Http);
        assert_eq!(error.http_status, Some(429));
        assert_eq!(
            error.code,
            Some(crate::FailureCode::provider("rate_limit_exceeded"))
        );
        assert_eq!(*wait, lash_trace::TraceRetryWait::Throttle);
        assert_eq!(*class, lash_trace::TraceRetryClass::RejectedHttpResponse);
        assert_eq!(ladder[0].delay_ms, Some(250));
        assert_eq!(ladder[1].ordinal, 2);
        assert!(matches!(
            ladder[1].detail,
            TraceRetryAttemptDetail::Llm {
                outcome: lash_trace::TraceLlmAttemptOutcome::Completed,
                ..
            }
        ));
        assert_eq!(ladder[1].delay_ms, None);
    }

    #[test]
    fn store_integrity_classes_emit_at_the_runtime_boundary_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("store.trace.jsonl");
        let standing = TraceRuntime::default()
            .with_trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(&path)))
            .unreplayed(None);
        let context = TraceContext::default().for_session("corrupt-session");

        emit_store_error(
            &standing,
            context.clone(),
            "session_restore",
            &crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: "invalid json".to_string(),
            },
        );
        emit_store_error(
            &standing,
            context.clone(),
            "turn_commit",
            &crate::StoreError::MonotonicCounterOverflow {
                counter: "head_revision",
                current: i64::MAX as u64,
            },
        );
        emit_store_error(
            &standing,
            context,
            "turn_commit",
            &crate::StoreError::Backend("transient".to_string()),
        );

        let records = lash_trace::parse_jsonl_records::<lash_trace::TraceRecord>(
            &std::fs::read_to_string(path).expect("trace file"),
        )
        .expect("trace records");
        assert_eq!(
            records.len(),
            2,
            "ordinary backend failures stay out of this evidence class"
        );
        assert!(matches!(
            &records[0].event,
            TraceEvent::StoreErrorObserved { error_class, .. }
                if *error_class == lash_trace::TraceStoreErrorClass::StoredDataCorrupt
        ));
        assert!(matches!(
            &records[1].event,
            TraceEvent::StoreErrorObserved { error_class, .. }
                if *error_class == lash_trace::TraceStoreErrorClass::MonotonicCounterOverflow
        ));
    }
}

#[cfg(test)]
#[path = "trace_feedback_tests.rs"]
mod feedback_tests;
