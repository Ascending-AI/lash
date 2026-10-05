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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_trace::{
    AttemptObservation, DurableTraceScope, EmissionPermit, TraceAnchor, TraceAttemptId, TraceCause,
    TraceContext, TraceDomainProjector, TraceEvent, TraceLevel, TraceRecord, TraceRecordIdentity,
    TraceScopeFactory, TraceScopeId, TraceScopeOwner, TraceSink, TraceTransitionKind,
    UntracedScopes, telemetry::metrics::TelemetryMetrics,
};

/// The runtime's shared trace handle: the scope factory, the clock, the
/// emitter and the host's run metadata. Every engine path and every plugin
/// is handed this same value.
#[derive(Clone)]
pub struct TraceRuntime {
    /// One allocation, so every holder carries a pointer.
    parts: Arc<TraceRuntimeParts>,
}

#[derive(Clone)]
struct TraceRuntimeParts {
    scopes: Arc<dyn TraceScopeFactory>,
    clock: Arc<dyn crate::Clock>,
    emitter: TraceEmitter,
    level: TraceLevel,
    base_context: TraceContext,
    metrics: TelemetryMetrics,
    wait_receipts: Option<Arc<dyn crate::store::WaitReceiptStore>>,
    tool_receipts: Option<Arc<dyn crate::StoreSet>>,
}

impl TraceRuntime {
    /// A runtime with no observer: untraced scopes, no sink.
    pub fn new(clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            parts: Arc::new(TraceRuntimeParts {
                scopes: Arc::new(UntracedScopes),
                clock,
                emitter: TraceEmitter::default(),
                level: TraceLevel::Standard,
                base_context: TraceContext::default(),
                metrics: TelemetryMetrics::default(),
                wait_receipts: None,
                tool_receipts: None,
            }),
        }
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

    #[must_use]
    pub fn with_wait_receipts(mut self, store: Arc<dyn crate::store::WaitReceiptStore>) -> Self {
        Arc::make_mut(&mut self.parts).wait_receipts = Some(store);
        self
    }
    pub(crate) fn wait_receipts(&self) -> Option<&Arc<dyn crate::store::WaitReceiptStore>> {
        self.parts.wait_receipts.as_ref()
    }

    /// Binds the deployment's durable logical tool receipts. The factory is
    /// opened only when a recorded tool fact is observed.
    #[must_use]
    pub fn with_tool_receipts(mut self, stores: Arc<dyn crate::StoreSet>) -> Self {
        Arc::make_mut(&mut self.parts).tool_receipts = Some(stores);
        self
    }

    pub(crate) fn tool_receipts(&self) -> Option<Arc<dyn crate::store::SessionCommitStore>> {
        self.parts.tool_receipts.as_ref().map(|stores| {
            let store: Arc<dyn crate::store::SessionCommitStore> = stores.session_store_factory();
            store
        })
    }

    /// The identity-producing scope factory: [`UntracedScopes`] unless the
    /// host installed an adapter.
    pub fn scopes(&self) -> &Arc<dyn TraceScopeFactory> {
        &self.parts.scopes
    }

    /// The runtime's injected clock.
    pub fn clock(&self) -> &Arc<dyn crate::Clock> {
        &self.parts.clock
    }

    pub fn emitter(&self) -> &TraceEmitter {
        &self.parts.emitter
    }

    pub fn level(&self) -> TraceLevel {
        self.parts.level
    }

    /// The host's run metadata, merged under every record's own context.
    pub fn base_context(&self) -> &TraceContext {
        &self.parts.base_context
    }

    /// Whether any record sink or domain projector observes this runtime.
    /// Sites test it before they build anything a record would hold.
    pub fn is_observed(&self) -> bool {
        self.parts.emitter.has_external_observers()
    }

