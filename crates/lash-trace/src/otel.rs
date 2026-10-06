//! Host-owned providers project permitted domain observations under retained anchors.
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use opentelemetry::global::{BoxedSpan, BoxedTracer};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{
    Link, Span, SpanContext, SpanId, Status, TraceContextExt, TraceFlags, TraceId, TraceState,
    Tracer, TracerProvider,
};
use opentelemetry::{Context, KeyValue};

use crate::telemetry::{
    AttemptObservation, DurableTraceScope, EmissionSource, TraceAdmissionCandidate, TraceAnchor,
    TraceCandidateOutcome, TraceCarrier, TraceCause, TraceDomainProjector, TraceHostOperation,
    TraceScopeFactory, TraceScopeId, TraceScopeKind, UntracedScopes, W3cSpanId, W3cTraceFlags,
    W3cTraceId, W3cTraceState,
};
use crate::{
    TraceDomainOperation, TraceDomainStatus, TraceEvent, TraceLlmAttemptOutcome, TraceRecord,
    TraceTokenUsage, TraceTurnOutcome,
};

/// Exact API namespace supported by this adapter.
pub use opentelemetry as api;

mod payload;
pub use crate::telemetry::metrics::registry;
pub use crate::telemetry::metrics::{
    ObligationMetrics, ParkedWorkMetrics, RuntimeTuningMetrics, TelemetryMetrics, ToolIntentMetrics,
};
use registry::{AttributeKey as A, DomainSpan, Ownership};
pub use registry::{GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT};

/// Payload and extended-event export is explicit and bounded per record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OtelPayloadExport {
    #[default]
    Off,
    Bounded {
        max_record_bytes: usize,
        max_events: usize,
    },
}

/// Host naming and attribute customization over the typed domain record.
pub trait OtelSpanEnricher: Send + Sync {
    fn span_name(&self, _default: &'static str, _record: &TraceRecord) -> Option<String> {
        None
    }
    fn attributes(&self, _record: &TraceRecord, _out: &mut Vec<KeyValue>) {}
}

#[derive(Clone, Default)]
pub struct OtelOptions {
    pub include_context_metadata: bool,
    pub payloads: OtelPayloadExport,
    pub enrich: Option<Arc<dyn OtelSpanEnricher>>,
}

/// One identity-producing adapter per runtime. The host owns flush and shutdown.
#[derive(Clone)]
pub struct OtelTelemetry {
    tracer: Arc<BoxedTracer>,
    metrics: TelemetryMetrics,
    options: OtelOptions,
}

