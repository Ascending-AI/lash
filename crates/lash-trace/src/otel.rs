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
pub use metrics::{ToolIntentMetrics, WorkerCapacityMetrics};

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

    fn start_active(&self, key: String, record: &TraceRecord, name: &'static str) {
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

    fn emit_instant(&self, record: &TraceRecord, name: &'static str, duration_ms: Option<u64>) {
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
        if is_error_event(&record.event) {
            span.set_status(error_status(record));
        }
        span.end_with_timestamp(end);
    }

    fn build_span(
        &self,
        record: &TraceRecord,
        name: &'static str,
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

    fn add_llm_event(&self, record: &TraceRecord, name: &'static str) -> bool {
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
            TraceEvent::TurnCompleted { status, .. } => {
                let ended = turn_key(&record.context)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, status != "failed"));
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
                    .is_some_and(|key| self.end_active(key, record, true));
                if !ended {
                    self.emit_instant(record, "lash.llm", Some(response.duration_ms));
                }
            }
            TraceEvent::LlmCallFailed { .. } => {
                let ended = llm_key(&record.context)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, false));
                if !ended {
                    self.emit_instant(record, "lash.llm", None);
                }
            }
            TraceEvent::ProviderRequest { .. } => {
                if !self.add_llm_event(record, "lash.provider_request") {
                    self.emit_instant(record, "lash.provider_request", None);
                }
            }
            TraceEvent::ProviderReplayDropped { .. } => {
                if !self.add_llm_event(record, "lash.provider_replay_dropped") {
                    self.emit_instant(record, "lash.provider_replay_dropped", None);
                }
            }
            TraceEvent::EffectEnvelopeDiff { .. } => {
                self.emit_instant(record, "lash.effect_envelope_diff", None)
            }
            TraceEvent::ProviderStreamEvent { .. } => {
                if !self.add_llm_event(record, "lash.provider_stream_event") {
                    self.emit_instant(record, "lash.provider_stream_event", None);
                }
            }
            TraceEvent::RuntimeStreamEvent { .. } => {
                if !self.add_llm_event(record, "lash.runtime_stream_event") {
                    self.emit_instant(record, "lash.runtime_stream_event", None);
                }
            }
            TraceEvent::ToolCallStarted { .. } => {
                if let Some(key) = tool_key(&record.event) {
                    self.start_active(key, record, "lash.tool");
                } else {
                    self.emit_instant(record, "lash.tool.started", None);
                }
            }
            TraceEvent::ToolCallCompleted {
                duration_ms,
                output,
                ..
            } => {
                let ended = tool_key(&record.event)
                    .as_deref()
                    .is_some_and(|key| self.end_active(key, record, output.is_success()));
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
            TraceEvent::SessionStarted { .. } => self.emit_instant(record, "lash.session", None),
            TraceEvent::PromptBuilt { .. } => self.emit_instant(record, "lash.prompt", None),
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
            TraceEvent::ProtocolStep { payload, .. } => {
                self.emit_instant(record, protocol_step_span_name(payload), None)
            }
            TraceEvent::TokenUsage { .. } => self.emit_instant(record, "lash.token_usage", None),
            TraceEvent::LashlangExecution { .. } => {
                self.emit_instant(record, "lash.lashlang_execution", None)
            }
            TraceEvent::Custom { .. } => self.emit_instant(record, "lash.custom", None),
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
        KeyValue::new("lash.trace.schema_version", record.schema_version as i64),
        KeyValue::new("lash.trace.record_id", record.id.clone()),
        KeyValue::new("lash.trace.event_type", event_type(&record.event)),
    ];
    context_attributes(&mut attrs, &record.context, options);
    if options.include_event_json
        && let Ok(json) = serde_json::to_string(record)
    {
        attrs.push(KeyValue::new("lash.trace.record_json", json));
    }
    attrs
}

