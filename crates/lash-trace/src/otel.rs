//! The OpenTelemetry exporter: one [`TraceSink`] that builds a host SDK span
//! from each published record, with lash's span ids and sampling decisions.
//!
//! A span is started and ended in one call when its closing (or whole)
//! record arrives, from the record's retained start and end. No SDK span is
//! held open anywhere. The exporter is also the runtime's
//! [`TraceIdentity`]: it reads the caller's ambient context, draws producer
//! and leaf ids from the host's fallback id generator, and asks the host's
//! sampler at producers, the one decision every lash span of the trace
//! follows.
use std::sync::Arc;

use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{
    Link, Span, SpanBuilder, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags,
    TraceId, TraceState, Tracer, TracerProvider,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{IdGenerator, SamplingDecision, SdkTracer};

use lash_sansio::llm::types::LlmUsage;

use crate::telemetry::{
    ProducerKind, ProducerSpec, TraceCarrier, TraceIdentity, TraceLinkMode, TraceScopeId,
    TraceScopeOwner, TraceToolOwner, W3cSpanId, W3cTraceFlags, W3cTraceId, W3cTraceState,
};
use crate::{
    TraceContext, TraceDomainStatus, TraceDomainSubject, TraceEvent, TracePosition, TraceRecord,
    TraceRetryAttemptDetail, TraceSink, TraceSinkError, TraceSpanKind, TraceToolAttemptOutcome,
    TraceTurnOutcome,
};

/// Exact API namespace supported by this adapter.
pub use opentelemetry as api;
/// Exact SDK namespace [`LashTracerProvider`] wraps.
pub use opentelemetry_sdk as sdk;

mod payload;
mod provider;
pub use crate::telemetry::metrics::registry;
pub use crate::telemetry::metrics::{
    ObligationMetrics, ParkedWorkMetrics, RuntimeTuningMetrics, TelemetryMetrics, ToolIntentMetrics,
};
pub use provider::LashTracerProvider;
use registry::{AttributeKey as A, DomainSpan};
pub use registry::{GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT};

/// Host naming and attribute customization over the typed domain record.
pub trait OtelSpanEnricher: Send + Sync {
    fn span_name(&self, _default: &'static str, _record: &TraceRecord) -> Option<String> {
        None
    }
    fn attributes(&self, _record: &TraceRecord, _out: &mut Vec<KeyValue>) {}
}

#[derive(Clone)]
pub struct OtelOptions {
    /// Bytes of one record's exported event payload. Whether a payload is
    /// exported at all follows the host's
    /// [`TelemetryContent`](crate::TelemetryContent) policy, read from
    /// [`TraceRecord::content`].
    pub max_payload_bytes: usize,
    pub enrich: Option<Arc<dyn OtelSpanEnricher>>,
}

impl OtelOptions {
    /// Standard preset: 4096 payload bytes per record and no enrichment.
    /// Export remains opt-in through installing this adapter.
    pub fn standard() -> Self {
        Self {
            max_payload_bytes: 4096,
            enrich: None,
        }
    }
}
impl Default for OtelOptions {
    fn default() -> Self {
        Self::standard()
    }
}

/// The runtime's OpenTelemetry exporter and identity. The host owns its
/// provider's flush and shutdown.
#[derive(Clone)]
pub struct OtelTelemetry {
    tracer: SdkTracer,
    provider: LashTracerProvider,
    metrics: TelemetryMetrics,
    options: OtelOptions,
}

impl OtelTelemetry {
    pub fn new(
        tracer_provider: &LashTracerProvider,
        meter_provider: &impl MeterProvider,
        options: OtelOptions,
    ) -> Self {
        Self {
            tracer: tracer_provider
                .provider()
                .tracer_with_scope(instrumentation_scope()),
            provider: tracer_provider.clone(),
            metrics: TelemetryMetrics::new(
                meter_provider.meter_with_scope(instrumentation_scope()),
            ),
            options,
        }
    }

    pub fn metrics(&self) -> &TelemetryMetrics {
        &self.metrics
    }
    pub fn options(&self) -> &OtelOptions {
        &self.options
    }

    fn fallback(&self) -> &Arc<dyn IdGenerator> {
        self.provider.id_fallback()
    }

    fn span_id(&self) -> W3cSpanId {
        loop {
            if let Ok(id) = W3cSpanId::from_bytes(self.fallback().new_span_id().to_bytes()) {
                return id;
            }
        }
    }

    fn trace_id(&self) -> W3cTraceId {
        loop {
            if let Ok(id) = W3cTraceId::from_bytes(self.fallback().new_trace_id().to_bytes()) {
                return id;
            }
        }
    }

    /// Builds the SDK span `record` publishes: a sampled closing or whole
    /// record, at its position's preset id, under its parent, from its
    /// retained start to its end.
    fn export(&self, record: &TraceRecord, position: &TracePosition) {
        let Some(projection) = Projection::for_record(record, position) else {
            return;
        };
        let Some(own) = span_context(&position.span) else {
            return;
        };
        let parent = position
            .parent
            .as_ref()
            .and_then(span_context)
            .map_or_else(Context::new, |parent| {
                Context::new().with_remote_span_context(parent)
            });
        let mut links = position
            .links
            .contexts()
            .iter()
            .filter_map(span_context)
            .map(|context| Link::new(context, Vec::new(), 0))
            .collect::<Vec<_>>();
        let mut attributes = Vec::new();
        if let Some(scope) = &position.scope {
            attributes.push(A::ScopeKind.value(scope.kind().as_str()));
            attributes
                .push(A::ScopeBoundary.value(i64::try_from(scope.boundary).unwrap_or(i64::MAX)));
        }
        if position.links.omitted() > 0 {
            attributes.push(A::LinksOmitted.value(i64::from(position.links.omitted())));
        }
        if let Some(attempt) = &position.attempt {
            if let Some(context) = attempt.context.as_ref().and_then(span_context) {
                links.push(Link::new(context, Vec::new(), 0));
            }
            if let Some(id) = &attempt.invocation_id {
                attributes.push(A::AttemptInvocationId.value(id.clone()));
            }
        }
        attributes.push(A::RecordId.value(record.id.clone()));
        attributes.push(A::EventType.value(record.event.kind().as_str()));
        correlation_attributes(position.scope.as_ref(), &record.context, &mut attributes);
        projection.attributes(record, &mut attributes);
        let definition = projection.span.definition();
        let mut name = projection.name();
        if let Some(enrich) = &self.options.enrich {
            if let Some(custom) = enrich.span_name(definition.name, record) {
                name = custom;
            }
            enrich.attributes(record, &mut attributes);
        }
        payload::attributes(record, &self.options, &mut attributes);
        let builder = SpanBuilder::from_name(name)
            .with_kind(definition.kind)
            .with_start_time(position.started_at.to_system_time())
            .with_attributes(attributes)
            .with_links(links);
        let mut span = {
            let _preset = provider::PresetGuard::set(
                own.span_id(),
                position.parent.is_none().then(|| own.trace_id()),
                own.trace_state().clone(),
            );
            self.tracer.build_with_context(builder, &parent)
        };
        if projection.failed(record) {
            span.set_status(Status::error("domain operation failed"));
        }
        let end = position.ended_at.unwrap_or(record.timestamp);
        span.end_with_timestamp(end.to_system_time());
    }
}

/// Builds a span from every sampled record that publishes one. An unsampled
/// record, an opening record and an event build nothing.
impl TraceSink for OtelTelemetry {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        if let Some(position) = &record.position
            && position.publishes()
            && position.is_sampled()
        {
            self.export(record, position);
        }
        Ok(())
    }
}

