//! The host tracing installation path, consent, selection and bounds.
use super::LashCoreBuilder;
use std::sync::Arc;

impl LashCoreBuilder {
    /// State whether built-in telemetry carries content: prompts, responses,
    /// rendered instructions, tool arguments and results, executed code and
    /// diagnostic or provider text. One choice governs every record sink and
    /// the telemetry adapter. Defaults to
    /// [`crate::tracing::ContentPolicy::standard`] (omitted): records keep
    /// identities, statuses and counts. It never changes what a session
    /// stores or returns.
    ///
    /// `ContentLimits::standard()` cuts parts at 16 KiB, which truncates typical
    /// agent system prompts. Hosts wanting the full prompt raise `max_part_bytes`;
    /// Figments uses 64 KiB parts and 256 KiB attributes.
    pub fn content_policy(mut self, content: crate::tracing::ContentPolicy) -> Self {
        self.content_policy = Some(content);
        self
    }
    /// Narrows content or optional leaves before building, keyed by stable ids.
    pub fn capture_selector(mut self, selector: Arc<dyn crate::tracing::CaptureSelector>) -> Self {
        self.capture_selector = Some(selector);
        self
    }
    /// Redacts or offloads captured content before every sink, transactionally.
    pub fn content_hook(mut self, hook: Arc<dyn crate::tracing::ContentHook>) -> Self {
        self.content_hook = Some(hook);
        self
    }

    /// Select the bounds on the content a record carries. Defaults to
    /// [`ContentLimits::standard`](crate::tracing::ContentLimits::standard).
    pub fn content_limits(mut self, limits: crate::tracing::ContentLimits) -> Self {
        self.content_limits = Some(limits);
        self
    }
    /// Inject a composed runtime for execution tests. Production hosts use
    /// `telemetry`, `trace_sink` and `trace_product_observer`.
    #[cfg(feature = "testing")]
    pub fn trace_runtime(mut self, runtime: lash_core::runtime::TraceRuntime) -> Self {
        self.trace_runtime = Some(runtime);
        self
    }

    /// Add a product observer for reconstructible execution observations.
    /// Unlike telemetry sinks, product observers also see reconstruction.
    /// The core supplies the clock and runtime; hosts never construct one.
    pub fn trace_product_observer(
        mut self,
        observer: Arc<dyn crate::tracing::ProductObservationSink>,
    ) -> Self {
        self.trace_product_observers.push(observer);
        self
    }

    /// Installs the runtime's OpenTelemetry adapter: the identity that
    /// decides each trace at its producer, a sink beside the others, and
    /// the metrics. Replaces the previously configured adapter.
    #[cfg(feature = "otel-trace")]
    pub fn telemetry(mut self, telemetry: lash_trace::otel::OtelTelemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Add a record sink beside the sinks configured already: each sees
    /// every record, under its own panic containment.
    ///
    /// Lash ships no trace writer: a host that wants records in a file or on a console
    /// implements [`TraceSink`](lash_trace::TraceSink) itself.
    pub fn trace_sink(mut self, trace_sink: Arc<dyn lash_trace::TraceSink>) -> Self {
        self.trace_sinks.push(trace_sink);
        self
    }

    /// Select what lash builds for each span kind, for every sink and the
    /// exporter. Defaults to
    /// [`TraceDetailPolicy::standard`](crate::tracing::TraceDetailPolicy::standard).
    /// Building refuses, with [`crate::EmbedError::TracePolicy`], trace limits that
    /// turn extension spans off (`extension_spans = 0`) while this policy
    /// still builds model calls or their attempts.
    pub fn trace_detail(mut self, detail: crate::tracing::TraceDetailPolicy) -> Self {
        self.trace_detail = Some(detail);
        self
    }

    /// Select the tracing seam's admission and buffering bounds. Defaults to
    /// [`TraceLimits::standard`](crate::tracing::TraceLimits::standard). No
    /// sink is installed.
    pub fn trace_limits(mut self, limits: crate::tracing::TraceLimits) -> Self {
        self.trace_limits = Some(limits);
        self
    }

    /// Select the bounds on the execution evidence lash records: tool
    /// attempt streams, effect diffs, failed generations and process effect
    /// occurrences. Defaults to [`EvidenceLimits::standard`](lash_core_store::execution_limits::EvidenceLimits::standard).
    pub fn evidence_limits(
        mut self,
        limits: lash_core_store::execution_limits::EvidenceLimits,
    ) -> Self {
        self.evidence_limits = Some(limits);
        self
    }

    /// Select how much of a diagnostic lash keeps and when a plugin's state
    /// size warns. Defaults to [`DiagnosticLimits::standard`](lash_core_store::execution_limits::DiagnosticLimits::standard).
    pub fn diagnostic_limits(
        mut self,
        limits: lash_core_store::execution_limits::DiagnosticLimits,
    ) -> Self {
        self.diagnostic_limits = Some(limits);
        self
    }

    /// Select how much tool-composition history the runtime remembers.
    /// Defaults to [`CompositionCacheLimits::standard`](lash_core_store::execution_limits::CompositionCacheLimits::standard).
    pub fn composition_cache_limits(
        mut self,
        limits: lash_core_store::execution_limits::CompositionCacheLimits,
    ) -> Self {
        self.composition_cache_limits = Some(limits);
        self
    }
}
