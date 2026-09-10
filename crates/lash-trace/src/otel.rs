#[path = "otel/attribute_keys.rs"]
mod attribute_keys;
use attribute_keys as attr;

#[cfg(feature = "otel")]
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use opentelemetry::trace::{
    Span, SpanContext, SpanKind, Status, TraceContextExt, Tracer, TracerProvider,
};
use opentelemetry::{Context, InstrumentationScope, KeyValue, Value as OtelValue, global};
use serde_json::Value;

use crate::{TraceContext, TraceEvent, TraceRecord, TraceSink, TraceSinkError, TraceTokenUsage};

mod metrics;
#[doc(hidden)]
pub use metrics::{RuntimeTuningMetrics, ToolIntentMetrics, WorkerCapacityMetrics};

const INSTRUMENTATION_NAME: &str = "lash-trace";

/// Controls which structured Lash trace data is attached to OpenTelemetry
/// spans.
#[derive(Clone, Debug)]
pub struct OtelTraceOptions {
    /// Attach the full Lash trace event as a JSON attribute. This is useful for
    /// local collectors and debugging, but it can exceed backend attribute
    /// limits in production.
    pub include_event_json: bool,
    /// Attach `TraceContext.metadata` as `lash.metadata.*` attributes.
    pub include_context_metadata: bool,
    /// Attach event payload fields that can be large, such as tool args/results
    /// and custom payloads, as compact JSON attributes.
    pub include_payload_json: bool,
}

impl Default for OtelTraceOptions {
    fn default() -> Self {
        Self {
            include_event_json: false,
            include_context_metadata: true,
            include_payload_json: false,
        }
    }
}

pub struct OtelTraceSink<T = global::BoxedTracer>
where
    T: Tracer + Send + Sync,
    T::Span: Send + Sync + 'static,
{
    tracer: T,
    options: OtelTraceOptions,
    active: Mutex<HashMap<String, ActiveSpan<T::Span>>>,
}

struct ActiveSpan<S: Span> {
    span: S,
    context: SpanContext,
}

impl OtelTraceSink<global::BoxedTracer> {
    /// Build a sink from the process-global OpenTelemetry tracer provider.
    ///
    /// This keeps exporter/provider setup with the embedding host while giving
    /// Lash a ready-to-install `TraceSink`.
    pub fn from_global_provider() -> Self {
        let scope = InstrumentationScope::builder(INSTRUMENTATION_NAME)
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        Self::new(global::tracer_provider().tracer_with_scope(scope))
    }
}

impl<T> OtelTraceSink<T>
where
    T: Tracer + Send + Sync,
    T::Span: Send + Sync + 'static,
{
    pub fn new(tracer: T) -> Self {
        Self::with_options(tracer, OtelTraceOptions::default())
    }

    pub fn with_options(tracer: T, options: OtelTraceOptions) -> Self {
        Self {
            tracer,
            options,
            active: Mutex::new(HashMap::new()),
        }
    }

    pub fn options(&self) -> &OtelTraceOptions {
        &self.options
    }

    fn start_active(
        &self,
        key: String,
        record: &TraceRecord,
        name: impl Into<std::borrow::Cow<'static, str>>,
    ) {
        let parent = if matches!(&record.event, TraceEvent::TurnStarted { .. }) {
            None
        } else {
            parent_for(record, &self.active)
        };
        let mut span = self.build_span(record, name, parent, record_time(record), None);
        span.set_attributes(self.attributes_for(record));
        let context = span.span_context().clone();
        let mut active = self.active.lock_recover();
        if let Some(mut existing) = active.remove(&key) {
            existing.span.end_with_timestamp(record_time(record));
        }
        active.insert(key, ActiveSpan { span, context });
    }

    fn end_active(&self, key: &str, record: &TraceRecord, success: bool) -> bool {
        let mut active = self.active.lock_recover();
        let Some(mut active_span) = active.remove(key) else {
            return false;
        };
        let mut attrs = lifecycle_end_attributes(record, &self.options);
        attrs.extend(self.attributes_for(record));
        active_span.span.set_attributes(attrs);
        if !success {
            active_span.span.set_status(error_status(record));
        }
        active_span.span.end_with_timestamp(record_time(record));
        true
    }

    fn emit_instant(
        &self,
        record: &TraceRecord,
        name: impl Into<std::borrow::Cow<'static, str>>,
        duration_ms: Option<u64>,
    ) {
        let end = record_time(record);
        let start = duration_ms
            .and_then(|ms| end.checked_sub(Duration::from_millis(ms)))
            .unwrap_or(end);
        let mut span = self.build_span(
            record,
            name,
            parent_for(record, &self.active),
            start,
            Some(end),
        );
        span.set_attributes(self.attributes_for(record));
        if record.event.is_failed() {
            span.set_status(error_status(record));
        }
        span.end_with_timestamp(end);
    }

    fn build_span(
        &self,
        record: &TraceRecord,
        name: impl Into<std::borrow::Cow<'static, str>>,
        parent: Option<SpanContext>,
        start: SystemTime,
        end: Option<SystemTime>,
    ) -> T::Span {
        let mut builder = self
            .tracer
            .span_builder(name)
            .with_kind(SpanKind::Internal)
            .with_start_time(start)
            .with_attributes(common_attributes(record, &self.options));
        if let Some(end) = end {
            builder = builder.with_end_time(end);
        }
        match parent {
            Some(parent) => {
                let parent_cx = Context::new().with_remote_span_context(parent);
                builder.start_with_context(&self.tracer, &parent_cx)
            }
            None => builder.start(&self.tracer),
        }
    }

    fn attributes_for(&self, record: &TraceRecord) -> Vec<KeyValue> {
        event_attributes(record, &self.options)
    }

    fn add_llm_event(
        &self,
        record: &TraceRecord,
        name: impl Into<std::borrow::Cow<'static, str>>,
    ) -> bool {
        let Some(key) = llm_key(&record.context) else {
            return false;
        };
        let mut active = self.active.lock_recover();
        let Some(active_span) = active.get_mut(&key) else {
            return false;
        };
        let mut attrs = common_attributes(record, &self.options);
        attrs.extend(self.attributes_for(record));
        active_span.span.add_event(name, attrs);
        true
    }
}

