use crate::AttachmentRef;
/// version_surface = "coexist"
/// version_guard(items(LASH_COMPOSITION_TOOL_DOMAIN_VERSION, composition_tool_fingerprint))
const LASH_COMPOSITION_TOOL_DOMAIN_VERSION: &str = "lash-composition-tool/v2";

/// version_surface = "coexist"
/// version_guard(items(LASH_MODEL_FACING_COMPOSITION_DOMAIN_VERSION, trace_composition_key))
const LASH_MODEL_FACING_COMPOSITION_DOMAIN_VERSION: &str = "lash-model-facing-composition/v3";

use lash_trace::{
    TraceAttachment, TraceContentBlock, TraceContext, TraceEvent, TraceLlmMessage, TraceLlmRequest,
    TraceRetryAttempt, TraceRetryAttemptDetail, TraceToolAttemptOutcome, TraceToolResultBlock,
    TraceToolSpec,
};

use crate::llm::types::{
    LlmContentBlock, LlmMessage, LlmOutputSpec, LlmRequest, LlmRole, LlmToolChoice, LlmToolSpec,
};
use crate::{ToolCallOutcome, ToolCallOutput};
use lash_protocol_types::core_support::Blake3DomainHasher;

/// LLM-call trace projections, re-exported at their original path from
/// `lash-core-llm`, which owns the provider records they project.
pub use lash_core_llm::trace::{trace_llm_attempts, trace_llm_response};

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static COMPOSITION_SCHEMA_SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "testing"))]
pub fn composition_schema_serialization_count() -> usize {
    COMPOSITION_SCHEMA_SERIALIZATIONS.with(std::cell::Cell::get)
}

mod boundary;
pub use boundary::TraceBoundaryReceipt;
mod composition;
mod runtime;
pub(crate) use runtime::extension;
pub(crate) use runtime::extension::HOLD_ENTRY_BYTES;
pub(crate) use runtime::extension::{BodySpec, BodyTrace, BoundCallback, SharedHold};
pub use runtime::{
    ExtensionSpan, ExtensionTrace, JournalFrontier, LiveStep, OpenSpan, StepIssue,
    ToolSpanAttributes, TraceEmitter, TraceInstrumented, TraceRuntime, TraceStanding,
    tool_trace_scope,
};

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
    context.replay_key = replay_key.map(str::to_string);
    context.caused_by = caused_by.cloned();
    context
}