    /// Adds one passive record sink.
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
        self
    }

    /// Replaces the passive record sinks with `sinks`.
    #[must_use]
    pub fn with_trace_sinks(mut self, sinks: impl IntoIterator<Item = Arc<dyn TraceSink>>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.emitter.sinks = sinks.into_iter().collect();
        self
    }

    /// Installs the runtime's one identity-producing adapter half.
    #[must_use]
    pub fn with_scopes(mut self, scopes: Arc<dyn TraceScopeFactory>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.scopes = scopes;
        self
    }

    /// Installs the adapter's projection half.
    #[must_use]
    pub fn with_projector(mut self, projector: Arc<dyn TraceDomainProjector>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.emitter.projector = Some(projector);
        self
    }

    /// Installs the product observation consumer
    /// ([`TraceEmitter::observe_product`]).
    #[must_use]
    pub fn with_product_observer(mut self, observer: Arc<dyn TraceSink>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.emitter.product = Some(observer);
        self
    }

    #[must_use]
    pub fn with_level(mut self, level: TraceLevel) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.level = level;
        self
    }

    #[must_use]
    pub fn with_base_context(mut self, context: TraceContext) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.base_context = context;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        let parts = Arc::make_mut(&mut self.parts);
        parts.clock = clock;
        self
    }

    /// Flushes the passive record sinks. An adapter's provider is the host's
    /// to flush.
    pub fn flush(&self) -> Result<(), lash_trace::TraceSinkError> {
        for sink in self.parts.emitter.sinks.iter() {
            sink.flush()?;
        }
        Ok(())
    }

    /// The standing of shift code that issues its steps through `controller`:
    /// it may emit once one of those steps' bodies has really run.
    pub fn shift(
        &self,
        scope: Option<DurableTraceScope>,
        controller: &crate::ScopedEffectController<'_>,
    ) -> TraceStanding {
        controller.frontier().bind_runtime(self.clone());
        self.standing(
            scope,
            EmissionRight::Shift {
                frontier: controller.frontier().clone(),
                attempt: controller.controller().attempt_observation(),
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
    pub fn turn_execution(&self, controller: &crate::ScopedEffectController<'_>) -> TraceStanding {
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
            right,
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
            .field("level", &self.parts.level)
            .field("observed", &self.is_observed())
            .finish_non_exhaustive()
    }
}

/// The one path an external observation takes out of the engine: the passive
/// record sinks, and the adapter's domain projection when one is installed.
#[derive(Clone, Default)]
pub struct TraceEmitter {
    sinks: Arc<[Arc<dyn TraceSink>]>,
    projector: Option<Arc<dyn TraceDomainProjector>>,
    product: Option<Arc<dyn TraceSink>>,
}

impl TraceEmitter {
    /// Whether a record sink or the domain projector observes external
    /// records. A site tests it before it reads the clock or builds anything
    /// a record would hold.
    pub fn has_external_observers(&self) -> bool {
        !self.sinks.is_empty() || self.projector.is_some()
    }

    /// Whether a product observation consumer is installed.
    pub fn has_product_observers(&self) -> bool {
        self.product.is_some()
    }

    /// The passive record sinks, in installation order.
    pub fn sinks(&self) -> &[Arc<dyn TraceSink>] {
        &self.sinks
    }

    /// The product observation consumer, when one is installed.
    pub fn product_observer(&self) -> Option<&Arc<dyn TraceSink>> {
        self.product.as_ref()
    }

    /// Emits one record of `scope`.
    ///
    /// Returns before calling `identity` or `record` when nothing observes or
    /// `permit` is `None`: a replayed step and a retained read build nothing.
    /// `at_ms` is the retained time of the fact the record reports.
    pub fn emit(
        &self,
        permit: Option<&EmissionPermit>,
        scope: &DurableTraceScope,
        attempt: Option<&AttemptObservation>,
        identity: impl FnOnce() -> TraceRecordIdentity,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        let Some(permit) = permit else {
            return;
        };
        if !self.has_external_observers() {
            return;
        }
        let (context, event) = record();
        let record = match TraceRecord::identified(&identity(), context, event, datetime(at_ms)) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(%error, "failed to derive a trace record identity");
                return;
            }
        };
        self.append(&record);
        if let Some(projector) = &self.projector {
            projector.project(scope, attempt, permit.source(), &record);
        }
    }

    /// Emits one record that belongs to no admitted scope: a diagnostic of
    /// session-level work. It reaches the record sinks alone, since a domain
    /// projection has no scope to place it under.
    pub fn emit_unscoped(
        &self,
        permit: Option<&EmissionPermit>,
        identity: impl FnOnce() -> TraceRecordIdentity,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        if permit.is_none() || self.sinks.is_empty() {
            return;
        }
        let (context, event) = record();
        match TraceRecord::identified(&identity(), context, event, datetime(at_ms)) {
            Ok(record) => self.append(&record),
            Err(error) => tracing::warn!(%error, "failed to derive unscoped observation identity"),
        }
    }

    /// Publishes one product observation (the process and language graph).
    /// It takes no permit and runs on replay, because the product folds it
    /// by its own identity; it never reaches a record sink or the adapter.
    pub fn observe_product(&self, record: impl FnOnce() -> TraceRecord) {
        let Some(product) = &self.product else {
            return;
        };
        if let Err(error) = product.append(&record()) {
            tracing::warn!(%error, "failed to publish a product observation");
        }
    }

    fn append(&self, record: &TraceRecord) {
        for sink in self.sinks.iter() {
            if let Err(error) = sink.append(record) {
                tracing::warn!(%error, "failed to append trace record");
            }
        }
    }
}

