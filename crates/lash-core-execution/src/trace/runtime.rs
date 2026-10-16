//! The runtime's one trace handle and the right to emit through it.
//!
//! External observations leave the engine through [`TraceEmitter::emit`]
//! alone, under an [`EmissionPermit`]. A permit has two holders. The body of
//! a recorded step holds one while it really runs: the engine-side wrapper
//! that runs the body ([`LiveStep`]) mints it, so a substrate that serves the
//! step from its journal never does. Shift code holds one once its shift has
//! passed its journal frontier ([`JournalFrontier`]): a shift re-runs from
//! its start on every attempt, and what it does before the first step whose
//! body really runs is reconstruction of what an earlier attempt already
//! observed.
//!
//! Nothing here asks a substrate whether it is replaying. The frontier moves
//! only when a step body the engine handed over runs.

use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lash_trace::{
    ContentPolicy, DurableTraceScope, EmissionPermit, LashIdentity, ProducerKind, ProducerSpan,
    ProducerSpec, ProductObservationSink, SpanRole, TraceAttemptId, TraceAttemptLink, TraceCall,
    TraceCarrier, TraceContext, TraceDetailPolicy, TraceEvent, TraceIdentity, TraceIds,
    TraceLinkMode, TraceLinks, TracePosition, TraceRecord, TraceRecordIdentity, TraceScopeId,
    TraceScopeKind, TraceScopeOwner, TraceSink, TraceSpanKind, TraceTimestamp, TraceTransitionKind,
    telemetry::metrics::TelemetryMetrics,
};

/// The runtime's shared trace handle: the identity authority, the clock and
/// the emitter. Every engine path and every plugin is handed this same
/// value.
#[derive(Clone)]
pub struct TraceRuntime {
    /// One allocation, so every holder carries a pointer.
    parts: Arc<TraceRuntimeParts>,
}

#[derive(Clone)]
struct TraceRuntimeParts {
    identity: Arc<dyn TraceIdentity>,
    clock: Arc<dyn crate::Clock>,
    emitter: TraceEmitter,
    detail: TraceDetailPolicy,
    metrics: TelemetryMetrics,
    limits: lash_trace::TraceLimits,
    content_limits: lash_trace::ContentLimits,
    evidence: crate::EvidenceLimits,
    diagnostics: crate::DiagnosticLimits,
    composition: crate::CompositionCacheLimits,
    omissions: Arc<lash_trace::TraceOmissionLedger>,
    compositions: Arc<super::composition::CompositionMemory>,
    /// The host's ambient context: what third-party instrumentation inside
    /// an extension body sees as current. None without an adapter.
    ambient: Option<Arc<dyn lash_trace::AmbientContext>>,
}

impl TraceRuntime {
    /// A runtime with no observer: lash's own identity, no sink.
    pub fn new(clock: Arc<dyn crate::Clock>) -> Self {
        let omissions = Arc::new(lash_trace::TraceOmissionLedger::default());
        Self {
            parts: Arc::new(TraceRuntimeParts {
                identity: Arc::new(LashIdentity),
                clock,
                emitter: TraceEmitter {
                    omissions: omissions.clone(),
                    ..Default::default()
                },
                detail: TraceDetailPolicy::standard(),
                metrics: TelemetryMetrics::default(),
                limits: lash_trace::TraceLimits::standard(),
                content_limits: lash_trace::ContentLimits::standard(),
                evidence: crate::EvidenceLimits::standard(),
                diagnostics: crate::DiagnosticLimits::standard(),
                composition: crate::CompositionCacheLimits::standard(),
                omissions,
                compositions: Arc::default(),
                ambient: None,
            }),
        }
    }

    /// Installs the host's ambient context: the exporter's, so third-party
    /// instrumentation inside an extension body nests under its position.
    #[must_use]
    pub fn with_ambient(mut self, ambient: Arc<dyn lash_trace::AmbientContext>) -> Self {
        Arc::make_mut(&mut self.parts).ambient = Some(ambient);
        self
    }

