//! Testing-only error-return injection over a driver's effect-journal calls
//! (FIG-3524).
//!
//! A fault is armed on a `(point, replay_key)` pair and fires on the first
//! call that matches both, so an armed `renew` cannot be consumed by a
//! sibling claim's renewal loop. Only `testing` builds carry the injector;
//! production drivers hold no field for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_sansio::sync::MutexExt;
use tokio::sync::Notify;

use super::{
    super::await_event_coordinator::AwaitEventBackend, EffectReplayFailure, EffectReplayRowStore,
    StoreEffectReplayDriver,
};
use crate::{RuntimeEffectControllerError, RuntimeErrorCode};

/// One of the row-store calls a [`StoreEffectReplayDriver`](super::StoreEffectReplayDriver)
/// makes that a test can arm to fail once (FIG-3524).
///
/// These are the integrator-owned durability seams: a fault armed on one
/// returns the backend's `Store` vocabulary error instead of calling the row
/// store, so a run sees exactly the typed error a real substrate failure
/// would produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectJournalFaultPoint {
    /// [`EffectReplayRowStore::claim`](super::EffectReplayRowStore::claim).
    Claim,
    /// [`EffectReplayRowStore::finalize`](super::EffectReplayRowStore::finalize).
    Finalize,
    /// [`EffectReplayRowStore::renew`](super::EffectReplayRowStore::renew).
    Renew,
}

impl EffectJournalFaultPoint {
    /// The operation name, for messages naming the faulted call.
    pub fn label(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Finalize => "finalize",
            Self::Renew => "renew",
        }
    }
}

/// One-shot error-return injector over a driver's effect-journal calls
/// (FIG-3524).
#[derive(Clone, Debug)]
pub struct EffectJournalFaults {
    state: Arc<EffectJournalFaultState>,
}

#[derive(Debug)]
struct EffectJournalFaultState {
    armed: std::sync::Mutex<Option<(EffectJournalFaultPoint, String)>>,
    /// The `(point, replay_key)` an armed fault fired for. A later call to
    /// the same pair is the driver re-attempting the faulted operation, so it
    /// counts toward `calls_after_fire` rather than consuming anything.
    fired_for: std::sync::Mutex<Option<(EffectJournalFaultPoint, String)>>,
    fired: AtomicBool,
    fired_notify: Notify,
    calls_after_fire: AtomicUsize,
    /// Whether the armed fault stays armed after firing, failing every
    /// matching call until [`EffectJournalFaults::heal`] (a store outage
    /// rather than a single blip).
    persistent: AtomicBool,
    /// How many calls an armed fault has failed.
    fires: AtomicUsize,
    /// This journal's `Store` vocabulary code: the code an injected error
    /// carries, so an observer can assert the exact code the caller sees.
    store_code: RuntimeErrorCode,
}

impl EffectJournalFaults {
    pub(crate) fn new(store_code: RuntimeErrorCode) -> Self {
        Self {
            state: Arc::new(EffectJournalFaultState {
                armed: std::sync::Mutex::new(None),
                fired_for: std::sync::Mutex::new(None),
                fired: AtomicBool::new(false),
                fired_notify: Notify::new(),
                calls_after_fire: AtomicUsize::new(0),
                persistent: AtomicBool::new(false),
                fires: AtomicUsize::new(0),
                store_code,
            }),
        }
    }

    /// Arm `point` to fail once on the next call for `replay_key`.
    pub fn fail_next(&self, point: EffectJournalFaultPoint, replay_key: &str) {
        self.state.persistent.store(false, Ordering::SeqCst);
        *self.state.armed.lock_recover() = Some((point, replay_key.to_string()));
    }

    /// Arm `point` to fail every call for `replay_key` until [`heal`](Self::heal):
    /// an outage that outlasts a retry budget, not a single blip.
    pub fn fail_until_healed(&self, point: EffectJournalFaultPoint, replay_key: &str) {
        self.state.persistent.store(true, Ordering::SeqCst);
        *self.state.armed.lock_recover() = Some((point, replay_key.to_string()));
    }

    /// Disarm any armed fault; later calls reach the row store.
    pub fn heal(&self) {
        *self.state.armed.lock_recover() = None;
        self.state.persistent.store(false, Ordering::SeqCst);
    }

    /// How many calls an armed fault has failed so far.
    pub fn fires(&self) -> usize {
        self.state.fires.load(Ordering::SeqCst)
    }

    /// The code an armed fault returns.
    pub fn store_code(&self) -> RuntimeErrorCode {
        self.state.store_code.clone()
    }

    /// Whether an armed fault has fired; a run that armed one and never saw
    /// it fire covered nothing.
    pub fn fired(&self) -> bool {
        self.state.fired.load(Ordering::SeqCst)
    }

    /// Resolves once an armed fault fires. An executing effect can hold on it
    /// to reach a `renew` injection deterministically.
    pub async fn wait_fired(&self) {
        loop {
            let notified = self.state.fired_notify.notified();
            if self.state.fired.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    /// How many times the driver called the faulted `(point, replay_key)`
    /// pair again after the fault fired: the retried/unretried qualifier the
    /// fail-stop oracle needs. Zero means the error was never retried.
    pub fn calls_after_fire(&self) -> usize {
        self.state.calls_after_fire.load(Ordering::SeqCst)
    }

    /// Consume an armed fault for `(point, replay_key)`; the driver calls
    /// this immediately before each row-store call it can fail.
    pub(crate) fn take(&self, point: EffectJournalFaultPoint, replay_key: &str) -> bool {
        let mut armed = self.state.armed.lock_recover();
        if armed
            .as_ref()
            .is_some_and(|(armed_point, key)| *armed_point == point && key == replay_key)
        {
            if !self.state.persistent.load(Ordering::SeqCst) {
                *armed = None;
            }
            self.state.fires.fetch_add(1, Ordering::SeqCst);
            *self.state.fired_for.lock_recover() = Some((point, replay_key.to_string()));
            self.state.fired.store(true, Ordering::SeqCst);
            self.state.fired_notify.notify_waiters();
            return true;
        }
        drop(armed);
        if self
            .state
            .fired_for
            .lock_recover()
            .as_ref()
            .is_some_and(|(fired_point, key)| *fired_point == point && key == replay_key)
        {
            self.state.calls_after_fire.fetch_add(1, Ordering::SeqCst);
        }
        false
    }
}

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// Testing seam (FIG-3524): the handle a test arms to make one `claim`,
    /// `finalize` or `renew` on a named replay key return this journal's
    /// `Store` vocabulary error instead of reaching the row store.
    pub fn journal_faults(&self) -> EffectJournalFaults {
        self.journal_faults.clone()
    }

    /// The error an armed journal fault substitutes for the `point` call on
    /// `replay_key`, when one is armed.
    pub(crate) fn take_journal_fault(
        &self,
        point: EffectJournalFaultPoint,
        replay_key: &str,
    ) -> Option<RuntimeEffectControllerError> {
        self.journal_faults.take(point, replay_key).then(|| {
            self.vocabulary().error(
                EffectReplayFailure::Store,
                format!(
                    "injected effect-journal {} error for replay key `{replay_key}`",
                    point.label()
                ),
            )
        })
    }
}