impl<T> TraceSink for OtelTraceSink<T>
where
    T: Tracer + Send + Sync,
    T::Span: Send + Sync + 'static,
{
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        match &record.event {
            TraceEvent::TurnStarted { .. } => {
                if let Some(key) = turn_key(&record.context) {
                    self.start_active(key, record, "lash.turn");
                } else {
                    self.emit_instant(record, "lash.turn.started", None);
                }
            }
            TraceEvent::TurnCompleted { .. } => {
                let ended = turn_key(&record.context)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, !record.event.is_failed()));
                if !ended {
                    self.emit_instant(record, "lash.turn.completed", None);
                }
            }
            TraceEvent::LlmCallStarted { .. } => {
                if let Some(key) = llm_key(&record.context) {
                    self.start_active(key, record, "lash.llm");
                } else {
                    self.emit_instant(record, "lash.llm.started", None);
                }
            }
            TraceEvent::LlmCallCompleted { response, .. } => {
                let ended = llm_key(&record.context)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, !record.event.is_failed()));
                if !ended {
                    self.emit_instant(record, "lash.llm", Some(response.duration_ms));
                }
            }
            TraceEvent::LlmCallFailed { .. } => {
                let ended = llm_key(&record.context)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, !record.event.is_failed()));
                if !ended {
                    self.emit_instant(record, "lash.llm", None);
                }
            }
            TraceEvent::ProviderRequest { .. } => {
                if !self.add_llm_event(record, format!("lash.{}", record.event.kind())) {
                    self.emit_instant(record, format!("lash.{}", record.event.kind()), None);
                }
            }
            TraceEvent::ProviderReplayDropped { .. } => {
                if !self.add_llm_event(record, format!("lash.{}", record.event.kind())) {
                    self.emit_instant(record, format!("lash.{}", record.event.kind()), None);
                }
            }
            TraceEvent::EffectEnvelopeDiff { .. } => {
                self.emit_instant(record, format!("lash.{}", record.event.kind()), None)
            }
            TraceEvent::ProviderStreamEvent { .. } => {
                if !self.add_llm_event(record, format!("lash.{}", record.event.kind())) {
                    self.emit_instant(record, format!("lash.{}", record.event.kind()), None);
                }
            }
            TraceEvent::RuntimeStreamEvent { .. } => {
                if !self.add_llm_event(record, format!("lash.{}", record.event.kind())) {
                    self.emit_instant(record, format!("lash.{}", record.event.kind()), None);
                }
            }
            TraceEvent::ToolCallStarted { .. } => {
                if let Some(key) = tool_key(&record.event) {
                    self.start_active(key, record, "lash.tool");
                } else {
                    self.emit_instant(record, "lash.tool.started", None);
                }
            }
            TraceEvent::ToolCallCompleted { duration_ms, .. } => {
                let ended = tool_key(&record.event)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, !record.event.is_failed()));
                if !ended {
                    self.emit_instant(record, "lash.tool", Some(*duration_ms));
                }
            }
            TraceEvent::JournaledEffectStarted { .. } => {
                self.emit_instant(record, "lash.durable.journaled_effect.started", None)
            }
            TraceEvent::JournaledEffectSettled { .. } => {
                self.emit_instant(record, "lash.durable.journaled_effect.settled", None)
            }
            TraceEvent::DurableWaitParked { .. } => {
                self.emit_instant(record, "lash.durable.wait.parked", None)
            }
            TraceEvent::DurableWaitResolved { .. } => {
                self.emit_instant(record, "lash.durable.wait.resolved", None)
            }
            TraceEvent::DurableTimerStarted { .. } => {
                self.emit_instant(record, "lash.durable.timer.started", None)
            }
            TraceEvent::DurableTimerResolved { duration_ms, .. } => {
                self.emit_instant(record, "lash.durable.timer.resolved", Some(*duration_ms))
            }
            TraceEvent::DurableSegmentBoundary { .. } => {
                self.emit_instant(record, "lash.durable.segment_boundary", None)
            }
            TraceEvent::StoreErrorObserved { .. } => {
                self.emit_instant(record, "lash.store.error", None)
            }
            TraceEvent::PromptBuilt { .. } => self.emit_instant(record, "lash.prompt", None),
            TraceEvent::AttachmentDegraded { .. } => {
                self.emit_instant(record, "lash.attachment.degraded", None)
            }
            TraceEvent::CompositionChanged { .. } => {
                self.emit_instant(record, "lash.composition.changed", None)
            }
            TraceEvent::RollingHistoryCompactionNeeded { .. } => {
                self.emit_instant(record, "lash.rolling_history.compaction_needed", None)
            }
            TraceEvent::RollingHistoryPromptPruned { .. } => {
                self.emit_instant(record, "lash.rolling_history.prompt_pruned", None)
            }
            TraceEvent::RollingHistoryCompactionStarted { .. } => {
                self.emit_instant(record, "lash.rolling_history.compaction_started", None)
            }
            TraceEvent::RollingHistoryCompactionCompleted { .. } => {
                self.emit_instant(record, "lash.rolling_history.compaction_completed", None)
            }
            TraceEvent::ExecCodeStarted { .. }
            | TraceEvent::ExecCodeCompleted { .. }
            | TraceEvent::ExecCodeFailed { .. }
            | TraceEvent::ObservationProjection { .. } => self.emit_instant(
                record,
                typed_diagnostic_span_name(&record.event)
                    .expect("typed diagnostic has an OTel span name"),
                None,
            ),
            TraceEvent::RlmStep { .. } => self.emit_instant(record, "lash.rlm.step", None),
            TraceEvent::ProtocolStep { .. } => {
                self.emit_instant(record, format!("lash.{}", record.event.kind()), None)
            }
            TraceEvent::LanguageExecution { .. } => {
                self.emit_instant(record, format!("lash.{}", record.event.kind()), None)
            }
            TraceEvent::Custom { .. } => {
                self.emit_instant(record, format!("lash.{}", record.event.kind()), None)
            }
        }
        Ok(())
    }

    /// No-op: span export durability is host-owned.
    ///
    /// This sink only starts and ends spans on the host's OpenTelemetry tracer;
    /// the buffering that risks span loss on exit lives in the host's
    /// `BatchSpanProcessor` / exporter, not here. Flushing that buffer is the
    /// host's duty — call `force_flush()` (or `shutdown()`) on your
    /// `TracerProvider` before the process exits. Lash cannot do it for you
    /// because it never owns the provider. See `docs/tracing.html`.
    fn flush(&self) -> Result<(), TraceSinkError> {
        Ok(())
    }
}

