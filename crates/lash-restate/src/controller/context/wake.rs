//! The wake machinery `RestateContextFuture` fuses a context future
//! across the SDK's two terminal poll shapes, and the relay that routes
//! a guarded `ctx.run` closure's own wakes past the guard's tracker.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;

use lash_sansio::sync::MutexExt;

/// Fuse a Restate context future across both of its terminal poll shapes.
///
/// `DurableFutureImpl` returns `Ready` on success. When the SDK records a
/// terminal handler state (including a genuine suspension), it synchronously
/// wakes the task and returns `Pending`; the SDK's outer
/// `HandlerStateAwareFuture` consumes that state on the next poll. In both
/// shapes the SDK future has produced its terminal outcome for this attempt and
/// must never be polled again.
pub(crate) struct RestateContextFuture<F> {
    future: Option<Pin<Box<F>>>,
    /// The relay a guarded `ctx.run` closure's own future wakes through, when
    /// there is one. The guard publishes its parent waker here before each poll
    /// so those wakes bypass the tracker entirely and can never be mistaken for
    /// the SDK's terminal park.
    closure_relay: Option<Arc<ClosureWakeRelay>>,
    tracked: Option<TrackedWaker>,
}

/// The synchronous-wake tracker and the waker derived from it, kept together so
/// the pair can be lent to one poll and handed back without ever being half
/// present. The guard sits on the streaming-hot path, so the pair is reused
/// across polls and only rebuilt when the parent waker or the polling thread
/// changes - the tracker's verdict is scoped to one thread.
struct TrackedWaker {
    tracker: Arc<SynchronousWakeTracker>,
    waker: Waker,
}

impl TrackedWaker {
    fn new(parent: &Waker, polling_thread: ThreadId) -> Self {
        let tracker = Arc::new(SynchronousWakeTracker {
            parent: parent.clone(),
            polling_thread,
            polling: AtomicBool::new(false),
            woke_during_poll: AtomicBool::new(false),
        });
        let waker = Waker::from(Arc::clone(&tracker));
        Self { tracker, waker }
    }

    fn matches(&self, parent: &Waker, polling_thread: ThreadId) -> bool {
        self.tracker.parent.will_wake(parent) && self.tracker.polling_thread == polling_thread
    }

    fn begin_poll(&self) {
        self.tracker
            .woke_during_poll
            .store(false, Ordering::Release);
        self.tracker.polling.store(true, Ordering::Release);
    }

    /// End the poll and report whether the guarded future woke this task
    /// synchronously while it was being polled.
    fn end_poll(&self) -> bool {
        self.tracker.polling.store(false, Ordering::Release);
        self.tracker.woke_during_poll.load(Ordering::Acquire)
    }
}

/// The relay a `ctx.run` closure's own future wakes through.
///
/// The guarded `ctx.run` future polls arbitrary lash code inside the run
/// closure, and a wake from that code - `yield_now`, a `FuturesUnordered`
/// re-arm, a provider stream woken cross-thread by the I/O driver - is benign:
/// it means the closure has more work, not that the attempt is over. Such a
/// wake must therefore never reach the guard's synchronous-wake tracker.
///
/// Attribution is by construction rather than by arithmetic. The guard
/// publishes its own parent waker here before each poll, and the waker
/// [`relay_closure_wakes`] installs beneath the closure forwards straight to
/// that parent - so a closure wake bypasses the tracker whatever thread it
/// comes from and whenever it lands. Counting instead would be racy: a
/// cross-thread closure wake is invisible to the tracker's same-thread gate,
/// so subtracting it would cancel out the SDK's terminal park and leave the
/// guard unfused.
///
/// What the tracker still sees is exactly the wake the closure cannot account
/// for: the SDK recording a terminal handler state, including the synthetic
/// `wake_by_ref` `InterceptErrorFuture` issues after `ctx.fail`. That holds on
/// the replay path too, where the SDK never invokes the closure at all.
#[derive(Default)]
pub(crate) struct ClosureWakeRelay {
    /// The guard's parent waker, republished on every guard poll.
    parent: StdMutex<Option<Waker>>,
}

impl ClosureWakeRelay {
    /// Publish the waker the guard was polled with, so closure wakes reach the
    /// task without passing through the guard's tracker.
    fn publish_parent(&self, parent: &Waker) {
        let mut slot = self.parent.lock_recover();
        match slot.as_ref() {
            Some(existing) if existing.will_wake(parent) => {}
            _ => *slot = Some(parent.clone()),
        }
    }

    fn parent(&self) -> Option<Waker> {
        self.parent.lock_recover().clone()
    }
}

