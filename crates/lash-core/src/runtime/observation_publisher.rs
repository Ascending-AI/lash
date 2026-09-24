use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use futures_util::task::AtomicWaker;
use tokio::sync::mpsc;

/// How many observations one task poll publishes at most before it yields, so
/// a host sink that is always ready, fed by a publisher that never pauses,
/// cannot keep the task from returning and the drive from being polled.
const PUBLISH_BUDGET: usize = 32;

/// Where [`drive_with_observations`] takes its observations from.
pub trait ObservationSource {
    type Item;

    /// The next observation, `Ready(None)` once the source is closed and
    /// empty, or `Pending` with `context`'s waker registered.
    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<Self::Item>>;

    /// The observation last returned has been published to the host.
    fn published_one(&mut self) {}

    /// The drive is over: take nothing more, but keep what is queued.
    fn close(&mut self);
}

impl<T> ObservationSource for mpsc::UnboundedReceiver<T> {
    type Item = T;

    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<T>> {
        self.poll_recv(context)
    }

    fn close(&mut self) {
        mpsc::UnboundedReceiver::close(self);
    }
}

/// Drive `future` to completion while `publish` delivers `observations` to the
/// host, outside the drive (ADR 0105 §1: observation never decides).
///
/// The drive and the publication keep separate wakers. The drive is polled
/// only when its own waker fired, and at most once per task poll (FIG-790):
/// durable substrates signal a terminal attempt state, a genuine suspension
/// included, by waking synchronously and returning `Pending`, and rely on
/// their handler wrapper consuming that state on the next poll, so re-polling
/// the drive inside one task poll would re-enter an already-completed
/// combinator. A publication that is slow, stalls or wakes often never polls
/// the drive, and the drive never waits on one. One task poll publishes at
/// most [`PUBLISH_BUDGET`] observations, then yields.
///
/// When the drive completes, the source is closed: the observations already
/// queued are published, anything a stray publisher sends later is dropped,
/// and the drive's output is returned.
pub async fn drive_with_observations<F, S, P, Fut>(
    future: Pin<&mut F>,
    observations: &mut S,
    publish: P,
) -> F::Output
where
    F: Future + ?Sized,
    S: ObservationSource + ?Sized,
    P: FnMut(S::Item) -> Fut,
    Fut: Future<Output = ()>,
{
    ObservedDrive {
        drive: future,
        output: None,
        drive_wake: Arc::new(SideWake::woken()),
        observations,
        publish,
        publishing: None,
        published_all: false,
        publish_wake: Arc::new(SideWake::woken()),
    }
    .await
}

/// One side of an [`ObservedDrive`]: it remembers that its own future was
/// woken, and wakes the task.
struct SideWake {
    woken: AtomicBool,
    task: AtomicWaker,
}

impl SideWake {
    fn woken() -> Self {
        Self {
            woken: AtomicBool::new(true),
            task: AtomicWaker::new(),
        }
    }

    fn take(&self) -> bool {
        self.woken.swap(false, Ordering::AcqRel)
    }
}

impl Wake for SideWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.task.wake();
    }
}

struct ObservedDrive<'d, 'o, F: Future + ?Sized, S: ?Sized, P, Fut> {
    drive: Pin<&'d mut F>,
    output: Option<F::Output>,
    drive_wake: Arc<SideWake>,
    observations: &'o mut S,
    publish: P,
    publishing: Option<Pin<Box<Fut>>>,
    published_all: bool,
    publish_wake: Arc<SideWake>,
}

// No field is structurally pinned: the drive is pinned by reference, the
// in-flight publication is boxed, and the output and publisher are only ever
// moved or called through `&mut`.
impl<F: Future + ?Sized, S: ?Sized, P, Fut> Unpin for ObservedDrive<'_, '_, F, S, P, Fut> {}

impl<F, S, P, Fut> ObservedDrive<'_, '_, F, S, P, Fut>
where
    F: Future + ?Sized,
    S: ObservationSource + ?Sized,
    P: FnMut(S::Item) -> Fut,
    Fut: Future<Output = ()>,
{
    /// Publish until the host end is pending, the closed source is empty, or
    /// this poll's budget is spent.
    fn poll_publication(&mut self) {
        let waker = Waker::from(Arc::clone(&self.publish_wake));
        let mut context = Context::from_waker(&waker);
        let mut budget = PUBLISH_BUDGET;
        loop {
            if let Some(publishing) = self.publishing.as_mut() {
                if publishing.as_mut().poll(&mut context).is_pending() {
                    return;
                }
                self.publishing = None;
                self.observations.published_one();
            }
            if budget == 0 {
                // Yield: the next task poll resumes publication after the
                // drive has had its turn.
                waker.wake_by_ref();
                return;
            }
            match self.observations.poll_next(&mut context) {
                Poll::Ready(Some(observation)) => {
                    budget -= 1;
                    self.publishing = Some(Box::pin((self.publish)(observation)));
                }
                Poll::Ready(None) => {
                    self.published_all = true;
                    return;
                }
                Poll::Pending => return,
            }
        }
    }
}