fn lifecycle_end_attributes(record: &TraceRecord, options: &OtelTraceOptions) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new(
            "lash.trace.end_schema_version",
            record.schema_version as i64,
        ),
        KeyValue::new("lash.trace.end_record_id", record.id.clone()),
        KeyValue::new("lash.trace.end_event_type", event_type(&record.event)),
    ];
    if options.include_event_json
        && let Ok(json) = serde_json::to_string(record)
    {
        attrs.push(KeyValue::new("lash.trace.end_record_json", json));
    }
    attrs
}

fn context_attributes(
    attrs: &mut Vec<KeyValue>,
    context: &TraceContext,
    options: &OtelTraceOptions,
) {
    push_opt(attrs, "lash.context.run_id", &context.run_id);
    push_opt(attrs, "lash.context.experiment_id", &context.experiment_id);
    push_opt(attrs, "lash.context.candidate_id", &context.candidate_id);
    push_opt(
        attrs,
        "lash.context.candidate_parent_id",
        &context.candidate_parent_id,
    );
    push_opt(attrs, "lash.context.example_id", &context.example_id);
    push_opt(attrs, "lash.context.split", &context.split);
    push_opt(attrs, "lash.context.session_id", &context.session_id);
    push_opt(attrs, "lash.context.turn_id", &context.turn_id);
    push_opt(attrs, "lash.context.graph_node_id", &context.graph_node_id);
    push_opt(
        attrs,
        "lash.context.parent_graph_node_id",
        &context.parent_graph_node_id,
    );
    if let Some(turn_index) = context.turn_index {
        attrs.push(KeyValue::new("lash.context.turn_index", turn_index as i64));
    }
    if let Some(protocol_iteration) = context.protocol_iteration {
        attrs.push(KeyValue::new(
            "lash.context.protocol_iteration",
            protocol_iteration as i64,
        ));
    }
    push_opt(attrs, "lash.context.effect_id", &context.effect_id);
    push_opt(attrs, "lash.context.llm_call_id", &context.llm_call_id);

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
        TraceEvent::SessionStarted { metadata } | TraceEvent::TurnStarted { metadata } => {
            attrs.push(KeyValue::new("lash.metadata.count", metadata.len() as i64));
            push_payload_json(&mut attrs, options, "lash.metadata.json", metadata);
        }
        TraceEvent::PromptBuilt {
            prompt_hash,
            prompt_chars,
            components,
        } => {
            attrs.push(KeyValue::new("lash.prompt.hash", prompt_hash.clone()));
            attrs.push(KeyValue::new("lash.prompt.chars", *prompt_chars as i64));
            attrs.push(KeyValue::new(
                "lash.prompt.component_count",
                components.len() as i64,
            ));
            push_payload_json(
                &mut attrs,
                options,
                "lash.prompt.components_json",
                components,
            );
        }
        TraceEvent::CompositionChanged {
            fingerprint,
            rendered_system_prompt,
            tool_schemas,
        } => {
            attrs.push(KeyValue::new(
                "lash.composition.fingerprint",
                fingerprint.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.composition.prompt_chars",
                rendered_system_prompt.chars().count() as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.composition.tool_count",
                tool_schemas.len() as i64,
            ));
            push_payload_json(
                &mut attrs,
                options,
                "lash.composition.rendered_system_prompt_json",
                rendered_system_prompt,
            );
            push_payload_json(
                &mut attrs,
                options,
                "lash.composition.tool_schemas_json",
                tool_schemas,
            );
        }
        TraceEvent::RollingHistoryCompactionNeeded {
            context_budget_tokens,
            max_context_tokens,
            threshold_tokens,
        } => {
            attrs.push(KeyValue::new(
                "lash.rolling_history.context_budget_tokens",
                *context_budget_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.rolling_history.max_context_tokens",
                *max_context_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.rolling_history.threshold_tokens",
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
                "lash.rolling_history.context_budget_tokens",
                *context_budget_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.rolling_history.max_context_tokens",
                *max_context_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.rolling_history.dropped_prefix_messages",
                *dropped_prefix_messages as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.rolling_history.retained_messages",
                *retained_messages as i64,
            ));
        }
        TraceEvent::RollingHistoryCompactionStarted {
            source_messages,
            instructions_present,
        } => {
            attrs.push(KeyValue::new(
                "lash.rolling_history.source_messages",
                *source_messages as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.rolling_history.instructions_present",
                *instructions_present,
            ));
        }
        TraceEvent::RollingHistoryCompactionCompleted { summary_nodes } => {
            attrs.push(KeyValue::new(
                "lash.rolling_history.summary_nodes",
                *summary_nodes as i64,
            ));
        }
        TraceEvent::LlmCallStarted { request } => {
            attrs.push(KeyValue::new("gen_ai.request.model", request.model.clone()));
            push_opt(
                &mut attrs,
                "gen_ai.request.model_variant",
                &request.model_variant,
            );
            attrs.push(KeyValue::new("lash.llm.stream", request.stream));
            attrs.push(KeyValue::new(
                "lash.llm.tool_choice",
                request.tool_choice.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.llm.message_count",
                request.messages.len() as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.llm.tool_count",
                request.tools.len() as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.llm.attachment_count",
                request.attachments.len() as i64,
            ));
            push_payload_json(&mut attrs, options, "lash.llm.request_json", request);
        }
        TraceEvent::LlmCallCompleted {
            response,
            usage,
            provider_usage,
            stream_summary,
            attempts,
        } => {
            attrs.push(KeyValue::new(
                "lash.llm.duration_ms",
                response.duration_ms as i64,
            ));
            attrs.push(KeyValue::new(
                "gen_ai.response.text_chars",
                response.text.len() as i64,
            ));
            if let Some(usage) = usage {
                usage_attributes(&mut attrs, "gen_ai.usage", usage);
            }
            push_payload_json(
                &mut attrs,
                options,
                "lash.llm.provider_usage_json",
                provider_usage,
            );
            push_payload_json(
                &mut attrs,
                options,
                "lash.llm.stream_summary_json",
                stream_summary,
            );
            push_payload_json(&mut attrs, options, "lash.llm.response_json", response);
            push_payload_json(&mut attrs, options, "lash.retry.attempts_json", attempts);
        }
        TraceEvent::LlmCallFailed {
            error,
            stream_summary,
            attempts,
        } => {
            attrs.push(KeyValue::new(
                "error.type",
                error.code.clone().unwrap_or_default(),
            ));
            attrs.push(KeyValue::new("error.message", error.message.clone()));
            attrs.push(KeyValue::new("lash.error.retryable", error.retryable));
            push_payload_json(
                &mut attrs,
                options,
                "lash.llm.stream_summary_json",
                stream_summary,
            );
            push_payload_json(&mut attrs, options, "lash.error.raw", &error.raw);
            push_payload_json(&mut attrs, options, "lash.retry.attempts_json", attempts);
        }
        TraceEvent::ProviderRequest { event } => {
            attrs.push(KeyValue::new("lash.provider.name", event.provider.clone()));
            attrs.push(KeyValue::new(
                "lash.provider.endpoint",
                event.endpoint.clone(),
            ));
            attrs.push(KeyValue::new("lash.stream.sequence", event.sequence as i64));
            attrs.push(KeyValue::new(
                "lash.stream.elapsed_ms",
                event.elapsed_ms as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.request.body_len",
                event.body_len as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.request.body_sha256",
                event.body_sha256.clone(),
            ));
            push_payload_json(
                &mut attrs,
                options,
                "lash.request.body_json",
                &event.body_json,
            );
            push_opt(
                &mut attrs,
                "lash.request.body_json_omitted_reason",
                &event.body_json_omitted_reason,
            );
        }
        TraceEvent::ProviderReplayDropped { event } => {
            attrs.push(KeyValue::new("lash.replay.kind", event.replay_kind.code()));
            attrs.push(KeyValue::new(
                "lash.replay.drop_reason",
                event.reason.code(),
            ));
            if let Some(route) = &event.minting_route {
                attrs.push(KeyValue::new(
                    "lash.replay.minting_provider",
                    route.provider.clone(),
                ));
                attrs.push(KeyValue::new(
                    "lash.replay.minting_endpoint",
                    route.endpoint.clone(),
                ));
                attrs.push(KeyValue::new(
                    "lash.replay.minting_model",
                    route.model.clone(),
                ));
            }
            attrs.push(KeyValue::new(
                "lash.replay.serving_provider",
                event.serving_route.provider.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.replay.serving_endpoint",
                event.serving_route.endpoint.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.replay.serving_model",
                event.serving_route.model.clone(),
            ));
        }
        TraceEvent::EffectEnvelopeDiff { event } => {
            attrs.push(KeyValue::new(
                "lash.effect_envelope.recorded_hash",
                event.recorded_envelope_hash.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.effect_envelope.reconstructed_hash",
                event.reconstructed_envelope_hash.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.effect_envelope.divergent_path_count",
                event.divergent_paths.len() as i64,
            ));
            push_payload_json(
                &mut attrs,
                options,
                "lash.effect_envelope.divergent_paths_json",
                &event.divergent_paths,
            );
        }
        TraceEvent::ProviderStreamEvent { event } => {
            attrs.push(KeyValue::new("lash.provider.name", event.provider.clone()));
            attrs.push(KeyValue::new("lash.stream.sequence", event.sequence as i64));
            attrs.push(KeyValue::new(
                "lash.stream.elapsed_ms",
                event.elapsed_ms as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.stream.event_name",
                event.event_name.clone(),
            ));
            push_opt(&mut attrs, "lash.stream.item_id", &event.item_id);
            if let Some(output_index) = event.output_index {
                attrs.push(KeyValue::new("lash.stream.output_index", output_index));
            }
            attrs.push(KeyValue::new("lash.stream.raw_len", event.raw_len as i64));
            attrs.push(KeyValue::new(
                "lash.stream.raw_sha256",
                event.raw_sha256.clone(),
            ));
            push_payload_json(&mut attrs, options, "lash.stream.raw_json", &event.raw_json);
        }
        TraceEvent::RuntimeStreamEvent { event } => {
            attrs.push(KeyValue::new("lash.stream.sequence", event.sequence as i64));
            attrs.push(KeyValue::new(
                "lash.stream.elapsed_ms",
                event.elapsed_ms as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.stream.event_name",
                event.event_name.clone(),
            ));
            if let Some(text) = &event.visible_text {
                attrs.push(KeyValue::new(
                    "lash.stream.visible_chars",
                    text.len() as i64,
                ));
            }
            if let Some(text) = &event.raw_text {
                attrs.push(KeyValue::new("lash.stream.raw_chars", text.len() as i64));
            }
            push_opt(&mut attrs, "lash.stream.item_id", &event.item_id);
            if let Some(output_index) = event.output_index {
                attrs.push(KeyValue::new("lash.stream.output_index", output_index));
            }
            push_opt(&mut attrs, "lash.tool.call_id", &event.call_id);
            push_opt(&mut attrs, "lash.tool.name", &event.tool_name);
            push_payload_json(
                &mut attrs,
                options,
                "lash.tool.input_json",
                &event.input_json,
            );
            if let Some(usage) = &event.usage {
                usage_attributes(&mut attrs, "gen_ai.usage", usage);
            }
        }
        TraceEvent::ToolCallStarted {
            call_id,
            name,
            args,
        } => {
            push_opt(&mut attrs, "lash.tool.call_id", call_id);
            attrs.push(KeyValue::new("lash.tool.name", name.clone()));
            push_payload_json(&mut attrs, options, "lash.tool.args_json", args);
        }
        TraceEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            duration_ms,
            attempts,
        } => {
            push_opt(&mut attrs, "lash.tool.call_id", call_id);
            attrs.push(KeyValue::new("lash.tool.name", name.clone()));
            attrs.push(KeyValue::new("lash.tool.success", output.is_success()));
            attrs.push(KeyValue::new(
                "lash.tool.status",
                format!("{:?}", output.status()).to_ascii_lowercase(),
            ));
            attrs.push(KeyValue::new("lash.tool.duration_ms", *duration_ms as i64));
            push_payload_json(&mut attrs, options, "lash.tool.args_json", args);
            push_payload_json(
                &mut attrs,
                options,
                "lash.tool.result_json",
                &output.value_for_projection(),
            );
            push_payload_json(&mut attrs, options, "lash.retry.attempts_json", attempts);
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
                "lash.durable.effect_name",
                effect_name.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.durable.effect_kind",
                effect_kind.clone(),
            ));
            if let TraceEvent::JournaledEffectSettled { status, .. } = &record.event {
                attrs.push(KeyValue::new("lash.durable.status", status.clone()));
            }
        }
        TraceEvent::DurableWaitParked { wait_kind } => {
            attrs.push(KeyValue::new("lash.durable.wait_kind", wait_kind.clone()));
        }
        TraceEvent::DurableWaitResolved {
            wait_kind,
            resolution,
        } => {
            attrs.push(KeyValue::new("lash.durable.wait_kind", wait_kind.clone()));
            attrs.push(KeyValue::new("lash.durable.resolution", resolution.clone()));
        }
        TraceEvent::DurableTimerStarted { duration_ms }
        | TraceEvent::DurableTimerResolved { duration_ms, .. } => {
            attrs.push(KeyValue::new(
                "lash.durable.timer.duration_ms",
                *duration_ms as i64,
            ));
            if let TraceEvent::DurableTimerResolved { status, .. } = &record.event {
                attrs.push(KeyValue::new("lash.durable.status", status.clone()));
            }
        }
        TraceEvent::DurableSegmentBoundary {
            reason,
            effects_executed,
            journaled_bytes_estimate,
        } => {
            attrs.push(KeyValue::new(
                "lash.durable.boundary_reason",
                reason.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.durable.effects_executed",
                *effects_executed as i64,
            ));
            if let Some(bytes) = journaled_bytes_estimate {
                attrs.push(KeyValue::new(
                    "lash.durable.journaled_bytes_estimate",
                    *bytes as i64,
                ));
            }
        }
        TraceEvent::StoreErrorObserved {
            operation,
            error_class,
            message,
        } => {
            attrs.push(KeyValue::new("lash.store.operation", operation.clone()));
            attrs.push(KeyValue::new("error.type", error_class.clone()));
            attrs.push(KeyValue::new("error.message", message.clone()));
        }
        TraceEvent::ProtocolStep { plugin_id, payload } => {
            attrs.push(KeyValue::new("lash.protocol.plugin_id", plugin_id.clone()));
            if let Some(phase) = protocol_step_diagnostic_phase(payload) {
                attrs.push(KeyValue::new(
                    "lash.protocol.diagnostic_phase",
                    phase.to_string(),
                ));
            }
            push_payload_json(&mut attrs, options, "lash.protocol.payload_json", payload);
        }
        TraceEvent::TokenUsage { usage, cumulative } => {
            usage_attributes(&mut attrs, "lash.usage", usage);
            if let Some(cumulative) = cumulative {
                usage_attributes(&mut attrs, "lash.usage.cumulative", cumulative);
            }
        }
        TraceEvent::LashlangExecution { event } => {
            lashlang_execution_attributes(&mut attrs, event);
            push_payload_json(
                &mut attrs,
                options,
                "lash.lashlang_execution.event_json",
                event,
            );
        }
        TraceEvent::TurnCompleted {
            status,
            done_reason,
            agent_frame_switch,
        } => {
            attrs.push(KeyValue::new("lash.turn.status", status.clone()));
            attrs.push(KeyValue::new("lash.turn.done_reason", done_reason.clone()));
            if let Some(agent_frame_switch) = agent_frame_switch {
                attrs.push(KeyValue::new(
                    "lash.turn.agent_frame_switch.frame_key",
                    agent_frame_switch.frame_key.clone(),
                ));
            }
        }
        TraceEvent::Custom { name, payload } => {
            attrs.push(KeyValue::new("lash.custom.name", name.clone()));
            push_payload_json(&mut attrs, options, "lash.custom.payload_json", payload);
        }
    }
    attrs
}