fn common_attributes(record: &TraceRecord, options: &OtelTraceOptions) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new(
            attr::LASH_TRACE_SCHEMA_VERSION,
            record.schema_version as i64,
        ),
        KeyValue::new(attr::LASH_TRACE_RECORD_ID, record.id.clone()),
        KeyValue::new(attr::LASH_TRACE_EVENT_TYPE, event_type(&record.event)),
    ];
    context_attributes(&mut attrs, &record.context, options);
    if options.include_event_json
        && let Ok(json) = serde_json::to_string(record)
    {
        attrs.push(KeyValue::new(attr::LASH_TRACE_RECORD_JSON, json));
    }
    attrs
}

fn lifecycle_end_attributes(record: &TraceRecord, options: &OtelTraceOptions) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new(
            attr::LASH_TRACE_END_SCHEMA_VERSION,
            record.schema_version as i64,
        ),
        KeyValue::new(attr::LASH_TRACE_END_RECORD_ID, record.id.clone()),
        KeyValue::new(attr::LASH_TRACE_END_EVENT_TYPE, event_type(&record.event)),
    ];
    if options.include_event_json
        && let Ok(json) = serde_json::to_string(record)
    {
        attrs.push(KeyValue::new(attr::LASH_TRACE_END_RECORD_JSON, json));
    }
    attrs
}

fn context_attributes(
    attrs: &mut Vec<KeyValue>,
    context: &TraceContext,
    options: &OtelTraceOptions,
) {
    push_opt(attrs, attr::LASH_CONTEXT_RUN_ID, &context.run_id);
    push_opt(
        attrs,
        attr::LASH_CONTEXT_EXPERIMENT_ID,
        &context.experiment_id,
    );
    push_opt(
        attrs,
        attr::LASH_CONTEXT_CANDIDATE_ID,
        &context.candidate_id,
    );
    push_opt(
        attrs,
        attr::LASH_CONTEXT_CANDIDATE_PARENT_ID,
        &context.candidate_parent_id,
    );
    push_opt(attrs, attr::LASH_CONTEXT_EXAMPLE_ID, &context.example_id);
    push_opt(attrs, attr::LASH_CONTEXT_SPLIT, &context.split);
    push_opt(attrs, attr::LASH_CONTEXT_SESSION_ID, &context.session_id);
    push_opt(attrs, attr::LASH_CONTEXT_TURN_ID, &context.turn_id);
    push_opt(
        attrs,
        attr::LASH_CONTEXT_GRAPH_NODE_ID,
        &context.graph_node_id,
    );
    push_opt(
        attrs,
        attr::LASH_CONTEXT_PARENT_GRAPH_NODE_ID,
        &context.parent_graph_node_id,
    );
    if let Some(turn_index) = context.turn_index {
        attrs.push(KeyValue::new(
            attr::LASH_CONTEXT_TURN_INDEX,
            turn_index as i64,
        ));
    }
    if let Some(protocol_iteration) = context.protocol_iteration {
        attrs.push(KeyValue::new(
            attr::LASH_CONTEXT_PROTOCOL_ITERATION,
            protocol_iteration as i64,
        ));
    }
    push_opt(attrs, attr::LASH_CONTEXT_EFFECT_ID, &context.effect_id);
    push_opt(attrs, attr::LASH_CONTEXT_LLM_CALL_ID, &context.llm_call_id);

    if options.include_context_metadata {
        for (key, value) in &context.metadata {
            attrs.push(KeyValue::new(
                format!("lash.metadata.{key}"),
                otel_value(value),
            ));
        }
    }
}

