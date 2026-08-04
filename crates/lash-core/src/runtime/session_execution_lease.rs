use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use super::Clock;
use crate::LeaseTimings;
use crate::store::{
    RuntimeCommit, RuntimeCommitResult, RuntimePersistence, SessionExecutionLease,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseCompletion, SessionExecutionLeaseFence,
    StoreError,
};

static NEXT_LEASE_GUARD_ID: AtomicU64 = AtomicU64::new(1);

/// Release progress of a [`SessionExecutionLeaseGuard`].
///
/// Release completion is recorded only on backend acknowledgement. A release
/// whose await is cancelled therefore stays in `Releasing`, which keeps the
/// completion token retained and the release retryable, instead of reporting a
/// durable release that never happened and leaving the lease to linger until
/// its TTL delays queued work for the session.
mod release_state {
    /// The lease is held and renewed; no release has been attempted.
    pub(super) const LIVE: u8 = 0;
    /// Release was requested (renewal stopped) but the backend has not
    /// acknowledged it yet. Still retryable.
    pub(super) const RELEASING: u8 = 1;
    /// The backend acknowledged the release, or the commit that carried it
    /// succeeded. Terminal.
    pub(super) const RELEASED: u8 = 2;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionExecutionLeaseContinuity {
    guard_id: u64,
    fencing_token: u64,
}

pub(super) struct SessionExecutionLeaseGuard {
    store: Arc<dyn RuntimePersistence>,
    lease: Arc<StdMutex<SessionExecutionLease>>,
    release_state: Arc<AtomicU8>,
    lost: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
    guard_id: u64,
    renew_task: tokio::task::JoinHandle<()>,
}

impl SessionExecutionLeaseGuard {
    pub(super) async fn try_acquire(
        store: Arc<dyn RuntimePersistence>,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        timings: LeaseTimings,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<Self>, StoreError> {
        let lease = match store
            .try_claim_session_execution_lease(session_id, owner, timings.ttl_ms())
            .await?
        {
            SessionExecutionLeaseClaimOutcome::Acquired(lease) => lease,
            SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                trace_busy(session_id, owner, &holder);
                return Ok(None);
            }
        };
        tracing::debug!(
            session_id = %lease.session_id,
            owner_id = %lease.owner.owner_id,
            incarnation_id = %lease.owner.incarnation_id,
            fencing_token = lease.fencing_token,
            event = "session_execution_lease.acquired",
            "acquired session execution lease"
        );
        let lease = Arc::new(StdMutex::new(lease));
        let release_state = Arc::new(AtomicU8::new(release_state::LIVE));
        let lost = Arc::new(AtomicBool::new(false));
        let renew_task = spawn_renewal_task(
            Arc::clone(&store),
            Arc::clone(&lease),
            Arc::clone(&release_state),
            Arc::clone(&lost),
            timings,
            Arc::clone(&clock),
        );
        Ok(Some(Self {
            store,
            lease,
            release_state,
            lost,
            clock,
            guard_id: NEXT_LEASE_GUARD_ID.fetch_add(1, Ordering::Relaxed),
            renew_task,
        }))
    }

    pub(super) fn fence(&self) -> SessionExecutionLeaseFence {
        self.lease.lock().expect("session lease lock").fence()
    }

    pub(super) fn completion(&self) -> SessionExecutionLeaseCompletion {
        self.lease.lock().expect("session lease lock").completion()
    }

    /// Record an already-acknowledged release: the commit that carried this
    /// lease's completion succeeded, so the backend has released it.
    pub(super) fn mark_released(&self) {
        if self
            .release_state
            .swap(release_state::RELEASED, Ordering::AcqRel)
            == release_state::RELEASED
        {
            return;
        }
        self.renew_task.abort();
        let completion = self.completion();
        tracing::debug!(
            session_id = %completion.session_id,
            owner_id = %completion.owner.owner_id,
            incarnation_id = %completion.owner.incarnation_id,
            fencing_token = completion.fencing_token,
            event = "session_execution_lease.released",
            "released session execution lease"
        );
    }

    pub(super) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(super) fn continuity(&self) -> Option<SessionExecutionLeaseContinuity> {
        let lease = self.lease.lock().expect("session lease lock");
        if self.is_lost() || lease.expires_at_epoch_ms <= self.clock.timestamp_ms() {
            return None;
        }
        Some(SessionExecutionLeaseContinuity {
            guard_id: self.guard_id,
            fencing_token: lease.fencing_token,
        })
    }

