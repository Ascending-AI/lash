//! Tokio task helpers that preserve the caller's tracing context.

use std::future::Future;

use tracing::Instrument as _;

/// The handle types a [`spawn`]ed task is driven through, re-exported so the
/// drive names this guarded spawn's task surface rather than `tokio::task`.
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
    tokio::spawn(future.instrument(tracing::Span::current()))
}