    /// Whether `other` is this same runtime.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.parts, &other.parts)
    }

    /// The host's ambient context, when an adapter installed one.
    pub(crate) fn ambient(&self) -> Option<&Arc<dyn lash_trace::AmbientContext>> {
        self.parts.ambient.as_ref()
    }

    /// The runtime's injected metric instruments: no-op handles unless the
    /// host installed its own.
    pub fn metrics(&self) -> &TelemetryMetrics {
        &self.parts.metrics
    }

    /// Installs the host's metric instruments.
    #[must_use]
    pub fn with_metrics(mut self, metrics: TelemetryMetrics) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.metrics = metrics;
        self
    }

    /// The identity authority: [`LashIdentity`] unless the host installed
    /// an adapter.
    pub fn identity(&self) -> &Arc<dyn TraceIdentity> {
        &self.parts.identity
    }

    /// Installs the runtime's identity authority: the adapter that captures
    /// the caller's context, decides producers and draws leaf ids.
    #[must_use]
    pub fn with_identity(mut self, identity: Arc<dyn TraceIdentity>) -> Self {
        Arc::make_mut(&mut self.parts).identity = identity;
        self
    }

    /// The runtime's injected clock.
    pub fn clock(&self) -> &Arc<dyn crate::Clock> {
        &self.parts.clock
    }

    pub fn emitter(&self) -> &TraceEmitter {
        &self.parts.emitter
    }

    /// The tracing seam's admission and buffering bounds.
    pub fn limits(&self) -> lash_trace::TraceLimits {
        self.parts.limits
    }

    /// The bounds on the content a record carries.
    pub fn content_limits(&self) -> lash_trace::ContentLimits {
        self.parts.content_limits
    }

    /// The bounds on the execution evidence lash records.
    pub fn evidence_limits(&self) -> crate::EvidenceLimits {
        self.parts.evidence
    }

    /// How much of a diagnostic lash keeps, and when it warns.
    pub fn diagnostic_limits(&self) -> crate::DiagnosticLimits {
        self.parts.diagnostics
    }

    /// What lash builds for each span kind.
    pub fn detail(&self) -> TraceDetailPolicy {
        self.parts.detail
    }

    /// The core-wide omission counters every tracing path records into.
    pub fn omissions(&self) -> &Arc<lash_trace::TraceOmissionLedger> {
        &self.parts.omissions
    }

    /// The fingerprints of a request's `tools`, in its order, shared by
    /// every session this runtime traces.
    pub fn composition_tool_fingerprints(
        &self,
        tools: &Arc<Vec<crate::llm::types::LlmToolSpec>>,
    ) -> Arc<Vec<[u8; 32]>> {
        self.parts
            .compositions
            .tool_fingerprints(tools, self.parts.composition.fingerprint_generations())
    }

    /// Whether `fingerprint`, `session`'s composition now, is one this
    /// runtime has not reported as the session's latest: once it returns
    /// `true`, it is.
    pub fn report_composition(&self, session: &crate::SessionId, fingerprint: [u8; 32]) -> bool {
        self.parts
            .compositions
            .report(session, fingerprint, self.parts.composition.sessions())
    }

    /// Installs the tracing seam's bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: lash_trace::TraceLimits) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.limits = limits;
        parts.emitter.policy.limits = limits;
        self
    }

    /// Installs the bounds on the content a record carries.
    #[must_use]
    pub fn with_content_limits(mut self, limits: lash_trace::ContentLimits) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.content_limits = limits;
        parts.emitter.policy.content_limits = limits;
        self
    }

    /// Installs the bounds on the execution evidence lash records.
    #[must_use]
    pub fn with_evidence_limits(mut self, limits: crate::EvidenceLimits) -> Self {
        Arc::make_mut(&mut self.parts).evidence = limits;
        self
    }

    /// Installs how much of a diagnostic lash keeps, and when it warns.
    #[must_use]
    pub fn with_diagnostic_limits(mut self, limits: crate::DiagnosticLimits) -> Self {
        Arc::make_mut(&mut self.parts).diagnostics = limits;
        self
    }

    /// Installs how much composition history the runtime remembers.
    #[must_use]
    pub fn with_composition_cache_limits(mut self, limits: crate::CompositionCacheLimits) -> Self {
        Arc::make_mut(&mut self.parts).composition = limits;
        self
    }

    /// The tracing policies in effect, as the core reports them: no
    /// exporter's, which the exporter adds.
    pub fn policy(&self) -> lash_trace::TracePolicySnapshot {
        lash_trace::TracePolicySnapshot {
            content: self.content(),
            content_limits: self.parts.content_limits,
            detail: self.parts.detail,
            limits: self.parts.limits,
            ..lash_trace::TracePolicySnapshot::standard()
        }
    }

    /// The host's telemetry content policy: [`ContentPolicy::standard`]
    /// (omitted) unless the host opted in.
    pub fn content(&self) -> ContentPolicy {
        self.parts.emitter.content
    }

    /// States whether records to the sinks carry content.
    #[must_use]
    pub fn with_content(mut self, content: ContentPolicy) -> Self {
        let emitter = &mut Arc::make_mut(&mut self.parts).emitter;
        emitter.content = content;
        emitter.policy.content = content;
        self
    }

    /// Installs the one pre-build, narrowing selector.
    pub fn with_capture_selector(mut self, selector: Arc<dyn lash_trace::CaptureSelector>) -> Self {
        Arc::make_mut(&mut self.parts).emitter.selector = Some(selector);
        self
    }
    /// Installs the synchronous transactional content hook.
    pub fn with_content_hook(mut self, hook: Arc<dyn lash_trace::ContentHook>) -> Self {
        let emitter = &mut Arc::make_mut(&mut self.parts).emitter;
        emitter.hook = Some(Arc::new(HookEpisode {
            hook,
            failed: emitter.hook_failure.clone(),
        }));
        self
    }

    /// Whether any record sink observes this runtime. Sites test it before
    /// they build anything a record would hold.
    pub fn is_observed(&self) -> bool {
        self.parts.emitter.has_external_observers()
    }

    /// Adds one record sink after those already installed. Each sink sees
    /// every record, and fails or panics on its own.
    #[must_use]
    pub fn with_trace_sink(mut self, sink: Arc<dyn TraceSink>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.emitter.sinks = parts
            .emitter
            .sinks
            .iter()
            .cloned()
            .chain(std::iter::once(sink))
            .collect();
        parts.emitter.sink_failures = parts
            .emitter
            .sink_failures
            .iter()
            .cloned()
            .chain(std::iter::once(Arc::default()))
            .collect();
        self
    }

    /// Adds one product observation consumer
    /// ([`TraceEmitter::observe_product`]) after those already installed.
    #[must_use]
    pub fn with_product_observer(mut self, observer: Arc<dyn ProductObservationSink>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.emitter.product = parts
            .emitter
            .product
            .iter()
            .cloned()
            .chain(std::iter::once(observer))
            .collect();
        self
    }

    /// Installs what lash builds for each span kind.
    #[must_use]
    pub fn with_detail(mut self, detail: TraceDetailPolicy) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.detail = detail;
        parts.emitter.policy.detail = detail;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.clock = clock;
        self
    }

    /// Flushes the record sinks. An adapter's provider is the host's to
    /// flush.
    pub fn flush(&self) -> Result<(), lash_trace::TraceSinkError> {
        for sink in self.parts.emitter.sinks.iter() {
            sink.flush()?;
        }
        Ok(())
    }

    /// Opens the producer span of a `kind` host call making `call`'s trace
    /// choices, which knows `ids` now: its caller is captured once, here,
    /// and the identity decides the trace. A runtime nothing observes opens
    /// an unobserved producer, which parents nothing.
    pub fn open_producer(
        &self,
        kind: ProducerKind,
        call: &TraceCall,
        ids: TraceIds,
    ) -> ProducerSpan {
        if !self.is_observed() {
            return ProducerSpan::unobserved();
        }
        let spec = call.producer(kind, self.parts.identity.as_ref(), ids);
        self.producer(spec)
    }

    fn producer(&self, spec: ProducerSpec) -> ProducerSpan {
        let span = self.parts.identity.open_producer(&spec);
        let started_at_ms = self.parts.clock.timestamp_ms();
        let (parent, links) = match (spec.caller, spec.link_mode) {
            (Some(caller), TraceLinkMode::Parent) => (Some(caller), TraceLinks::new()),
            (Some(caller), TraceLinkMode::Link) => (None, std::iter::once(caller).collect()),
            (None, _) => (None, TraceLinks::new()),
        };
        let runtime = self.clone();
        let carrier = span.clone();
        let kind = spec.kind;
        ProducerSpan::opened(
            span,
            spec.ids,
            Box::new(move |outcome, ids| {
                let ended_at_ms = runtime.parts.clock.timestamp_ms();
                let position = TracePosition {
                    span: carrier,
                    parent,
                    links,
                    role: SpanRole::Whole,
                    kind: TraceSpanKind::Producer,
                    started_at: TraceTimestamp::from_unix_ms(started_at_ms),
                    ended_at: Some(TraceTimestamp::from_unix_ms(ended_at_ms)),
                    scope: None,
                    attempt: None,
                };
                let attempt = fresh_attempt();
                runtime.parts.emitter.emit(
                    Some(&EmissionPermit::live_execution(attempt.clone())),
                    Some(position),
                    || TraceRecordIdentity::UnscopedLive {
                        attempt,
                        ordinal: 0,
                    },
                    ended_at_ms,
                    || {
                        (
                            TraceContext::of_ids(ids),
                            TraceEvent::ProducerSettled {
                                producer: kind,
                                outcome,
                            },
                        )
                    },
                );
            }),
        )
    }

    /// The standing of shift code that issues its steps through `controller`:
    /// it may emit once one of those steps' bodies has really run.
    pub fn shift(
        &self,
        scope: Option<DurableTraceScope>,
        controller: &crate::ActorContext,
    ) -> TraceStanding {
        controller.frontier().bind_runtime(self.clone());
        self.standing(
            scope,
            EmissionRight::Shift {
                frontier: controller.frontier().clone(),
                attempt: None,
            },
        )
    }

    /// The standing of a substrate executing a step for the shift that issued
    /// it: the issuing shift's. A substrate observes its own handling of a
    /// step (a wait it parked, a timer it resolved) only once that shift has
    /// passed its journal. A step no shift issued has no standing.
    pub fn issued(&self, scope: Option<DurableTraceScope>, issue: &StepIssue) -> TraceStanding {
        self.standing(
            issue.scope.clone().or(scope),
            EmissionRight::Shift {
                frontier: issue.frontier.clone().unwrap_or_default(),
                attempt: issue.attempt.clone(),
            },
        )
    }

    /// The standing of the shift code that issues its steps through
    /// `controller`, under the scope that controller retained.
    pub fn turn_execution(&self, controller: &crate::ActorContext) -> TraceStanding {
        self.shift(controller.trace_scope().cloned(), controller)
    }

    /// The standing of a recorded effect body while it really runs, under the
    /// scope its step was issued in.
    pub fn effect_body(&self, live: &Arc<LiveStep>) -> TraceStanding {
        self.body(live.scope.clone(), live)
    }

    /// The standing of a recorded step's body while it really runs.
    pub fn body(&self, scope: Option<DurableTraceScope>, live: &Arc<LiveStep>) -> TraceStanding {
        self.standing(scope, EmissionRight::Body(Arc::clone(live)))
    }

    /// The standing of code no journal replays, which is therefore its own
    /// live attempt every time it runs: a host call made outside any recorded
    /// step, the tail of a first-writer store write this caller just made, or
    /// a diagnostic of the attempt itself (a store fault, a replay
    /// divergence), which every attempt that meets it reports.
    pub fn unreplayed(&self, scope: Option<DurableTraceScope>) -> TraceStanding {
        self.standing(scope, EmissionRight::Body(LiveStep::unreplayed()))
    }

    fn standing(&self, scope: Option<DurableTraceScope>, right: EmissionRight) -> TraceStanding {
        TraceStanding {
            runtime: self.clone(),
            scope: scope.map(Arc::new),
            leaf: None,
            right,
            silenced: false,
            narrowing: None,
            selector_failed: false,
            gate: None,
        }
    }
}

