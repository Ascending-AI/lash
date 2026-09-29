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
                    // starvation: hand the frame up now rather than
                    // self-waking for a repoll. A synchronous wake +
                    // Pending is the SDK's terminal-trap shape, and an
                    // observer above this stream (the run-future guard in
                    // lash-restate) must be able to fuse on it without
                    // stranding a live wait beside an unread frame.
                    if !self.receiver.is_empty() {
                        self.probe.set(false);
                        if let Poll::Ready(Some(bytes)) = self.receiver.poll_recv(cx) {
                            return Poll::Ready(Some(Ok(Frame::data(bytes))));
                        }
                    } else {
                        (self.on_starved)();
                    }
                }
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    struct CountingWake {
        wakes: AtomicUsize,
    }

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A `Pending` poll never carries a synchronous wake: that pair is the
    /// SDK's terminal-trap shape, and the run-future guard in lash-restate
    /// fuses the whole wait on it. Input that races an empty read is handed
    /// back as `Ready`, never self-woken.
    #[test]
    fn pending_poll_frame_never_wakes_synchronously() {
        let (_tx, rx) = mpsc::unbounded_channel::<Bytes>();
        let probe = Arc::new(InputProbe::default());
        let starved = Arc::new(AtomicUsize::new(0));
        let mut body = AttemptBody::new(rx, probe, {
            let starved = Arc::clone(&starved);
            Arc::new(move || {
                starved.fetch_add(1, Ordering::SeqCst);
            })
        });
        let mut body = Pin::new(&mut body);

        let count = Arc::new(CountingWake {
            wakes: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);

        assert!(body.as_mut().poll_frame(&mut cx).is_pending());
        assert_eq!(count.wakes.load(Ordering::SeqCst), 0);
        assert_eq!(starved.load(Ordering::SeqCst), 1);

        // Starved stays latched: a second empty poll neither wakes nor
        // reports starvation again.
        assert!(body.as_mut().poll_frame(&mut cx).is_pending());
        assert_eq!(count.wakes.load(Ordering::SeqCst), 0);
        assert_eq!(starved.load(Ordering::SeqCst), 1);
    }
}