fn lashlang_execution_attributes(
    attrs: &mut Vec<KeyValue>,
    event: &crate::TraceLashlangExecutionEvent,
) {
    use crate::TraceLashlangExecutionEvent as Event;

    let kind = match event {
        Event::ExecutionStarted { .. } => "execution_started",
        Event::ExecutionFinished { .. } => "execution_finished",
        Event::NodeStarted { .. } => "node_started",
        Event::NodeCompleted { .. } => "node_completed",
        Event::NodeFailed { .. } => "node_failed",
        Event::BranchSelected { .. } => "branch_selected",
        Event::ChildStarted { .. } => "child_started",
    };
    attrs.push(KeyValue::new("lash.lashlang_execution.kind", kind));

    match event {
        Event::ExecutionStarted {
            event_key,
            identity,
            ..
        }
        | Event::ExecutionFinished {
            event_key,
            identity,
            ..
        }
        | Event::NodeStarted {
            event_key,
            identity,
            ..
        }
        | Event::NodeCompleted {
            event_key,
            identity,
            ..
        }
        | Event::NodeFailed {
            event_key,
            identity,
            ..
        }
        | Event::BranchSelected {
            event_key,
            identity,
            ..
        }
        | Event::ChildStarted {
            event_key,
            identity,
            ..
        } => {
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.event_key",
                event_key.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.graph_key",
                identity.graph_key(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.session_id",
                identity.scope.session_id.clone(),
            ));
            if let Some(turn_id) = &identity.scope.turn_id {
                attrs.push(KeyValue::new(
                    "lash.lashlang_execution.turn_id",
                    turn_id.clone(),
                ));
            }
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.module_ref",
                identity.module_ref.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.entry_kind",
                identity.entry_kind.clone(),
            ));
            push_opt(
                attrs,
                "lash.lashlang_execution.entry_ref",
                &identity.entry_ref,
            );
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.entry_name",
                identity.entry_name.clone(),
            ));
            match &identity.subject {
                crate::TraceRuntimeSubject::Effect { effect_id, kind } => {
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.subject_type",
                        "effect",
                    ));
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.effect_id",
                        effect_id.clone(),
                    ));
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.effect_kind",
                        kind.clone(),
                    ));
                }
                crate::TraceRuntimeSubject::Process { process_id } => {
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.subject_type",
                        "process",
                    ));
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.process_id",
                        process_id.clone(),
                    ));
                }
            }
        }
    }

    match event {
        Event::NodeStarted {
            node_id,
            node_kind,
            occurrence,
            ..
        }
        | Event::NodeCompleted {
            node_id,
            node_kind,
            occurrence,
            ..
        }
        | Event::NodeFailed {
            node_id,
            node_kind,
            occurrence,
            ..
        } => {
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.node_id",
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.node_kind",
                node_kind.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.occurrence",
                *occurrence as i64,
            ));
        }
        Event::BranchSelected {
            node_id,
            occurrence,
            edge_id,
            selected,
            ..
        } => {
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.node_id",
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.edge_id",
                edge_id.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.branch",
                format!("{selected:?}").to_ascii_lowercase(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.occurrence",
                *occurrence as i64,
            ));
        }
        Event::ChildStarted {
            parent_node_id,
            child,
            ..
        } => {
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.parent_node_id",
                parent_node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.child_graph_key",
                child.graph_key(),
            ));
            match &child.subject {
                crate::TraceRuntimeSubject::Effect { effect_id, kind } => {
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.child_subject_type",
                        "effect",
                    ));
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.child_effect_id",
                        effect_id.clone(),
                    ));
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.child_effect_kind",
                        kind.clone(),
                    ));
                }
                crate::TraceRuntimeSubject::Process { process_id } => {
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.child_subject_type",
                        "process",
                    ));
                    attrs.push(KeyValue::new(
                        "lash.lashlang_execution.child_process_id",
                        process_id.clone(),
                    ));
                }
            }
        }
        Event::ExecutionFinished { status, error, .. } => {
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.status",
                format!("{status:?}").to_ascii_lowercase(),
            ));
            push_opt(attrs, "lash.lashlang_execution.error", error);
        }
        Event::ExecutionStarted { execution_map, .. } => {
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.node_count",
                execution_map.nodes.len() as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.lashlang_execution.edge_count",
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

fn push_opt(attrs: &mut Vec<KeyValue>, key: &'static str, value: &Option<String>) {
    if let Some(value) = value {
        attrs.push(KeyValue::new(key, value.clone()));
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

/// Runtime diagnostics ride on `ProtocolStep` under a `diagnostic.phase`
/// envelope (see lash-core's `emit_protocol_diagnostic_trace`). Extract that
/// phase so diagnostics get distinguishable span names/attributes instead of a
/// uniform `lash.protocol_step`.
fn protocol_step_diagnostic_phase(payload: &Value) -> Option<&str> {
    payload
        .get("diagnostic")
        .and_then(|diagnostic| diagnostic.get("phase"))
        .and_then(Value::as_str)
}

/// Map a `ProtocolStep` to a span name. Runtime exec diagnostics collapse into
/// the `lash.exec_code` family (with the precise phase carried as an
/// attribute); other diagnostics keep a phase-scoped `lash.<noun>` name. Plain
/// protocol steps stay `lash.protocol_step`.
fn protocol_step_span_name(payload: &Value) -> &'static str {
    match protocol_step_diagnostic_phase(payload) {
        Some("exec_code_started" | "exec_code_completed" | "exec_code_failed") => "lash.exec_code",
        Some("observation_projection") => "lash.observation_projection",
        _ => "lash.protocol_step",
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

fn is_error_event(event: &TraceEvent) -> bool {
    match event {
        TraceEvent::LlmCallFailed { .. }
        | TraceEvent::EffectEnvelopeDiff { .. }
        | TraceEvent::StoreErrorObserved { .. } => true,
        TraceEvent::JournaledEffectSettled { status, .. }
        | TraceEvent::DurableTimerResolved { status, .. } => status == "failed",
        TraceEvent::DurableWaitResolved { resolution, .. } => resolution == "failed",
        TraceEvent::ToolCallStarted { .. } => false,
        TraceEvent::ToolCallCompleted { output, .. } => !output.is_success(),
        TraceEvent::TurnCompleted { status, .. } => status == "failed",
        _ => false,
    }
}

fn error_status(record: &TraceRecord) -> Status {
    match &record.event {
        TraceEvent::LlmCallFailed { error, .. } => Status::error(error.message.clone()),
        TraceEvent::ToolCallCompleted { name, .. } => {
            Status::error(format!("tool call failed: {name}"))
        }
        TraceEvent::TurnCompleted { status, .. } => Status::error(status.clone()),
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
        _ => Status::error("lash trace event failed"),
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::noop::NoopTracerProvider;

    use super::*;
    use crate::{TraceLlmRequest, TraceRecord};

    #[test]
    fn protocol_step_exec_diagnostics_get_distinct_span_names() {
        let diagnostic =
            |phase: &str| serde_json::json!({ "diagnostic": { "phase": phase, "payload": {} } });
        // Exec-code diagnostics collapse into the lash.exec_code family, with
        // the precise phase available as an attribute; other diagnostics keep a
        // phase-scoped name; plain protocol steps stay lash.protocol_step.
        assert_eq!(
            protocol_step_span_name(&diagnostic("exec_code_started")),
            "lash.exec_code"
        );
        assert_eq!(
            protocol_step_span_name(&diagnostic("exec_code_completed")),
            "lash.exec_code"
        );
        assert_eq!(
            protocol_step_span_name(&diagnostic("observation_projection")),
            "lash.observation_projection"
        );
        assert_eq!(
            protocol_step_span_name(&serde_json::json!({ "code": "print 1" })),
            "lash.protocol_step"
        );
        assert_eq!(
            protocol_step_diagnostic_phase(&diagnostic("exec_code_completed")),
            Some("exec_code_completed")
        );
    }

    #[test]
    fn composition_change_projects_fingerprint_counts_and_opt_in_full_payload() {
        let record = TraceRecord::new(
            TraceContext::default().for_session("session-1"),
            TraceEvent::CompositionChanged {
                fingerprint: "composition-sha".to_string(),
                rendered_system_prompt: "system policy".to_string(),
                tool_schemas: vec![crate::TraceToolSpec {
                    name: "search".to_string(),
                    description: "Search documents".to_string(),
                    input_schema: serde_json::json!({ "type": "object" }),
                    output_schema: serde_json::json!({ "type": "array" }),
                }],
            },
        );
        let attrs = event_attributes(
            &record,
            &OtelTraceOptions {
                include_payload_json: true,
                ..OtelTraceOptions::default()
            },
        );
        let attribute = |key: &str| {
            attrs
                .iter()
                .find(|attribute| attribute.key.as_str() == key)
                .map(|attribute| &attribute.value)
                .unwrap_or_else(|| panic!("missing OTel attribute {key}"))
        };

        assert_eq!(
            attribute("lash.composition.fingerprint"),
            &OtelValue::String("composition-sha".into())
        );
        assert_eq!(
            attribute("lash.composition.prompt_chars"),
            &OtelValue::I64(13)
        );
        assert_eq!(attribute("lash.composition.tool_count"), &OtelValue::I64(1));
        assert!(
            attribute("lash.composition.rendered_system_prompt_json")
                .to_string()
                .contains("system policy")
        );
        assert!(
            attribute("lash.composition.tool_schemas_json")
                .to_string()
                .contains("search")
        );
    }

    #[test]
    fn otel_sink_accepts_turn_and_llm_lifecycle() {
        let tracer = NoopTracerProvider::new().tracer("test");
        let sink = OtelTraceSink::new(tracer);
        let context = TraceContext::default()
            .for_session("session-1")
            .for_llm_call("llm-1");
        let turn_context = TraceContext {
            turn_id: Some("turn-1".to_string()),
            ..context.clone()
        };

        sink.append(&TraceRecord::new(
            turn_context.clone(),
            TraceEvent::TurnStarted {
                metadata: Default::default(),
            },
        ))
        .unwrap();
        sink.append(&TraceRecord::new(
            turn_context.clone(),
            TraceEvent::LlmCallStarted {
                request: TraceLlmRequest {
                    model: "gpt-test".to_string(),
                    model_variant: Default::default(),
                    messages: Vec::new(),
                    attachments: Vec::new(),
                    tools: Vec::new(),
                    tool_choice: "auto".to_string(),
                    output_spec: None,
                    stream: true,
                },
            },
        ))
        .unwrap();
        sink.append(&TraceRecord::new(
            turn_context.clone(),
            TraceEvent::LlmCallFailed {
                error: crate::TraceError {
                    message: "boom".to_string(),
                    retryable: false,
                    terminal_reason: None,
                    code: Some("test".to_string()),
                    raw: None,
                },
                stream_summary: None,
                attempts: None,
            },
        ))
        .unwrap();
        sink.append(&TraceRecord::new(
            turn_context,
            TraceEvent::TurnCompleted {
                status: "failed".to_string(),
                done_reason: "error".to_string(),
                agent_frame_switch: None,
            },
        ))
        .unwrap();

        assert!(sink.active.lock_recover().is_empty());
    }
}