/// A runtime with no observer on the system clock.
impl Default for TraceRuntime {
    fn default() -> Self {
        Self::new(Arc::new(crate::SystemClock))
    }
}

impl std::fmt::Debug for TraceRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceRuntime")
            .field("detail", &self.parts.detail)
            .field("content", &self.content())
            .field("observed", &self.is_observed())
            .finish_non_exhaustive()
    }
}

struct HookEpisode {
    hook: Arc<dyn lash_trace::ContentHook>,
    failed: Arc<std::sync::atomic::AtomicBool>,
}
impl lash_trace::ContentHook for HookEpisode {
    fn on_content(
        &self,
        view: &lash_trace::ContentView<'_>,
        content: &mut lash_trace::RecordContent<'_>,
    ) {
        self.hook.on_content(view, content);
        self.failed.store(false, Ordering::Relaxed);
    }
}

/// The one path an external observation takes out of the engine: the record
/// sinks, the OpenTelemetry exporter among them when one is installed.
#[derive(Clone, Default)]
pub struct TraceEmitter {
    sinks: Arc<[Arc<dyn TraceSink>]>,
    sink_failures: Arc<[Arc<Mutex<u64>>]>,
    product: Arc<[Arc<dyn ProductObservationSink>]>,
    /// Applied to every record before a sink sees it.
    content: ContentPolicy,
    policy: lash_trace::TracePolicySnapshot,
    hook: Option<Arc<dyn lash_trace::ContentHook>>,
    selector: Option<Arc<dyn lash_trace::CaptureSelector>>,
    hook_failure: Arc<std::sync::atomic::AtomicBool>,
    selected: bool,
    selector_failed: bool,
    omissions: Arc<lash_trace::TraceOmissionLedger>,
}

impl TraceEmitter {
    /// Whether a record sink observes external records. A site tests it
    /// before it reads the clock or builds anything a record would hold.
    pub fn has_external_observers(&self) -> bool {
        !self.sinks.is_empty()
    }

    /// Whether a product observation consumer is installed.
    pub fn has_product_observers(&self) -> bool {
        !self.product.is_empty()
    }

    /// The record sinks, in installation order.
    pub fn sinks(&self) -> &[Arc<dyn TraceSink>] {
        &self.sinks
    }

    /// The product observation consumers, in installation order.
    pub fn product_observers(&self) -> &[Arc<dyn ProductObservationSink>] {
        &self.product
    }

    /// Emits one record at `position`.
    ///
    /// Returns before calling `identity` or `record` when nothing observes or
    /// `permit` is `None`: a replayed step and a retained read build nothing.
    /// `at_ms` is the retained time of the fact the record reports. The
    /// caller holds no engine lock: every sink is host code.
    pub fn emit(
        &self,
        permit: Option<&EmissionPermit>,
        position: Option<TracePosition>,
        identity: impl FnOnce() -> TraceRecordIdentity,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        if let Some(record) = self.prepare(permit, position, identity, at_ms, record) {
            self.append(&record);
        }
    }

