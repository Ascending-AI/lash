//! Live provider-attempt control. Nothing here is stored or digested.

use crate::sync::MutexExt as _;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use super::types::{LlmResponse, LlmStreamEvent, StreamBlockEvent};

/// Why Lash asks an attempt to stop, or it ended without returning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptEndReason {
    ModelDeadline,
    ImmediateCancel,
    TailDrainDrop,
    SupersededRetry,
    /// The contained send unwound before returning a result.
    ProviderPanicked,
    /// The stream consumer or its assistant-stream hook failed.
    StreamConsumerFailed,
}

#[derive(Debug, Default)]
struct State {
    reason: Option<AttemptEndReason>,
    time_source: Option<AttemptTimeSource>,
    transport_started: Option<std::time::Instant>,
    stream_evidence: Option<Box<super::types::ModelStreamEvidence>>,
    waiters: Vec<Waker>,
    partial: Option<Box<LlmResponse>>,
    reported_usage: bool,
    /// Output streamed, whether or not its content is retained in `partial`.
    output_started: bool,
}

#[derive(Clone)]
struct AttemptTimeSource(Arc<dyn Fn() -> std::time::Instant + Send + Sync>);

impl std::fmt::Debug for AttemptTimeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttemptTimeSource")
    }
}

/// The live attempt's cancellation signal and response observed so far.
/// Providers can await `cancelled()` and return their partial response;
/// Lash retains observations even if a provider does not cooperate.
#[derive(Clone, Debug, Default)]
pub struct ProviderAttemptControl(Arc<Mutex<State>>);

impl ProviderAttemptControl {
    /// Use the caller's monotonic clock for transport observations.
    pub fn set_time_source(&self, now: Arc<dyn Fn() -> std::time::Instant + Send + Sync>) {
        self.state().time_source = Some(AttemptTimeSource(now));
    }

    fn now(&self) -> std::time::Instant {
        let source = self.state().time_source.clone();
        source.map_or_else(std::time::Instant::now, |source| (source.0)())
    }

    pub fn begin_transport(&self) {
        let now = self.now();
        let mut state = self.state();
        state.transport_started = Some(now);
        state.stream_evidence = Some(Box::default());
    }

    pub fn observe_response_headers(&self, status: u16) {
        let now = self.now();
        let mut state = self.state();
        let elapsed = state
            .transport_started
            .map(|start| millis(now.saturating_duration_since(start)));
        if let Some(evidence) = state.stream_evidence.as_mut() {
            evidence.http_status = Some(status);
            evidence.response_headers_ms = elapsed;
        }
    }

    pub fn observe_body_chunk(&self, bytes: usize) {
        let now = self.now();
        let mut state = self.state();
        let elapsed = state
            .transport_started
            .map(|start| millis(now.saturating_duration_since(start)));
        if let Some(evidence) = state.stream_evidence.as_mut() {
            evidence.bytes_received = Some(
                evidence
                    .bytes_received
                    .unwrap_or(0)
                    .saturating_add(bytes as u64),
            );
            if bytes > 0 && evidence.first_body_chunk_ms.is_none() {
                evidence.first_body_chunk_ms = elapsed;
            }
        }
    }

    /// Record a decoded wire event even when no application delta is emitted.
    pub fn observe_output_kinds(
        &self,
        kinds: impl IntoIterator<Item = super::types::PartialOutputKind>,
    ) {
        let mut state = self.state();
        if let Some(evidence) = state.stream_evidence.as_mut() {
            let mut kinds = kinds.into_iter().peekable();
            if kinds.peek().is_none() {
                evidence
                    .partial_output
                    .insert(super::types::PartialOutputKind::OtherEvent);
            }
            evidence.partial_output.extend(kinds);
        }
    }

    pub fn stream_evidence(&self) -> Option<Box<super::types::ModelStreamEvidence>> {
        self.state().stream_evidence.clone()
    }

    fn empty_partial() -> Box<LlmResponse> {
        Box::new(LlmResponse {
            usage: super::types::LlmUsage::unknown_call(),
            ..Default::default()
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock_recover()
    }

    /// First stop reason wins. Wake outside the mutex so a wake can poll.
    pub fn cancel(&self, reason: AttemptEndReason) {
        let waiters = {
            let mut state = self.state();
            if state.reason.is_some() {
                return;
            }
            state.reason = Some(reason);
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    pub fn reason(&self) -> Option<AttemptEndReason> {
        self.state().reason
    }

    pub fn cancelled(&self) -> impl std::future::Future<Output = AttemptEndReason> + '_ {
        std::future::poll_fn(|cx| {
            let mut state = self.state();
            if let Some(reason) = state.reason {
                return Poll::Ready(reason);
            }
            if !state
                .waiters
                .iter()
                .any(|waker| waker.will_wake(cx.waker()))
            {
                state.waiters.push(cx.waker().clone());
            }
            Poll::Pending
        })
    }

    /// Retain precisely the allowlisted metadata captured by the transport.
    pub fn capture_metadata(
        &self,
        metadata: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.state()
            .partial
            .get_or_insert_with(Self::empty_partial)
            .response_metadata
            .extend(metadata);
    }

    /// Retain stream evidence before downstream observers can preempt the call.
    pub fn observe(&self, event: &LlmStreamEvent) {
        let mut state = self.state();
        match event {
            LlmStreamEvent::Block(StreamBlockEvent::Delta { text, .. }) => {
                state.output_started |= !text.is_empty();
            }
            LlmStreamEvent::Block(_) | LlmStreamEvent::Part(_) => state.output_started = true,
            LlmStreamEvent::Usage(usage) => {
                state.reported_usage = true;
                state.partial.get_or_insert_with(Self::empty_partial).usage = usage.clone();
            }
            LlmStreamEvent::Evidence(evidence) => {
                let raw_usage_received = !state.reported_usage
                    && evidence
                        .provider_usage
                        .as_ref()
                        .is_some_and(super::types::provider_usage_has_quantities);
                let partial = state.partial.get_or_insert_with(Self::empty_partial);
                partial
                    .response_metadata
                    .extend(evidence.response_metadata.clone());
                if evidence.provider_usage.is_some() {
                    partial.provider_usage = evidence.provider_usage.clone();
                    // Providers need not emit a Usage delta for explicit zero:
                    // its counters did not change, but usage still arrived.
                    if raw_usage_received {
                        partial.usage = Default::default();
                    }
                }
                // The normal response decoder reports conflicting identities.
                // Retention never changes the decoder's failure order.
                let _ = super::types::ExecutionEvidence::merge_optional(
                    &mut partial.execution_evidence,
                    evidence.execution_evidence.clone(),
                );
            }
            LlmStreamEvent::AttemptReset | LlmStreamEvent::RetryStatus(_) => {}
        }
    }

    /// Whether the attempt streamed any output block or part.
    pub fn output_started(&self) -> bool {
        self.state().output_started
    }

    pub fn partial_response(&self) -> Option<Box<LlmResponse>> {
        self.state().partial.clone()
    }
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
