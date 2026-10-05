//! Fuse context futures from recorded SDK terminal state and relay callback
//! wakes to their logical Run owner.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll, Wake, Waker};

use lash_sansio::sync::MutexExt;
use restate_sdk::endpoint::ContextInternal;

/// Fuse success and the SDK's terminal failure or suspension, which it hides
/// behind `Pending`. Wakes only request progress; they say nothing about the
/// result's terminal state. Observation leaves the outer handler notification
/// available to the SDK.
pub(crate) struct RestateContextFuture<F> {
    future: Option<Pin<Box<F>>>,
    context: ContextInternal,
    closure_relay: Option<Arc<ClosureWakeRelay>>,
}

/// Routes a registered Run callback's wakes to its logical owner, even when the
/// SDK progresses that callback while polling another owner's result.
#[derive(Default)]
pub(crate) struct ClosureWakeRelay {
    /// The guard's parent waker, republished on every guard poll.
    parent: StdMutex<Option<Waker>>,
}

impl ClosureWakeRelay {
    /// Publish the logical owner's current task before progressing the SDK.
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
/// the guard last published.
struct RelayedWaker {
    relay: Arc<ClosureWakeRelay>,
    /// Use the SDK's progress waker until the logical owner first publishes
    /// its task; another result can progress this callback before then.
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

impl<F> Future for RestateContextFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.context.is_failed_or_suspended() {
            this.future = None;
            return Poll::Pending;
        }
        let Some(future) = this.future.as_mut() else {
            return Poll::Pending;
        };
        if let Some(relay) = this.closure_relay.as_ref() {
            relay.publish_parent(cx.waker());
        }
        let result = future.as_mut().poll(cx);
        if result.is_ready() || this.context.is_failed_or_suspended() {
            this.future = None;
        }
        result
    }
}

/// Fuse a context future a lash owner polls outside the SDK's handler future:
/// a source selection or an awaited receipt.
pub(crate) fn guard_restate_context_future<F>(
    future: F,
    context: ContextInternal,
) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
        context,
        closure_relay: None,
    }
}

/// Fuse a Run result using its attempt's recorded terminal state, retaining
/// wake routing to the logical owner of its registered callback.
pub(crate) fn guard_restate_run_future<F>(
    future: F,
    closure_relay: Arc<ClosureWakeRelay>,
    context: ContextInternal,
) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
        context,
        closure_relay: Some(closure_relay),
    }
}
