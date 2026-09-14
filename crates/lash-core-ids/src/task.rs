//! Tokio task helpers that preserve the caller's tracing context.

use std::future::Future;

use tracing::Instrument as _;

/// Spawn a Tokio task as a child of the current tracing span.
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
