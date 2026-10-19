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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash_trace::{AmbientFrame, Framed};

    use crate::cancel_sweep::sweep_drops;

    fn is_current(expected: &Arc<AmbientFrame>) -> bool {
        AmbientFrame::current().is_some_and(|current| Arc::ptr_eq(&current, expected))
    }

    fn frame() -> Arc<AmbientFrame> {
        AmbientFrame::new(None, Arc::new(()))
    }

    /// Ownership at the claim (FIG-6858): a task's frame is installed for one
    /// poll at a time and restored by the poll that installed it, so a framed
    /// task dropped at any await, with a framed child pending inside it,
    /// leaves the caller's frame installed, never its own or its child's.
    #[tokio::test]
    async fn a_framed_task_dropped_at_each_await_leaves_the_prior_frame_installed() {
        let prior = frame();
        let _prior = prior.enter();
        sweep_drops(
            async || {
                let (task, child) = (frame(), frame());
                let polled = Arc::clone(&task);
                let task = Framed::new(
                    async move {
                        tokio::task::yield_now().await;
                        assert!(is_current(&polled), "the task polls under its frame");
                        let entered = Arc::clone(&child);
                        Framed::new(
                            async move {
                                tokio::task::yield_now().await;
                                assert!(is_current(&entered), "the child polls under its frame");
                            },
                            child,
                        )
                        .await;
                        assert!(
                            is_current(&polled),
                            "the child's exit restores the task's frame"
                        );
                    },
                    task,
                );
                ((), task)
            },
            async |(), cut| {
                assert!(
                    is_current(&prior),
                    "the caller's frame is installed once the task is {cut}"
                );
            },
        )
        .await;
    }
}