impl TraceIdentity for OtelTelemetry {
    fn ambient(&self) -> Option<TraceCarrier> {
        carrier(Context::current().span().span_context())
    }

    /// Draws the producer's ids from the host's fallback generator, then
    /// asks the host's sampler: the producer's trace is sampled iff the
    /// sampler records and samples it, and takes the result's tracestate.
    fn open_producer(&self, spec: &ProducerSpec) -> TraceCarrier {
        let span_id = self.span_id();
        let caller = spec.caller.as_ref();
        let (parent, trace_id, links) = match (caller.and_then(span_context), spec.link_mode) {
            (Some(caller), TraceLinkMode::Parent) => (
                Context::new().with_remote_span_context(caller.clone()),
                W3cTraceId::from_bytes(caller.trace_id().to_bytes())
                    .unwrap_or_else(|_| self.trace_id()),
                Vec::new(),
            ),
            (Some(caller), TraceLinkMode::Link) => (
                Context::new(),
                self.trace_id(),
                vec![Link::new(caller, Vec::new(), 0)],
            ),
            (None, _) => (Context::new(), self.trace_id(), Vec::new()),
        };
        let result = self.provider.sampler().should_sample(
            Some(&parent),
            TraceId::from_bytes(trace_id.to_bytes()),
            producer(spec.kind).definition().name,
            &SpanKind::Producer,
            &[],
            &links,
        );
        let sampled = result.decision == SamplingDecision::RecordAndSample;
        let caller_flags = match spec.link_mode {
            TraceLinkMode::Parent => caller.map_or(0, |caller| caller.flags().to_byte()),
            TraceLinkMode::Link => 0,
        };
        let flags = if sampled {
            caller_flags | W3cTraceFlags::SAMPLED
        } else {
            caller_flags & !W3cTraceFlags::SAMPLED
        };
        TraceCarrier::new(
            trace_id,
            span_id,
            W3cTraceFlags::from_byte(flags),
            W3cTraceState::parse(&result.trace_state.header()).unwrap_or_default(),
        )
    }

