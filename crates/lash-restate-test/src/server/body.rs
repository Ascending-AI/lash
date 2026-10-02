//! The request body of one invocation attempt: the runtime's half of the
//! stream, fed by the server and observed for input starvation.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame};
use tokio::sync::mpsc;

/// What the server knows about an attempt's appetite for input.
///
/// The SDK reads its request body only from an await that the journal cannot
/// resolve, so "polled with nothing left to read" is exactly "blocked on the
/// runtime" — the condition under which a real server's inactivity timeout
/// runs, and the one the double's quiescence check waits for.
///
/// The server and the attempt write the probe from different threads, so
/// each owns its own marks and neither can erase the other's: the server
/// counts the frames it queues, the attempt counts the frames it reads and
/// says whether its last read came up empty. A park that lands between the
/// server's count and the frame's queuing therefore still shows an unread
/// frame (FIG-4778).
///
/// Starvation alone is not idleness: the SDK blocks on its input before the
/// server has read the output it just wrote, so the attempt task also
/// reports whether its last read of the response body came up empty.
#[derive(Debug, Default)]
pub struct InputProbe {
    /// Frames the server has queued, counted before each is queued.
    fed: AtomicU64,
    /// Frames the attempt has read.
    read: AtomicU64,
    /// Whether the attempt's last read of its input came up empty.
    parked: AtomicBool,
    response_drained: AtomicBool,
}

impl InputProbe {
    /// Parked on its input with every queued frame read.
    pub fn is_starved(&self) -> bool {
        // The counts before the park: an attempt clears its park before it
        // counts a read, so a reader that sees the last frame read never
        // pairs that count with the park the frame ended.
        self.read.load(Ordering::SeqCst) == self.fed.load(Ordering::SeqCst)
            && self.parked.load(Ordering::SeqCst)
    }

    /// Starved, with every frame it wrote applied: nothing moves until the
    /// server feeds it.
    pub fn is_idle(&self) -> bool {
        self.is_starved() && self.response_drained.load(Ordering::SeqCst)
    }

    /// The server is about to queue a frame for the attempt: it is not
    /// starved until it has read that frame and blocked again.
    pub fn fed(&self) {
        self.fed.fetch_add(1, Ordering::SeqCst);
    }

    /// The attempt read a frame: no longer parked, then one more read (in
    /// that order, see [`is_starved`](Self::is_starved)).
    fn read_frame(&self) {
        self.parked.store(false, Ordering::SeqCst);
        self.read.fetch_add(1, Ordering::SeqCst);
    }

    fn set_parked(&self, parked: bool) {
        self.parked.store(parked, Ordering::SeqCst);
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
                self.probe.read_frame();
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(None) => {
                self.probe.set_parked(false);
                Poll::Ready(None)
            }
            Poll::Pending => {
                if !self.probe.parked.load(Ordering::SeqCst) {
                    self.probe.set_parked(true);
                    // Input fed between the empty read and the park is not
                    // starvation: hand the frame up now rather than
                    // self-waking for a repoll. A synchronous wake +
                    // Pending is the SDK's terminal-trap shape, and an
                    // observer above this stream (the run-future guard in
                    // lash-restate) must be able to fuse on it without
                    // stranding a live wait beside an unread frame.
                    if !self.receiver.is_empty() {
                        self.probe.set_parked(false);
                        if let Poll::Ready(Some(bytes)) = self.receiver.poll_recv(cx) {
                            self.probe.read_frame();
                            return Poll::Ready(Some(Ok(Frame::data(bytes))));
                        }
                    } else if self.probe.is_starved() {
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

    /// A frame the server marked fed is never read as starvation before the
    /// attempt reads it, even when the attempt's empty read and park land
    /// between the server's mark and the frame's queuing, as they can on
    /// another worker. A park that overwrote the mark made an attempt with an
    /// unread frame look blocked on the server, so a settle returned while
    /// the attempt still had its answer to read (FIG-4778).
    #[test]
    fn a_frame_fed_as_the_attempt_parks_is_not_starvation() {
        let (input, rx) = mpsc::unbounded_channel::<Bytes>();
        let probe = Arc::new(InputProbe::default());
        let starved = Arc::new(AtomicUsize::new(0));
        let mut body = AttemptBody::new(rx, Arc::clone(&probe), {
            let starved = Arc::clone(&starved);
            Arc::new(move || {
                starved.fetch_add(1, Ordering::SeqCst);
            })
        });
        let mut body = Pin::new(&mut body);
        let mut cx = Context::from_waker(Waker::noop());

        // The server's push marks the attempt fed first; the attempt finds
        // its input still empty and parks; only then is the frame queued.
        probe.fed();
        assert!(body.as_mut().poll_frame(&mut cx).is_pending());
        input
            .send(Bytes::from_static(b"frame"))
            .expect("the body holds the receiver");

        assert!(!probe.is_starved(), "the fed frame is still unread");
        assert_eq!(starved.load(Ordering::SeqCst), 0);

        // Once the attempt has read the frame and found nothing more, it is
        // starved, and says so once.
        assert!(body.as_mut().poll_frame(&mut cx).is_ready());
        assert!(!probe.is_starved());
        assert!(body.as_mut().poll_frame(&mut cx).is_pending());
        assert!(probe.is_starved());
        assert_eq!(starved.load(Ordering::SeqCst), 1);
    }
}
