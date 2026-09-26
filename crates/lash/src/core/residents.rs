//! The sessions this core has open in this process (FIG-3600 S5b).
//!
//! The core's session driver runs a drive on the host's open session when
//! there is one, not on a second runtime it opens from the store: the open
//! session carries what the host configured on it (its protocol options,
//! its plugins, a state it was opened with), and one runtime per open
//! session keeps the resident in step with what the drive commits.
//!
//! The registry holds each runtime weakly. It owns nothing and caches
//! nothing: a session that closes or parks drops out, and the driver opens
//! the session from the store as it does for a session no host holds open.
//! A drive borrows the runtime for as long as it runs, so closing or parking
//! the session first withdraws it and lets a drive already running on it
//! stop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::{RuntimeHandle, WeakRuntimeHandle};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use tokio::sync::watch;

use crate::support::EffectHost;

/// How long a close or park waits for drives running on the session's
/// runtime before it reports the runtime still in use.
const RELEASE_GRACE: Duration = Duration::from_secs(5);

/// An open session's runtime, held weakly, the effect host it runs on, and
/// how many drives borrow it now.
struct Resident {
    runtime: WeakRuntimeHandle,
    effect_host: Arc<dyn EffectHost>,
    borrows: Arc<watch::Sender<usize>>,
}

#[derive(Default)]
pub(crate) struct ResidentSessions {
    entries: Mutex<HashMap<SessionId, Resident>>,
}

/// A drive's hold on an open session's runtime: the runtime, its effect
/// host, and the borrow the session's close waits out.
///
/// Fields drop in declaration order: the runtime is released before the
/// count, so a close woken by the count finds no hold left.
pub(crate) struct ResidentBorrow {
    runtime: RuntimeHandle,
    effect_host: Arc<dyn EffectHost>,
    _count: BorrowCount,
}

impl ResidentBorrow {
    pub(crate) fn runtime(&self) -> &RuntimeHandle {
        &self.runtime
    }

    pub(crate) fn effect_host(&self) -> &Arc<dyn EffectHost> {
        &self.effect_host
    }
}

/// One drive's place in a resident's borrow count.
struct BorrowCount(Arc<watch::Sender<usize>>);

impl Drop for BorrowCount {
    fn drop(&mut self) {
        self.0
            .send_modify(|borrows| *borrows = borrows.saturating_sub(1));
    }
}

impl ResidentSessions {
    /// Record `handle` as `session`'s open runtime in this process. The most
    /// recent open of a session is the one a drive runs on.
    pub(crate) fn register(
        &self,
        session: &SessionId,
        handle: &RuntimeHandle,
        effect_host: Arc<dyn EffectHost>,
    ) {
        let mut entries = self.entries.lock_recover();
        entries.retain(|_, resident| resident.runtime.is_alive());
        entries.insert(
            session.clone(),
            Resident {
                runtime: handle.downgrade(),
                effect_host,
                borrows: Arc::new(watch::Sender::new(0)),
            },
        );
    }

    /// Borrow `session`'s open runtime for one drive, while a host holds it.
    pub(crate) fn borrow(&self, session: &SessionId) -> Option<ResidentBorrow> {
        let entries = self.entries.lock_recover();
        let resident = entries.get(session)?;
        let runtime = resident.runtime.upgrade()?;
        resident.borrows.send_modify(|borrows| *borrows += 1);
        Some(ResidentBorrow {
            runtime,
            effect_host: Arc::clone(&resident.effect_host),
            _count: BorrowCount(Arc::clone(&resident.borrows)),
        })
    }

    /// Withdraw `handle` as `session`'s open runtime, so no further drive
    /// borrows it, then wait (bounded) for the drives borrowing it to stop.
    /// A later open of the session registered another runtime; that one
    /// stays. Answers whether `handle` was the session's resident.
    pub(crate) async fn release(&self, session: &SessionId, handle: &RuntimeHandle) -> bool {
        let borrows = {
            let mut entries = self.entries.lock_recover();
            match entries.get(session) {
                Some(resident) if resident.runtime.names(handle) => {
                    entries.remove(session).map(|resident| resident.borrows)
                }
                _ => None,
            }
        };
        let Some(borrows) = borrows else {
            return false;
        };
        let mut idle = borrows.subscribe();
        let _ = tokio::time::timeout(RELEASE_GRACE, idle.wait_for(|borrows| *borrows == 0)).await;
        true
    }
}