    fn leaf_span_id(&self) -> W3cSpanId {
        self.span_id()
    }
}

fn producer(kind: ProducerKind) -> DomainSpan {
    match kind {
        ProducerKind::Send => DomainSpan::Send,
        ProducerKind::SendBatch => DomainSpan::SendBatch,
        ProducerKind::ProcessStart => DomainSpan::ProcessStart,
        ProducerKind::ToolIntentSubmit => DomainSpan::IntentSubmit,
        ProducerKind::PluginInvoke => DomainSpan::PluginInvoke,
    }
}

pub use registry::{instrumentation_scope, recommended_latency_histogram_boundaries};

/// A retained or transported context always has remote provenance.
pub fn span_context(context: &TraceCarrier) -> Option<SpanContext> {
    let state = TraceState::from_key_value(context.tracestate().members()).ok()?;
    Some(SpanContext::new(
        TraceId::from_bytes(context.trace_id().to_bytes()),
        SpanId::from_bytes(context.span_id().to_bytes()),
        TraceFlags::new(context.flags().to_byte()),
        true,
        state,
    ))
}

/// Includes valid unsampled contexts; an invalid no-op context has no carrier.
pub fn carrier(context: &SpanContext) -> Option<TraceCarrier> {
    Some(TraceCarrier::new(
        W3cTraceId::from_bytes(context.trace_id().to_bytes()).ok()?,
        W3cSpanId::from_bytes(context.span_id().to_bytes()).ok()?,
        W3cTraceFlags::from_byte(context.trace_flags().to_u8()),
        W3cTraceState::parse(&context.trace_state().header()).ok()?,
    ))
}

/// Correlation is exported independently of content: the record's stable
/// ids, and the run its scope's logical owner names.
fn correlation_attributes(
    scope: Option<&TraceScopeId>,
    context: &TraceContext,
    out: &mut Vec<KeyValue>,
) {
    let (session, turn, run, process) = match scope.map(|scope| &scope.owner) {
        Some(TraceScopeOwner::Turn {
            session_id,
            turn_id,
        }) => (Some(session_id), Some(turn_id), None, None),
        Some(TraceScopeOwner::Process { process_id }) => (None, None, None, Some(process_id)),
        Some(TraceScopeOwner::Tool { owner, .. }) => match owner {
            TraceToolOwner::Turn {
                session_id,
                turn_id,
            } => (Some(session_id), None, Some(turn_id), None),
            TraceToolOwner::Operation { session_id, .. } => (Some(session_id), None, None, None),
            TraceToolOwner::Process { process_id } => (None, None, None, Some(process_id)),
        },
        Some(TraceScopeOwner::ToolIntent { owner, .. }) => {
            (owner.session_id(), None, None, owner.process_id())
        }
        None => (None, None, None, None),
    };
    if let Some(session) = context.session_id.as_ref().or(session) {
        out.push(A::SessionId.value(session.to_string()));
        out.push(A::ConversationId.value(session.to_string()));
    }
    if let Some(turn) = context.turn_id.as_ref().or(turn) {
        out.push(A::TurnId.value(turn.to_string()));
    }
    if let Some(run) = run {
        out.push(A::RunId.value(run.to_string()));
    }
    if let Some(process) = context.process_id.as_ref().or(process) {
        out.push(A::ProcessId.value(process.to_string()));
    }
    if let Some(send) = &context.send_id {
        out.push(A::SendId.value(send.to_string()));
    }
    if let Some(id) = &context.llm_call_id {
        out.push(A::LlmCallId.value(id.clone()));
    }
}