fn event_attributes(record: &TraceRecord, options: &OtelTraceOptions) -> Vec<KeyValue> {
    let mut attrs = Vec::new();
    match &record.event {
        TraceEvent::TurnStarted { metadata } => {
            attrs.push(KeyValue::new(
                attr::LASH_METADATA_COUNT,
                metadata.len() as i64,
            ));
            push_payload_json(&mut attrs, options, attr::LASH_METADATA_JSON, metadata);
        }
        TraceEvent::PromptBuilt {
            prompt_hash,
            prompt_chars,
            components,
        } => {
            attrs.push(KeyValue::new(attr::LASH_PROMPT_HASH, prompt_hash.clone()));
            attrs.push(KeyValue::new(attr::LASH_PROMPT_CHARS, *prompt_chars as i64));
            attrs.push(KeyValue::new(
                attr::LASH_PROMPT_COMPONENT_COUNT,
                components.len() as i64,
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_PROMPT_COMPONENTS_JSON,
                components,
            );
        }
        TraceEvent::AttachmentDegraded {
            attachment_id,
            label,
            media_type,
            source,
            reason,
        } => {
            push_opt(&mut attrs, attr::LASH_ATTACHMENT_ID, attachment_id);
            push_opt(&mut attrs, attr::LASH_ATTACHMENT_LABEL, label);
            push_opt(&mut attrs, attr::LASH_ATTACHMENT_MEDIA_TYPE, media_type);
            attrs.push(KeyValue::new(
                attr::LASH_ATTACHMENT_SOURCE,
                serde_json::to_value(source)
                    .ok()
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "unknown".to_string()),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ATTACHMENT_DEGRADATION_REASON,
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "unknown".to_string()),
            ));
        }
        TraceEvent::CompositionChanged {
            fingerprint,
            rendered_system_prompt,
            tool_schemas,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_COMPOSITION_FINGERPRINT,
                fingerprint.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_COMPOSITION_PROMPT_CHARS,
                rendered_system_prompt.chars().count() as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_COMPOSITION_TOOL_COUNT,
                tool_schemas.len() as i64,
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_COMPOSITION_RENDERED_SYSTEM_PROMPT_JSON,
                rendered_system_prompt,
            );
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_COMPOSITION_TOOL_SCHEMAS_JSON,
                tool_schemas,
            );
        }
        TraceEvent::RollingHistoryCompactionNeeded {
            context_budget_tokens,
            max_context_tokens,
            threshold_tokens,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_CONTEXT_BUDGET_TOKENS,
                *context_budget_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_MAX_CONTEXT_TOKENS,
                *max_context_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_THRESHOLD_TOKENS,
                *threshold_tokens as i64,
            ));
        }
        TraceEvent::RollingHistoryPromptPruned {
            context_budget_tokens,
            max_context_tokens,
            dropped_prefix_messages,
            retained_messages,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_CONTEXT_BUDGET_TOKENS,
                *context_budget_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_MAX_CONTEXT_TOKENS,
                *max_context_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_DROPPED_PREFIX_MESSAGES,
                *dropped_prefix_messages as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_RETAINED_MESSAGES,
                *retained_messages as i64,
            ));
        }
        TraceEvent::RollingHistoryCompactionStarted {
            source_messages,
            instructions_present,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_SOURCE_MESSAGES,
                *source_messages as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_INSTRUCTIONS_PRESENT,
                *instructions_present,
            ));
        }
        TraceEvent::RollingHistoryCompactionCompleted { summary_nodes } => {
            attrs.push(KeyValue::new(
                attr::LASH_ROLLING_HISTORY_SUMMARY_NODES,
                *summary_nodes as i64,
            ));
        }
        TraceEvent::LlmCallStarted { request } => {
            attrs.push(KeyValue::new(
                attr::GEN_AI_REQUEST_MODEL,
                request.model.clone(),
            ));
            push_opt(
                &mut attrs,
                attr::GEN_AI_REQUEST_MODEL_VARIANT,
                &request.model_variant,
            );
            attrs.push(KeyValue::new(attr::LASH_LLM_STREAM, request.stream));
            attrs.push(KeyValue::new(
                attr::LASH_LLM_TOOL_CHOICE,
                request.tool_choice.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LLM_MESSAGE_COUNT,
                request.messages.len() as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LLM_TOOL_COUNT,
                request.tools.len() as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LLM_ATTACHMENT_COUNT,
                request.attachments().len() as i64,
            ));
            push_payload_json(&mut attrs, options, attr::LASH_LLM_REQUEST_JSON, request);
        }
        TraceEvent::LlmCallCompleted {
            response,
            usage,
            provider_usage,
            stream_summary,
            attempts,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LLM_DURATION_MS,
                response.duration_ms as i64,
            ));
            attrs.push(KeyValue::new(
                attr::GEN_AI_RESPONSE_TEXT_CHARS,
                response.text.len() as i64,
            ));
            if let Some(usage) = usage {
                usage_attributes(&mut attrs, attr::GEN_AI_USAGE, usage);
            }
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_LLM_PROVIDER_USAGE_JSON,
                provider_usage,
            );
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_LLM_STREAM_SUMMARY_JSON,
                stream_summary,
            );
            push_payload_json(&mut attrs, options, attr::LASH_LLM_RESPONSE_JSON, response);
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_RETRY_ATTEMPTS_JSON,
                attempts,
            );
        }
        TraceEvent::LlmCallFailed {
            error,
            stream_summary,
            attempts,
        } => {
            attrs.push(KeyValue::new(
                attr::ERROR_TYPE,
                error.code.clone().unwrap_or_default(),
            ));
            attrs.push(KeyValue::new(attr::ERROR_MESSAGE, error.message.clone()));
            attrs.push(KeyValue::new(attr::LASH_ERROR_RETRYABLE, error.retryable));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_LLM_STREAM_SUMMARY_JSON,
                stream_summary,
            );
            push_payload_json(&mut attrs, options, attr::LASH_ERROR_RAW, &error.raw);
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_RETRY_ATTEMPTS_JSON,
                attempts,
            );
        }
        TraceEvent::ProviderRequest { event } => {
            attrs.push(KeyValue::new(
                attr::LASH_PROVIDER_NAME,
                event.provider.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_PROVIDER_ENDPOINT,
                event.endpoint.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_SEQUENCE,
                event.sequence as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_ELAPSED_MS,
                event.elapsed_ms as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_REQUEST_BODY_LEN,
                event.body_len as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_REQUEST_BODY_SHA256,
                event.body_sha256.clone(),
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_REQUEST_BODY_JSON,
                &event.body_json,
            );
            push_opt(
                &mut attrs,
                attr::LASH_REQUEST_BODY_JSON_OMITTED_REASON,
                &event.body_json_omitted_reason,
            );
        }
        TraceEvent::ProviderReplayDropped { event } => {
            attrs.push(KeyValue::new(
                attr::LASH_REPLAY_KIND,
                event.replay_kind.code(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_REPLAY_DROP_REASON,
                event.reason.code(),
            ));
            if let Some(route) = &event.minting_route {
                attrs.push(KeyValue::new(
                    attr::LASH_REPLAY_MINTING_PROVIDER,
                    route.provider.clone(),
                ));
                attrs.push(KeyValue::new(
                    attr::LASH_REPLAY_MINTING_ENDPOINT,
                    route.endpoint.clone(),
                ));
                attrs.push(KeyValue::new(
                    attr::LASH_REPLAY_MINTING_MODEL,
                    route.model.clone(),
                ));
            }
            attrs.push(KeyValue::new(
                attr::LASH_REPLAY_SERVING_PROVIDER,
                event.serving_route.provider.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_REPLAY_SERVING_ENDPOINT,
                event.serving_route.endpoint.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_REPLAY_SERVING_MODEL,
                event.serving_route.model.clone(),
            ));
        }
        TraceEvent::EffectEnvelopeDiff { event } => {
            attrs.push(KeyValue::new(
                attr::LASH_EFFECT_ENVELOPE_RECORDED_HASH,
                event.recorded_envelope_hash.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_EFFECT_ENVELOPE_RECONSTRUCTED_HASH,
                event.reconstructed_envelope_hash.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_EFFECT_ENVELOPE_DIVERGENT_PATH_COUNT,
                event.divergent_paths.len() as i64,
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_EFFECT_ENVELOPE_DIVERGENT_PATHS_JSON,
                &event.divergent_paths,
            );
        }
        TraceEvent::ProviderStreamEvent { event } => {
            attrs.push(KeyValue::new(
                attr::LASH_PROVIDER_NAME,
                event.provider.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_SEQUENCE,
                event.sequence as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_ELAPSED_MS,
                event.elapsed_ms as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_EVENT_NAME,
                event.event_name.clone(),
            ));
            push_opt(&mut attrs, attr::LASH_STREAM_ITEM_ID, &event.item_id);
            if let Some(output_index) = event.output_index {
                attrs.push(KeyValue::new(attr::LASH_STREAM_OUTPUT_INDEX, output_index));
            }
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_RAW_LEN,
                event.raw_len as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_RAW_SHA256,
                event.raw_sha256.clone(),
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_STREAM_RAW_JSON,
                &event.raw_json,
            );
        }
        TraceEvent::RuntimeStreamEvent { event } => {
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_SEQUENCE,
                event.sequence as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_ELAPSED_MS,
                event.elapsed_ms as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_STREAM_EVENT_NAME,
                event.event_name.clone(),
            ));
            if let Some(text) = &event.visible_text {
                attrs.push(KeyValue::new(
                    attr::LASH_STREAM_VISIBLE_CHARS,
                    text.len() as i64,
                ));
            }
            if let Some(text) = &event.raw_text {
                attrs.push(KeyValue::new(
                    attr::LASH_STREAM_RAW_CHARS,
                    text.len() as i64,
                ));
            }
            push_opt(&mut attrs, attr::LASH_STREAM_ITEM_ID, &event.item_id);
            if let Some(output_index) = event.output_index {
                attrs.push(KeyValue::new(attr::LASH_STREAM_OUTPUT_INDEX, output_index));
            }
            push_opt(&mut attrs, attr::LASH_TOOL_CALL_ID, &event.call_id);
            push_opt(&mut attrs, attr::LASH_TOOL_NAME, &event.tool_name);
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_TOOL_INPUT_JSON,
                &event.input_json,
            );
            if let Some(usage) = &event.usage {
                usage_attributes(&mut attrs, attr::GEN_AI_USAGE, usage);
            }
        }
        TraceEvent::ToolCallStarted {
            call_id,
            name,
            args,
        } => {
            push_opt(&mut attrs, attr::LASH_TOOL_CALL_ID, call_id);
            attrs.push(KeyValue::new(attr::LASH_TOOL_NAME, name.clone()));
            push_payload_json(&mut attrs, options, attr::LASH_TOOL_ARGS_JSON, args);
        }
        TraceEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            duration_ms,
            attempts,
        } => {
            push_opt(&mut attrs, attr::LASH_TOOL_CALL_ID, call_id);
            attrs.push(KeyValue::new(attr::LASH_TOOL_NAME, name.clone()));
            attrs.push(KeyValue::new(attr::LASH_TOOL_SUCCESS, output.is_success()));
            attrs.push(KeyValue::new(
                attr::LASH_TOOL_STATUS,
                format!("{:?}", output.status()).to_ascii_lowercase(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_TOOL_DURATION_MS,
                *duration_ms as i64,
            ));
            push_payload_json(&mut attrs, options, attr::LASH_TOOL_ARGS_JSON, args);
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_TOOL_RESULT_JSON,
                &output.value_for_projection(),
            );
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_RETRY_ATTEMPTS_JSON,
                attempts,
            );
        }
        TraceEvent::JournaledEffectStarted {
            effect_name,
            effect_kind,
        }
        | TraceEvent::JournaledEffectSettled {
            effect_name,
            effect_kind,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_EFFECT_NAME,
                effect_name.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_EFFECT_KIND,
                effect_kind.clone(),
            ));
            if let TraceEvent::JournaledEffectSettled { status, .. } = &record.event {
                attrs.push(KeyValue::new(attr::LASH_DURABLE_STATUS, status.wire_tag()));
            }
        }
        TraceEvent::DurableWaitParked { wait_kind } => {
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_WAIT_KIND,
                wait_kind.clone(),
            ));
        }
        TraceEvent::DurableWaitResolved {
            wait_kind,
            resolution,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_WAIT_KIND,
                wait_kind.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_RESOLUTION,
                resolution.wire_tag(),
            ));
        }
        TraceEvent::DurableTimerStarted { duration_ms }
        | TraceEvent::DurableTimerResolved { duration_ms, .. } => {
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_TIMER_DURATION_MS,
                *duration_ms as i64,
            ));
            if let TraceEvent::DurableTimerResolved { status, .. } = &record.event {
                attrs.push(KeyValue::new(attr::LASH_DURABLE_STATUS, status.wire_tag()));
            }
        }
        TraceEvent::DurableSegmentBoundary {
            reason,
            effects_executed,
            journaled_bytes_estimate,
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_BOUNDARY_REASON,
                reason.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_DURABLE_EFFECTS_EXECUTED,
                *effects_executed as i64,
            ));
            if let Some(bytes) = journaled_bytes_estimate {
                attrs.push(KeyValue::new(
                    attr::LASH_DURABLE_JOURNALED_BYTES_ESTIMATE,
                    *bytes as i64,
                ));
            }
        }
        TraceEvent::StoreErrorObserved {
            operation,
            error_class,
            message,
        } => {
            attrs.push(KeyValue::new(attr::LASH_STORE_OPERATION, operation.clone()));
            attrs.push(KeyValue::new(attr::ERROR_TYPE, error_class.clone()));
            attrs.push(KeyValue::new(attr::ERROR_MESSAGE, message.clone()));
        }
        TraceEvent::RlmStep {
            step_index,
            outcome,
        } => {
            attrs.push(KeyValue::new(attr::LASH_RLM_STEP_INDEX, *step_index as i64));
            match outcome {
                crate::TraceRlmStepOutcome::Ok => {
                    attrs.push(KeyValue::new(attr::LASH_RLM_STEP_OUTCOME, "ok"));
                }
                crate::TraceRlmStepOutcome::Failure { diagnostic } => {
                    attrs.push(KeyValue::new(attr::LASH_RLM_STEP_OUTCOME, "failure"));
                    attrs.push(KeyValue::new(attr::ERROR_MESSAGE, diagnostic.clone()));
                }
            }
        }
        TraceEvent::ProtocolStep { plugin_id, payload } => {
            attrs.push(KeyValue::new(
                attr::LASH_PROTOCOL_PLUGIN_ID,
                plugin_id.clone(),
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_PROTOCOL_PAYLOAD_JSON,
                payload,
            );
        }
        TraceEvent::ExecCodeStarted { .. }
        | TraceEvent::ExecCodeCompleted { .. }
        | TraceEvent::ExecCodeFailed { .. }
        | TraceEvent::ObservationProjection { .. } => {
            attrs.push(KeyValue::new(attr::LASH_PROTOCOL_PLUGIN_ID, "runtime"));
            attrs.push(KeyValue::new(
                attr::LASH_PROTOCOL_DIAGNOSTIC_PHASE,
                record.event.kind(),
            ));
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_PROTOCOL_PAYLOAD_JSON,
                &typed_diagnostic_protocol_payload(&record.event),
            );
        }
        TraceEvent::LanguageExecution { language, event } => {
            language_execution_attributes(&mut attrs, language, event);
            push_payload_json(
                &mut attrs,
                options,
                attr::LASH_LANGUAGE_EXECUTION_EVENT_JSON,
                event,
            );
        }
        TraceEvent::TurnCompleted { outcome } => {
            attrs.push(KeyValue::new(attr::LASH_TURN_STATUS, outcome.status_tag()));
            match outcome {
                crate::TraceTurnOutcome::Completed { done_reason } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_TURN_DONE_REASON,
                        done_reason.wire_tag(),
                    ));
                }
                crate::TraceTurnOutcome::Failed { done_reason } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_TURN_DONE_REASON,
                        done_reason.wire_tag(),
                    ));
                }
                crate::TraceTurnOutcome::AgentFrameSwitch { frame_switch } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_TURN_AGENT_FRAME_SWITCH_FRAME_KEY,
                        frame_switch.frame_key.clone(),
                    ));
                }
                crate::TraceTurnOutcome::Cancelled { evidence } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_TURN_CANCELLATION_REQUEST_ID,
                        evidence.request_id.clone(),
                    ));
                    if let Some(origin) = &evidence.origin {
                        attrs.push(KeyValue::new(
                            attr::LASH_TURN_CANCELLATION_ORIGIN,
                            origin.clone(),
                        ));
                    }
                    if let Some(reason) = &evidence.reason {
                        attrs.push(KeyValue::new(
                            attr::LASH_TURN_CANCELLATION_REASON,
                            reason.clone(),
                        ));
                    }
                }
            }
        }
        TraceEvent::Custom { name, payload } => {
            attrs.push(KeyValue::new(attr::LASH_CUSTOM_NAME, name.clone()));
            push_payload_json(&mut attrs, options, attr::LASH_CUSTOM_PAYLOAD_JSON, payload);
        }
    }
    attrs
}