    pub(super) async fn release_if_live(&self) -> Result<(), StoreError> {
        // Entering `Releasing` stops renewal, but nothing is recorded as
        // released until the backend acknowledges it below. A cancelled or
        // failed release therefore leaves this guard in `Releasing`, where the
        // retained completion token makes a later call retry the same release
        // rather than silently short-circuit it.
        match self.release_state.compare_exchange(
            release_state::LIVE,
            release_state::RELEASING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => self.renew_task.abort(),
            Err(release_state::RELEASED) => return Ok(()),
            // Already `Releasing`: an earlier attempt was cancelled or failed
            // before acknowledgement, so fall through and retry it.
            Err(_) => {}
        }
        if self.is_lost() {
            // A lost lease has no owner-side release to perform: renewal
            // already failed the fence, so the release is complete by
            // definition.
            self.release_state
                .store(release_state::RELEASED, Ordering::Release);
            return Ok(());
        }
        let completion = self.completion();
        self.store
            .release_session_execution_lease(&completion)
            .await?;
        self.release_state
            .store(release_state::RELEASED, Ordering::Release);
        tracing::debug!(
            session_id = %completion.session_id,
            owner_id = %completion.owner.owner_id,
            incarnation_id = %completion.owner.incarnation_id,
            fencing_token = completion.fencing_token,
            event = "session_execution_lease.released",
            "released session execution lease"
        );
        Ok(())
    }
}

pub(super) async fn commit_runtime_state_with_fresh_session_execution_lease(
    store: Arc<dyn RuntimePersistence>,
    commit: RuntimeCommit,
    owner: &crate::LeaseOwnerIdentity,
    timings: LeaseTimings,
    clock: Arc<dyn Clock>,
) -> Result<RuntimeCommitResult, StoreError> {
    let session_id = commit.session_id.clone();
    let Some(lease) = SessionExecutionLeaseGuard::try_acquire(
        Arc::clone(&store),
        &session_id,
        owner,
        timings,
        clock,
    )
    .await?
    else {
        return Err(StoreError::Backend(format!(
            "session execution lease for session `{session_id}` is busy"
        )));
    };
    let commit = commit.releasing_session_execution_lease(lease.completion());
    let result = crate::store::commit_runtime_state_verified(store.as_ref(), commit).await?;
    lease.mark_released();
    Ok(result)
}

impl Drop for SessionExecutionLeaseGuard {
    fn drop(&mut self) {
        self.renew_task.abort();
        // A guard dropped in `Releasing` expressed release intent that the
        // backend never acknowledged — typically a cancelled release await —
        // and there is no owner left to retry it in band. Hand the retained
        // completion to a detached best-effort release, matching how this
        // module already owns background lease work (the renewal task). A
        // duplicate release is a no-op: the backend only clears a lease whose
        // owner, lease token and fencing token all still match.
        //
        // `Live` guards are deliberately left alone: holding the lease to its
        // TTL is the intended behavior when a turn is torn down without ever
        // asking to release.
        if self.release_state.load(Ordering::Acquire) != release_state::RELEASING {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let store = Arc::clone(&self.store);
        let release_state = Arc::clone(&self.release_state);
        let completion = self.completion();
        crate::task::spawn(async move {
            match store.release_session_execution_lease(&completion).await {
                Ok(()) => {
                    release_state.store(release_state::RELEASED, Ordering::Release);
                    tracing::debug!(
                        session_id = %completion.session_id,
                        owner_id = %completion.owner.owner_id,
                        incarnation_id = %completion.owner.incarnation_id,
                        fencing_token = completion.fencing_token,
                        event = "session_execution_lease.released_after_drop",
                        "released session execution lease after a cancelled release"
                    );
                }
                Err(err) => tracing::warn!(
                    error = %err,
                    session_id = %completion.session_id,
                    owner_id = %completion.owner.owner_id,
                    incarnation_id = %completion.owner.incarnation_id,
                    fencing_token = completion.fencing_token,
                    event = "session_execution_lease.release_after_drop_failed",
                    "failed to release session execution lease after a cancelled release"
                ),
            }
        });
    }
}

fn spawn_renewal_task(
    store: Arc<dyn RuntimePersistence>,
    lease: Arc<StdMutex<SessionExecutionLease>>,
    release_state: Arc<AtomicU8>,
    lost: Arc<AtomicBool>,
    timings: LeaseTimings,
    clock: Arc<dyn Clock>,
) -> tokio::task::JoinHandle<()> {
    let renew_every = timings.renew_interval();
    crate::task::spawn(async move {
        loop {
            clock.sleep(renew_every).await;
            if release_state.load(Ordering::Acquire) != release_state::LIVE {
                break;
            }
            let fence = lease.lock().expect("session lease lock").fence();
            match store
                .renew_session_execution_lease(&fence, timings.ttl_ms())
                .await
            {
                Ok(renewed) => {
                    tracing::debug!(
                        session_id = %renewed.session_id,
                        owner_id = %renewed.owner.owner_id,
                        incarnation_id = %renewed.owner.incarnation_id,
                        fencing_token = renewed.fencing_token,
                        event = "session_execution_lease.renewed",
                        "renewed session execution lease"
                    );
                    *lease.lock().expect("session lease lock") = renewed;
                }
                Err(err) => {
                    lost.store(true, Ordering::Release);
                    tracing::warn!(
                        error = %err,
                        session_id = %fence.session_id,
                        owner_id = %fence.owner.owner_id,
                        incarnation_id = %fence.owner.incarnation_id,
                        fencing_token = fence.fencing_token,
                        event = "session_execution_lease.lost",
                        "lost session execution lease"
                    );
                    break;
                }
            }
        }
    })
}

fn trace_busy(
    session_id: &str,
    claimant: &crate::LeaseOwnerIdentity,
    holder: &SessionExecutionLease,
) {
    tracing::debug!(
        session_id,
        claimant_owner_id = %claimant.owner_id,
        claimant_incarnation_id = %claimant.incarnation_id,
        holder_owner_id = %holder.owner.owner_id,
        holder_incarnation_id = %holder.owner.incarnation_id,
        holder_fencing_token = holder.fencing_token,
        holder_expires_at_epoch_ms = holder.expires_at_epoch_ms,
        event = "session_execution_lease.busy",
        "session execution lease is busy"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::in_memory_store::InMemorySessionStore;
    use crate::store::SessionExecutionLeaseStore;

    const SESSION_ID: &str = "cancelled-release";

    async fn acquire_gated_guard() -> (
        Arc<InMemorySessionStore>,
        SessionExecutionLeaseGuard,
        Arc<crate::runtime::in_memory_store::test_support::SessionExecutionLeaseReleaseGate>,
    ) {
        let store = Arc::new(InMemorySessionStore::new());
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            LeaseTimings::default(),
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");
        let gate = store.gate_session_execution_lease_release();
        (store, guard, gate)
    }

    async fn lease_is_held(store: &Arc<InMemorySessionStore>) -> bool {
        let outcome = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &crate::LeaseOwnerIdentity::opaque("peer", "peer-incarnation"),
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("peer claim attempt");
        matches!(outcome, SessionExecutionLeaseClaimOutcome::Busy { .. })
    }

    /// The release await is the hazardous point: dropping the future there used
    /// to mark the lease released without any backend acknowledgement, leaving
    /// the durable lease to linger until its TTL with no retry possible.
    #[tokio::test]
    async fn cancelled_lease_release_stays_retryable_until_the_backend_acknowledges() {
        let (store, guard, gate) = acquire_gated_guard().await;

        let mut release = Box::pin(guard.release_if_live());
        tokio::select! {
            _ = gate.wait_entered() => {}
            result = release.as_mut() => panic!("gated release must not complete: {result:?}"),
        }
        drop(release);

        assert_eq!(
            store.session_execution_lease_release_count(),
            0,
            "the cancelled release never reached the backend"
        );
        assert!(
            lease_is_held(&store).await,
            "an unacknowledged release must not report the lease as free"
        );

        gate.admit_one();
        guard
            .release_if_live()
            .await
            .expect("cancelled release stays retryable");

        assert_eq!(store.session_execution_lease_release_count(), 1);
        // The peer claim above holds the lease now; assert against the guard's
        // own state instead: a released guard short-circuits further releases.
        gate.admit_one();
        guard
            .release_if_live()
            .await
            .expect("acknowledged release is terminal");
        assert_eq!(
            store.session_execution_lease_release_count(),
            1,
            "an acknowledged release must not be repeated"
        );
    }

    /// Dropping the guard after a cancelled release hands the retained
    /// completion to a detached best-effort release, so the lease does not wait
    /// out its TTL when the owning turn is torn down mid-release.
    #[tokio::test]
    async fn guard_dropped_mid_release_completes_the_release_out_of_band() {
        let (store, guard, gate) = acquire_gated_guard().await;

        let mut release = Box::pin(guard.release_if_live());
        tokio::select! {
            _ = gate.wait_entered() => {}
            result = release.as_mut() => panic!("gated release must not complete: {result:?}"),
        }
        drop(release);
        gate.admit_one();
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.session_execution_lease_release_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped guard releases the lease out of band");
        assert!(
            !lease_is_held(&store).await,
            "the lease must be free once the out-of-band release lands"
        );
    }
}
