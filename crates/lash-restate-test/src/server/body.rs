//! The request body of one invocation attempt: the runtime's half of the
//! stream, fed by the server and observed for input starvation.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame};
use tokio::sync::mpsc;

/// What the server knows about an attempt's appetite for input.
///
/// The SDK reads its request body only from an await that the journal cannot
/// resolve, so "polled with nothing buffered" is exactly "blocked on the
/// runtime" — the condition under which a real server's inactivity timeout
/// runs, and the one the double's quiescence check waits for.
///
/// Starvation alone is not idleness: the SDK blocks on its input before the
/// server has read the output it just wrote, so the attempt task also
/// reports whether its last read of the response body came up empty.
#[derive(Debug, Default)]
pub struct InputProbe {
    starved: AtomicBool,
    response_drained: AtomicBool,
}

impl InputProbe {
    pub fn is_starved(&self) -> bool {
        self.starved.load(Ordering::SeqCst)
    }

    /// Starved, with every frame it wrote applied: nothing moves until the
    /// server feeds it.
    pub fn is_idle(&self) -> bool {
        self.is_starved() && self.response_drained.load(Ordering::SeqCst)
    }

    fn set(&self, starved: bool) {
        self.starved.store(starved, Ordering::SeqCst);
    }

    /// The server just queued input for the attempt: it is not starved
    /// until it has read that input and blocked again.
    pub fn fed(&self) {
        self.set(false);
    }

    /// Whether the attempt task's last poll of the response body found
    /// nothing to apply.
    pub fn set_response_drained(&self, drained: bool) {
        self.response_drained.store(drained, Ordering::SeqCst);
    }
}

/// The attempt's request body: frames the server pushes, then end of input
/// once the server closes its sender.
pub struct AttemptBody {
    receiver: mpsc::UnboundedReceiver<Bytes>,
    probe: Arc<InputProbe>,
    on_starved: Arc<dyn Fn() + Send + Sync>,
}

impl AttemptBody {
    pub fn new(
        receiver: mpsc::UnboundedReceiver<Bytes>,
        probe: Arc<InputProbe>,
        on_starved: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            receiver,
            probe,
            on_starved,
        }
    }
}

impl Body for AttemptBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(bytes)) => {
                self.probe.set(false);
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(None) => {
                self.probe.set(false);
                Poll::Ready(None)
            }
            Poll::Pending => {
                if !self.probe.is_starved() {
                    self.probe.set(true);
                    // Input fed between the empty read and the flag is not
                    // starvation.
                    if !self.receiver.is_empty() {
                        self.probe.set(false);
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    (self.on_starved)();
                }
                Poll::Pending
            }
        }
    }
}