impl<F, S, P, Fut> Future for ObservedDrive<'_, '_, F, S, P, Fut>
where
    F: Future + ?Sized,
    S: ObservationSource + ?Sized,
    P: FnMut(S::Item) -> Fut,
    Fut: Future<Output = ()>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        this.drive_wake.task.register(context.waker());
        this.publish_wake.task.register(context.waker());
        if this.output.is_none() && this.drive_wake.take() {
            let waker = Waker::from(Arc::clone(&this.drive_wake));
            if let Poll::Ready(output) = this.drive.as_mut().poll(&mut Context::from_waker(&waker))
            {
                this.output = Some(output);
                // Nothing the drive publishes after this point is its own.
                this.observations.close();
                this.publish_wake.woken.store(true, Ordering::Release);
            }
        }
        if !this.published_all && this.publish_wake.take() {
            this.poll_publication();
        }
        if this.published_all
            && let Some(output) = this.output.take()
        {
            return Poll::Ready(output);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use super::drive_with_observations;

    struct PanicsWhenPolledAfterErrorWake {
        polled: bool,
    }

    impl Future for PanicsWhenPolledAfterErrorWake {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            assert!(
                !self.polled,
                "synchronously-woken pending future was re-polled in one task poll"
            );
            self.polled = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    #[test]
    fn synchronously_woken_pending_future_is_polled_once_per_task_poll() {
        let (observer, mut observations) = tokio::sync::mpsc::unbounded_channel();
        observer.send(()).expect("pre-queue an observation");
        let mut driven = Box::pin(PanicsWhenPolledAfterErrorWake { polled: false });
        let mut drive = Box::pin(drive_with_observations(
            driven.as_mut(),
            &mut observations,
            |_| async {},
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);

        assert_eq!(drive.as_mut().poll(&mut context), Poll::Pending);
    }

    /// Counts its polls and completes once its own waker has fired `wakes`
    /// times.
    struct CountsPolls {
        polls: Arc<AtomicUsize>,
        wakes_left: usize,
    }

    impl Future for CountsPolls {
        type Output = &'static str;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.wakes_left == 0 {
                return Poll::Ready("completed");
            }
            self.wakes_left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn a_waking_publication_never_polls_the_drive() {
        let polls = Arc::new(AtomicUsize::new(0));
        let mut driven = Box::pin(CountsPolls {
            polls: Arc::clone(&polls),
            wakes_left: 2,
        });
        let (observer, mut observations) = tokio::sync::mpsc::unbounded_channel();
        for observation in 0..64 {
            observer.send(observation).expect("queue an observation");
        }
        let published = Arc::new(AtomicUsize::new(0));
        let output = drive_with_observations(driven.as_mut(), &mut observations, |_| {
            let published = Arc::clone(&published);
            async move {
                // A host sink that is pending and wakes itself several times
                // for every observation.
                for _ in 0..3 {
                    tokio::task::yield_now().await;
                }
                published.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;

        assert_eq!(output, "completed");
        assert_eq!(
            polls.load(Ordering::SeqCst),
            3,
            "the drive is polled once per wake of its own, never for a publication"
        );
        assert_eq!(published.load(Ordering::SeqCst), 64);
        drop(observer);
    }

    #[tokio::test]
    async fn a_stalled_publication_never_holds_the_drive() {
        let (observer, mut observations) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let mut released = Some(released);
        let drive_finished = Arc::new(AtomicUsize::new(0));
        let mut driven = Box::pin({
            let drive_finished = Arc::clone(&drive_finished);
            async move {
                for observation in 1..=3 {
                    observer.send(observation).expect("publish");
                    tokio::task::yield_now().await;
                }
                drive_finished.store(1, Ordering::SeqCst);
                // The host sink is still stalled on the first observation.
                release.send(()).expect("release the stalled sink");
                "completed"
            }
        });
        let mut published = Vec::new();
        let output = drive_with_observations(driven.as_mut(), &mut observations, |observation| {
            let stall = released.take();
            published.push(observation);
            async move {
                if let Some(stall) = stall {
                    stall.await.expect("the drive releases the sink");
                }
            }
        })
        .await;

        assert_eq!(output, "completed");
        assert_eq!(drive_finished.load(Ordering::SeqCst), 1);
        assert_eq!(published, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn publishes_what_was_queued_and_drops_later_sends() {
        let (observer, mut observations) = tokio::sync::mpsc::unbounded_channel();
        let stray = observer.clone();
        let mut driven = Box::pin(async move {
            for observation in 1..=3 {
                observer.send(observation).expect("publish");
            }
            "completed"
        });
        let mut published = Vec::new();
        let output = drive_with_observations(driven.as_mut(), &mut observations, |observation| {
            published.push(observation);
            async {}
        })
        .await;

        assert_eq!(output, "completed");
        assert_eq!(published, vec![1, 2, 3]);
        assert!(
            stray.send(4).is_err(),
            "a stray publisher's later send is refused, not awaited"
        );
    }

    /// A source that always has another observation ready until it is
    /// closed: a publisher that never pauses.
    struct Endless {
        next: u64,
        closed: bool,
    }

    impl super::ObservationSource for Endless {
        type Item = u64;

        fn poll_next(&mut self, _context: &mut Context<'_>) -> Poll<Option<u64>> {
            if self.closed {
                return Poll::Ready(None);
            }
            self.next += 1;
            Poll::Ready(Some(self.next))
        }

        fn close(&mut self) {
            self.closed = true;
        }
    }

    /// A single-thread executor with no cooperative budget of any kind: it
    /// polls until ready, parking between wakes.
    fn block_on<F: Future>(future: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
            std::thread::park();
        }
    }

    #[test]
    fn an_always_ready_host_cannot_starve_the_drive() {
        let polls = Arc::new(AtomicUsize::new(0));
        let mut driven = Box::pin(CountsPolls {
            polls: Arc::clone(&polls),
            wakes_left: 4,
        });
        let mut observations = Endless {
            next: 0,
            closed: false,
        };
        let mut published = 0_u64;

        let output = block_on(drive_with_observations(
            driven.as_mut(),
            &mut observations,
            |_| {
                published += 1;
                async {}
            },
        ));

        assert_eq!(output, "completed");
        assert_eq!(polls.load(Ordering::SeqCst), 5);
        assert!(
            published <= 5 * super::PUBLISH_BUDGET as u64,
            "each task poll publishes a bounded batch, then lets the drive run"
        );
    }
}
