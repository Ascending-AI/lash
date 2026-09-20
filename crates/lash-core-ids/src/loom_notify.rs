//! A `tokio::sync::Notify` stand-in built on loom primitives (FIG-1161).
//!
//! `loom::sync::Notify` is a blocking, single-waiter mock, so the seams that
//! carry `Arc<tokio::sync::Notify>` through their public types cannot model
//! `notified()`/`notify_waiters()` with it. This module implements just the
//! semantics those seams rely on:
//!
//! - [`Notify::notify_one`] wakes one registered waiter, or stores a permit
//!   when none is registered — a notify between a waiter's registration check
//!   and its first poll is still delivered.
//! - [`Notify::notify_waiters`] wakes every registered waiter and stores no
//!   permit.
//! - [`Notify::notified`] registers nothing until [`Notified::enable`] or the
//!   first poll, matching `tokio::sync::Notify` so the enable-under-lock
//!   ordering the await-event registry relies on is exercised for real.
//!
//! It exists only under `cfg(loom)`; production signatures alias it so the
//! `changed`/`dispatcher_changed` notifiers that cross crate boundaries keep
//! one type.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use loom::sync::Mutex;

#[derive(Debug)]
struct Waiter {
    waker: Option<Waker>,
    fired: bool,
}

#[derive(Debug)]
struct State {
    next_id: u64,
    /// Permits stored by `notify_one` while no waiter was registered.
    permits: usize,
    waiters: BTreeMap<u64, Waiter>,
}

impl State {
    fn lock(state: &Mutex<State>) -> loom::sync::MutexGuard<'_, State> {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// See module docs; a drop-in for `tokio::sync::Notify` under `cfg(loom)`.
#[derive(Debug)]
pub struct Notify {
    state: Mutex<State>,
}

impl Notify {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                next_id: 0,
                permits: 0,
                waiters: BTreeMap::new(),
            }),
        }
    }

    /// Wake the longest-registered waiter, or store one permit.
    pub fn notify_one(&self) {
        let mut state = State::lock(&self.state);
        for waiter in state.waiters.values_mut() {
            if !waiter.fired {
                waiter.fired = true;
                if let Some(waker) = waiter.waker.take() {
                    waker.wake();
                }
                return;
            }
        }
        state.permits += 1;
    }

    /// Wake every registered waiter; stores no permit.
    pub fn notify_waiters(&self) {
        let mut state = State::lock(&self.state);
        for waiter in state.waiters.values_mut() {
            waiter.fired = true;
            if let Some(waker) = waiter.waker.take() {
                waker.wake();
            }
        }
    }

    /// The waiter's registration is deferred to `enable`/first poll, matching
    /// `tokio::sync::Notify::notified`.
    pub fn notified(self: &Arc<Self>) -> Notified {
        Notified {
            notify: Arc::clone(self),
            id: None,
        }
    }

    /// `tokio::sync::Notify::notified_owned` equivalent.
    pub fn notified_owned(self: &Arc<Self>) -> Notified {
        self.notified()
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

/// The future returned by [`Notify::notified`]/[`Notify::notified_owned`].
pub struct Notified {
    notify: Arc<Notify>,
    id: Option<u64>,
}

impl Notified {
    /// Register the waiter before the first poll so a notification between
    /// `enable` and `poll` is not missed (the `tokio` `Notified::enable`
    /// contract).
    pub fn enable(self: Pin<&mut Self>) {
        self.get_mut().register();
    }

    fn register(&mut self) {
        if self.id.is_some() {
            return;
        }
        let id = {
            let mut state = State::lock(&self.notify.state);
            let id = state.next_id;
            state.next_id += 1;
            state.waiters.insert(
                id,
                Waiter {
                    waker: None,
                    fired: false,
                },
            );
            id
        };
        self.id = Some(id);
    }
}

impl Future for Notified {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.register();
        let Some(id) = self.id else {
            return Poll::Ready(());
        };
        let mut state = State::lock(&self.notify.state);
        let fired = state.waiters.get(&id).is_some_and(|waiter| waiter.fired);
        if fired || state.permits > 0 {
            if !fired {
                state.permits -= 1;
            }
            state.waiters.remove(&id);
            drop(state);
            self.id = None;
            return Poll::Ready(());
        }
        if let Some(waiter) = state.waiters.get_mut(&id) {
            waiter.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Notified {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            State::lock(&self.notify.state).waiters.remove(&id);
        }
    }
}
