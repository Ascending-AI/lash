//! Live provider-attempt control. Nothing here is stored or digested.

use crate::sync::MutexExt as _;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use super::types::{LlmResponse, LlmStreamEvent};

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
    waiters: Vec<Waker>,
    partial: Option<Box<LlmResponse>>,
    reported_usage: bool,
}

/// The live attempt's cancellation signal and response observed so far.
/// Providers can await `cancelled()` and return their partial response;
/// Lash retains observations even if a provider does not cooperate.
#[derive(Clone, Debug, Default)]
pub struct ProviderAttemptControl(Arc<Mutex<State>>);

impl ProviderAttemptControl {
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
            _ => {}
        }
    }

    pub fn partial_response(&self) -> Option<Box<LlmResponse>> {
        self.state().partial.clone()
    }
}
