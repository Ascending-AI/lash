//! The host's tracer provider, wrapped so lash spans keep lash's ids and
//! sampling decisions.
//!
//! SDK 0.33 draws a new span's id from the provider's id generator and then
//! asks the provider's sampler; a span builder has no id field. The wrapper
//! therefore installs two private pieces on the host's builder:
//! [`PresetIdGenerator`], which hands out a thread-local preset for one
//! synchronous span start and draws from the host's fallback otherwise, and
//! [`LashSampler`], which returns the preset decision for a lash span and
//! asks the host's sampler for every other span.

use std::cell::RefCell;
use std::sync::Arc;

use opentelemetry::trace::{Link, SpanId, SpanKind, TraceId, TraceState};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{
    IdGenerator, SamplingDecision, SamplingResult, SdkTracerProvider, ShouldSample,
    TracerProviderBuilder,
};

/// The host's [`SdkTracerProvider`], built with lash's id generator and
/// sampler wrapper around the host's own choices.
///
/// [`OtelTelemetry::new`](super::OtelTelemetry::new) takes this type, so a
/// provider lash exports through always presets lash's span ids and
/// decisions. The host keeps flush and shutdown.
#[derive(Clone, Debug)]
pub struct LashTracerProvider {
    provider: SdkTracerProvider,
    sampler: Box<dyn ShouldSample>,
    id_fallback: Arc<dyn IdGenerator>,
}

impl LashTracerProvider {
    /// Installs lash's id generator over `id_fallback` and lash's sampler
    /// over `sampler` on `builder`, and builds it. Both are required: lash
    /// has no default sampler and no default id generator. A sampler or id
    /// generator set on `builder` earlier is **replaced**: pass the host's
    /// choices here instead. The SDK's own defaults are
    /// `Sampler::ParentBased(Box::new(Sampler::AlwaysOn))` and
    /// `RandomIdGenerator::default()`.
    ///
    /// `sampler` decides at producers (the trace's one decision, which every
    /// lash span below follows) and for every span lash did not preset.
    /// `RecordOnly` and `Drop` both make a producer's trace unsampled.
    /// `id_fallback` draws producer, leaf and every non-lash span id.
    pub fn new(
        builder: TracerProviderBuilder,
        sampler: impl ShouldSample + 'static,
        id_fallback: impl IdGenerator + 'static,
    ) -> Self {
        let sampler: Box<dyn ShouldSample> = Box::new(sampler);
        let id_fallback: Arc<dyn IdGenerator> = Arc::new(id_fallback);
        let provider = builder
            .with_sampler(LashSampler {
                host: sampler.clone(),
            })
            .with_id_generator(PresetIdGenerator {
                fallback: Arc::clone(&id_fallback),
            })
            .build();
        Self {
            provider,
            sampler,
            id_fallback,
        }
    }

    pub fn provider(&self) -> &SdkTracerProvider {
        &self.provider
    }

    pub(super) fn sampler(&self) -> &dyn ShouldSample {
        self.sampler.as_ref()
    }

    pub(super) fn id_fallback(&self) -> &Arc<dyn IdGenerator> {
        &self.id_fallback
    }
}

impl std::ops::Deref for LashTracerProvider {
    type Target = SdkTracerProvider;

    fn deref(&self) -> &SdkTracerProvider {
        &self.provider
    }
}

/// The ids and decision of the one lash span being started on this thread.
struct Preset {
    span: Option<SpanId>,
    /// The trace id of a span with no parent: the SDK draws one only then.
    trace: Option<TraceId>,
    /// Lash presets only sampled spans, so its one decision samples.
    decision: Option<TraceState>,
}

thread_local! {
    static PRESET: RefCell<Option<Preset>> = const { RefCell::new(None) };
}

/// Presets the next span this thread starts: its id, its trace id when it
/// has no parent, and a sampled decision under `trace_state`. Dropping the
/// guard clears whatever the start did not take.
pub(super) struct PresetGuard(());

impl PresetGuard {
    pub(super) fn set(span: SpanId, trace: Option<TraceId>, trace_state: TraceState) -> Self {
        PRESET.with(|preset| {
            *preset.borrow_mut() = Some(Preset {
                span: Some(span),
                trace,
                decision: Some(trace_state),
            });
        });
        Self(())
    }
}

impl Drop for PresetGuard {
    fn drop(&mut self) {
        PRESET.with(|preset| preset.borrow_mut().take());
    }
}

fn take<T>(field: impl FnOnce(&mut Preset) -> Option<T>) -> Option<T> {
    PRESET.with(|preset| preset.borrow_mut().as_mut().and_then(field))
}

/// Draws the preset span id once, then the host's fallback: a span a
/// `SpanProcessor::on_start` opens inside the same start draws a fresh id.
#[derive(Debug)]
struct PresetIdGenerator {
    fallback: Arc<dyn IdGenerator>,
}

impl IdGenerator for PresetIdGenerator {
    fn new_trace_id(&self) -> TraceId {
        take(|preset| preset.trace.take()).unwrap_or_else(|| self.fallback.new_trace_id())
    }

    fn new_span_id(&self) -> SpanId {
        take(|preset| preset.span.take()).unwrap_or_else(|| self.fallback.new_span_id())
    }
}

/// Returns the preset decision for a lash span, and the host's sampler's
/// for every other span.
#[derive(Clone, Debug)]
struct LashSampler {
    host: Box<dyn ShouldSample>,
}

impl ShouldSample for LashSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        if let Some(trace_state) = take(|preset| preset.decision.take()) {
            return SamplingResult {
                decision: SamplingDecision::RecordAndSample,
                attributes: Vec::new(),
                trace_state,
            };
        }
        self.host
            .should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}