impl OtelTelemetry {
    pub fn new<P: TracerProvider, M: MeterProvider>(
        tracer_provider: &P,
        meter_provider: &M,
        options: OtelOptions,
    ) -> Self
    where
        P::Tracer: Send + Sync + 'static,
        <P::Tracer as Tracer>::Span: Send + Sync + 'static,
    {
        let tracer = tracer_provider.tracer_with_scope(instrumentation_scope());
        Self {
            tracer: Arc::new(BoxedTracer::new(Box::new(tracer))),
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

    fn completion(
        &self,
        scope: &DurableTraceScope,
        attempt: Option<&AttemptObservation>,
        source: &EmissionSource,
        record: &TraceRecord,
    ) {
        let TraceAnchor::Context(anchor) = &scope.anchor else {
            return;
        };
        let Some(projection) = Projection::for_record(record) else {
            return;
        };
        let definition = projection.span.definition();
        if !matches!(
            (definition.ownership, source),
            (Ownership::Live, EmissionSource::LiveExecution { .. })
                | (Ownership::Transition, EmissionSource::NewTransition)
        ) {
            return;
        }
        let end: SystemTime = projection
            .ended_at_ms
            .map(epoch_ms)
            .unwrap_or_else(|| record.timestamp.into());
        let start = projection.started_at_ms.map(epoch_ms).unwrap_or_else(|| {
            projection
                .duration_ms
                .map(|duration| {
                    end.checked_sub(Duration::from_millis(duration))
                        .unwrap_or(end)
                })
                .unwrap_or_else(|| epoch_ms(scope.started_at_ms))
        });
        let mut attributes = vec![A::ScopeKind.value(scope.scope.kind().as_str())];
        if let Some(operation) = projection.operation {
            attributes.push(A::OperationName.value(operation));
        }
        if let Some(provider) = projection.provider {
            attributes.push(A::ProviderName.value(provider.to_owned()));
        }
        if let Some(model) = projection.model {
            attributes.push(A::RequestModel.value(model.to_owned()));
        }
        if let Some(tool) = projection.tool {
            attributes.push(A::ToolName.value(tool.to_owned()));
        }
        let mut links = Vec::new();
        if let Some(observation) = attempt {
            if let Some(context) = &observation.context
                && let Some(context) = span_context(context)
            {
                links.push(Link::new(context, Vec::new(), 0));
            }
            if let Some(id) = &observation.invocation_id {
                attributes.push(A::AttemptInvocationId.value(id.clone()));
            }
        }
        let Some(anchor_context) = span_context(anchor) else {
            return;
        };
        let parent = Context::new().with_remote_span_context(anchor_context);
        let mut span = self
            .tracer
            .span_builder(projection.name())
            .with_kind(definition.kind)
            .with_start_time(start)
            .with_attributes(attributes)
            .with_links(links)
            .start_with_context(self.tracer.as_ref(), &parent);
        // Sampling is decided for this child, independently of its parent's recording.
        if span.is_recording() {
            let mut attrs = vec![
                A::RecordId.value(record.id.clone()),
                A::EventType.value(record.event.kind().as_str()),
                A::ScopeBoundary.value(i64::try_from(scope.scope.boundary).unwrap_or(i64::MAX)),
            ];
            if let Some(session) = &record.context.session_id {
                attrs.push(A::SessionId.value(session.to_string()));
            }
            if let Some(turn) = &record.context.turn_id {
                attrs.push(A::TurnId.value(turn.to_string()));
            }
            projection.attributes(record, &mut attrs);
            let mut name = projection.name();
            if let Some(enrich) = &self.options.enrich {
                if let Some(custom) = enrich.span_name(definition.name, record) {
                    name = custom;
                }
                enrich.attributes(record, &mut attrs);
            }
            span.update_name(name);
            payload::attributes(record, &self.options, &mut attrs);
            span.set_attributes(attrs);
            if projection.failed(record) {
                span.set_status(Status::error("domain operation failed"));
            }
        }
        span.end_with_timestamp(end);
    }
}

impl TraceScopeFactory for OtelTelemetry {
    fn capture_current(&self) -> Option<TraceCarrier> {
        carrier(Context::current().span().span_context())
    }

    fn begin_host_send(
        &self,
        parent: Option<&TraceCarrier>,
    ) -> Option<Box<dyn TraceHostOperation>> {
        let definition = DomainSpan::Send.definition();
        let parent = parent
            .and_then(span_context)
            .map(|context| Context::new().with_remote_span_context(context))
            .unwrap_or_default();
        let span = self
            .tracer
            .span_builder("lash.send.attempt")
            .with_kind(definition.kind)
            .with_attributes([A::OperationName.value("send")])
            .start_with_context(self.tracer.as_ref(), &parent);
        let context = carrier(span.span_context())?;
        Some(Box::new(HostOperation {
            span: Some(span),
            context,
        }))
    }

    fn propose(
        &self,
        scope: &TraceScopeId,
        cause: &TraceCause,
    ) -> Box<dyn TraceAdmissionCandidate> {
        let definition = if scope.kind() == TraceScopeKind::TriggerOccurrence {
            DomainSpan::TriggerAdmissionAttempt
        } else {
            DomainSpan::AdmissionAttempt
        }
        .definition();
        let parent = match cause {
            TraceCause::Parent(context) => {
                let Some(context) = span_context(context) else {
                    return UntracedScopes.propose(scope, cause);
                };
                Context::new().with_remote_span_context(context)
            }
            _ => Context::new(),
        };
        let mut links = Vec::new();
        if let TraceCause::Linked(causes) = cause {
            for context in causes.contexts() {
                let Some(context) = span_context(context) else {
                    return UntracedScopes.propose(scope, cause);
                };
                links.push(Link::new(context, Vec::new(), 0));
            }
        }
        let mut attributes = vec![A::ScopeKind.value(scope.kind().as_str())];
        if let TraceCause::Linked(links) = cause {
            attributes.push(A::LinksOmitted.value(i64::from(links.omitted())));
        }
        let span = self
            .tracer
            .span_builder(definition.name)
            .with_kind(definition.kind)
            .with_attributes(attributes)
            .with_links(links)
            .start_with_context(self.tracer.as_ref(), &parent);
        let anchor = carrier(span.span_context())
            .map(TraceAnchor::Context)
            .unwrap_or(TraceAnchor::Untraced);
        Box::new(AdmissionCandidate {
            span: Some(span),
            anchor,
            kind: scope.kind(),
        })
    }
}

impl TraceDomainProjector for OtelTelemetry {
    fn project(
        &self,
        scope: &DurableTraceScope,
        attempt: Option<&AttemptObservation>,
        source: &EmissionSource,
        record: &TraceRecord,
    ) {
        self.completion(scope, attempt, source, record);
    }
}

struct HostOperation {
    span: Option<BoxedSpan>,
    context: TraceCarrier,
}
impl HostOperation {
    fn finish(&mut self, outcome: TraceCandidateOutcome) {
        if let Some(mut span) = self.span.take() {
            if span.is_recording() {
                span.set_attribute(A::AdmissionOutcome.value(outcome.as_str()));
                if outcome == TraceCandidateOutcome::Selected {
                    span.update_name(DomainSpan::Send.definition().name);
                }
            }
            span.end();
        }
    }
}
impl TraceHostOperation for HostOperation {
    fn carrier(&self) -> TraceCarrier {
        self.context.clone()
    }
    fn settle(mut self: Box<Self>, outcome: TraceCandidateOutcome) {
        self.finish(outcome);
    }
}
impl Drop for HostOperation {
    fn drop(&mut self) {
        self.finish(TraceCandidateOutcome::Refused);
    }
}

struct AdmissionCandidate {
    span: Option<BoxedSpan>,
    anchor: TraceAnchor,
    kind: TraceScopeKind,
}
impl AdmissionCandidate {
    fn finish(&mut self, outcome: TraceCandidateOutcome) {
        if let Some(mut span) = self.span.take() {
            if span.is_recording() {
                span.set_attribute(A::AdmissionOutcome.value(outcome.as_str()));
                if outcome == TraceCandidateOutcome::Selected {
                    span.update_name(admitted(self.kind).definition().name);
                }
            }
            span.end();
        }
    }
}
impl TraceAdmissionCandidate for AdmissionCandidate {
    fn anchor(&self) -> TraceAnchor {
        self.anchor.clone()
    }
    fn settle(mut self: Box<Self>, outcome: TraceCandidateOutcome) {
        self.finish(outcome);
    }
}
impl Drop for AdmissionCandidate {
    fn drop(&mut self) {
        self.finish(TraceCandidateOutcome::Refused);
    }
}

fn admitted(kind: TraceScopeKind) -> DomainSpan {
    match kind {
        TraceScopeKind::Run => DomainSpan::RunAdmitted,
        TraceScopeKind::Turn => DomainSpan::TurnAdmitted,
        TraceScopeKind::Tool => DomainSpan::ToolAdmitted,
        TraceScopeKind::ToolIntent => DomainSpan::IntentAdmitted,
        TraceScopeKind::Process => DomainSpan::ProcessAdmitted,
        TraceScopeKind::TriggerOccurrence => DomainSpan::TriggerFire,
    }
}

pub use registry::instrumentation_scope;

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

fn epoch_ms(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

struct Projection<'a> {
    span: DomainSpan,
    operation: Option<&'static str>,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    tool: Option<&'a str>,
    duration_ms: Option<u64>,
    started_at_ms: Option<u64>,
    ended_at_ms: Option<u64>,
}
impl<'a> Projection<'a> {
    fn for_record(record: &'a TraceRecord) -> Option<Self> {
        let mut projection = Self {
            span: DomainSpan::Turn,
            operation: None,
            provider: None,
            model: None,
            tool: None,
            duration_ms: None,
            started_at_ms: None,
            ended_at_ms: None,
        };
        match &record.event {
            TraceEvent::TurnCompleted { .. } => {
                projection.operation = Some("invoke_agent");
            }
            TraceEvent::LlmAttemptCompleted { attempt } => {
                projection.span = DomainSpan::Model;
                projection.operation = Some("chat");
                projection.provider = attempt.provider.as_deref();
                projection.model = Some(&attempt.request_model);
                // Missing provider timings describe an instantaneous observation,
                // never the duration of the enclosing turn.
                projection.started_at_ms = Some(attempt.started_at_ms.unwrap_or_else(|| {
                    attempt.ended_at_ms.unwrap_or_else(|| {
                        u64::try_from(record.timestamp.timestamp_millis()).unwrap_or(0)
                    })
                }));
                projection.ended_at_ms = attempt.ended_at_ms;
            }
            TraceEvent::DomainCompleted { completion } => {
                projection.span = match completion.operation {
                    TraceDomainOperation::Run => DomainSpan::Run,
                    TraceDomainOperation::Process => DomainSpan::Process,
                    TraceDomainOperation::ProcessSegment => DomainSpan::ProcessSegment,
                    TraceDomainOperation::Send => DomainSpan::Send,
                    TraceDomainOperation::ToolIntent => DomainSpan::Intent,
                };
                projection.started_at_ms = Some(completion.started_at_ms);
                projection.provider = completion.provider.as_deref();
                projection.model = completion.model.as_deref();
                projection.tool = completion.tool_name.as_deref();
            }
            TraceEvent::ToolCallCompleted { name, .. } => {
                projection.span = DomainSpan::Tool;
                projection.operation = Some("execute_tool");
                projection.tool = Some(name);
            }
            TraceEvent::ToolReceipt {
                name,
                started_at_ms,
                terminal: Some(_),
                ..
            } => {
                projection.span = DomainSpan::Tool;
                projection.operation = Some("execute_tool");
                projection.tool = Some(name);
                projection.started_at_ms = Some(*started_at_ms);
            }
            TraceEvent::ExecCodeCompleted { duration_ms, .. } => {
                projection.span = DomainSpan::ExecCode;
                projection.duration_ms = Some(*duration_ms);
            }
            TraceEvent::ExecCodeFailed { .. } => projection.span = DomainSpan::ExecCode,
            TraceEvent::DurableWaitResolved { started_at_ms, .. } => {
                projection.span = DomainSpan::Wait;
                projection.started_at_ms = Some(*started_at_ms);
            }
            TraceEvent::DurableTimerResolved { duration_ms, .. } => {
                projection.span = DomainSpan::Wait;
                projection.duration_ms = Some(*duration_ms);
            }
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
            TraceEvent::LlmAttemptCompleted { attempt } => {
                matches!(attempt.outcome, TraceLlmAttemptOutcome::Failed)
            }
            TraceEvent::DomainCompleted { completion } => {
                completion.status == TraceDomainStatus::Failed
            }
            _ => record.event.is_failed(),
        }
    }
    fn attributes(&self, record: &TraceRecord, out: &mut Vec<KeyValue>) {
        match &record.event {
            TraceEvent::LlmAttemptCompleted { attempt } => {
                out.push(A::ModelAttemptOrdinal.value(i64::from(attempt.ordinal)));
                out.push(A::Outcome.value(match attempt.outcome {
                    TraceLlmAttemptOutcome::Completed => "completed",
                    TraceLlmAttemptOutcome::Failed => "failed",
                    TraceLlmAttemptOutcome::Aborted => "aborted",
                    TraceLlmAttemptOutcome::Interrupted => "interrupted",
                }));
                if let Some(model) = &attempt.response_model {
                    out.push(A::ResponseModel.value(model.clone()));
                }
                if let Some(usage) = &attempt.usage {
                    usage_attributes(usage, out);
                }
                if let Some(error) = &attempt.error {
                    out.push(
                        A::ErrorType.value(
                            error
                                .code
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_else(|| error.class.code().to_owned()),
                        ),
                    );
                } else if self.failed(record) {
                    out.push(A::ErrorType.value("provider_failure"));
                }
            }
            TraceEvent::DomainCompleted { completion } => {
                out.push(A::Outcome.value(match completion.status {
                    TraceDomainStatus::Completed => "completed",
                    TraceDomainStatus::Failed => "failed",
                    TraceDomainStatus::Cancelled => "cancelled",
                    TraceDomainStatus::Yielded => "yielded",
                }));
                if let Some(kind) = &completion.intent_kind {
                    out.push(A::ToolIntentKind.value(kind.clone()));
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
            TraceEvent::ToolCallCompleted { call_id, .. }
            | TraceEvent::ToolReceipt { call_id, .. } => {
                out.push(A::ToolCallId.value(call_id.to_string()));
                if self.failed(record) {
                    out.push(A::ErrorType.value("tool_failure"));
                }
            }
            TraceEvent::TurnCompleted { outcome } => {
                out.push(A::Outcome.value(match outcome {
                    TraceTurnOutcome::Completed { .. } => "completed",
                    TraceTurnOutcome::AgentFrameSwitch { .. } => "agent_frame_switch",
                    TraceTurnOutcome::SegmentBoundary { .. } => "segment_boundary",
                    TraceTurnOutcome::Cancelled { .. } => "cancelled",
                    TraceTurnOutcome::Failed { .. } => "failed",
                }));
                if self.failed(record) {
                    out.push(A::ErrorType.value("turn_failure"));
                }
            }
            TraceEvent::DurableWaitResolved { wait_kind, .. } => {
                out.push(A::WaitKind.value(wait_kind.clone()));
                if self.failed(record) {
                    out.push(A::ErrorType.value("wait_failure"));
                }
            }
            TraceEvent::DurableTimerResolved { .. } => out.push(A::WaitKind.value("timer")),
            _ if self.failed(record) => out.push(A::ErrorType.value("domain_failure")),
            _ => {}
        }
    }
}
fn usage_attributes(usage: &TraceTokenUsage, out: &mut Vec<KeyValue>) {
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
