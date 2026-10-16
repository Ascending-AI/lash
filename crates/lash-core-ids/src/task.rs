//! Tokio task helpers that preserve the caller's tracing context and its
//! trace position: a task spawned while an extension body's position is
//! entered polls under that position too.

use std::future::Future;

use tracing::Instrument as _;

/// Opt-in, process-window receipts for concrete futures before type erasure.
#[cfg(feature = "perf-witness")]
pub mod sizes;

/// The handle types a [`spawn`]ed task is executed through, re-exported so the
/// shift names this guarded spawn's task surface rather than `tokio::task`.
pub use tokio::task::{AbortHandle, JoinError, JoinHandle};

#[allow(
    clippy::disallowed_methods,
    reason = "this is the single guarded entry point for Tokio task spawning"
)]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "perf-witness")]
    sizes::record("task.spawn", &future);
    // Lash's frame is the innermost wrapper: it enters after the `tracing`
    // span, so the host's current context during each poll is the position.
    match lash_trace::AmbientFrame::current() {
        Some(frame) => tokio::spawn(
            lash_trace::Framed::new(future, frame).instrument(tracing::Span::current()),
        ),
        None => tokio::spawn(future.instrument(tracing::Span::current())),
    }
}
