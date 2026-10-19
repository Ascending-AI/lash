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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_protocol_types::sync::MutexExt;
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{
    Link, Span, SpanBuilder, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags,
    TraceId, TraceState, Tracer, TracerProvider,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{IdGenerator, SamplingDecision, SdkTracer};

use lash_protocol_types::{llm::types::LlmUsage, llm::types::TokenUsageOverflow};

use crate::policy::{Contributions, TraceAttributeWriter, TraceOmissionCounts};
use crate::telemetry::{
    ProducerKind, ProducerSpec, TraceCarrier, TraceIdentity, TraceLinkMode, TraceScopeId,
    TraceScopeOwner, TraceToolOwner, W3cSpanId, W3cTraceFlags, W3cTraceId, W3cTraceState,
};
use crate::{
    SpanRole, TraceContext, TraceDomainStatus, TraceDomainSubject, TraceEvent, TraceExportPolicy,
    TraceMetrics, TraceOmissionLedger, TraceOmissionSite, TracePolicySnapshot, TracePosition,
    TraceRecord, TraceSink, TraceSinkError, TraceSpanKind, TraceTimestamp, TraceTurnOutcome,
};

/// Exact API namespace supported by this adapter.
pub use opentelemetry as api;
/// Exact SDK namespace [`LashTracerProvider`] wraps.
pub use opentelemetry_sdk as sdk;

mod content_attributes;
mod enrich;
mod genai;
mod model;
mod provider;
pub use crate::telemetry::metrics::registry;
pub use crate::telemetry::metrics::{
    ObligationMetrics, ParkedWorkMetrics, RuntimeTuningMetrics, TelemetryMetrics, ToolIntentMetrics,
};
pub use enrich::{SpanEnricher, SpanEnrichment, SpanView};
pub use provider::LashTracerProvider;
use registry::{AttributeKey as A, DomainSpan};
pub use registry::{GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT};

/// The exporter's host choices.
#[derive(Clone)]
pub struct OtelOptions {
    /// The host's enricher: none by default.
    pub enricher: Option<Arc<dyn SpanEnricher>>,
    /// Which kinds become spans: every kind lash builds by default.
    pub export: TraceExportPolicy,
    /// Which metric families are recorded: all by default.
    pub metrics: TraceMetrics,
}

impl OtelOptions {
    /// No enricher, every kind exported, every metric family recorded.
    /// Export remains opt-in through installing the exporter.
    pub fn standard() -> Self {
        Self {
            enricher: None,
            export: TraceExportPolicy::standard(),
            metrics: TraceMetrics::All,
        }
    }
}
impl Default for OtelOptions {
    fn default() -> Self {
        Self::standard()
    }
}

/// A producer's contributions, made before its decision and exported on its
/// span at settle: its enrichment, then the host sampler's result
/// attributes. Held in memory only, from the producer's open to its settle.
type ProducerContributions = (Contributions, TraceOmissionCounts);

/// The runtime's OpenTelemetry exporter and identity. The host owns its
/// provider's flush and shutdown.
#[derive(Clone)]
pub struct OtelTelemetry {
    tracer: SdkTracer,
    provider: LashTracerProvider,
    metrics: TelemetryMetrics,
    genai: genai::GenAiMetrics,
    options: OtelOptions,
    policy: Arc<TracePolicySnapshot>,
    omissions: Arc<TraceOmissionLedger>,
    producers: Arc<Mutex<HashMap<W3cSpanId, ProducerContributions>>>,
}

impl OtelTelemetry {
    pub fn new(
        tracer_provider: &LashTracerProvider,
        meter_provider: &dyn MeterProvider,
        options: OtelOptions,
    ) -> Self {
        let metrics = if options.metrics.records_runtime() {
            TelemetryMetrics::new(meter_provider.meter_with_scope(instrumentation_scope()))
        } else {
            TelemetryMetrics::default()
        };
        let genai_meter = if options.metrics.records_gen_ai() {
            meter_provider.meter_with_scope(instrumentation_scope())
        } else {
            crate::telemetry::metrics::unobserved_meter()
        };
        let omissions = Arc::new(TraceOmissionLedger::default());
        omissions.bind_metrics(metrics.runtime_tuning.clone());
        let policy = TracePolicySnapshot {
            export: Some(options.export),
            metrics: Some(options.metrics),
            enricher: options.enricher.is_some(),
            ..TracePolicySnapshot::standard()
        };
        Self {
            tracer: tracer_provider
                .provider()
                .tracer_with_scope(instrumentation_scope()),
            provider: tracer_provider.clone(),
            metrics,
            genai: genai::GenAiMetrics::new(genai_meter),
            options,
            policy: Arc::new(policy),
            omissions,
            producers: Arc::default(),
        }
    }

    pub fn metrics(&self) -> &TelemetryMetrics {
        &self.metrics
    }
    pub fn options(&self) -> &OtelOptions {
        &self.options
    }

    /// Binds the core's effective policies and its omission ledger: the
    /// builder installing this exporter calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn bound(
        mut self,
        policy: TracePolicySnapshot,
        omissions: Arc<TraceOmissionLedger>,
    ) -> Self {
        self.policy = Arc::new(TracePolicySnapshot {
            export: Some(self.options.export),
            metrics: Some(self.options.metrics),
            enricher: self.options.enricher.is_some(),
            ..policy
        });
        omissions.bind_metrics(self.metrics.runtime_tuning.clone());
        self.omissions = omissions;
        self
    }

    /// The policies the exporter applies.
    pub fn policy(&self) -> &TracePolicySnapshot {
        &self.policy
    }

    /// Runs the enricher over a span that already holds `writer`'s
    /// contributions, and counts what was refused at the span.
    fn enriched(&self, view: &SpanView<'_>, writer: TraceAttributeWriter) -> TraceAttributeWriter {
        match &self.options.enricher {
            Some(enricher) => enrich::enrich(enricher.as_ref(), view, writer),
            None => writer,
        }
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
    fn export(
        &self,
        record: &TraceRecord,
        position: &TracePosition,
        producer: Option<ProducerContributions>,
    ) {
        if !self.options.export.exports(position.kind) {
            return;
        }
        let Some(projection) = Projection::for_record(record, position) else {
            return;
        };
        self.export_projection(record, position, producer, projection);
    }

    fn export_projection(
        &self,
        record: &TraceRecord,
        position: &TracePosition,
        producer: Option<ProducerContributions>,
        projection: Projection<'_>,
    ) {
        if !self.options.export.exports(position.kind) {
            return;
        }
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
        if position.kind != TraceSpanKind::ModelAttempt {
            content_attributes::attributes(record, &mut attributes);
        }
        let mut name = projection.name();
        let (contributions, mut omissions) = match producer {
            Some(contributed) => contributed,
            None => {
                let ids = record.context.ids();
                let view = SpanView {
                    kind: position.kind,
                    default_name: &name,
                    ids: &ids,
                    position,
                    record,
                    attempt: projection.attempt,
                    usage: projection.usage.as_ref().and_then(|u| u.as_ref().ok()),
                    attributes: &attributes,
                    policy: &self.policy,
                };
                let mut writer = TraceAttributeWriter::new(position.kind, &self.policy.limits);
                contributed(record, &mut writer);
                if position.kind != TraceSpanKind::ModelAttempt {
                    for (key, value) in &record.content.references {
                        let _ = writer.set(key.clone(), value.clone());
                    }
                }
                self.enriched(&view, writer).finish()
            }
        };
        if !omissions.is_empty() {
            self.omissions.record(
                &TraceOmissionSite::Span {
                    kind: position.kind,
                    span: position.span.span_id(),
                },
                &omissions,
            );
        }
        omissions.merge(&record.omissions);
        if let Some(custom) = enrich::assemble(&mut attributes, contributions, &omissions) {
            name = custom.into_owned();
        }
        let builder = SpanBuilder::from_name(name)
            .with_kind(projection.kind())
            .with_events(extension_events(projection.extension))
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

/// Writes what a body contributed to the span `record` publishes through
/// the span's writer, in the order the body admitted it: a tool body's
/// annotations of its own tool span, or an extension span's attributes and
/// captured content. The body's writer admitted each against the same
/// budget, so the span's writer admits the same set.
fn contributed(record: &TraceRecord, writer: &mut TraceAttributeWriter) {
    match &record.event {
        TraceEvent::ToolCallCompleted {
            span_attributes, ..
        } => {
            for attribute in &span_attributes.0 {
                let _ = match registry::RefinableKey::of_key(&attribute.key) {
                    Some(key) => writer.replace(key, attribute.value.clone()),
                    None => writer.set(attribute.key.clone(), attribute.value.clone()),
                };
            }
        }
        TraceEvent::ExtensionSpan(span) => {
            for attribute in &span.attributes {
                let _ = writer.set(attribute.key.clone(), attribute.value.clone());
            }
            let captured = record
                .content
                .captured
                .contains(&crate::ContentCategory::Extension);
            for entry in span.content.iter().filter(|_| captured) {
                let path = format!("extension.{}", entry.key);
                if record
                    .content
                    .omitted
                    .iter()
                    .chain(&record.content.oversize)
                    .any(|omitted| *omitted == path)
                {
                    continue;
                }
                let _ = writer.set(
                    entry.key.clone(),
                    crate::TraceValue::Str(std::borrow::Cow::Owned(entry.value.to_string())),
                );
            }
        }
        _ => {}
    }
}

/// An extension span's events, at their own times.
fn extension_events(
    extension: Option<&crate::ExtensionSpanRecord>,
) -> Vec<opentelemetry::trace::Event> {
    extension
        .map(|extension| {
            extension
                .events
                .iter()
                .map(|event| {
                    opentelemetry::trace::Event::new(
                        event.name.clone(),
                        TraceTimestamp::from_unix_ms(event.at_ms).to_system_time(),
                        event
                            .attributes
                            .iter()
                            .map(|attribute| {
                                KeyValue::new(
                                    attribute.key.clone(),
                                    enrich::otel_value(attribute.value.clone()),
                                )
                            })
                            .collect(),
                        0,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Builds a span from every sampled record that publishes one. An unsampled
/// record, an opening record and an event build nothing.
impl TraceSink for OtelTelemetry {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let Some(position) = &record.position else {
            return Ok(());
        };
        // A producer's contributions are taken at its one whole record,
        // sampled or not, so none outlives its call.
        let producer = (position.kind == TraceSpanKind::Producer
            && position.role == SpanRole::Whole)
            .then(|| {
                self.producers
                    .lock_recover()
                    .remove(&position.span.span_id())
            })
            .flatten();
        if position.publishes() {
            self.genai.record(record, position);
        }
        if position.publishes() && position.is_sampled() {
            self.export(record, position, producer);
            self.export_model_attempts(record, position);
        }
        Ok(())
    }
}

/// Third-party OpenTelemetry instrumentation inside an extension body runs
/// with the body's position as its remote parent: a `reqwest` or `tonic`
/// client, a plugin-owned span, or a `tracing` span through
/// `tracing-opentelemetry` at its default `context_activation`.
impl crate::AmbientContext for OtelTelemetry {
    fn for_position(&self, position: &TraceCarrier) -> Arc<dyn crate::AmbientScope> {
        Arc::new(OtelAmbient(otel_context(Some(position))))
    }
}

/// The OpenTelemetry context whose remote parent is `position`, or the
/// empty context when there is none.
pub fn otel_context(position: Option<&TraceCarrier>) -> Context {
    position
        .and_then(span_context)
        .map_or_else(Context::new, |span| {
            Context::new().with_remote_span_context(span)
        })
}

struct OtelAmbient(Context);

impl crate::AmbientScope for OtelAmbient {
    fn enter(&self) -> crate::AmbientGuard {
        crate::AmbientGuard::new(self.0.clone().attach())
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
        let (mut writer, name) = self.enrich_producer(spec, trace_id, span_id);
        let enriched: Vec<KeyValue> = writer
            .contributions()
            .added
            .iter()
            .map(|(key, value)| {
                KeyValue::new(key.as_str().to_owned(), enrich::otel_value(value.clone()))
            })
            .collect();
        let result = self.provider.sampler().should_sample(
            Some(&parent),
            TraceId::from_bytes(trace_id.to_bytes()),
            &name,
            &SpanKind::Producer,
            &enriched,
            &links,
        );
        let sampled = result.decision == SamplingDecision::RecordAndSample;
        // The sampler's result attributes stay with the producer, written
        // after its enrichment against what the producer's budget has left.
        for attribute in result.attributes {
            let _refused = writer.set(
                attribute.key.as_str().to_owned(),
                enrich::trace_value(attribute.value),
            );
        }
        self.producers
            .lock_recover()
            .insert(span_id, writer.finish());
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

impl OtelTelemetry {
    /// Runs the enricher over the producer `spec` opens at `span_id` in
    /// `trace_id`, before its decision: its position is complete, its flags
    /// are the caller's. Returns its contributions and its name.
    fn enrich_producer(
        &self,
        spec: &ProducerSpec,
        trace_id: W3cTraceId,
        span_id: W3cSpanId,
    ) -> (TraceAttributeWriter, String) {
        let caller = spec.caller.as_ref();
        let (parent, links, flags, tracestate) = match (caller, spec.link_mode) {
            (Some(caller), TraceLinkMode::Parent) => (
                Some(caller.clone()),
                crate::TraceLinks::new(),
                caller.flags(),
                caller.tracestate().clone(),
            ),
            (Some(caller), TraceLinkMode::Link) => (
                None,
                std::iter::once(caller.clone()).collect(),
                W3cTraceFlags::default(),
                W3cTraceState::default(),
            ),
            (None, _) => (
                None,
                crate::TraceLinks::new(),
                W3cTraceFlags::default(),
                W3cTraceState::default(),
            ),
        };
        let now = TraceTimestamp::from_system_time(std::time::SystemTime::now())
            .unwrap_or(TraceTimestamp::from_unix_ms(0));
        let position = TracePosition {
            span: TraceCarrier::new(trace_id, span_id, flags, tracestate),
            parent,
            links,
            role: SpanRole::Whole,
            kind: TraceSpanKind::Producer,
            started_at: now,
            ended_at: Some(now),
            scope: None,
            attempt: None,
        };
        let name = producer(spec.kind).definition().name;
        let writer = TraceAttributeWriter::new(TraceSpanKind::Producer, &self.policy.limits);
        let Some(enricher) = &self.options.enricher else {
            return (writer, name.to_owned());
        };
        let record = TraceRecord {
            schema_version: crate::TRACE_SCHEMA_VERSION,
            id: format!("producer:{span_id}"),
            timestamp: now,
            content: crate::ContentPolicy::omitted().into(),
            context: TraceContext::of_ids(spec.ids.clone()),
            position: Some(position.clone()),
            omissions: TraceOmissionCounts::default(),
            event: TraceEvent::ProducerSettled {
                producer: spec.kind,
                outcome: crate::ProducerOutcome::Selected,
                phases: Vec::new(),
            },
        };
        let mut attributes = Vec::new();
        correlation_attributes(None, &record.context, &mut attributes);
        let view = SpanView {
            kind: TraceSpanKind::Producer,
            default_name: name,
            ids: &spec.ids,
            position: &position,
            record: &record,
            attempt: None,
            usage: None,
            attributes: &attributes,
            policy: &self.policy,
        };
        let writer = enrich::enrich(enricher.as_ref(), &view, writer);
        let renamed = writer
            .contributions()
            .name
            .as_deref()
            .map(ToOwned::to_owned);
        (writer, renamed.unwrap_or_else(|| name.to_owned()))
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

/// The `lash.process.start.*_us` attribute of one start phase.
fn start_phase_key(phase: crate::ProcessStartPhase) -> A {
    use crate::ProcessStartPhase as P;
    match phase {
        P::Dispatch => A::StartDispatchUs,
        P::Admission => A::StartAdmissionUs,
        P::Environment => A::StartEnvironmentUs,
        P::DefinitionLoad => A::StartDefinitionLoadUs,
        P::DefinitionResolve => A::StartDefinitionResolveUs,
        P::DefinitionAcquire => A::StartDefinitionAcquireUs,
        P::DefinitionVerify => A::StartDefinitionVerifyUs,
        P::Definition => A::StartDefinitionUs,
        P::Admit => A::StartAdmitUs,
        P::Artifacts => A::StartArtifactsUs,
        P::LinkCheck => A::StartLinkCheckUs,
        P::Input => A::StartInputUs,
        P::Prepare => A::StartPrepareUs,
        P::Commit => A::StartCommitUs,
        P::Adopt => A::StartAdoptUs,
        P::Observe => A::StartObserveUs,
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
    tool_identity: Option<&'a lash_protocol_types::ToolId>,
    process: Option<&'a str>,
    /// A workflow node's site, which names its span.
    site: Option<String>,
    attempt: Option<&'a crate::TraceModelAttempt>,
    usage: Option<Result<LlmUsage, TokenUsageOverflow>>,
    extension: Option<&'a crate::ExtensionSpanRecord>,
}
impl<'a> Projection<'a> {
    fn for_record(record: &'a TraceRecord, position: &TracePosition) -> Option<Self> {
        let mut projection = Self {
            span: DomainSpan::Turn,
            model: None,
            tool: None,
            tool_identity: None,
            process: None,
            site: None,
            attempt: None,
            usage: None,
            extension: None,
        };
        match (position.kind, &record.event) {
            (TraceSpanKind::Extension | TraceSpanKind::Point, TraceEvent::ExtensionSpan(span)) => {
                projection.span = DomainSpan::Extension;
                projection.extension = Some(span);
            }
            (TraceSpanKind::Producer, TraceEvent::ProducerSettled { producer: kind, .. }) => {
                projection.span = producer(*kind);
            }
            (TraceSpanKind::Agent, TraceEvent::TurnCompleted { .. }) => {}
            (TraceSpanKind::Model, TraceEvent::LlmCallCompleted { response, .. }) => {
                projection.span = DomainSpan::Model;
                projection.model = Some(&response.request_model);
                projection.usage = model::logical_usage(&record.event);
            }
            (TraceSpanKind::Model, TraceEvent::LlmCallFailed { model, .. }) => {
                projection.span = DomainSpan::Model;
                projection.model = model.as_ref().map(|model| model.request_model.as_str());
                projection.usage = model::logical_usage(&record.event);
            }
            (TraceSpanKind::ModelAttempt, TraceEvent::LlmAttemptCompleted { .. }) => {
                projection.span = DomainSpan::ModelAttempt;
            }
            (TraceSpanKind::Process, TraceEvent::DomainCompleted { completion }) => {
                projection.span = DomainSpan::Process;
                projection.process = match &completion.subject {
                    TraceDomainSubject::Process { kind, .. } => Some(kind.as_str()),
                    _ => None,
                };
            }
            (TraceSpanKind::Workflow, TraceEvent::DomainCompleted { completion }) => {
                projection.span = DomainSpan::Workflow;
                if let TraceDomainSubject::Workflow { name, .. } = &completion.subject {
                    projection.process = name.as_deref();
                }
            }
            (TraceSpanKind::WorkflowNode, TraceEvent::DomainCompleted { completion }) => {
                let TraceDomainSubject::WorkflowNode { at } = &completion.subject else {
                    return None;
                };
                projection.span = DomainSpan::WorkflowNode;
                projection.site = Some(at.position().to_string());
            }
            (TraceSpanKind::ToolIntent, TraceEvent::DomainCompleted { completion }) => {
                projection.span = DomainSpan::Intent;
                projection.tool = completion.tool_name.as_deref();
            }
            (TraceSpanKind::Tool, TraceEvent::ToolCallCompleted { name, .. }) => {
                projection.span = DomainSpan::Tool;
                projection.tool = name.name();
                if let crate::TraceToolName::Identity { tool_id } = name {
                    projection.tool_identity = Some(tool_id);
                }
            }
            (
                TraceSpanKind::Cell,
                TraceEvent::ExecCodeCompleted { language, .. }
                | TraceEvent::ExecCodeFailed { language, .. }
                | TraceEvent::ExecCodeDiscarded { language, .. },
            ) => {
                projection.span = DomainSpan::ExecCode;
                projection.tool = Some(language);
            }
            _ => return None,
        }
        Some(projection)
    }
    fn name(&self) -> String {
        if let Some(extension) = self.extension {
            return extension.name.clone();
        }
        let default = self.span.definition().name;
        let suffix = match self.span {
            DomainSpan::Model => self.model,
            DomainSpan::Tool => self
                .tool
                .or_else(|| self.tool_identity.map(|id| id.as_str())),
            DomainSpan::ExecCode => self.tool,
            DomainSpan::Process | DomainSpan::Workflow => self.process,
            DomainSpan::WorkflowNode => self.site.as_deref(),
            _ => None,
        };
        match suffix {
            Some(suffix) => format!("{default} {suffix}"),
            None => default.to_owned(),
        }
    }
    fn failed(&self, record: &TraceRecord) -> bool {
        if let Some(attempt) = self.attempt {
            return matches!(
                attempt.attempt.outcome,
                lash_protocol_types::llm::types::AttemptOutcome::Failed
                    | lash_protocol_types::llm::types::AttemptOutcome::Interrupted
            );
        }
        match &record.event {
            TraceEvent::DomainCompleted { completion } => {
                completion.status == TraceDomainStatus::Failed
            }
            TraceEvent::LlmCallFailed { error, .. } => {
                error.terminal_reason != crate::TraceLlmTerminalReason::Cancelled
            }
            TraceEvent::LlmAttemptCompleted { attempt, .. } => matches!(
                attempt.outcome,
                lash_protocol_types::llm::types::AttemptOutcome::Failed
                    | lash_protocol_types::llm::types::AttemptOutcome::Interrupted
            ),
            _ => record.event.is_failed(),
        }
    }
    /// The SDK kind of the span: an extension span's own, else its type's.
    fn kind(&self) -> SpanKind {
        match self.extension.map(|extension| extension.kind) {
            Some(crate::ExtensionSpanKind::Internal) => SpanKind::Internal,
            Some(crate::ExtensionSpanKind::Client) => SpanKind::Client,
            Some(crate::ExtensionSpanKind::Producer) => SpanKind::Producer,
            Some(crate::ExtensionSpanKind::Consumer) => SpanKind::Consumer,
            None => self.span.definition().kind,
        }
    }

    fn attributes(&self, record: &TraceRecord, out: &mut Vec<KeyValue>) {
        if let Some(attempt) = self.attempt {
            if let Some(span) = &attempt.span {
                if span.terminal_delivery_timed_out {
                    out.push(A::ProviderTerminalDeliveryTimedOut.value(true));
                }
                // Keys only: a captured value may be billing or identity data.
                if !span.response_metadata_keys.is_empty() {
                    out.push(
                        A::ModelAttemptResponseMetadataKeys.value(opentelemetry::Value::Array(
                            opentelemetry::Array::String(
                                span.response_metadata_keys
                                    .iter()
                                    .cloned()
                                    .map(Into::into)
                                    .collect(),
                            ),
                        )),
                    );
                }
            }
            model::attempt_attributes(&attempt.attempt, out);
            return;
        }
        if let Some(extension) = self.extension {
            extension_attributes(extension, out);
            return;
        }
        if let Some(operation) = self.operation() {
            out.push(A::OperationName.value(operation));
        }
        if let Some(tool) = self.tool {
            out.push(A::ToolName.value(tool.to_owned()));
        }
        match &record.event {
            TraceEvent::ProducerSettled {
                outcome, phases, ..
            } => {
                out.push(A::ProducerOutcome.value(outcome.as_str()));
                if let crate::ProducerOutcome::Refused { error_type } = outcome {
                    out.push(A::ErrorType.value(error_type.to_string()));
                }
                for time in phases {
                    out.push(
                        start_phase_key(time.phase)
                            .value(i64::try_from(time.elapsed_us).unwrap_or(i64::MAX)),
                    );
                }
            }
            TraceEvent::LlmCallCompleted {
                model,
                response,
                attempts,
                ..
            } => {
                model::call_attributes(model.as_ref(), Some(response), attempts.as_deref(), out);
                logical_usage_attributes(&self.usage, out);
                if model::usage_unknown(&record.event) {
                    out.push(A::UsageUnknown.value(true));
                }
            }
            TraceEvent::LlmCallFailed {
                model,
                error,
                attempts,
                ..
            } => {
                model::call_attributes(model.as_ref(), None, attempts.as_deref(), out);
                logical_usage_attributes(&self.usage, out);
                if model::usage_unknown(&record.event) {
                    out.push(A::UsageUnknown.value(true));
                }
                if error.terminal_reason == crate::TraceLlmTerminalReason::Cancelled {
                    out.push(A::Outcome.value("cancelled"));
                    return;
                }
                let last = attempts
                    .iter()
                    .flatten()
                    .rev()
                    .find_map(|attempt| attempt.error.as_ref());
                out.push(A::ErrorType.value(match (last, &error.code) {
                    (Some(error), _) => model::error_type(error),
                    (None, Some(code)) => code.to_string(),
                    (None, None) => error.failure_kind.code().to_string(),
                }));
            }
            TraceEvent::LlmAttemptCompleted { attempt, .. } => {
                model::attempt_attributes(attempt, out)
            }
            TraceEvent::DomainCompleted { completion } => {
                out.push(A::Outcome.value(match completion.status {
                    TraceDomainStatus::Completed => "completed",
                    TraceDomainStatus::Failed => "failed",
                    TraceDomainStatus::Cancelled => "cancelled",
                }));
                match &completion.subject {
                    TraceDomainSubject::Process { kind, label } => {
                        out.push(A::ProcessKind.value(kind.clone()));
                        if let Some(label) = label {
                            out.push(A::ProcessLabel.value(label.clone()));
                        }
                    }
                    TraceDomainSubject::Workflow { kind, name } => {
                        out.push(A::ProcessKind.value(kind.clone()));
                        if let Some(name) = name {
                            out.push(A::ProcessLabel.value(name.clone()));
                            out.push(A::WorkflowName.value(name.clone()));
                        }
                    }
                    TraceDomainSubject::WorkflowNode { at } => {
                        out.push(A::WorkflowNodeSite.value(at.position().to_string()));
                        // The canonical JSON of the whole identity, as the
                        // kernel's node facts key the occurrence.
                        out.push(
                            A::WorkflowNodeOccurrence
                                .value(serde_json::to_string(at).unwrap_or_default()),
                        );
                    }
                    TraceDomainSubject::ToolIntent { kind } => {
                        out.push(A::ToolIntentKind.value(kind.as_str()))
                    }
                }
                if let Some(id) = &completion.tool_call_id {
                    out.push(A::ToolCallId.value(id.clone()));
                }
                if let Some(code) = &completion.error_code {
                    out.push(A::ErrorType.value(code.to_string()));
                } else if self.failed(record) {
                    out.push(A::ErrorType.value("domain_failure"));
                }
            }
            TraceEvent::ToolCallCompleted {
                call_id,
                provider_call_id,
                name,
                output,
                attempts,
                ..
            } => {
                out.push(A::ToolType.value("function"));
                out.push(A::ToolId.value(name.label().to_owned()));
                if let Some(id) = provider_call_id {
                    out.push(A::ProviderCallId.value(id.clone()));
                }
                if let Some(attempts) = attempts {
                    out.push(
                        A::ToolAttempts.value(i64::try_from(attempts.len()).unwrap_or(i64::MAX)),
                    );
                }
                out.push(A::ToolCallId.value(call_id.to_string()));
                out.push(A::Outcome.value(match output.status() {
                    crate::TraceToolCallStatus::Success => "completed",
                    crate::TraceToolCallStatus::Failure => "failed",
                    crate::TraceToolCallStatus::Cancelled => "cancelled",
                    crate::TraceToolCallStatus::Discarded => "discarded",
                }));
                // The terminal failure's class is authoritative: a controller
                // failure after an attempt replaces the attempt's.
                if let crate::TraceToolCallOutcome::Failure(failure) = &output.outcome {
                    out.push(A::ErrorType.value(crate::wire_tag(&failure.class)));
                }
            }
            TraceEvent::TurnCompleted {
                outcome, inputs, ..
            } => {
                out.push(A::TurnInputs.value(i64::try_from(*inputs).unwrap_or(i64::MAX)));
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
                cell_attributes(record, "failed", None, out);
                out.push(A::ErrorType.value(crate::wire_tag(reason)));
            }
            TraceEvent::ExecCodeCompleted {
                error, tool_calls, ..
            } => {
                let outcome = if error.is_some() {
                    "failed"
                } else {
                    "completed"
                };
                cell_attributes(record, outcome, Some(tool_calls.len()), out);
                if let Some(error) = error {
                    let reason = error
                        .exec_failure
                        .as_ref()
                        .map(crate::wire_tag)
                        .unwrap_or_else(|| crate::wire_tag(&error.kind));
                    out.push(A::ErrorType.value(reason));
                }
            }
            TraceEvent::ExecCodeDiscarded { .. } => {
                cell_attributes(record, "discarded", None, out);
            }
            _ if self.failed(record) => out.push(A::ErrorType.value("domain_failure")),
            _ => {}
        }
    }
    fn operation(&self) -> Option<&'static str> {
        match self.span {
            DomainSpan::Turn => Some("invoke_agent"),
            DomainSpan::Model => Some("chat"),
            DomainSpan::Workflow => Some("invoke_workflow"),
            DomainSpan::Tool | DomainSpan::ExecCode => Some("execute_tool"),
            _ => None,
        }
    }
}
/// An extension span's lash attributes: the callback it was bound from, how
/// it ended, and its operation preset's semconv keys. What the plugin
/// contributed is written through the span's writer at export.
fn extension_attributes(extension: &crate::ExtensionSpanRecord, out: &mut Vec<KeyValue>) {
    out.push(A::PluginId.value(extension.plugin_id.clone()));
    out.push(A::PluginCallback.value(extension.callback.clone()));
    if extension.point {
        out.push(A::SpanPoint.value(true));
    }
    if let Some(ended_by) = extension.ended_by {
        out.push(A::SpanEndedBy.value(ended_by.as_str()));
    }
    if let Some(operation) = &extension.operation {
        out.push(A::OperationName.value(operation.name()));
        match operation {
            crate::ExtensionOperation::Embeddings {
                provider,
                request_model,
            } => {
                out.push(A::GenAiProviderName.value(provider.clone()));
                out.push(A::RequestModel.value(request_model.clone()));
            }
            crate::ExtensionOperation::Retrieval { data_source_id } => {
                out.push(A::DataSourceId.value(data_source_id.clone()));
            }
        }
    }
    if let Some(error_type) = &extension.error_type {
        out.push(A::ErrorType.value(error_type.clone()));
    }
}

/// A code cell's own attributes (§4.5): a cell is a function tool named by
/// its language, keyed by its graph key, with its outcome and, once it
/// answered, the number of tool calls it made.
fn cell_attributes(
    record: &TraceRecord,
    outcome: &'static str,
    tool_calls: Option<usize>,
    out: &mut Vec<KeyValue>,
) {
    out.push(A::ToolType.value("function"));
    if let Some(cell) = &record.context.cell {
        out.push(A::CellId.value(cell.clone()));
    }
    if let TraceEvent::ExecCodeCompleted { language, .. }
    | TraceEvent::ExecCodeFailed { language, .. }
    | TraceEvent::ExecCodeDiscarded { language, .. } = &record.event
    {
        out.push(A::CellLanguage.value(language.clone()));
    }
    if let TraceEvent::ExecCodeCompleted {
        trailing_discarded, ..
    }
    | TraceEvent::ExecCodeFailed {
        trailing_discarded, ..
    }
    | TraceEvent::ExecCodeDiscarded {
        trailing_discarded, ..
    } = &record.event
        && *trailing_discarded > 0
    {
        out.push(
            A::CellTrailingDiscarded.value(i64::try_from(*trailing_discarded).unwrap_or(i64::MAX)),
        );
    }
    if let Some(calls) = tool_calls {
        out.push(A::CellToolCalls.value(i64::try_from(calls).unwrap_or(i64::MAX)));
    }
    out.push(A::Outcome.value(outcome));
}

/// Writes the projection's folded logical usage as `gen_ai.usage.*`, or the
/// overflow marker when the fold overflowed; nothing when it folded none.
fn logical_usage_attributes(
    usage: &Option<Result<LlmUsage, TokenUsageOverflow>>,
    out: &mut Vec<KeyValue>,
) {
    match usage {
        Some(Ok(usage)) => usage_attributes(usage, out),
        Some(Err(_)) => out.push(A::UsageOverflow.value(true)),
        None => {}
    }
}

fn usage_attributes(usage: &LlmUsage, out: &mut Vec<KeyValue>) {
    match usage.checked_input_total() {
        Ok(total) => out.push(A::InputTokens.value(total)),
        Err(_) => out.push(A::UsageOverflow.value(true)),
    }
    out.extend([
        A::OutputTokens.value(usage.output_tokens),
        A::CacheReadTokens.value(usage.cache_read_input_tokens),
        A::CacheWriteTokens.value(usage.cache_write_input_tokens),
        A::ReasoningTokens.value(usage.reasoning_output_tokens),
    ]);
}

#[cfg(test)]
mod tests;