    /// The record [`emit`](Self::emit) would hand the sinks, built, selected
    /// and hooked, without appending it: for a record a publication hold
    /// keeps until its parent publishes. `None` when nothing is built.
    pub(crate) fn prepare(
        &self,
        permit: Option<&EmissionPermit>,
        position: Option<TracePosition>,
        identity: impl FnOnce() -> TraceRecordIdentity,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) -> Option<TraceRecord> {
        if permit.is_none() || !self.has_external_observers() {
            return None;
        }
        let (context, event) = record();
        match TraceRecord::identified(
            &identity(),
            context,
            event,
            TraceTimestamp::from_unix_ms(at_ms),
        ) {
            Ok(record) => {
                let mut record = record.at(position);
                let mut policy = self.policy.clone();
                let mut failed = self.selector_failed;
                if !self.selected
                    && let Some(selector) = &self.selector
                {
                    let ids = record.context.ids();
                    let mut narrow = lash_trace::core_internal::content_narrowing(&policy, None);
                    let kind = record
                        .position
                        .as_ref()
                        .map_or(TraceSpanKind::Point, |p| p.kind);
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            selector.select(
                                &lash_trace::SelectorView {
                                    kind,
                                    ids: &ids,
                                    policy: &policy,
                                },
                                &mut narrow,
                            )
                        }))
                    {
                        narrow = lash_trace::core_internal::content_narrowing(&policy, None);
                        narrow.omit_all();
                        failed = true;
                        if crate::panic_containment::is_loud() {
                            std::panic::resume_unwind(payload);
                        }
                    }
                    if !narrow.builds(kind) {
                        return None;
                    }
                    policy.content = narrow.content();
                }
                if failed {
                    record
                        .omissions
                        .add(lash_trace::TraceOmission::SelectorFailed);
                }
                let panic = record.apply_content(&policy, self.hook.as_deref());
                if record.content.hook_failed && !self.hook_failure.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        event = "trace.content_hook.failed",
                        "content hook panicked; content omitted"
                    );
                }
                if let Some(payload) = panic
                    && crate::panic_containment::is_loud()
                {
                    std::panic::resume_unwind(payload);
                }
                self.omissions.record(
                    &lash_trace::TraceOmissionSite::Record {
                        kind: record
                            .position
                            .as_ref()
                            .map_or(TraceSpanKind::Point, |p| p.kind),
                        record: record.id.clone(),
                        span: record.position.as_ref().map(|p| p.span.span_id()),
                    },
                    &record.omissions,
                );
                Some(record)
            }
            Err(error) => {
                tracing::warn!(%error, "failed to derive a trace record identity");
                None
            }
        }
    }

    /// Publishes one product observation (the process and language graph).
    /// It takes no permit and runs on replay, because the product folds it
    /// by its own identity; it never reaches a record sink.
    pub fn observe_product(&self, record: impl FnOnce() -> TraceRecord) {
        if self.product.is_empty() {
            return;
        }
        let record = record();
        for observer in self.product.iter() {
            observer.observe(&record);
        }
    }

    /// Hands `record` to every sink in order. Each append runs under panic
    /// containment and counts its own failures: a sink that fails or panics
    /// costs only its own delivery.
    pub(crate) fn append(&self, record: &TraceRecord) {
        for (sink_index, (sink, failures)) in
            self.sinks.iter().zip(self.sink_failures.iter()).enumerate()
        {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.append(record)));
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(payload) => {
                    let message = crate::panic_containment::payload_message(payload.as_ref());
                    if crate::panic_containment::is_loud() {
                        std::panic::resume_unwind(payload);
                    }
                    Some(format!("the sink panicked: {message}"))
                }
            };
            let mut count = failures.lock_recover();
            match failure {
                Some(error) => {
                    *count = count.saturating_add(1);
                    if *count == 1 {
                        // A host subscriber can forward this diagnostic to a
                        // sink; release the counter before invoking its layer.
                        drop(count);
                        tracing::warn!(event = "trace_sink.degraded", sink_index, %error,
                            failure_count = 1_u64,
                            session_id = record.context.session_id.as_ref().map(|id| id.as_str()),
                            turn_id = record.context.turn_id.as_ref().map(|id| id.as_str()),
                            "trace sink append failed; records may be lost");
                    }
                }
                None if *count > 0 => {
                    let failure_count = std::mem::take(&mut *count);
                    drop(count);
                    tracing::info!(
                        event = "trace_sink.recovered",
                        sink_index,
                        failure_count,
                        "trace sink append recovered"
                    );
                }
                None => {}
            }
        }
    }

    /// The failures each sink has counted since its last success, in
    /// installation order.
    pub fn sink_failures(&self) -> Vec<u64> {
        self.sink_failures
            .iter()
            .map(|failures| *failures.lock_recover())
            .collect()
    }
}

/// One real execution of a recorded step's body.
///
/// The engine-side wrapper that runs a body begins one inside it and hands it
/// to the work. Its attempt id identifies this execution; it is never
/// journaled as a freshness claim. A
/// holder must not keep it past the body.
#[derive(Debug)]
pub struct LiveStep {
    permit: EmissionPermit,
    attempt: TraceAttemptId,
    next_ordinal: AtomicU64,
    observation: Option<TraceAttemptLink>,
    scope: Option<DurableTraceScope>,
}

impl LiveStep {
    fn of(
        attempt: TraceAttemptId,
        observation: Option<TraceAttemptLink>,
        scope: Option<DurableTraceScope>,
    ) -> Arc<Self> {
        Arc::new(Self {
            permit: EmissionPermit::live_execution(attempt.clone()),
            attempt,
            next_ordinal: AtomicU64::new(0),
            observation,
            scope,
        })
    }

    /// Begun where a recorded step's body starts running.
    pub(crate) fn begin(
        effect_attempt: Option<&crate::EffectAttempt>,
        observation: Option<TraceAttemptLink>,
        scope: Option<DurableTraceScope>,
    ) -> Arc<Self> {
        let attempt = match effect_attempt {
            Some(attempt) => attempt.trace_id(),
            None => fresh_attempt(),
        };
        Self::of(attempt, observation, scope)
    }

    /// Code no journal replays ([`TraceRuntime::unreplayed`]).
    fn unreplayed() -> Arc<Self> {
        Self::of(fresh_attempt(), None, None)
    }

    pub fn permit(&self) -> &EmissionPermit {
        &self.permit
    }

    pub fn attempt(&self) -> &TraceAttemptId {
        &self.attempt
    }

    /// The substrate attempt this body runs under, if the substrate has one.
    pub fn attempt_observation(&self) -> Option<&TraceAttemptLink> {
        self.observation.as_ref()
    }

    fn next_ordinal(&self) -> u64 {
        self.next_ordinal.fetch_add(1, Ordering::Relaxed)
    }
}

fn fresh_attempt() -> TraceAttemptId {
    TraceAttemptId::new(format!("attempt:{}", uuid::Uuid::new_v4().simple()))
}

/// Where one shift stands relative to its journal.
///
/// A shift issues its recorded steps through one scoped controller and its
/// clones, and re-runs from its start on every attempt with a new one. The
/// engine's step wrapper tells the frontier what became of each step it
/// issued: its body really ran, or the journal answered it.
///
/// While the shift is behind its journal it cannot know whether what it
/// observes is new, so the observation is held:
///
/// - a step the journal answers proves everything held was a reconstruction
///   of work an earlier attempt observed, and it is dropped;
/// - a step whose body runs proves the attempt
///   reached new work: what is still held was made after the last answered
///   step, by this attempt, and is emitted.
///
/// Past the journal, the shift emits as it observes.
#[derive(Clone)]
pub struct JournalFrontier {
    inner: Arc<FrontierInner>,
}

struct FrontierInner {
    runtime: std::sync::Mutex<Option<TraceRuntime>>,
    state: std::sync::Mutex<FrontierState>,
    attempt: TraceAttemptId,
    next_ordinal: AtomicU64,
}

/// One held shift observation: emits itself under the frontier's attempt.
type HeldObservation = Box<dyn FnOnce(&EmissionPermit, TraceAttemptId, u64) + Send>;