fn language_execution_attributes(
    attrs: &mut Vec<KeyValue>,
    language: &str,
    event: &crate::TraceLanguageExecution,
) {
    use crate::TraceLanguageExecutionPayload as Payload;

    let kind = match &event.payload {
        Payload::ExecutionStarted { .. } => "execution_started",
        Payload::ExecutionFinished { .. } => "execution_finished",
        Payload::NodeStarted { .. } => "node_started",
        Payload::NodeCompleted { .. } => "node_completed",
        Payload::NodeFailed { .. } => "node_failed",
        Payload::BranchSelected { .. } => "branch_selected",
        Payload::ChildStarted { .. } => "child_started",
    };
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_LANGUAGE,
        language.to_string(),
    ));
    attrs.push(KeyValue::new(attr::LASH_LANGUAGE_EXECUTION_KIND, kind));

    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_EVENT_KEY,
        event.event_key.clone(),
    ));
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_GRAPH_KEY,
        event.identity.graph_key(),
    ));
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_SESSION_ID,
        event.identity.scope.session_id.to_string(),
    ));
    if let Some(turn_id) = &event.identity.scope.turn_id {
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_TURN_ID,
            turn_id.to_string(),
        ));
    }
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_MODULE_REF,
        event.identity.module_ref.clone(),
    ));
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_ENTRY_KIND,
        event.identity.entry_kind.clone(),
    ));
    push_opt(
        attrs,
        attr::LASH_LANGUAGE_EXECUTION_ENTRY_REF,
        &event.identity.entry_ref,
    );
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_ENTRY_NAME,
        event.identity.entry_name.clone(),
    ));
    match &event.identity.subject {
        crate::TraceRuntimeSubject::Effect { effect_id, kind } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_SUBJECT_TYPE,
                "effect",
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EFFECT_ID,
                effect_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EFFECT_KIND,
                kind.clone(),
            ));
        }
        crate::TraceRuntimeSubject::Process { process_id } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_SUBJECT_TYPE,
                "process",
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_PROCESS_ID,
                process_id.to_string(),
            ));
        }
    }

    match &event.payload {
        Payload::NodeStarted {
            node_id,
            node_kind,
            occurrence,
            ..
        }
        | Payload::NodeCompleted {
            node_id,
            node_kind,
            occurrence,
            ..
        }
        | Payload::NodeFailed {
            node_id,
            node_kind,
            occurrence,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_KIND,
                node_kind.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
        }
        Payload::BranchSelected {
            node_id,
            occurrence,
            edge_id,
            selected,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EDGE_ID,
                edge_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_BRANCH,
                format!("{selected:?}").to_ascii_lowercase(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
        }
        Payload::ChildStarted {
            parent_node_id,
            child,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_PARENT_NODE_ID,
                parent_node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_CHILD_GRAPH_KEY,
                child.graph_key(),
            ));
            match &child.subject {
                crate::TraceRuntimeSubject::Effect { effect_id, kind } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_LANGUAGE_EXECUTION_CHILD_SUBJECT_TYPE,
                        "effect",
                    ));
                    attrs.push(KeyValue::new(
                        attr::LASH_LANGUAGE_EXECUTION_CHILD_EFFECT_ID,
                        effect_id.clone(),
                    ));
                    attrs.push(KeyValue::new(
                        attr::LASH_LANGUAGE_EXECUTION_CHILD_EFFECT_KIND,
                        kind.clone(),
                    ));
                }
                crate::TraceRuntimeSubject::Process { process_id } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_LANGUAGE_EXECUTION_CHILD_SUBJECT_TYPE,
                        "process",
                    ));
                    attrs.push(KeyValue::new(
                        attr::LASH_LANGUAGE_EXECUTION_CHILD_PROCESS_ID,
                        process_id.to_string(),
                    ));
                }
            }
        }
        Payload::ExecutionFinished { status, error, .. } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_STATUS,
                format!("{status:?}").to_ascii_lowercase(),
            ));
            push_opt(attrs, attr::LASH_LANGUAGE_EXECUTION_ERROR, error);
        }
        Payload::ExecutionStarted { execution_map, .. } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_COUNT,
                execution_map.nodes.len() as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EDGE_COUNT,
                execution_map.edges.len() as i64,
            ));
        }
    }
}

