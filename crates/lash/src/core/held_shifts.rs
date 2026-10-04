//! The session runtimes this core's executes hold open in this process
//! (FIG-3825).
//!
//! An engine holds a session for one attempt of a shift invocation
//! ([`hold_shift`](lash_core::SessionShifts::hold_shift)). The first run
//! of the session that runs while the hold is up opens the runtime, and
//! later sequential runs reuse it: the runs the attempt calls whose
//! handlers run in this process. Each used to open a plugin host and load
//! the session. An admission needs no runtime: it reads the session's store.
//!
//! The registry holds each held session weakly: the last hold's drop
//! releases its runtime, so nothing opened for one attempt serves the next.
//! Each attempt registers a fresh hold. A running run pins the hold it
//! captured past its parent, but no later attempt joins it. A redelivered
//! run bypasses a cache already leased by a live run, even while its parent
//! lives: the superseded writer may be waiting on an unreplicable engine
//! acknowledgement. Idle caches are reused within the shift. An admission
//! never takes the writer: a replayed admission never waits for the run
//! its shift already called (FIG-4729, FIG-4755).
//! A session a host holds open is executed on the host's runtime
//! ([`residents`](super::residents)) instead.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use lash_core::facade_support::RuntimeHandle;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

#[derive(Default)]
pub(crate) struct HeldShifts {
    entries: Mutex<HashMap<SessionId, Weak<HeldSession>>>,
}

impl HeldShifts {
    /// Register a fresh lifetime for one attempt of `session`'s shift.
    pub(crate) fn hold(&self, session: &SessionId) -> Arc<HeldSession> {
        let mut entries = self.entries.lock_recover();
        entries.retain(|_, held| held.strong_count() > 0);
        let held = Arc::new(HeldSession::default());
        entries.insert(session.clone(), Arc::downgrade(&held));
        held
    }

    /// Capture the runtime for a run before it can wait on its writer.
    pub(crate) fn run(&self, session: &SessionId) -> HeldRun {
        let mut held = self.held(session).unwrap_or_default();
        if held.leased.swap(true, Ordering::AcqRel) {
            // A live execution owns this cache, so a retry opens from durable
            // state rather than queuing behind the superseded writer.
            held = Arc::new(HeldSession::default());
            held.leased.store(true, Ordering::Relaxed);
        }
        HeldRun { held }
    }

    /// `session`'s hold, while a shift attempt keeps it up.
    fn held(&self, session: &SessionId) -> Option<Arc<HeldSession>> {
        self.entries
            .lock_recover()
            .get(session)
            .and_then(Weak::upgrade)
    }
}

/// One held session: the runtime its shift opened, once a step did.
#[derive(Default)]
pub(crate) struct HeldSession {
    runtime: tokio::sync::OnceCell<RuntimeHandle>,
    unsettled_run: UnsettledRun,
    leased: AtomicBool,
}

impl HeldSession {
    /// The held runtime, opened with `open` when no step of the shift has
    /// opened it yet. A failed open holds nothing, so the next step opens
    /// again.
    pub(crate) async fn runtime<E>(
        &self,
        open: impl Future<Output = Result<RuntimeHandle, E>>,
    ) -> Result<RuntimeHandle, E> {
        self.runtime.get_or_try_init(|| open).await.cloned()
    }

    pub(crate) fn unsettled_run(&self) -> &UnsettledRun {
        &self.unsettled_run
    }
}

/// Keeps a run pinned to the attempt whose runtime it captured.
pub(crate) struct HeldRun {
    held: Arc<HeldSession>,
}

impl Drop for HeldRun {
    fn drop(&mut self) {
        self.held.leased.store(false, Ordering::Release);
    }
}

impl HeldRun {
    pub(crate) async fn runtime<E>(
        &self,
        open: impl Future<Output = Result<RuntimeHandle, E>>,
    ) -> Result<RuntimeHandle, E> {
        self.held.runtime(open).await
    }

    pub(crate) fn unsettled_run(&self) -> &UnsettledRun {
        self.held.unsettled_run()
    }
}

/// Whether a run that ran on a held runtime did not end: its attempt
/// failed, or the engine dropped it mid-flight. A failed attempt may leave
/// residue on the runtime that a redrive in a fresh process would not see,
/// so the next run discards it first; a dropped one's the kernel discards
/// as it drops (FIG-3984), and discarding again is harmless. A run that
/// ended leaves the committed session, as the kernel's own shift loop leaves
/// it between runs.
#[derive(Default)]
pub(crate) struct UnsettledRun(AtomicBool);

impl UnsettledRun {
    /// Mark a run entering the runtime unsettled; answers whether the run
    /// before it was, so this one discards its residue.
    pub(crate) fn enter(&self) -> bool {
        self.0.swap(true, Ordering::SeqCst)
    }

    /// The entered run ended.
    pub(crate) fn settle(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