enum FrontierState {
    /// No step body has run in this attempt yet.
    Behind(Vec<HeldObservation>),
    /// A step body has really run in this attempt.
    Past,
}

impl std::fmt::Debug for JournalFrontier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalFrontier")
            .field("attempt", &self.inner.attempt)
            .field("crossed", &self.is_crossed())
            .finish()
    }
}

impl Default for JournalFrontier {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalFrontier {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(FrontierInner {
                runtime: std::sync::Mutex::new(None),
                state: std::sync::Mutex::new(FrontierState::Behind(Vec::new())),
                attempt: fresh_attempt(),
                next_ordinal: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn bind_runtime(&self, runtime: TraceRuntime) {
        *self
            .inner
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime);
    }
    pub(crate) fn runtime(&self) -> Option<TraceRuntime> {
        self.inner
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FrontierState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records that a step body of this shift really ran ([`StepIssue`]).
    fn cross(&self) {
        let held = match std::mem::replace(&mut *self.state(), FrontierState::Past) {
            FrontierState::Behind(held) => held,
            FrontierState::Past => return,
        };
        for observation in held {
            self.emit_held(observation);
        }
    }

    /// Records that the journal answered a step of this shift without its
    /// body running.
    fn served(&self) {
        if let FrontierState::Behind(held) = &mut *self.state() {
            held.clear();
        }
    }

    /// Whether a step body of this shift has really run in this attempt.
    pub fn is_crossed(&self) -> bool {
        matches!(*self.state(), FrontierState::Past)
    }

    /// Emits `observation` now when the shift is past its journal, and holds
    /// it otherwise.
    fn observe(&self, observation: HeldObservation) {
        let runtime = self.runtime();
        let capacity = runtime
            .as_ref()
            .map_or_else(lash_trace::TraceLimits::standard, TraceRuntime::limits)
            .held_observations();
        {
            let mut state = self.state();
            if let FrontierState::Behind(held) = &mut *state {
                if held.len() < usize::try_from(capacity).unwrap_or(usize::MAX) {
                    held.push(observation);
                } else if let Some(runtime) = runtime {
                    drop(state);
                    runtime.omissions().record_one(
                        &lash_trace::TraceOmissionSite::Shift { scope: None },
                        lash_trace::TraceOmission::HeldObservations,
                    );
                }
                return;
            }
        }
        self.emit_held(observation);
    }

    fn emit_held(&self, observation: HeldObservation) {
        observation(
            &EmissionPermit::live_execution(self.inner.attempt.clone()),
            self.inner.attempt.clone(),
            self.inner.next_ordinal.fetch_add(1, Ordering::Relaxed),
        );
    }
}

/// What the shift that issues a step lends the step's body: its journal
/// frontier, told when the body really runs, and the substrate attempt the
/// shift executes under.
///
/// The issue an executor carries also tells the frontier when the step is
/// answered without its body running: it is dropped unbegun. A copy carries
/// the frontier and the attempt and reports nothing by being dropped.
#[derive(Debug, Default)]
pub struct StepIssue {
    frontier: Option<JournalFrontier>,
    attempt: Option<TraceAttemptLink>,
    scope: Option<DurableTraceScope>,
    /// Whether dropping this issue unbegun means the journal served the step.
    reports_served: bool,
}

impl Clone for StepIssue {
    fn clone(&self) -> Self {
        Self {
            frontier: self.frontier.clone(),
            attempt: self.attempt.clone(),
            scope: self.scope.clone(),
            reports_served: false,
        }
    }
}

impl Drop for StepIssue {
    fn drop(&mut self) {
        if self.reports_served
            && let Some(frontier) = &self.frontier
        {
            // A step whose body ran crossed the frontier, which this leaves
            // as it is.
            frontier.served();
        }
    }
}

impl StepIssue {
    pub(crate) fn new(
        frontier: JournalFrontier,
        attempt: Option<TraceAttemptLink>,
        scope: Option<DurableTraceScope>,
    ) -> Self {
        Self {
            frontier: Some(frontier),
            attempt,
            scope,
            reports_served: true,
        }
    }

    /// Begins the live step of a body the engine's step wrapper is running.
    pub(crate) fn begin(&self, effect_attempt: Option<&crate::EffectAttempt>) -> Arc<LiveStep> {
        self.cross();
        LiveStep::begin(effect_attempt, self.attempt.clone(), self.scope.clone())
    }

    /// Begins the live step of a body a substrate records itself: called
    /// inside that body, where the substrate's journal runs it once.
    #[doc(hidden)]
    pub fn begin_native(&self) -> Arc<LiveStep> {
        self.cross();
        LiveStep::of(fresh_attempt(), self.attempt.clone(), self.scope.clone())
    }

    /// Marks the step as one that has no recorded body of its own: one that
    /// replays by re-execution, or one the substrate answers natively (a
    /// wait, a timer). Its completion says nothing about the journal.
    pub(crate) fn unrecorded(&mut self) {
        self.reports_served = false;
    }

    /// The issuing shift's frontier, when a shift issued the step.
    pub fn frontier(&self) -> Option<&JournalFrontier> {
        self.frontier.as_ref()
    }

    pub fn attempt_observation(&self) -> Option<&TraceAttemptLink> {
        self.attempt.as_ref()
    }

    pub fn scope(&self) -> Option<&DurableTraceScope> {
        self.scope.as_ref()
    }

    fn cross(&self) {
        if let Some(frontier) = &self.frontier {
            frontier.cross();
        }
    }
}

#[derive(Clone)]
enum EmissionRight {
    Body(Arc<LiveStep>),
    Shift {
        frontier: JournalFrontier,
        attempt: Option<TraceAttemptLink>,
    },
}

/// Where one piece of engine code stands when it observes: the runtime, the
/// scope it runs under, the execution-local leaf span it is inside, if any,
/// and its right to emit.
#[derive(Clone)]
pub struct TraceStanding {
    runtime: TraceRuntime,
    scope: Option<Arc<DurableTraceScope>>,
    leaf: Option<Arc<LeafSpan>>,
    right: EmissionRight,
    /// Inside a span of a kind the detail policy turns off: nothing here
    /// is built, and nothing a child opens is either.
    silenced: bool,
    narrowing: Option<Arc<lash_trace::TraceNarrowing>>,
    selector_failed: bool,
    /// Inside an open extension span: the closing records of the leaves
    /// opened here are held in that span's hold until it publishes (P3).
    gate: Option<extension::HoldGate>,
}

/// The span a standing's records name instead of its scope's: a model call,
/// whose id is drawn when it opens and carried, never recomputed, or a
/// derived container such as a code cell, whose id every owner derives.
#[derive(Debug)]
struct LeafSpan {
    span: TraceCarrier,
    parent: TraceCarrier,
    kind: TraceSpanKind,
    started_at_ms: u64,
}

/// The kind of span a scope's own records name.
fn scope_span_kind(kind: TraceScopeKind) -> TraceSpanKind {
    match kind {
        TraceScopeKind::Turn => TraceSpanKind::Agent,
        TraceScopeKind::Tool => TraceSpanKind::Tool,
        TraceScopeKind::ToolIntent => TraceSpanKind::ToolIntent,
        TraceScopeKind::Process => TraceSpanKind::Process,
    }
}

impl TraceStanding {
    pub fn runtime(&self) -> &TraceRuntime {
        &self.runtime
    }

    /// What lash builds for each span kind.
    pub fn detail(&self) -> TraceDetailPolicy {
        self.runtime.parts.detail
    }

    /// The host's telemetry content policy. A site that would clone, render
    /// or serialize content for a record builds it through
    /// [`ContentPolicy::capture`], so omitted content costs nothing.
    pub fn content(&self) -> ContentPolicy {
        self.narrowing
            .as_deref()
            .map_or_else(|| self.runtime.content(), |n| n.content())
    }

    /// Select before capturing any fields. Children of a leaf retain this
    /// narrowing, including a skipped leaf's no-op standing.
    pub fn selected(&self, kind: TraceSpanKind, ids: &TraceIds) -> Self {
        let policy = self.runtime.policy();
        let mut narrow =
            lash_trace::core_internal::content_narrowing(&policy, self.narrowing.as_deref());
        let mut failed = false;
        if let Some(selector) = &self.runtime.parts.emitter.selector {
            let view = lash_trace::SelectorView {
                kind,
                ids,
                policy: &policy,
            };
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                selector.select(&view, &mut narrow)
            })) {
                narrow = lash_trace::core_internal::content_narrowing(
                    &policy,
                    self.narrowing.as_deref(),
                );
                narrow.omit_all();
                failed = true;
                if crate::panic_containment::is_loud() {
                    std::panic::resume_unwind(payload);
                }
            }
        }
        let silenced = self.silenced || !narrow.builds(kind);
        Self {
            narrowing: Some(Arc::new(narrow)),
            selector_failed: failed,
            silenced,
            ..self.clone()
        }
    }