fn usage_attributes(attrs: &mut Vec<KeyValue>, prefix: &str, usage: &TraceTokenUsage) {
    attrs.push(KeyValue::new(
        format!("{prefix}.input_tokens"),
        usage.input_tokens,
    ));
    attrs.push(KeyValue::new(
        format!("{prefix}.output_tokens"),
        usage.output_tokens,
    ));
    attrs.push(KeyValue::new(
        format!("{prefix}.cache_read_input_tokens"),
        usage.cache_read_input_tokens,
    ));
    attrs.push(KeyValue::new(
        format!("{prefix}.cache_write_input_tokens"),
        usage.cache_write_input_tokens,
    ));
    attrs.push(KeyValue::new(
        format!("{prefix}.reasoning_output_tokens"),
        usage.reasoning_output_tokens,
    ));
}

fn push_opt<T: AsRef<str>>(attrs: &mut Vec<KeyValue>, key: &'static str, value: &Option<T>) {
    if let Some(value) = value {
        attrs.push(KeyValue::new(key, value.as_ref().to_string()));
    }
}

fn push_payload_json<T: serde::Serialize>(
    attrs: &mut Vec<KeyValue>,
    options: &OtelTraceOptions,
    key: &'static str,
    value: &T,
) {
    if options.include_payload_json
        && let Ok(json) = serde_json::to_string(value)
    {
        attrs.push(KeyValue::new(key, json));
    }
}