/// The waker handed to a run closure's future. It forwards to whatever parent
/// the guard last published, never to the guard's tracked waker.
struct RelayedWaker {
    relay: Arc<ClosureWakeRelay>,
    /// Used only before the guard's first poll has published a parent, which
    /// cannot happen while the closure is being polled by the guarded future.
    fallback: Waker,
}

impl RelayedWaker {
    fn relay_wake(&self) {
        match self.relay.parent() {
            Some(parent) => parent.wake(),
            None => self.fallback.wake_by_ref(),
        }
    }
}

impl Wake for RelayedWaker {
    fn wake(self: Arc<Self>) {
        self.relay_wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.relay_wake();
    }
}

pub(crate) struct RelayedWakeFuture<F> {
    future: Pin<Box<F>>,
    relay: Arc<ClosureWakeRelay>,
    waker: Option<(Arc<RelayedWaker>, Waker)>,
}

impl<F> Future for RelayedWakeFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let (relayed, waker) = match this.waker.take() {
            Some((relayed, waker)) if relayed.fallback.will_wake(cx.waker()) => (relayed, waker),
            _ => {
                let relayed = Arc::new(RelayedWaker {
                    relay: Arc::clone(&this.relay),
                    fallback: cx.waker().clone(),
                });
                let waker = Waker::from(Arc::clone(&relayed));
                (relayed, waker)
            }
        };
        let result = this.future.as_mut().poll(&mut Context::from_waker(&waker));
        this.waker = Some((relayed, waker));
        result
    }
}

pub(crate) fn relay_closure_wakes<F>(
    future: F,
    relay: Arc<ClosureWakeRelay>,
) -> RelayedWakeFuture<F>
where
    F: Future,
{
    RelayedWakeFuture {
        future: Box::pin(future),
        relay,
        waker: None,
    }
}

impl<F> RestateContextFuture<F> {
    pub(crate) fn is_fused(&self) -> bool {
        self.future.is_none()
    }
}

impl<F> Future for RestateContextFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let polling_thread = std::thread::current().id();
        let tracked = match this.tracked.take() {
            Some(tracked) if tracked.matches(cx.waker(), polling_thread) => tracked,
            _ => TrackedWaker::new(cx.waker(), polling_thread),
        };
        let Some(future) = this.future.as_mut() else {
            return Poll::Pending;
        };
        // Publish this poll's parent waker before entering the guarded future,
        // so a wake from inside the run closure - on any thread, at any time -
        // reaches the task directly instead of registering as a synchronous
        // wake the closure cannot account for.
        if let Some(relay) = this.closure_relay.as_ref() {
            relay.publish_parent(cx.waker());
        }
        tracked.begin_poll();
        let result = future
            .as_mut()
            .poll(&mut Context::from_waker(&tracked.waker));
        // Every wake the tracker saw is the SDK's own: the closure's wakes were
        // routed past it by construction.
        let woke_during_poll = tracked.end_poll();

        if result.is_ready() || woke_during_poll {
            this.future = None;
        } else {
            this.tracked = Some(tracked);
        }
        result
    }
}

struct SynchronousWakeTracker {
    // Deliberately redundant: the Restate SDK also wakes the handler through
    // its output channel when it records suspension. Forwarding preserves the
    // ordinary Future/Waker contract for other synchronous wake paths, but the
    // suspension fix does not depend on this parent wake; the tracker flag is
    // what fuses the one-shot SDK future.
    parent: Waker,
    polling_thread: ThreadId,
    polling: AtomicBool,
    woke_during_poll: AtomicBool,
}

impl Wake for SynchronousWakeTracker {
    fn wake(self: Arc<Self>) {
        self.record_wake();
        self.parent.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.record_wake();
        self.parent.wake_by_ref();
    }
}

impl SynchronousWakeTracker {
    fn record_wake(&self) {
        if self.polling.load(Ordering::Acquire)
            && std::thread::current().id() == self.polling_thread
        {
            self.woke_during_poll.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn guard_restate_context_future<F>(future: F) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
        closure_relay: None,
        tracked: None,
    }
}

/// Guard a `ctx.run` future whose closure polls arbitrary lash code.
///
/// `closure_relay` is the relay the closure's own future wakes through (see
/// [`relay_closure_wakes`]). Those wakes are routed to the guard's parent waker
/// by construction, so a wake from inside the closure - on any thread - leaves
/// the run pollable, while any synchronous wake that does reach the tracker -
/// the SDK's terminal park, on a live attempt or on a replay that never invokes
/// the closure - fuses it.
pub(crate) fn guard_restate_run_future<F>(
    future: F,
    closure_relay: Arc<ClosureWakeRelay>,
) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
        closure_relay: Some(closure_relay),
        tracked: None,
    }
}