    pub fn scope(&self) -> Option<&DurableTraceScope> {
        self.scope.as_deref()
    }

    /// The span a child of this code is parented to: the leaf it is inside,
    /// else its scope's span.
    pub fn container(&self) -> Option<&TraceCarrier> {
        match &self.leaf {
            Some(leaf) => Some(&leaf.span),
            None => self.scope.as_deref().and_then(|scope| scope.span.as_ref()),
        }
    }

    /// Whether a record made here now would reach an observer. Sites that
    /// must prepare a record's parts ahead of the call test it first. Code
    /// inside a span of a kind the detail policy turns off is unobserved.
    pub fn is_observed(&self) -> bool {
        !self.silenced && self.runtime.is_observed()
    }

    /// Drop observations held by a replay-only drive. Reaching the end does
    /// not prove that a body executed; logical terminals use SQL receipts.
    pub fn conclude(&self) {
        if let EmissionRight::Shift { frontier, .. } = &self.right {
            frontier.served();
        }
    }

    /// The same right under another scope: a child scope of this one.
    #[must_use]
    pub fn under(&self, scope: DurableTraceScope) -> Self {
        Self {
            runtime: self.runtime.clone(),
            scope: Some(Arc::new(scope)),
            leaf: None,
            right: self.right.clone(),
            silenced: false,
            narrowing: None,
            selector_failed: false,
            gate: None,
        }
    }

    /// The same scope inside the body of a recorded step.
    #[must_use]
    pub fn in_body(&self, live: &Arc<LiveStep>) -> Self {
        Self {
            runtime: self.runtime.clone(),
            scope: self.scope.clone(),
            leaf: self.leaf.clone(),
            right: EmissionRight::Body(Arc::clone(live)),
            silenced: self.silenced,
            narrowing: self.narrowing.clone(),
            selector_failed: self.selector_failed,
            gate: self.gate.clone(),
        }
    }

    fn attempt_observation(&self) -> Option<&TraceAttemptLink> {
        match &self.right {
            EmissionRight::Body(live) => live.attempt_observation(),
            EmissionRight::Shift { attempt, .. } => attempt.as_ref(),
        }
    }

    /// The position of a record made here in `role`, at `at_ms`: under the
    /// leaf this code is inside, else its scope's span. `None` when the
    /// scope is unobserved, or there is none.
    fn position(&self, role: SpanRole, at_ms: u64) -> Option<TracePosition> {
        let scope = self.scope.as_deref();
        let (span, parent, links, kind, started_at_ms) = match &self.leaf {
            Some(leaf) => (
                leaf.span.clone(),
                Some(leaf.parent.clone()),
                TraceLinks::new(),
                leaf.kind,
                leaf.started_at_ms,
            ),
            None => {
                let scope = scope?;
                (
                    scope.span.clone()?,
                    scope
                        .cause
                        .as_parent()
                        .map(|parent| parent.carrier().clone()),
                    scope.links.clone(),
                    scope_span_kind(scope.scope.kind()),
                    scope.started_at_ms,
                )
            }
        };
        Some(TracePosition {
            span,
            parent,
            links,
            role,
            kind,
            started_at: TraceTimestamp::from_unix_ms(started_at_ms),
            ended_at: role
                .publishes()
                .then(|| TraceTimestamp::from_unix_ms(at_ms)),
            scope: scope.map(|scope| scope.scope.clone()),
            attempt: self.attempt_observation().cloned(),
        })
    }

    /// Emits an observation of this attempt's own work: a model request, a
    /// diagnostic, a step of the shift. Each real attempt names its own
    /// record. Shift code observes through its [`JournalFrontier`]. The
    /// record is an event of the span this code is in.
    pub fn observe(&self, record: impl FnOnce() -> (TraceContext, TraceEvent)) {
        self.observe_as(SpanRole::Event, None, record);
    }

    /// Emits the opening record of this standing's span (its leaf's, else
    /// its scope's): the start its admitting owner observes. A live hint:
    /// the span is built from its closing record.
    pub fn open_scope(&self, record: impl FnOnce() -> (TraceContext, TraceEvent)) {
        self.observe_as(SpanRole::Opens, None, record);
    }

    /// Emits the closing record of this standing's span (its leaf's, else
    /// its scope's), which ended at `ended_at_ms`, the end its terminal's
    /// commit retained. Only the owner whose acknowledged commit wrote that
    /// terminal calls it (P1), so the span is published at most once.
    pub fn close_scope(
        &self,
        ended_at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        self.observe_as(SpanRole::Closes, Some(ended_at_ms), record);
    }

