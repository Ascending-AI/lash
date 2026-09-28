//! Replay-keyed observation sinks and cursors.

use std::sync::Arc;

use super::context::DriveObservation;

/// Where a step body publishes observations.
pub trait ObservationSink: Send + Sync {
    fn observe(&self, observation: DriveObservation);
}

/// An [`ObservationSink`] that drops every observation, for drive paths that
/// run without a host stream.
pub struct NullObservationSink;

impl NullObservationSink {
    /// A shared null sink.
    pub fn arc() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl ObservationSink for NullObservationSink {
    fn observe(&self, _observation: DriveObservation) {}
}

/// An [`ObservationSink`] that forwards to `inner` while `open` holds, then
/// drops everything. A live-opener registration wraps the shared drive sink
/// in this so a child tool keeps publishing while the entry is live and stops
/// the moment the caller's turn removes it — the same gate the forwarder
/// task's `select` imposed, without a spawned forwarding task.
pub struct GatedObservationSink<F> {
    open: F,
    inner: Arc<dyn ObservationSink>,
}

impl<F> GatedObservationSink<F>
where
    F: Fn() -> bool + Send + Sync,
{
    pub fn new(open: F, inner: Arc<dyn ObservationSink>) -> Arc<Self> {
        Arc::new(Self { open, inner })
    }
}

impl<F> ObservationSink for GatedObservationSink<F>
where
    F: Fn() -> bool + Send + Sync,
{
    fn observe(&self, observation: DriveObservation) {
        if (self.open)() {
            self.inner.observe(observation);
        }
    }
}

/// The drive-side emitter a step body publishes through: one cursor per
/// effect body or drive step, keyed by that body's replay key, minting the
/// ordinal sequence itself (ADR 0105 §1).
///
/// A cursor is owned locally — there is no shared counter. The
/// `(key, ordinal)` pairs it mints are *stable identities*: a replay or a
/// re-executed live body (a retried `LlmCall`, a re-attempted group child)
/// re-derives the same ids, and a sink MAY deduplicate on them. Nothing
/// deduplicates today — a retried body may re-emit different content under
/// the same ids — and whether a sink dedupes is FIG-3753's call.
#[derive(Debug, Clone)]
pub struct ObservationCursor {
    key: super::context::ReplayKey,
    next: u32,
}

impl ObservationCursor {
    pub fn new(key: super::context::ReplayKey) -> Self {
        Self { key, next: 0 }
    }

    /// Publish `event` under this cursor's key at the next ordinal, then
    /// advance. Synchronous and non-waking; what the sink does with an
    /// observation is never a decision input.
    pub fn observe(&mut self, sink: &dyn ObservationSink, event: super::context::ObservedEvent) {
        sink.observe(DriveObservation {
            key: self.key.clone(),
            ordinal: self.next,
            event,
        });
        self.next += 1;
    }
}