/// The request's identity under an omitted content policy, and its messages,
/// tool contracts and output contract as well when content is captured.
pub(crate) fn trace_llm_request(
    req: &LlmRequest,
    content: lash_trace::ContentPolicy,
    composition_fingerprint: String,
) -> TraceLlmRequest {
    TraceLlmRequest {
        model: req.model.wire_model().to_string(),
        model_variant: match &req.model.reasoning {
            crate::ReasoningSelection::ProviderDefault => None,
            crate::ReasoningSelection::Disabled => Some("disabled".to_string()),
            crate::ReasoningSelection::Effort(effort) => Some(effort.clone()),
        },
        system_instructions: content
            .capture(lash_trace::ContentCategory::SystemInstructions, || {
                req.instructions.as_deref().map(str::to_owned)
            }),
        composition_fingerprint,
        messages: content.capture(lash_trace::ContentCategory::InputMessages, || {
            req.messages
                .iter()
                .map(|message| trace_llm_message(message, content))
                .collect()
        }),
        tools: trace_tool_specs(&req.tools, content),
        tool_choice: match req.tool_choice {
            LlmToolChoice::Auto => "auto",
            LlmToolChoice::None => "none",
            LlmToolChoice::Required => "required",
        }
        .to_string(),
        output_spec: req
            .output_spec
            .as_ref()
            .map(|spec| trace_output_spec(spec, content)),
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

/// The offered tools by name, with their contracts when content is captured.
fn trace_tool_specs(
    tools: &[LlmToolSpec],
    content: lash_trace::ContentPolicy,
) -> Vec<TraceToolSpec> {
    if content.captures(lash_trace::ContentCategory::ToolDefinitions) {
        return tools.iter().map(trace_tool_spec).collect();
    }
    tools
        .iter()
        .map(|tool| TraceToolSpec {
            name: tool.name.clone(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        })
        .collect()
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
    content: lash_trace::ContentPolicy,
) -> CompositionTraceSnapshot {
    #[cfg(any(test, feature = "testing"))]
    if content.captures(lash_trace::ContentCategory::SystemInstructions) {
        COMPOSITION_SCHEMA_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
    }
    let rendered_system_prompt = content
        .capture(lash_trace::ContentCategory::SystemInstructions, || {
            req.instructions.as_deref().unwrap_or_default().to_owned()
        });
    let tool_schemas = trace_tool_specs(&req.tools, content);
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

/// The call's result under the content policy. A success payload and the
/// control are built only when content is captured; a failure or cancellation
/// keeps its typed classification without serializing omitted diagnostics.
pub(crate) fn trace_tool_call_output(
    output: &ToolCallOutput,
    content: lash_trace::ContentPolicy,
) -> lash_trace::TraceToolCallOutput {
    let outcome = match &output.outcome {
        ToolCallOutcome::Success(value) => lash_trace::TraceToolCallOutcome::Success(
            content.capture(lash_trace::ContentCategory::ToolResults, || {
                value.to_json_value()
            }),
        ),
        ToolCallOutcome::Failure(failure) => lash_trace::TraceToolCallOutcome::Failure(
            lash_trace::TraceToolFailure::new(failure, content),
        ),
        ToolCallOutcome::Cancelled(cancellation) => lash_trace::TraceToolCallOutcome::Cancelled(
            lash_trace::TraceToolCancellation::new(cancellation, content),
        ),
    };
    lash_trace::TraceToolCallOutput {
        outcome,
        control: content.capture(lash_trace::ContentCategory::ToolResults, || {
            output
                .control
                .as_ref()
                .and_then(|control| serde_json::to_value(crate::Durable(control)).ok())
        }),
    }
}

/// Preserve the cell's outcome and control identity without copying omitted content.
pub fn trace_cell_outcome(
    result: &crate::CellOutcome,
    content: lash_trace::ContentPolicy,
) -> crate::CellOutcome {
    use lash_protocol_types::{CellControl, CellOutcome, OutputValue};
    match result {
        CellOutcome::Completed => CellOutcome::Completed,
        CellOutcome::Failed(failure) => CellOutcome::Failed(trace_cell_failure(failure, content)),
        CellOutcome::Controlled {
            tool_name,
            call_id,
            control,
        } => CellOutcome::Controlled {
            tool_name: tool_name.clone(),
            call_id: call_id.clone(),
            control: match control {
                CellControl::Finish { value } => CellControl::Finish {
                    value: if content.captures(lash_trace::ContentCategory::CodeOutput) {
                        value.clone()
                    } else {
                        OutputValue::Inline(serde_json::Value::Null)
                    },
                },
                CellControl::SwitchAgentFrame { frame_key } => CellControl::SwitchAgentFrame {
                    frame_key: frame_key.clone(),
                },
            },
        },
    }
}

/// Build a cell's classification without copying omitted diagnostics or repairs.
fn trace_cell_failure(
    failure: &crate::CellFailure,
    content: lash_trace::ContentPolicy,
) -> crate::CellFailure {
    if content.captures(lash_trace::ContentCategory::CodeOutput) {
        return failure.clone();
    }
    use lash_kernel_value::json_schema::SchemaAdmissionError;
    crate::CellFailure {
        kind: failure.kind,
        message: String::new(),
        worker_limit: failure.worker_limit,
        tool_call_limit: failure.tool_call_limit,
        exec_failure: failure.exec_failure,
        bound: failure.bound.clone(),
        read_panic: failure.read_panic.as_ref().map(|panic| {
            Box::new(lash_protocol_types::ReadPanic {
                tool_name: panic.tool_name.clone(),
                message: String::new(),
            })
        }),
        defect: failure.defect.clone(),
        schema_admission: failure.schema_admission.as_deref().map(|error| {
            Box::new(match error {
                SchemaAdmissionError::InvalidKind { actual } => {
                    SchemaAdmissionError::InvalidKind { actual: *actual }
                }
                SchemaAdmissionError::Compilation { schema_path, .. } => {
                    SchemaAdmissionError::Compilation {
                        schema_path: schema_path.clone(),
                        message: String::new(),
                    }
                }
                SchemaAdmissionError::NonLocalReference { schema_path, .. } => {
                    SchemaAdmissionError::NonLocalReference {
                        schema_path: schema_path.clone(),
                        reference: String::new(),
                    }
                }
                SchemaAdmissionError::Unprojectable { schema_path, .. } => {
                    SchemaAdmissionError::Unprojectable {
                        schema_path: schema_path.clone(),
                        message: String::new(),
                    }
                }
            })
        }),
        feedback: failure
            .feedback
            .iter()
            .map(|feedback| lash_protocol_types::CellFailureRepair {
                plugin_id: feedback.plugin_id.clone(),
                callback: feedback.callback.clone(),
                repairs: Vec::new(),
            })
            .collect(),
    }
}

fn trace_llm_message(message: &LlmMessage, content: lash_trace::ContentPolicy) -> TraceLlmMessage {
    TraceLlmMessage {
        role: match message.role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "system",
        }
        .to_string(),
        blocks: message
            .blocks
            .iter()
            .filter(|block| {
                content.captures(lash_trace::ContentCategory::Reasoning)
                    || !matches!(block, LlmContentBlock::Reasoning { .. })
            })
            .map(trace_content_block)
            .collect(),
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
        LlmContentBlock::Attachment { reference } => TraceContentBlock::Attachment {
            reference: Box::new(trace_attachment(reference, "message")),
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
            ..
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
                    crate::ModelToolReturnPart::Attachment(reference) => {
                        TraceToolResultBlock::Attachment {
                            reference: Box::new(trace_attachment(reference, "tool_result")),
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

fn trace_attachment(attachment: &AttachmentRef, position: &str) -> TraceAttachment {
    TraceAttachment {
        id: attachment.id.to_string(),
        media_type: attachment.media_type.to_string(),
        byte_len: attachment.byte_len,
        position: position.to_string(),
        delivery_form: None,
    }
}

fn trace_output_spec(
    spec: &LlmOutputSpec,
    content: lash_trace::ContentPolicy,
) -> serde_json::Value {
    match spec {
        LlmOutputSpec::JsonObject => serde_json::json!({ "type": "json_object" }),
        LlmOutputSpec::JsonSchema(schema) => serde_json::json!({
            "type": "json_schema",
            "name": schema.name,
            "schema": content.capture(lash_trace::ContentCategory::OutputSchema,||serde_json::to_value(&schema.schema).unwrap_or_default()),
            "strict": schema.strict,
        }),
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
            suggested_delay_ms: failure.suggested_delay_ms,
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

/// Construct a language observation with the same occurrence identity for every owner.
/// Caller attribution and count accumulation retain their own semantics.
pub fn observe_language(
    tracing: &crate::plugin::PluginExecutionTrace,
    identity: &lash_trace::TraceLanguageExecutionIdentity,
    payload: lash_trace::TraceLanguageExecutionPayload,
    mut context: TraceContext,
    language: Option<String>,
) {
    use lash_trace::{TraceLanguageExecutionPayload as Payload, TraceNodeFact};
    let (suffix, node) = match &payload {
        Payload::ExecutionStarted { .. } => ("started".to_owned(), None),
        Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
        Payload::Counts { counts } => (counts.event_key(identity), None),
        Payload::Node { at, fact, .. } => {
            let occurrence = serde_json::to_string(at).unwrap_or_default();
            let transition = match fact {
                TraceNodeFact::BranchSelected { .. } => "branch",
                TraceNodeFact::ChildStarted { .. } => "child",
                TraceNodeFact::Origin { .. } => "origin",
                TraceNodeFact::Started { .. } => "started",
                TraceNodeFact::Completed { .. } => "completed",
                TraceNodeFact::Failed { .. } => "failed",
                TraceNodeFact::Cancelled => "cancelled",
                TraceNodeFact::Waiting { .. } => "waiting",
                TraceNodeFact::Resumed { .. } => "resumed",
            };
            (
                format!("node:{occurrence}:{transition}"),
                Some(at.position().to_string()),
            )
        }
    };
    let event_key = if matches!(&payload, Payload::Counts { .. }) {
        suffix
    } else {
        format!(
            "kernel_execution:{}:{suffix}",
            identity.graph_key(&payload.subject())
        )
    };
    context.graph_node_id = node;
    let event = lash_trace::TraceLanguageExecution {
        event_key,
        identity: identity.clone(),
        payload,
    };
    tracing.observe_language(&event.event_key, || {
        (
            context.clone(),
            TraceEvent::LanguageExecution {
                language: language.clone(),
                event: event.clone(),
            },
        )
    });
}

#[cfg(test)]
mod span_identity_tests {
    use super::*;
    use std::sync::Arc;

    fn turn_scope(started_at_ms: u64) -> lash_trace::DurableTraceScope {
        lash_trace::DurableTraceScope {
            scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                session_id: "session".into(),
                turn_id: "turn".into(),
            }),
            cause: lash_trace::TraceCause::None,
            links: lash_trace::TraceLinks::new(),
            span: None,
            started_at_ms,
        }
    }

    #[test]
    fn a_live_body_cannot_authorize_a_logical_terminal() {
        let sink = Arc::new(lash_trace::RecordingTraceSink::new());
        let runtime = TraceRuntime::default().with_trace_sink(sink.clone());
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
                        profile: None,
                        output: Vec::new(),
                        input: Vec::new(),
                        inference_calls: None,
                        tool_calls: None,
                        inputs: 1,
                        outcome: lash_trace::TraceTurnOutcome::Completed {
                            done_reason: lash_trace::TraceTurnCompletionReason::AssistantMessage,
                        },
                    },
                )
            },
        );
        runtime.flush().expect("flush");
        assert!(
            sink.records().is_empty(),
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
        let sink = Arc::new(lash_trace::RecordingTraceSink::new());
        let runtime = TraceRuntime::new(Arc::new(SpyClock(clock_reads.clone())))
            .with_trace_sink(sink.clone());
        let scoped = crate::ActorContext::unavailable()
            .scoped(crate::AdmittedScope::turn("session", "turn"))
            .expect("scope");
        let standing = runtime.shift(Some(turn_scope(1)), &scoped);
        standing.observe(|| {
            constructions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                TraceContext::default(),
                TraceEvent::CompactionCompleted { summary_nodes: 0 },
            )
        });
        let held = constructions.clone();
        standing.observe_deferred(move || {
            held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                TraceContext::default(),
                TraceEvent::CompactionCompleted { summary_nodes: 0 },
            )
        });
        standing.conclude();
        assert!(!scoped.frontier().is_crossed());
        assert_eq!(constructions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(clock_reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(sink.records().is_empty());
    }

    /// An effect invocation's record carries the effect's replay key and
    /// cause as typed fields, and its own identity over any ambient one.
    #[test]
    fn effect_projection_types_the_replay_key_and_cause_over_ambient_identity() {
        let base = TraceContext::default()
            .for_session("ambient-session")
            .for_turn("ambient-turn")
            .for_turn_index(99)
            .for_protocol_iteration(42);
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
        assert_eq!(context.replay_key.as_deref(), Some("process-step"));
        assert_eq!(context.caused_by, invocation.caused_by);
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
                        wait: lash_protocol_types::llm::types::RetryWait::Throttle,
                        class: lash_protocol_types::llm::types::RetryClass::RejectedHttpResponse,
                    }),
                    error: Some(crate::NormalizedError {
                        stream_evidence: None,
                        transport: None,
                        class: lash_protocol_types::llm::types::ProviderFailureKind::Http,
                        code: Some(crate::FailureCode::provider("rate_limit_exceeded")),
                        http_status: Some(429),
                        provider_request_id: None,
                        retry_after: Some(std::time::Duration::from_millis(250)),
                    }),
                    evidence: None,
                    generation_disposition: None,
                    usage: Some(crate::LlmUsage {
                        input_tokens: 7,
                        output_tokens: 0,
                        ..crate::LlmUsage::default()
                    }),
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
                    usage: Some(crate::LlmUsage {
                        input_tokens: 11,
                        output_tokens: 5,
                        ..crate::LlmUsage::default()
                    }),
                },
            ],
        };

        let sink = Arc::new(lash_trace::RecordingTraceSink::new());
        let standing = TraceRuntime::default()
            .with_trace_sink(sink.clone())
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
        // Unscoped, the call opens no span: its records are unpositioned.
        let call = standing.open(lash_trace::TraceSpanKind::Model, || {
            (
                TraceContext::default(),
                TraceEvent::CompactionCompleted { summary_nodes: 0 },
            )
        });
        crate::runtime::effect::emit_llm_trace_failed(
            call,
            TraceContext::default().for_session("llm-retry-session"),
            crate::runtime::effect::LlmTraceFailure::from(&error),
            None,
            Some(&record),
        );
        let emitted = sink
            .records()
            .into_iter()
            .find(|record| matches!(record.event, TraceEvent::LlmCallFailed { .. }))
            .expect("one emitted LLM trace record");
        let TraceEvent::LlmCallFailed { attempts, .. } = emitted.event else {
            panic!("expected emitted LLM failure");
        };
        // The emitted ladder is the sealed record, attempt for attempt: each
        // ordinal, the retry decision with its delay, and reported usage.
        let ladder = attempts.expect("emitted attempt ladder");
        assert_eq!(
            ladder.iter().map(|a| &a.attempt).collect::<Vec<_>>(),
            record.attempts.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            ladder
                .iter()
                .map(|attempt| attempt.ordinal)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            ladder[0]
                .retry_decision
                .as_ref()
                .and_then(|decision| decision.delay()),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(ladder[1].retry_decision, None);
        assert_eq!(
            ladder
                .iter()
                .map(|attempt| attempt.usage.as_ref().map(|usage| usage.input_tokens))
                .collect::<Vec<_>>(),
            [Some(7), Some(11)]
        );
    }

    #[test]
    fn store_integrity_classes_emit_at_the_runtime_boundary_only() {
        let sink = Arc::new(lash_trace::RecordingTraceSink::new());
        let standing = TraceRuntime::default()
            .with_trace_sink(sink.clone())
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

        let records = sink.records();
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