    /// Opens a `kind` leaf under this code's container with the opening
    /// record `record`: the leaf's id is drawn now, and its events and close
    /// go through the returned [`OpenSpan`]. Nothing is opened, and
    /// `record` and the returned span's records are this standing's, when
    /// there is no observed container.
    pub fn open(
        &self,
        kind: TraceSpanKind,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) -> OpenSpan {
        if !self.runtime.parts.detail.builds(kind) {
            // A kind the detail policy turns off builds nothing, and by the
            // off rule nothing it could parent is built either.
            return OpenSpan {
                model: None,
                standing: Self {
                    silenced: true,
                    ..self.clone()
                },
                request: None,
            };
        }
        let Some(parent) = self.container().cloned().filter(|_| self.is_observed()) else {
            // With no observed container the leaf has no span: its records
            // are this standing's, unpositioned.
            self.observe(record);
            return OpenSpan {
                model: None,
                standing: self.clone(),
                request: None,
            };
        };
        let span = TraceCarrier::new(
            parent.trace_id(),
            self.runtime.parts.identity.leaf_span_id(),
            parent.flags(),
            parent.tracestate().clone(),
        );
        let standing = Self {
            runtime: self.runtime.clone(),
            scope: self.scope.clone(),
            leaf: Some(Arc::new(LeafSpan {
                span,
                parent,
                kind,
                started_at_ms: self.runtime.parts.clock.timestamp_ms(),
            })),
            right: self.right.clone(),
            silenced: false,
            narrowing: self.narrowing.clone(),
            selector_failed: self.selector_failed,
            gate: self.gate.clone(),
        };
        standing.observe_as(SpanRole::Opens, None, record);
        OpenSpan {
            standing,
            model: None,
            request: None,
        }
    }

    /// The standing of code inside the `kind` container keyed `key` under
    /// this code's container, which started at `started_at_ms`: a container
    /// whose id any owner derives the same way. Its records open, are events
    /// of, and close that span. `None` when there is no observed container.
    #[must_use]
    pub fn derived(
        &self,
        kind: TraceSpanKind,
        key: &lash_trace::SpanKey<'_>,
        started_at_ms: u64,
    ) -> Option<Self> {
        let parent = self.container().cloned()?;
        let span = lash_trace::derive_span(&parent, key);
        Some(Self {
            runtime: self.runtime.clone(),
            scope: self.scope.clone(),
            leaf: Some(Arc::new(LeafSpan {
                span,
                parent,
                kind,
                started_at_ms,
            })),
            right: self.right.clone(),
            silenced: self.silenced || !self.runtime.parts.detail.builds(kind),
            narrowing: self.narrowing.clone(),
            selector_failed: self.selector_failed,
            gate: None,
        })
    }

    /// Whether a `kind` span opened here would be built: something observes,
    /// the detail policy builds the kind, and neither this standing's
    /// narrowing nor an enclosing skipped span turned it off.
    pub(crate) fn builds(&self, kind: TraceSpanKind) -> bool {
        !self.silenced
            && self.runtime.is_observed()
            && self.runtime.parts.detail.builds(kind)
            && self
                .narrowing
                .as_deref()
                .is_none_or(|narrowing| narrowing.builds(kind))
    }

    /// The standing of code inside the open extension span `span` (whose
    /// parent is `parent`), opened at `started_at_ms`: its leaves are the
    /// span's children, and their closing records are held by `gate` until
    /// the span publishes.
    pub(crate) fn in_extension(
        &self,
        span: TraceCarrier,
        parent: TraceCarrier,
        started_at_ms: u64,
        gate: extension::HoldGate,
    ) -> Self {
        Self {
            leaf: Some(Arc::new(LeafSpan {
                span,
                parent,
                kind: TraceSpanKind::Extension,
                started_at_ms,
            })),
            gate: Some(gate),
            ..self.clone()
        }
    }

    /// The standing of code inside the `kind` container `span` under this
    /// standing's container: a span another owner derives and closes, which
    /// the work opened here takes as its parent.
    pub(crate) fn within(&self, span: TraceCarrier, kind: TraceSpanKind) -> Self {
        let parent = self.container().cloned().unwrap_or_else(|| span.clone());
        Self {
            leaf: Some(Arc::new(LeafSpan {
                span,
                parent,
                kind,
                started_at_ms: self.runtime.parts.clock.timestamp_ms(),
            })),
            ..self.clone()
        }
    }

    /// A random span id for a leaf opened here, from the deployment's id
    /// source.
    pub(crate) fn leaf_span_id(&self) -> lash_trace::W3cSpanId {
        self.runtime.parts.identity.leaf_span_id()
    }

    /// Emits the one whole record of a span at `position` through this
    /// standing's right, and hands the built record to `deliver` instead of
    /// appending it: a hold may keep it until its parent publishes. A shift
    /// behind its journal frontier holds the record with its own
    /// observations: released when a step body runs, dropped when the
    /// journal answers one (§7.4 rule 5).
    pub(crate) fn emit_whole(
        &self,
        position: TracePosition,
        context: TraceContext,
        event: TraceEvent,
        deliver: impl FnOnce(TraceRecord, &TraceEmitter) + Send + 'static,
    ) {
        if !self.runtime.is_observed() {
            return;
        }
        let at_ms = position.ended_at.map_or_else(
            || self.runtime.parts.clock.timestamp_ms(),
            TraceTimestamp::unix_ms,
        );
        let emit = {
            let standing = self.clone();
            move |permit: &EmissionPermit, attempt: TraceAttemptId, ordinal: u64| {
                let identity = match standing.scope.as_deref() {
                    Some(scope) => TraceRecordIdentity::Live {
                        scope: scope.scope.clone(),
                        attempt,
                        ordinal,
                    },
                    None => TraceRecordIdentity::UnscopedLive { attempt, ordinal },
                };
                let emitter = standing.emitter();
                if let Some(record) = emitter.prepare(
                    Some(permit),
                    Some(position),
                    || identity,
                    at_ms,
                    || (context, event),
                ) {
                    deliver(record, &emitter);
                }
            }
        };
        match &self.right {
            EmissionRight::Body(live) => {
                emit(live.permit(), live.attempt.clone(), live.next_ordinal());
            }
            EmissionRight::Shift { frontier, .. } if frontier.is_crossed() => {
                let permit = EmissionPermit::live_execution(frontier.inner.attempt.clone());
                emit(
                    &permit,
                    frontier.inner.attempt.clone(),
                    frontier.inner.next_ordinal.fetch_add(1, Ordering::Relaxed),
                );
            }
            EmissionRight::Shift { frontier, .. } => frontier.observe(Box::new(emit)),
        }
    }

