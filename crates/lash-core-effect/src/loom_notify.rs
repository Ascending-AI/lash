//! A `tokio::sync::Notify` stand-in built on loom primitives (FIG-1161).
//!
//! `loom::sync::Notify` is a blocking, single-waiter mock, so the await-event
//! registry's `notified_owned()`/`notify_waiters()` pair cannot be modeled
//! with it. A `loom::sync::Mutex`-based shim was tried first, but loom's
//! `Mutex::lock` asserts (`expected to be able to acquire lock`) when a
//! thread blocked on the mutex resumes after a third party re-acquired it —
//! a model limitation, not a production bug — so this shim keeps no internal
//! mutex at all: an `AtomicU8` state plus `loom::future::AtomicWaker`.
//!
//! Semantics modeled (the ones the seam relies on):
//!
//! - [`Notify::notify_waiters`] fires the registered waiter and stores no
//!   permit — a waiter that registers *after* the notify is stranded,
//!   exactly like `tokio::sync::Notify`.
//! - [`Notify::notified_owned`] registers nothing until [`Notified::enable`]
//!   or the first poll, matching `tokio::sync::Notify` so the
//!   enable-under-lock ordering the await-event registry relies on is
//!   exercised for real.
//!
//! Single-waiter only: each `Notified` owns one waiter slot and the registry
//! model checks park at most one waiter per entry. It exists only under
//! `cfg(loom)` and only inside this crate: the registry's per-entry notifier
//! is a private field, so the shim stays module-local. (`lash-core-ids`
//! carries its own copy for the permit and scheduler notifiers.)

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};
use std::task::{Context, Poll};

use loom::future::AtomicWaker;
use loom::sync::atomic::AtomicU8;

/// No waiter has registered yet; a `notify_waiters` now is a no-op.
const IDLE: u8 = 0;
/// A waiter registered via `enable`/first poll; `notify_waiters` fires it.
const REGISTERED: u8 = 1;
/// A notification landed; the waiter's next poll completes.
const FIRED: u8 = 2;

/// See module docs; a drop-in for `tokio::sync::Notify` under `cfg(loom)`.
#[derive(Debug)]
pub struct Notify {
    state: AtomicU8,
    waker: AtomicWaker,
}

impl Notify {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(IDLE),
            waker: AtomicWaker::new(),
        }
    }

    /// Fire the registered waiter, if any; stores no permit. The flag is set
    /// before the wake so a waiter between `register` and the state re-check
    /// still observes the notification — the same order `poll` relies on.
    pub fn notify_waiters(&self) {
        let _ = self
            .state
            .compare_exchange(REGISTERED, FIRED, AcqRel, Acquire);
        self.waker.wake();
    }

    /// `tokio::sync::Notify::notified_owned` equivalent; registration is
    /// deferred to `enable`/first poll, matching `notified`.
    pub fn notified_owned(self: &Arc<Self>) -> Notified {
        Notified {
            notify: Arc::clone(self),
            registered: false,
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

/// The future returned by [`Notify::notified_owned`].
#[derive(Debug)]
pub struct Notified {
    notify: Arc<Notify>,
    registered: bool,
}

impl Notified {
    /// Register the waiter before the first poll so a notification between
    /// `enable` and `poll` is not missed (the `tokio` `Notified::enable`
    /// contract).
    pub fn enable(self: Pin<&mut Self>) {
        self.get_mut().register();
    }

    fn register(&mut self) {
        if self.registered {
            return;
        }
        self.registered = true;
        // Mark REGISTERED before storing the waker: a `notify_waiters` that
        // wins the CAS and finds no waker stored is still observed by the
        // state re-check in `poll`.
        let _ = self
            .notify
            .state
            .compare_exchange(IDLE, REGISTERED, AcqRel, Acquire);
    }
}

impl Future for Notified {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.register();
        self.notify.waker.register(cx.waker().clone());
        if self
            .notify
            .state
            .compare_exchange(FIRED, IDLE, AcqRel, Acquire)
            .is_ok()
        {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Drop for Notified {
    fn drop(&mut self) {
        if self.registered {
            self.notify.state.store(IDLE, Release);
            self.registered = false;
        }
    }
}