fn otel_value(value: &Value) -> OtelValue {
    match value {
        Value::Bool(value) => OtelValue::Bool(*value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                OtelValue::I64(value)
            } else if let Some(value) = value.as_u64() {
                OtelValue::I64(value.min(i64::MAX as u64) as i64)
            } else if let Some(value) = value.as_f64() {
                OtelValue::F64(value)
            } else {
                OtelValue::String(value.to_string().into())
            }
        }
        Value::String(value) => OtelValue::String(value.clone().into()),
        Value::Null => OtelValue::String("null".into()),
        Value::Array(_) | Value::Object(_) => {
            OtelValue::String(serde_json::to_string(value).unwrap_or_default().into())
        }
    }
}

fn parent_for<T>(
    record: &TraceRecord,
    active: &Mutex<HashMap<String, ActiveSpan<T>>>,
) -> Option<SpanContext>
where
    T: Span,
{
    let key = turn_key(&record.context)?;
    let active = active.lock_recover();
    active.get(&key).map(|span| span.context.clone())
}

fn turn_key(context: &TraceContext) -> Option<String> {
    let session_id = context.session_id.as_deref()?;
    let turn_id = context
        .turn_id
        .as_deref()
        .or(context.graph_node_id.as_deref())?;
    Some(format!("turn:{session_id}:{turn_id}"))
}

