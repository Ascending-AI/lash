//! The session runtimes this core's drives hold open in this process
//! (FIG-3825).
//!
//! An engine holds a session for one attempt of a drive invocation
//! ([`hold_drive`](lash_core::SessionDriver::hold_drive)). The first
//! admission or root of the session that needs a runtime while the hold is
//! up opens it, and every later one runs on it: the attempt's other
//! admissions, replayed ones included, and the roots it calls whose handlers
//! run in this process. Each of them used to open a plugin host and load the
//! session.
//!
//! The registry holds each held session weakly: the last hold's drop
//! releases its runtime, so nothing opened for one attempt serves the next.
//! A session a host holds open is driven on the host's runtime
//! ([`residents`](super::residents)) instead.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use lash_core::facade_support::RuntimeHandle;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

#[derive(Default)]
pub(crate) struct HeldDrives {
    entries: Mutex<HashMap<SessionId, Weak<HeldSession>>>,
}

impl HeldDrives {
    /// Hold `session` for one drive attempt, joining the hold another
    /// attempt of it already keeps up.
    pub(crate) fn hold(&self, session: &SessionId) -> Arc<HeldSession> {
        let mut entries = self.entries.lock_recover();
        entries.retain(|_, held| held.strong_count() > 0);
        if let Some(held) = entries.get(session).and_then(Weak::upgrade) {
            return held;
        }
        let held = Arc::new(HeldSession::default());
        entries.insert(session.clone(), Arc::downgrade(&held));
        held
    }

    /// `session`'s hold, while a drive attempt keeps it up.
    pub(crate) fn held(&self, session: &SessionId) -> Option<Arc<HeldSession>> {
        self.entries
            .lock_recover()
            .get(session)
            .and_then(Weak::upgrade)
    }
}

/// One held session: the runtime its drive opened, once a step did.
#[derive(Default)]
pub(crate) struct HeldSession {
    runtime: tokio::sync::OnceCell<RuntimeHandle>,
    unsettled_root: UnsettledRoot,
}

impl HeldSession {
    /// The held runtime, opened with `open` when no step of the drive has
    /// opened it yet. A failed open holds nothing, so the next step opens
    /// again.
    pub(crate) async fn runtime<E>(
        &self,
        open: impl Future<Output = Result<RuntimeHandle, E>>,
    ) -> Result<RuntimeHandle, E> {
        self.runtime.get_or_try_init(|| open).await.cloned()
    }

    pub(crate) fn unsettled_root(&self) -> &UnsettledRoot {
        &self.unsettled_root
    }
}

/// Whether a root that ran on a held runtime did not end: its attempt
/// failed, or the engine dropped it mid-flight. A failed attempt may leave
/// residue on the runtime that a redrive in a fresh process would not see,
/// so the next root discards it first; a dropped one's the kernel discards
/// as it drops (FIG-3984), and discarding again is harmless. A root that
/// ended leaves the committed session, as the kernel's own drive loop leaves
/// it between roots.
#[derive(Default)]
pub(crate) struct UnsettledRoot(AtomicBool);

impl UnsettledRoot {
    /// Mark a root entering the runtime unsettled; answers whether the root
    /// before it was, so this one discards its residue.
    pub(crate) fn enter(&self) -> bool {
        self.0.swap(true, Ordering::SeqCst)
    }

    /// The entered root ended.
    pub(crate) fn settle(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
