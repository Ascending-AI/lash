//! Host-owned providers project permitted domain observations under retained anchors.
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use opentelemetry::global::{BoxedSpan, BoxedTracer};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{
    Link, Span, SpanContext, SpanId, Status, TraceContextExt, TraceFlags, TraceId, TraceState,
    Tracer, TracerProvider,
};
use opentelemetry::{Context, KeyValue};

use lash_sansio::llm::types::{AttemptOutcome, LlmUsage};

use crate::telemetry::{
    AttemptObservation, DurableTraceScope, EmissionSource, TraceAdmissionCandidate, TraceAnchor,
    TraceCandidateOutcome, TraceCarrier, TraceCause, TraceDomainProjector, TraceHostOperation,
    TraceScopeFactory, TraceScopeId, TraceScopeKind, TraceScopeOwner, TraceToolOwner,
    UntracedScopes, W3cSpanId, W3cTraceFlags, W3cTraceId, W3cTraceState,
};
use crate::{
    TraceContext, TraceDomainOperation, TraceDomainStatus, TraceEvent, TraceRecord,
    TraceRetryAttemptDetail, TraceToolAttemptOutcome, TraceTurnOutcome,
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

/// Host naming and attribute customization over the typed domain record.
pub trait OtelSpanEnricher: Send + Sync {
    fn span_name(&self, _default: &'static str, _record: &TraceRecord) -> Option<String> {
        None
    }
    fn attributes(&self, _record: &TraceRecord, _out: &mut Vec<KeyValue>) {}
}

#[derive(Clone)]
pub struct OtelOptions {
    pub admission_limits: OtelAdmissionLimits,
    pub include_context_metadata: bool,
    /// Bytes of one record's exported JSON: its context metadata, when
    /// included, and its event payload. Whether a payload is exported at all
    /// follows the host's [`TelemetryContent`](crate::TelemetryContent)
    /// policy, read from [`TraceRecord::content`].
    pub max_payload_bytes: usize,
    pub enrich: Option<Arc<dyn OtelSpanEnricher>>,
}

/// One identity-producing adapter per runtime. The host owns flush and shutdown.
#[derive(Clone)]
pub struct OtelTelemetry {
    tracer: Arc<BoxedTracer>,
    metrics: TelemetryMetrics,
    options: OtelOptions,
    admissions: Arc<Mutex<Admissions>>,
}

/// Admission working capacities, independent of trace-carrier wire limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OtelAdmissionLimits {
    /// Remembered export identities; zero disables deduplication.
    pub exported: usize,
    /// Held deferred candidates; overflow refuses the oldest. Zero holds none.
    pub deferred: usize,
}
impl OtelAdmissionLimits {
    /// Standard preset: 4096 export identities and 256 deferred candidates.
    /// No workload measurements justify these exact capacities.
    pub const fn standard() -> Self {
        Self {
            exported: 4096,
            deferred: 256,
        }
    }
}
impl Default for OtelAdmissionLimits {
    fn default() -> Self {
        Self::standard()
    }
}
impl OtelOptions {
    /// Standard preset: metadata excluded, 4096 payload bytes per record, no
    /// enrichment and [`OtelAdmissionLimits::standard`] working capacities.
    /// Export remains opt-in through installing this adapter; its capacities
    /// lack workload measurements.
    pub fn standard() -> Self {
        Self {
            admission_limits: OtelAdmissionLimits::standard(),
            include_context_metadata: false,
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

/// An admission's export identity: its anchor's trace and span.
type AdmissionIdentity = (W3cTraceId, W3cSpanId);

fn identity(anchor: &TraceAnchor) -> Option<AdmissionIdentity> {
    anchor
        .context()
        .map(|context| (context.trace_id(), context.span_id()))
}

/// A candidate whose admission's fate its owner could not tell.
struct Deferred {
    span: BoxedSpan,
    identity: AdmissionIdentity,
    scope: TraceScopeId,
}

/// The admissions an adapter exported, by identity, and the candidates it
/// holds deferred.
#[derive(Default)]
struct Admissions {
    limits: OtelAdmissionLimits,
    exported: HashSet<AdmissionIdentity>,
    order: VecDeque<AdmissionIdentity>,
    deferred: VecDeque<Deferred>,
}

impl Admissions {
    /// Records `identity` as exported; `false` when it already was.
    fn export(&mut self, identity: AdmissionIdentity) -> bool {
        if !self.exported.insert(identity) {
            return false;
        }
        self.order.push_back(identity);
        if self.order.len() > self.limits.exported
            && let Some(oldest) = self.order.pop_front()
        {
            self.exported.remove(&oldest);
        }
        true
    }

    /// `identity` was selected for `scope`: its deferred candidate, if one
    /// is held, and every other deferred candidate of `scope`, which lost.
    fn select(
        &mut self,
        scope: &TraceScopeId,
        identity: AdmissionIdentity,
    ) -> (Option<Deferred>, Vec<Deferred>) {
        let mut selected = None;
        let mut lost = Vec::new();
        for deferred in std::mem::take(&mut self.deferred) {
            if deferred.identity == identity {
                selected = Some(deferred);
            } else if deferred.scope == *scope {
                lost.push(deferred);
            } else {
                self.deferred.push_back(deferred);
            }
        }
        (selected, lost)
    }

    /// Holds `deferred`, returning the oldest held candidate past the bound.
    fn defer(&mut self, deferred: Deferred) -> Option<Deferred> {
        self.deferred.push_back(deferred);
        if self.deferred.len() > self.limits.deferred {
            return self.deferred.pop_front();
        }
        None
    }
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
            admissions: Arc::new(Mutex::new(Admissions {
                limits: options.admission_limits,
                ..Default::default()
            })),
            options,
        }
    }

    pub fn metrics(&self) -> &TelemetryMetrics {
        &self.metrics
    }
    pub fn options(&self) -> &OtelOptions {
        &self.options
    }

    fn admissions(&self) -> std::sync::MutexGuard<'_, Admissions> {
        self.admissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
            correlation_attributes(&scope.scope, Some(&record.context), &mut attrs);
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
        let definition = DomainSpan::AdmissionAttempt.definition();
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
        correlation_attributes(scope, None, &mut attributes);
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
            scope: scope.clone(),
            admissions: Arc::clone(&self.admissions),
        })
    }

    /// A deferred candidate of the anchor is selected. Without one, the
    /// anchor's span died with the owner that proposed it, and the SDK mints
    /// every span's id: the admission is exported as a span under the
    /// anchor, which names its identity.
    fn export_admitted(&self, scope: &DurableTraceScope) {
        let TraceAnchor::Context(anchor) = &scope.anchor else {
            return;
        };
        let identity = (anchor.trace_id(), anchor.span_id());
        let (selected, lost) = {
            let mut admissions = self.admissions();
            if !admissions.export(identity) {
                return;
            }
            admissions.select(&scope.scope, identity)
        };
        let kind = scope.scope.kind();
        for deferred in lost {
            end_candidate(deferred.span, kind, TraceCandidateOutcome::Refused);
        }
        if let Some(deferred) = selected {
            end_candidate(deferred.span, kind, TraceCandidateOutcome::Selected);
            return;
        }
        let Some(context) = span_context(anchor) else {
            return;
        };
        let definition = admitted(kind).definition();
        let mut attributes = vec![
            A::ScopeKind.value(kind.as_str()),
            A::AdmissionOutcome.value(TraceCandidateOutcome::Selected.as_str()),
        ];
        correlation_attributes(&scope.scope, None, &mut attributes);
        let mut span = self
            .tracer
            .span_builder(definition.name)
            .with_kind(definition.kind)
            .with_start_time(epoch_ms(scope.started_at_ms))
            .with_attributes(attributes)
            .start_with_context(
                self.tracer.as_ref(),
                &Context::new().with_remote_span_context(context),
            );
        span.end();
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
    scope: TraceScopeId,
    admissions: Arc<Mutex<Admissions>>,
}
impl AdmissionCandidate {
    fn finish(&mut self, outcome: TraceCandidateOutcome) {
        if let Some(span) = self.span.take() {
            end_candidate(span, self.scope.kind(), outcome);
        }
    }
}
/// Ends a candidate's span: a selected one takes its scope's admitted name.
fn end_candidate(mut span: BoxedSpan, kind: TraceScopeKind, outcome: TraceCandidateOutcome) {
    if span.is_recording() {
        span.set_attribute(A::AdmissionOutcome.value(outcome.as_str()));
        if outcome == TraceCandidateOutcome::Selected {
            span.update_name(admitted(kind).definition().name);
        }
    }
    span.end();
}
impl TraceAdmissionCandidate for AdmissionCandidate {
    fn anchor(&self) -> TraceAnchor {
        self.anchor.clone()
    }
    /// A selected candidate's identity is exported: a later reconcile of it
    /// exports nothing, and the deferred candidates of its scope lost.
    fn settle(mut self: Box<Self>, outcome: TraceCandidateOutcome) {
        if outcome == TraceCandidateOutcome::Selected
            && let Some(identity) = identity(&self.anchor)
        {
            let lost = {
                let mut admissions = self
                    .admissions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                admissions.export(identity);
                admissions.select(&self.scope, identity).1
            };
            for deferred in lost {
                end_candidate(
                    deferred.span,
                    self.scope.kind(),
                    TraceCandidateOutcome::Refused,
                );
            }
        }
        self.finish(outcome);
    }
    fn defer(mut self: Box<Self>) {
        let Some(identity) = identity(&self.anchor) else {
            self.finish(TraceCandidateOutcome::Refused);
            return;
        };
        let Some(span) = self.span.take() else {
            return;
        };
        let evicted = self
            .admissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .defer(Deferred {
                span,
                identity,
                scope: self.scope.clone(),
            });
        if let Some(evicted) = evicted {
            end_candidate(
                evicted.span,
                evicted.scope.kind(),
                TraceCandidateOutcome::Refused,
            );
        }
    }
}
impl Drop for AdmissionCandidate {
    fn drop(&mut self) {
        self.finish(TraceCandidateOutcome::Refused);
    }
}