fn datetime(at_ms: u64) -> chrono::DateTime<chrono::Utc> {
    i64::try_from(at_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .unwrap_or_default()
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
    observation: Option<AttemptObservation>,
    scope: Option<DurableTraceScope>,
}

impl LiveStep {
    fn of(
        attempt: TraceAttemptId,
        observation: Option<AttemptObservation>,
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
        observation: Option<AttemptObservation>,
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
    pub fn attempt_observation(&self) -> Option<&AttemptObservation> {
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

/// What a shift holds between two recorded steps, at most. A shift records a
/// step within a few observations; the bound keeps one that never does from
/// growing.
const HELD_OBSERVATIONS_MAX: usize = 256;

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
        {
            let mut state = self.state();
            if let FrontierState::Behind(held) = &mut *state {
                if held.len() < HELD_OBSERVATIONS_MAX {
                    held.push(observation);
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
    attempt: Option<AttemptObservation>,
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
        attempt: Option<AttemptObservation>,
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

    pub fn attempt_observation(&self) -> Option<&AttemptObservation> {
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
        attempt: Option<AttemptObservation>,
    },
}

/// Where one piece of engine code stands when it observes: the runtime, the
/// scope it runs under and its right to emit.
#[derive(Clone)]
pub struct TraceStanding {
    runtime: TraceRuntime,
    scope: Option<Arc<DurableTraceScope>>,
    right: EmissionRight,
}

impl TraceStanding {
    pub fn runtime(&self) -> &TraceRuntime {
        &self.runtime
    }

    pub fn level(&self) -> TraceLevel {
        self.runtime.parts.level
    }

    pub fn scope(&self) -> Option<&DurableTraceScope> {
        self.scope.as_deref()
    }

    /// Whether a record made here now would reach an observer. Sites that
    /// must prepare a record's parts ahead of the call test it first.
    pub fn is_observed(&self) -> bool {
        self.runtime.is_observed()
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
            right: self.right.clone(),
        }
    }

    /// The same scope inside the body of a recorded step.
    #[must_use]
    pub fn in_body(&self, live: &Arc<LiveStep>) -> Self {
        Self {
            runtime: self.runtime.clone(),
            scope: self.scope.clone(),
            right: EmissionRight::Body(Arc::clone(live)),
        }
    }

    fn attempt_observation(&self) -> Option<&AttemptObservation> {
        match &self.right {
            EmissionRight::Body(live) => live.attempt_observation(),
            EmissionRight::Shift { attempt, .. } => attempt.as_ref(),
        }
    }

    /// Emits an observation of this attempt's own work: a model request, a
    /// diagnostic, a step of the shift. Each real attempt names its own
    /// record. Shift code observes through its [`JournalFrontier`].
    pub fn observe(&self, record: impl FnOnce() -> (TraceContext, TraceEvent)) {
        if !self.runtime.is_observed() {
            return;
        }
        match &self.right {
            EmissionRight::Body(live) => self.emit_live(
                live.permit(),
                live.attempt.clone(),
                live.next_ordinal(),
                self.runtime.parts.clock.timestamp_ms(),
                record,
            ),
            EmissionRight::Shift { frontier, .. } if frontier.is_crossed() => {
                let permit = EmissionPermit::live_execution(frontier.inner.attempt.clone());
                self.emit_live(
                    &permit,
                    frontier.inner.attempt.clone(),
                    frontier.inner.next_ordinal.fetch_add(1, Ordering::Relaxed),
                    self.runtime.parts.clock.timestamp_ms(),
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
        if !self.runtime.is_observed() {
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
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        let Some(scope) = self.scope.as_deref() else {
            self.runtime.parts.emitter.emit_unscoped(
                Some(permit),
                || TraceRecordIdentity::UnscopedLive { attempt, ordinal },
                at_ms,
                || self.project(record()),
            );
            return;
        };
        self.runtime.parts.emitter.emit(
            Some(permit),
            scope,
            self.attempt_observation(),
            || TraceRecordIdentity::Live {
                scope: scope.scope.clone(),
                attempt,
                ordinal,
            },
            at_ms,
            || self.project(record()),
        );
    }

    pub fn provider_attempts(
        &self,
        sideband: lash_core_llm::core_internal::ProviderCompletionSideband,
        context: TraceContext,
    ) -> lash_core_llm::core_internal::ProviderCompletionSideband {
        if !self.is_observed() {
            return sideband;
        }
        let standing = self.clone();
        sideband.with_attempt_observer(
            Arc::new(move |attempt| {
                standing.observe(|| (context.clone(), TraceEvent::LlmAttemptCompleted { attempt }));
            }),
            self.runtime.clock().clone(),
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
    pub fn attempt(&self) -> Option<&AttemptObservation> {
        self.attempt_observation()
    }

    /// Emits the logical record of one lifecycle `transition` of this
    /// standing's scope.
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
        self.runtime.parts.emitter.emit(
            Some(receipt),
            scope,
            self.attempt_observation(),
            || TraceRecordIdentity::Transition {
                scope: scope.scope.clone(),
                transition,
                ordinal,
            },
            at_ms,
            || self.project(record()),
        );
    }

    /// The record's context under the host's run metadata, with its span
    /// identity assigned.
    fn project(&self, (context, event): (TraceContext, TraceEvent)) -> (TraceContext, TraceEvent) {
        let mut merged = super::merge_runtime_projection(&self.runtime.parts.base_context, context);
        super::assign_span_identity(&mut merged, &event);
        (merged, event)
    }
}

/// The scope of a call under its admitted logical owner. A retained trace
/// parent supplies only the causal anchor, even across physical turns.
pub fn tool_trace_scope(
    opener: &crate::EffectOpener,
    parent: Option<&DurableTraceScope>,
    call_id: &crate::ToolCallId,
    started_at_ms: u64,
) -> DurableTraceScope {
    DurableTraceScope {
        scope: TraceScopeId::admission(TraceScopeOwner::Tool {
            owner: lash_trace::TraceToolOwner::from(opener),
            call_id: call_id.to_string(),
        }),
        cause: parent.map_or(TraceCause::Root, DurableTraceScope::parent_cause),
        anchor: TraceAnchor::Untraced,
        started_at_ms,
    }
}