fn llm_key(context: &TraceContext) -> Option<String> {
    context
        .llm_call_id
        .as_deref()
        .map(|llm_call_id| format!("llm:{llm_call_id}"))
}

fn tool_key(event: &TraceEvent) -> Option<String> {
    match event {
        TraceEvent::ToolCallStarted {
            call_id: Some(call_id),
            ..
        }
        | TraceEvent::ToolCallCompleted {
            call_id: Some(call_id),
            ..
        } => Some(format!("tool:{call_id}")),
        _ => None,
    }
}

fn typed_diagnostic_protocol_payload(event: &TraceEvent) -> Value {
    let mut payload = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Value::Object(object) = &mut payload {
        object.remove("type");
    }
    serde_json::json!({
        "diagnostic": {
            "phase": event.kind(),
            "payload": payload,
        }
    })
}

fn typed_diagnostic_span_name(event: &TraceEvent) -> Option<&'static str> {
    match event.kind() {
        "exec_code_started" | "exec_code_completed" | "exec_code_failed" => Some("lash.exec_code"),
        "observation_projection" => Some("lash.observation_projection"),
        _ => None,
    }
}

fn record_time(record: &TraceRecord) -> SystemTime {
    DateTime::parse_from_rfc3339(&record.timestamp)
        .map(|time| time.with_timezone(&Utc).into())
        .unwrap_or_else(|_| SystemTime::now())
}

fn event_type(event: &TraceEvent) -> &'static str {
    event.kind()
}

fn error_status(record: &TraceRecord) -> Status {
    match &record.event {
        TraceEvent::RlmStep {
            outcome: crate::TraceRlmStepOutcome::Failure { diagnostic },
            ..
        } => Status::error(diagnostic.clone()),
        TraceEvent::LlmCallFailed { error, .. } => Status::error(error.message.clone()),
        TraceEvent::ToolCallCompleted { name, .. } => {
            Status::error(format!("tool call failed: {name}"))
        }
        TraceEvent::TurnCompleted { outcome } => match outcome {
            crate::TraceTurnOutcome::Failed { done_reason } => {
                Status::error(format!("turn failed: {}", done_reason.wire_tag()))
            }
            _ => Status::error(outcome.status_tag()),
        },
        TraceEvent::EffectEnvelopeDiff { event } => Status::error(format!(
            "effect envelope hash mismatch at {} paths",
            event.divergent_paths.len()
        )),
        TraceEvent::JournaledEffectSettled { effect_name, .. } => {
            Status::error(format!("journaled effect failed: {effect_name}"))
        }
        TraceEvent::DurableWaitResolved { wait_kind, .. } => {
            Status::error(format!("durable wait failed: {wait_kind}"))
        }
        TraceEvent::DurableTimerResolved { .. } => Status::error("durable timer failed"),
        TraceEvent::StoreErrorObserved { message, .. } => Status::error(message.clone()),
        TraceEvent::LanguageExecution { event, .. } => match &event.payload {
            crate::TraceLanguageExecutionPayload::NodeFailed { error, .. } => {
                Status::error(error.clone())
            }
            crate::TraceLanguageExecutionPayload::ExecutionFinished { error, .. } => Status::error(
                error
                    .as_deref()
                    .unwrap_or("language execution failed")
                    .to_string(),
            ),
            _ => Status::error("language execution failed"),
        },
        _ => Status::error("lash trace event failed"),
    }
}

#[cfg(test)]
mod tests;
