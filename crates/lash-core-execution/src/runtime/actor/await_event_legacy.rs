//! The await-event methods the context still answers for its callers'
//! ports (L5, FIG-5173). Each is addressed by the retired [`AwaitEventKey`]:
//! a scope and a wait identity anyone can recompute, which no wait row can
//! serve without keeping that forgeable addressing alive. They are deleted
//! with their callers' ports: the plugin task cancel signal to
//! session mail, process signals to L6's mail. (Tool completion keys are
//! L5 wait rows a round pins: L4, FIG-5174.) Until then each reaches the
//! stub of the lane that owns its wait identity.
//!
//! [`AwaitEventKey`]: crate::AwaitEventKey

use tokio_util::sync::CancellationToken;

use super::ActorContext;
use crate::AwaitEventWaitIdentity;

/// The stub of the lane that ports `wait`'s callers.
pub(super) fn port_pending(wait: &AwaitEventWaitIdentity) -> ! {
    match wait {
        AwaitEventWaitIdentity::SessionCommandCancelSignal => {
            todo!("L3 (FIG-5172): delete with the plugin task cancel signal's port to session mail")
        }
        AwaitEventWaitIdentity::ProcessSignal { .. } => todo!(
            "L6 (FIG-5175): delete with the process signals' port to process mail and L5's pin and race"
        ),
    }
}

impl ActorContext {
    /// The await-event key of a wait.
    ///
    /// # Errors
    ///
    /// The key's refusal.
    pub async fn await_event_key(
        &self,
        _scope: &crate::ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        port_pending(&wait)
    }

    /// Resolve a wait by its key.
    ///
    /// # Errors
    ///
    /// The resolution's refusal.
    pub async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        _resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        port_pending(&key.wait)
    }

    /// Publish a resolution of a wait by its key.
    ///
    /// # Errors
    ///
    /// The resolution's refusal.
    pub async fn publish_await_event(
        &self,
        key: &crate::AwaitEventKey,
        _resolution: crate::Resolution,
    ) -> Result<Option<crate::ResolveOutcome>, crate::RuntimeError> {
        port_pending(&key.wait)
    }

    /// Read a wait's resolution without waiting.
    ///
    /// # Errors
    ///
    /// The read's refusal.
    pub async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        port_pending(&key.wait)
    }

    /// Wait for a wait's resolution.
    ///
    /// # Errors
    ///
    /// The wait's refusal.
    pub async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        _cancel: CancellationToken,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        port_pending(&key.wait)
    }
}
