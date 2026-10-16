//! Runs futures under a position (§7.7).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// The owned future adapter [`ExtensionTrace::instrument`] returns: it
/// enters the position's ambient frame for each poll, so third-party
/// OpenTelemetry and `tracing` instrumentation in the future nests under
/// the position, and so does a model call it makes.
///
/// [`ExtensionTrace::instrument`]: super::ExtensionTrace::instrument
#[must_use = "futures do nothing unless polled"]
pub struct TraceInstrumented<F> {
    inner: Pin<Box<F>>,
    frame: Option<Arc<lash_trace::AmbientFrame>>,
}

impl<F: Future> TraceInstrumented<F> {
    pub(super) fn new(inner: F, frame: Option<Arc<lash_trace::AmbientFrame>>) -> Self {
        Self {
            inner: Box::pin(inner),
            frame,
        }
    }
}

impl<F: Future> Future for TraceInstrumented<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _entered = self.frame.as_ref().map(|frame| frame.enter());
        self.inner.as_mut().poll(cx)
    }
}

impl<F> std::fmt::Debug for TraceInstrumented<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceInstrumented")
            .field("framed", &self.frame.is_some())
            .finish()
    }
}