    fn observe_as(
        &self,
        role: SpanRole,
        at_ms: Option<u64>,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        if !self.is_observed() {
            return;
        }
        // The clock is read only for a record that is made: a served prefix
        // reads none.
        let at_ms = || at_ms.unwrap_or_else(|| self.runtime.parts.clock.timestamp_ms());
        match &self.right {
            EmissionRight::Body(live) => self.emit_live(
                live.permit(),
                live.attempt.clone(),
                live.next_ordinal(),
                role,
                at_ms(),
                record,
            ),
            EmissionRight::Shift { frontier, .. } if frontier.is_crossed() => {
                let permit = EmissionPermit::live_execution(frontier.inner.attempt.clone());
                self.emit_live(
                    &permit,
                    frontier.inner.attempt.clone(),
                    frontier.inner.next_ordinal.fetch_add(1, Ordering::Relaxed),
                    role,
                    at_ms(),
                    record,
                );
            }
            EmissionRight::Shift { .. } => {}
        }
    }

    /// Defers an owned observation until a real body admits this shift's suffix.
    /// Neither the clock nor the record constructor runs on a served prefix.
    pub fn observe_deferred(
        &self,
        record: impl FnOnce() -> (TraceContext, TraceEvent) + Send + 'static,
    ) {
        if !self.is_observed() {
            return;
        }
        match &self.right {
            EmissionRight::Body(_) => self.observe(record),
            EmissionRight::Shift { frontier, .. } => {
                let standing = self.clone();
                frontier.observe(Box::new(move |permit, attempt, ordinal| {
                    standing.emit_live(
                        permit,
                        attempt,
                        ordinal,
                        SpanRole::Event,
                        standing.runtime.parts.clock.timestamp_ms(),
                        record,
                    );
                }));
            }
        }
    }

    fn emit_live(
        &self,
        permit: &EmissionPermit,
        attempt: TraceAttemptId,
        ordinal: u64,
        role: SpanRole,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        let position = self.position(role, at_ms);
        let identity = match self.scope.as_deref() {
            Some(scope) => TraceRecordIdentity::Live {
                scope: scope.scope.clone(),
                attempt,
                ordinal,
            },
            None => TraceRecordIdentity::UnscopedLive { attempt, ordinal },
        };
        let emitter = self.emitter();
        match &self.gate {
            // A leaf inside an open extension span publishes after it.
            Some(gate) if role.publishes() => {
                if let Some(record) =
                    emitter.prepare(Some(permit), position, || identity, at_ms, record)
                {
                    gate.release(record, &emitter);
                }
            }
            _ => emitter.emit(Some(permit), position, || identity, at_ms, record),
        }
    }

    /// The emitter as this standing sees it: under its selected narrowing.
    fn emitter(&self) -> TraceEmitter {
        let mut emitter = self.runtime.parts.emitter.clone();
        if self.narrowing.is_some() {
            emitter.policy.content = self.content();
            emitter.content = self.content();
            emitter.selected = true;
            emitter.selector_failed = self.selector_failed;
        }
        emitter
    }

    /// `future`, polled under this code's position as the ambient context:
    /// an instrumented library it runs, such as a provider's HTTP client or
    /// a host transport that propagates the current context, takes this
    /// span as its parent (§7.7). Without an adapter's context, or a
    /// position, its polls attach nothing.
    pub fn ambient<F: std::future::Future>(
        &self,
        future: F,
    ) -> impl std::future::Future<Output = F::Output> + use<F> {
        let scope = self
            .container()
            .zip(self.runtime.ambient())
            .map(|(position, ambient)| ambient.for_position(position));
        lash_trace::Framed::new(
            future,
            lash_trace::AmbientFrame::new(scope, std::sync::Arc::new(())),
        )
    }

    /// The live permit of the body this standing is in, and none for shift
    /// code: what a live-class metric is recorded under.
    pub fn body_permit(&self) -> Option<&EmissionPermit> {
        match &self.right {
            EmissionRight::Body(live) => Some(live.permit()),
            EmissionRight::Shift { .. } => None,
        }
    }

    /// The substrate attempt this standing's code runs under, if any.
    pub fn attempt(&self) -> Option<&TraceAttemptLink> {
        self.attempt_observation()
    }

    /// Emits the logical record of one lifecycle `transition` of this
    /// standing's scope: a start opens the scope's span, a terminal closes
    /// it at `at_ms`, and any other transition is an event of it.
    ///
    /// `receipt` is the permit of the committed store receipt that reported
    /// the transition as newly inserted or changed
    /// ([`TraceScopeAdmission::permit`](lash_trace::TraceScopeAdmission::permit),
    /// or [`EmissionPermit::new_transition`] minted beside such a receipt):
    /// a retained read has none and emits nothing. Neither a running body nor
    /// a shift past its journal stands in for it. `at_ms` is the time the
    /// owning record retained for the fact, never the clock now. `ordinal` is
    /// the transition's index within the scope and kind, 0 for a transition
    /// that happens once.
    pub fn transition(
        &self,
        receipt: Option<&EmissionPermit>,
        at_ms: u64,
        transition: TraceTransitionKind,
        ordinal: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        let Some(receipt) = receipt
            .filter(|permit| matches!(permit.source(), lash_trace::EmissionSource::NewTransition))
        else {
            return;
        };
        if !self.runtime.is_observed() {
            return;
        }
        let Some(scope) = self.scope.as_deref() else {
            return;
        };
        let role = match transition {
            TraceTransitionKind::Admitted | TraceTransitionKind::Started => SpanRole::Opens,
            TraceTransitionKind::Terminal => SpanRole::Closes,
            _ => SpanRole::Event,
        };
        let (context, event) = record();
        let mut position = self.position(role, at_ms);
        if matches!(&event, TraceEvent::DomainCompleted { completion }
            if matches!(completion.subject, lash_trace::TraceDomainSubject::Workflow { .. }))
            && let Some(position) = &mut position
        {
            position.kind = TraceSpanKind::Workflow;
        }
        self.runtime.parts.emitter.emit(
            Some(receipt),
            position,
            || TraceRecordIdentity::Transition {
                scope: scope.scope.clone(),
                transition,
                ordinal,
            },
            at_ms,
            || (context, event),
        );
    }
}

#[path = "model_span.rs"]
mod model_span;
pub use model_span::OpenSpan;

#[path = "extension/mod.rs"]
pub(crate) mod extension;
pub use extension::{ExtensionSpan, ExtensionTrace, ToolSpanAttributes, TraceInstrumented};

/// The scope of a call under its admitted logical owner, issued under
/// `parent`, its issuing container (the cell it was called from, else its
/// owner's span): its span is derived from that parent's, even across
/// physical turns; a call with no observed parent is unobserved.
pub fn tool_trace_scope(
    opener: &crate::EffectOpener,
    parent: Option<&lash_trace::TraceParent>,
    call_id: &crate::ToolCallId,
    started_at_ms: u64,
) -> DurableTraceScope {
    DurableTraceScope::admit(
        TraceScopeId::admission(TraceScopeOwner::Tool {
            owner: lash_trace::TraceToolOwner::from(opener),
            call_id: call_id.to_string(),
        }),
        parent,
        lash_trace::TraceLinks::new(),
        started_at_ms,
    )
}

#[cfg(test)]
mod diagnostic_tests;