fn admitted(kind: TraceScopeKind) -> DomainSpan {
    match kind {
        TraceScopeKind::Turn => DomainSpan::TurnAdmitted,
        TraceScopeKind::Tool => DomainSpan::ToolAdmitted,
        TraceScopeKind::ToolIntent => DomainSpan::IntentAdmitted,
        TraceScopeKind::Process => DomainSpan::ProcessAdmitted,
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

fn epoch_ms(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

/// Correlation is exported independently of content. `TraceContext::run_id`
/// is host metadata; only a scope's logical run owner supplies `lash.run.id`.
fn correlation_attributes(
    scope: &TraceScopeId,
    context: Option<&TraceContext>,
    out: &mut Vec<KeyValue>,
) {
    let (session, turn, run, process) = match &scope.owner {
        TraceScopeOwner::Turn {
            session_id,
            turn_id,
        } => (Some(session_id), Some(turn_id), None, None),
        TraceScopeOwner::Process { process_id } => (None, None, None, Some(process_id)),
        TraceScopeOwner::Tool { owner, .. } => match owner {
            TraceToolOwner::Turn {
                session_id,
                turn_id,
            } => (Some(session_id), None, Some(turn_id), None),
            TraceToolOwner::Operation { session_id, .. } => (Some(session_id), None, None, None),
            TraceToolOwner::Process { process_id } => (None, None, None, Some(process_id)),
        },
        TraceScopeOwner::ToolIntent { owner, .. } => {
            (owner.session_id(), None, None, owner.process_id())
        }
    };
    if let Some(session) = context
        .and_then(|context| context.session_id.as_ref())
        .or(session)
    {
        out.push(A::SessionId.value(session.to_string()));
        out.push(A::ConversationId.value(session.to_string()));
    }
    if let Some(turn) = context
        .and_then(|context| context.turn_id.as_ref())
        .or(turn)
    {
        out.push(A::TurnId.value(turn.to_string()));
    }
    if let Some(run) = run {
        out.push(A::RunId.value(run.to_string()));
    }
    if let Some(process) = process {
        out.push(A::ProcessId.value(process.to_string()));
    }
    if let Some(context) = context {
        if let Some(id) = &context.llm_call_id {
            out.push(A::LlmCallId.value(id.clone()));
        }
        if let Some(id) = &context.run_id {
            out.push(A::HostRunId.value(id.clone()));
        }
    }
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
            TraceEvent::LlmAttemptCompleted { observation, .. } => {
                projection.span = DomainSpan::Model;
                projection.operation = Some("chat");
                projection.provider = observation.provider.as_deref();
                projection.model = Some(&observation.request_model);
                // Missing provider timings describe an instantaneous observation,
                // never the duration of the enclosing turn.
                projection.started_at_ms = Some(observation.started_at_ms.unwrap_or_else(|| {
                    observation.ended_at_ms.unwrap_or_else(|| {
                        u64::try_from(record.timestamp.timestamp_millis()).unwrap_or(0)
                    })
                }));
                projection.ended_at_ms = observation.ended_at_ms;
            }
            TraceEvent::DomainCompleted { completion } => {
                projection.span = match completion.operation {
                    TraceDomainOperation::Process => DomainSpan::Process,
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
            TraceEvent::ExecCodeCompleted { duration_ms, .. } => {
                projection.span = DomainSpan::ExecCode;
                projection.duration_ms = Some(*duration_ms);
            }
            TraceEvent::ExecCodeFailed { .. } => projection.span = DomainSpan::ExecCode,
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
            TraceEvent::LlmAttemptCompleted { attempt, .. } => {
                matches!(attempt.outcome, AttemptOutcome::Failed)
            }
            TraceEvent::DomainCompleted { completion } => {
                completion.status == TraceDomainStatus::Failed
            }
            _ => record.event.is_failed(),
        }
    }
    fn attributes(&self, record: &TraceRecord, out: &mut Vec<KeyValue>) {
        match &record.event {
            TraceEvent::LlmAttemptCompleted { attempt, .. } => {
                out.push(A::ModelAttemptOrdinal.value(i64::from(attempt.ordinal)));
                out.push(A::Outcome.value(match attempt.outcome {
                    AttemptOutcome::Completed => "completed",
                    AttemptOutcome::Failed => "failed",
                    AttemptOutcome::Aborted => "aborted",
                    AttemptOutcome::Interrupted => "interrupted",
                }));
                if let Some(model) = attempt
                    .evidence
                    .as_ref()
                    .and_then(|evidence| evidence.served_model.as_ref())
                {
                    out.push(A::ResponseModel.value(model.clone()));
                }
                if let Some(usage) = &attempt.usage {
                    usage_attributes(usage, out);
                }
                if let Some(error) = &attempt.error {
                    if let Some(status) = error.http_status {
                        out.push(A::HttpResponseStatusCode.value(i64::from(status)));
                    }
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
            TraceEvent::ToolCallCompleted {
                call_id,
                output,
                attempts,
                ..
            } => {
                out.push(A::ToolCallId.value(call_id.to_string()));
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