struct Projection<'a> {
    span: DomainSpan,
    model: Option<&'a str>,
    tool: Option<&'a str>,
}
impl<'a> Projection<'a> {
    fn for_record(record: &'a TraceRecord, position: &TracePosition) -> Option<Self> {
        let mut projection = Self {
            span: DomainSpan::Turn,
            model: None,
            tool: None,
        };
        match (position.kind, &record.event) {
            (TraceSpanKind::Producer, TraceEvent::ProducerSettled { producer: kind, .. }) => {
                projection.span = producer(*kind);
            }
            (TraceSpanKind::Agent, TraceEvent::TurnCompleted { .. }) => {}
            (TraceSpanKind::Model, TraceEvent::LlmCallCompleted { response, .. }) => {
                projection.span = DomainSpan::Model;
                projection.model = Some(&response.request_model);
            }
            (TraceSpanKind::Model, TraceEvent::LlmCallFailed { .. }) => {
                projection.span = DomainSpan::Model;
            }
            (TraceSpanKind::Process, TraceEvent::DomainCompleted { completion }) => {
                projection.span = DomainSpan::Process;
                projection.model = completion.model.as_deref();
                projection.tool = completion.tool_name.as_deref();
            }
            (TraceSpanKind::ToolIntent, TraceEvent::DomainCompleted { completion }) => {
                projection.span = DomainSpan::Intent;
                projection.tool = completion.tool_name.as_deref();
            }
            (TraceSpanKind::Tool, TraceEvent::ToolCallCompleted { name, .. }) => {
                projection.span = DomainSpan::Tool;
                projection.tool = Some(name);
            }
            (
                TraceSpanKind::Cell,
                TraceEvent::ExecCodeCompleted { .. } | TraceEvent::ExecCodeFailed { .. },
            ) => projection.span = DomainSpan::ExecCode,
            _ => return None,
        }
        Some(projection)
    }
    fn name(&self) -> String {
        let default = self.span.definition().name;
        let suffix = match self.span {
            DomainSpan::Model => self.model,
            DomainSpan::Tool => self.tool,
            _ => None,
        };
        match suffix {
            Some(suffix) => format!("{default} {suffix}"),
            None => default.to_owned(),
        }
    }
    fn failed(&self, record: &TraceRecord) -> bool {
        match &record.event {
            TraceEvent::DomainCompleted { completion } => {
                completion.status == TraceDomainStatus::Failed
            }
            _ => record.event.is_failed(),
        }
    }
    fn attributes(&self, record: &TraceRecord, out: &mut Vec<KeyValue>) {
        if let Some(operation) = self.operation() {
            out.push(A::OperationName.value(operation));
        }
        if let Some(tool) = self.tool {
            out.push(A::ToolName.value(tool.to_owned()));
        }
        match &record.event {
            TraceEvent::ProducerSettled { outcome, .. } => {
                out.push(A::ProducerOutcome.value(outcome.as_str()));
                if let crate::ProducerOutcome::Refused { error_type } = outcome {
                    out.push(A::ErrorType.value(error_type.to_string()));
                }
            }
            TraceEvent::LlmCallCompleted {
                response,
                usage,
                attempts,
                ..
            } => {
                out.push(A::RequestModel.value(response.request_model.clone()));
                if let Some(model) = attempts
                    .iter()
                    .flatten()
                    .rev()
                    .find_map(|attempt| attempt.evidence.as_ref()?.served_model.clone())
                {
                    out.push(A::ResponseModel.value(model));
                }
                if let Some(attempts) = attempts {
                    out.push(
                        A::ModelAttempts.value(i64::try_from(attempts.len()).unwrap_or(i64::MAX)),
                    );
                }
                if let Some(usage) = usage {
                    usage_attributes(usage, out);
                }
            }
            TraceEvent::LlmCallFailed {
                error, attempts, ..
            } => {
                if let Some(attempts) = attempts {
                    out.push(
                        A::ModelAttempts.value(i64::try_from(attempts.len()).unwrap_or(i64::MAX)),
                    );
                }
                let last = attempts
                    .iter()
                    .flatten()
                    .rev()
                    .find_map(|attempt| attempt.error.as_ref());
                out.push(
                    A::ErrorType.value(match (last, &error.code) {
                        (Some(error), _) => error
                            .code
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| error.class.code().to_owned()),
                        (None, Some(code)) => code.to_string(),
                        (None, None) => "provider_failure".to_owned(),
                    }),
                );
            }
            TraceEvent::DomainCompleted { completion } => {
                out.push(A::Outcome.value(match completion.status {
                    TraceDomainStatus::Completed => "completed",
                    TraceDomainStatus::Failed => "failed",
                    TraceDomainStatus::Cancelled => "cancelled",
                }));
                if let Some(model) = &completion.model {
                    out.push(A::RequestModel.value(model.clone()));
                }
                if let Some(provider) = &completion.provider {
                    out.push(A::ProviderName.value(provider.clone()));
                }
                if let TraceDomainSubject::ToolIntent { kind } = completion.subject {
                    out.push(A::ToolIntentKind.value(kind.as_str()));
                }
                if let Some(id) = &completion.tool_call_id {
                    out.push(A::ToolCallId.value(id.clone()));
                }
                if let Some(usage) = &completion.usage {
                    usage_attributes(usage, out);
                }
                if let Some(code) = &completion.error_code {
                    out.push(A::ErrorType.value(code.to_string()));
                } else if self.failed(record) {
                    out.push(A::ErrorType.value("domain_failure"));
                }
            }
            TraceEvent::ToolCallCompleted {
                call_id,
                output,
                attempts,
                ..
            } => {
                out.push(A::ToolCallId.value(call_id.to_string()));
                out.push(A::Outcome.value(match output.status() {
                    crate::TraceToolCallStatus::Success => "completed",
                    crate::TraceToolCallStatus::Failure => "failed",
                    crate::TraceToolCallStatus::Cancelled => "cancelled",
                    crate::TraceToolCallStatus::Discarded => "discarded",
                }));
                if self.failed(record) {
                    // A controller failure after an attempt can replace the
                    // terminal output; its class is authoritative over earlier attempts.
                    let terminal_class = match &output.outcome {
                        crate::TraceToolCallOutcome::Failure(value) => value
                            .get("class")
                            .and_then(|class| {
                                serde_json::from_value::<lash_sansio::ToolFailureClass>(
                                    class.clone(),
                                )
                                .ok()
                            })
                            .map(|class| crate::wire_tag(&class)),
                        _ => None,
                    };
                    let class = terminal_class.or_else(|| {
                        attempts
                            .as_ref()
                            .and_then(|attempts| attempts.last())
                            .and_then(|attempt| match &attempt.detail {
                                TraceRetryAttemptDetail::Tool {
                                    outcome: TraceToolAttemptOutcome::Failed { class, .. },
                                } => Some(crate::wire_tag(class)),
                                _ => None,
                            })
                    });
                    out.push(A::ErrorType.value(class.unwrap_or_else(|| "unknown".into())));
                }
            }
            TraceEvent::TurnCompleted { outcome } => {
                out.push(A::Outcome.value(match outcome {
                    TraceTurnOutcome::Completed { .. } => "completed",
                    TraceTurnOutcome::AgentFrameSwitch { .. } => "agent_frame_switch",
                    TraceTurnOutcome::Cancelled { .. } => "cancelled",
                    TraceTurnOutcome::Failed { .. } => "failed",
                }));
                if let TraceTurnOutcome::Cancelled { evidence } = outcome
                    && let Some(origin) = &evidence.origin
                {
                    out.push(A::CancelOrigin.value(origin.clone()));
                }
                if let TraceTurnOutcome::Failed { done_reason } = outcome {
                    out.push(A::ErrorType.value(done_reason.wire_tag()));
                }
            }
            TraceEvent::ExecCodeFailed { reason, .. } => {
                out.push(A::ErrorType.value(crate::wire_tag(reason)));
            }
            TraceEvent::ExecCodeCompleted {
                error: Some(error), ..
            } => {
                let reason = error
                    .exec_failure
                    .as_ref()
                    .map(crate::wire_tag)
                    .unwrap_or_else(|| crate::wire_tag(&error.kind));
                out.push(A::ErrorType.value(reason));
            }
            _ if self.failed(record) => out.push(A::ErrorType.value("domain_failure")),
            _ => {}
        }
    }
    fn operation(&self) -> Option<&'static str> {
        match self.span {
            DomainSpan::Turn => Some("invoke_agent"),
            DomainSpan::Model => Some("chat"),
            DomainSpan::Tool => Some("execute_tool"),
            _ => None,
        }
    }
}
fn usage_attributes(usage: &LlmUsage, out: &mut Vec<KeyValue>) {
    out.extend([
        A::InputTokens.value(usage.input_tokens),
        A::OutputTokens.value(usage.output_tokens),
        A::CacheReadTokens.value(usage.cache_read_input_tokens),
        A::CacheWriteTokens.value(usage.cache_write_input_tokens),
        A::ReasoningTokens.value(usage.reasoning_output_tokens),
    ]);
}

#[cfg(test)]
mod tests;
