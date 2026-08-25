//! Session-execution-lease acquisition, renewal, and the trace timeline an
//! operator reconstructs a takeover from.
//!
//! The lease is advisory: it serializes the common case so two runners do not
//! duplicate work, but the commit's head compare-and-set is the only authority
//! on who publishes (ADR 0029). That makes a handful of transitions decisive when
//! a turn looks stuck; each structured event carries the decision-specific lane,
//! identity, and session facts shown below:
//!
//! | event | level | emitted by | meaning |
//! |---|---|---|---|
//! | `session_execution_lease.acquired` | INFO | the claimant | this runner acquired the lane |
//! | `session_execution_lease.taken_over` | INFO | the *winner* | this claim displaced a named lapsed holder |
//! | `session_execution_lease.lost` | WARN | the loser | a store verdict (`consulted = "renewal_fence_rejected"`) or refused response install (`consulted = "renewal_response_refused"`) ended continuity; this runner no longer holds the lane |
//! | `session_execution_lease.renewal_refused` | WARN | the store | durable owner/token decision evidence for a refused renewal |
//! | `session_execution_lease.renewal_install_refused` | WARN | the runner | a backend returned a renewal that did not preserve the presented lease |
//! | `session_execution_lease.release_refused` | WARN | the store | durable owner/token decision evidence for a refused release |
//! | `session_execution_lease.renewal_failed` | WARN | the holder | renewal stopped on a transient error; the lease is still ours to release |
//! | `session_execution_lease.busy` | DEBUG | the claimant | the claim observed a named live holder and did not acquire the lane |
//! | `session_execution_lease.busy_advisory` | DEBUG | the turn claimant | a turn proceeds lane-lessly because the head CAS is the authority |
//! | `session_execution_lease.busy_wait` | INFO | the durable queued drain | a durable-workflow-controller drain is waiting out a crashed-looking holder before re-claiming |
//! | `session_execution_lease.busy_gave_up` | INFO | the durable queued drain | the drain stopped waiting (`give_up = "holder_is_alive"` or `"wait_budget_exhausted"`) and reported `session_execution_lane_busy` so the engine's retry policy paces the next attempt |
//! | `session_execution_lease.commit_busy_advisory` | INFO | the persistence claimant | a lane-less commit proceeds despite a live holder because the head CAS is the authority |
//! | `session_execution_lease.commit_cas_rejected` | WARN | the losing writer | the commit's head CAS lost to a concurrent writer |
//!
//! **`taken_over` is the winner's event, emitted atomically with the claim that
//! displaced the previous holder.** That placement is not a detail: the displaced
//! runner is usually *why* its lease lapsed, so it is frequently dead, frozen, or
//! already replaced, and a takeover reported from its renewal path would be
//! missing in exactly the case an operator most needs it, or would name whichever
//! holder happens to be current by the time it wakes up. The substrate hands the
//! winner the prior holder inside the claim
//! ([`SessionExecutionLeaseAcquisition::displaced`](crate::store::SessionExecutionLeaseAcquisition::displaced)),
//! so the event is true by construction and needs no liveness on the loser's side.
//!
//! The loser's `lost` remains a purely local observation: *this* runner no longer
//! holds the lane. It deliberately does not name a successor.
//!
//! These are trace events, not durable session history: lease churn is per-attempt telemetry. A
//! lost lease is not a turn failure: the turn may still commit, and the
//! `commit_cas_rejected` event is what proves it did not.
//! Reentry requires the same host owner, boot incarnation, and runtime-minted
//! executor id. A second runtime open under the same host identity is a distinct
//! claimant and therefore observes `Busy` while the first executor is live.
//!
//! `acquired` and `taken_over` are INFO rather than DEBUG because reconstructing
//! takeover order is an ordinary production question; requiring debug logging to
//! answer it would make the timeline unavailable exactly when it is needed.

use lash_sansio::sync::MutexExt;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::Clock;
use crate::LeaseTimings;
use crate::store::{
    RuntimeCommit, RuntimeCommitReceipt, RuntimePersistence, SessionExecutionLease,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome, StoreError,
};

mod observability;
pub(crate) mod queued_lane_wait;

pub(crate) use observability::trace_acquisition;
use observability::trace_busy;
pub(super) use observability::trace_commit_cas_rejected;

static NEXT_LEASE_GUARD_ID: AtomicU64 = AtomicU64::new(1);

/// Release progress of a [`SessionExecutionLeaseGuard`].
///
/// Release completion is recorded only on backend acknowledgement, never on
/// intent. A cancelled or failed release therefore stays in `Releasing` with its
/// completion token retained, so the owner can retry the same release in band
/// instead of reporting a durable release that never happened. If the guard is
/// dropped, that same token is safe for a best-effort out-of-band release:
/// every successor claim rotates it, so a late release is refused by name.
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

/// Why this runner can no longer continue under its resident lease.
mod loss_cause {
    /// Renewal has not established any loss of continuity.
    pub(super) const NONE: u8 = 0;
    /// The store proved that the durable owner, lifecycle token, or lease had
    /// stopped matching. No owner-side release remains to perform.
    pub(super) const STORE_VERDICT: u8 = 1;
    /// Core refused the returned renewal response. The durable row may still be
    /// ours, so cleanup must attempt the normal token-fenced release.
    pub(super) const RESPONSE_REFUSED: u8 = 2;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionExecutionLeaseContinuity {
    guard_id: u64,
    fencing_token: u64,
}

/// What a commit-time trace event says about the lane the writer held.
///
/// Built only on the rejection path, from the guard that is already in scope
/// there. It is deliberately never carried through the commit: a value alive
/// across that await grows every turn future, and these facts are still readable
/// when the rejection arrives.
///
/// Absent evidence is meaningful, not missing: it means the writer proceeded under
/// the busy advisory with no lane at all (ADR 0029 makes the CAS the authority, so
/// that is legal). The event reports `lane_held = false` and names the writer from
/// the claimant instead, so a rejection is never anonymous.
#[derive(Clone, Debug)]
pub(super) struct SessionExecutionLeaseCommitEvidence {
    /// The lane holder's identity.
    owner: crate::LeaseOwnerIdentity,
    executor_id: String,
    /// The generation this runner held.
    fencing_token: u64,
    /// Whether this runner had already observed its own lease loss before the
    /// commit. Repeated rejections with this false are livelock, not takeover.
    lease_lost: bool,
}

pub(super) struct SessionExecutionLeaseGuard {
    store: Arc<dyn RuntimePersistence>,
    lease: Arc<StdMutex<SessionExecutionLease>>,
    release_state: Arc<AtomicU8>,
    loss_cause: Arc<AtomicU8>,
    clock: Arc<dyn Clock>,
    guard_id: u64,
    renew_task: tokio::task::JoinHandle<()>,
}

pub(super) enum SessionExecutionLeaseGuardAcquisition {
    Acquired(SessionExecutionLeaseGuard),
    Busy(SessionExecutionLease),
}

/// A non-owning view of a turn driver's execution lane.
///
/// It shares only the live fence and loss evidence needed by nested commits.
/// Retaining this authority cannot renew, release, or otherwise keep the lane
/// alive after the uniquely-owned turn-driver guard is dropped.
#[derive(Clone)]
pub(crate) struct BorrowedLaneAuthority {
    lease: Arc<StdMutex<SessionExecutionLease>>,
    loss_cause: Arc<AtomicU8>,
}

impl BorrowedLaneAuthority {
    pub(super) fn fence(&self) -> SessionExecutionLeaseAuthority {
        self.lease.lock_recover().fence()
    }

    fn commit_evidence(&self) -> Box<SessionExecutionLeaseCommitEvidence> {
        let lease = self.lease.lock_recover();
        Box::new(SessionExecutionLeaseCommitEvidence {
            owner: lease.owner.clone(),
            executor_id: lease.executor_id.clone(),
            fencing_token: lease.fencing_token,
            lease_lost: self.loss_cause.load(Ordering::Acquire) != loss_cause::NONE,
        })
    }
}

impl SessionExecutionLeaseGuard {
    /// Test-only shorthand for
    /// [`try_acquire_for_executor`](Self::try_acquire_for_executor).
    ///
    /// The executor stays a required parameter here too: it is identity, and a
    /// minted-per-call default would silently make every acquisition a distinct
    /// claimant, which is precisely the distinction these tests exercise.
    #[cfg(test)]
    pub(super) async fn try_acquire(
        store: Arc<dyn RuntimePersistence>,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        executor_id: &str,
        timings: LeaseTimings,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<Self>, StoreError> {
        Self::try_acquire_for_executor(store, session_id, owner, executor_id, timings, clock).await
    }

    pub(super) async fn try_acquire_for_executor(
        store: Arc<dyn RuntimePersistence>,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        executor_id: &str,
        timings: LeaseTimings,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<Self>, StoreError> {
        match Self::try_acquire_with_busy_holder(
            store,
            session_id,
            owner,
            executor_id,
            timings,
            clock,
        )
        .await?
        {
            SessionExecutionLeaseGuardAcquisition::Acquired(guard) => Ok(Some(guard)),
            SessionExecutionLeaseGuardAcquisition::Busy(_) => Ok(None),
        }
    }

    pub(super) async fn try_acquire_with_busy_holder(
        store: Arc<dyn RuntimePersistence>,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        executor_id: &str,
        timings: LeaseTimings,
        clock: Arc<dyn Clock>,
    ) -> Result<SessionExecutionLeaseGuardAcquisition, StoreError> {
        let claim_nonce = crate::LeaseClaimNonce::new();
        let acquisition = match store
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                executor_id,
                &claim_nonce,
                timings.ttl_ms(),
            )
            .await?
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition,
            SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                trace_busy(session_id, owner, executor_id, &holder);
                return Ok(SessionExecutionLeaseGuardAcquisition::Busy(holder));
            }
        };
        let guard = Self::from_acquisition(store, acquisition, timings, clock);
        guard.store.admit_session_state(&guard.fence()).await?;
        Ok(SessionExecutionLeaseGuardAcquisition::Acquired(guard))
    }

    /// Report the claim, then start renewing it.
    ///
    /// `taken_over` is emitted here, by the winner, because this is the only
    /// moment the displaced holder is known to be the one this claim actually
    /// displaced, and the only party guaranteed alive to say so.
    fn from_acquisition(
        store: Arc<dyn RuntimePersistence>,
        acquisition: crate::store::SessionExecutionLeaseAcquisition,
        timings: LeaseTimings,
        clock: Arc<dyn Clock>,
    ) -> Self {
        trace_acquisition(&acquisition);
        let lease = acquisition.lease;
        let lease = Arc::new(StdMutex::new(lease));
        let release_state = Arc::new(AtomicU8::new(release_state::LIVE));
        let loss_cause = Arc::new(AtomicU8::new(loss_cause::NONE));
        let renew_task = spawn_renewal_task(
            Arc::clone(&store),
            Arc::clone(&lease),
            Arc::clone(&release_state),
            Arc::clone(&loss_cause),
            timings,
            Arc::clone(&clock),
        );
        Self {
            store,
            lease,
            release_state,
            loss_cause,
            clock,
            guard_id: NEXT_LEASE_GUARD_ID.fetch_add(1, Ordering::Relaxed),
            renew_task,
        }
    }

    pub(super) fn borrowed_authority(&self) -> BorrowedLaneAuthority {
        BorrowedLaneAuthority {
            lease: Arc::clone(&self.lease),
            loss_cause: Arc::clone(&self.loss_cause),
        }
    }

    pub(super) fn fence(&self) -> SessionExecutionLeaseAuthority {
        self.lease.lock_recover().fence()
    }

    /// Poison-tolerant on purpose: this is read on the `Drop` path, where a
    /// panic would escalate an unwind into an abort. The lease behind the mutex
    /// is only ever replaced wholesale, so a poisoned lock still holds a
    /// complete lease.
    pub(super) fn completion(&self) -> SessionExecutionLeaseAuthority {
        self.lease.lock_recover().completion()
    }

    /// Snapshot the holder facts a commit-rejection trace event reports.
    ///
    /// Returned boxed: the caller holds this across the commit await, and even an
    /// unboxed temporary inside that async body is enough to push the turn futures
    /// past the workspace's large-future budget.
    pub(super) fn commit_evidence(&self) -> Box<SessionExecutionLeaseCommitEvidence> {
        let lease = self.lease.lock_recover();
        Box::new(SessionExecutionLeaseCommitEvidence {
            owner: lease.owner.clone(),
            executor_id: lease.executor_id.clone(),
            fencing_token: lease.fencing_token,
            lease_lost: self.loss_cause.load(Ordering::Acquire) != loss_cause::NONE,
        })
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
            executor_id = %completion.executor_id,
            fencing_token = completion.fencing_token,
            event = "session_execution_lease.released",
            "released session execution lease"
        );
    }

    pub(super) fn is_lost(&self) -> bool {
        self.loss_cause.load(Ordering::Acquire) != loss_cause::NONE
    }

    pub(super) fn continuity(&self) -> Option<SessionExecutionLeaseContinuity> {
        let lease = self.lease.lock_recover();
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
        if self.loss_cause.load(Ordering::Acquire) == loss_cause::STORE_VERDICT {
            // Definitive fence loss only: renewal proved the durable owner,
            // token or fence stopped matching, so there is no owner-side
            // release left to perform. A transient renewal failure must never
            // reach here — it does not prove the lease stopped being ours, and
            // skipping the backend release would block a successor until TTL.
            let completion = self.completion();
            self.release_state
                .store(release_state::RELEASED, Ordering::Release);
            tracing::debug!(
                session_id = %completion.session_id,
                owner_id = %completion.owner.owner_id,
                incarnation_id = %completion.owner.incarnation_id,
                executor_id = %completion.executor_id,
                fencing_token = completion.fencing_token,
                consulted = "renewal_fence_lost",
                outcome = "skipped",
                event = "session_execution_lease.release",
                "skipped owner-side release: the lease fence was definitively lost"
            );
            return Ok(());
        }
        let completion = self.completion();
        let outcome = match self
            .store
            .release_session_execution_lease(&completion)
            .await
        {
            Ok(()) => "released",
            // Rotation or TTL failover can make an in-band release stale after
            // the turn has already committed. That is terminal cleanup, not a
            // store-commit failure and never a reason to duplicate host work.
            Err(StoreError::SessionExecutionLeaseReleaseRefused { .. }) => "stale_refused",
            Err(error) => return Err(error),
        };
        self.release_state
            .store(release_state::RELEASED, Ordering::Release);
        tracing::debug!(
            session_id = %completion.session_id,
            owner_id = %completion.owner.owner_id,
            incarnation_id = %completion.owner.incarnation_id,
            executor_id = %completion.executor_id,
            fencing_token = completion.fencing_token,
            outcome,
            event = "session_execution_lease.release",
            "finished in-band session execution lease release"
        );
        Ok(())
    }
}

/// An acquired lane commits and releases. A busy claimant commits lane-lessly without displacing
/// or releasing the holder, whose lane remains untouched; the head CAS is sole authority.
pub(super) async fn commit_runtime_state_with_fresh_session_execution_lease(
    store: Arc<dyn RuntimePersistence>,
    commit: RuntimeCommit,
    owner: &crate::LeaseOwnerIdentity,
    executor_id: &str,
    timings: LeaseTimings,
    clock: Arc<dyn Clock>,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let session_id = commit.session_id.clone();
    let acquisition = SessionExecutionLeaseGuard::try_acquire_with_busy_holder(
        Arc::clone(&store),
        &session_id,
        owner,
        executor_id,
        timings,
        clock,
    )
    .await?;
    let lease = match acquisition {
        SessionExecutionLeaseGuardAcquisition::Acquired(lease) => lease,
        SessionExecutionLeaseGuardAcquisition::Busy(holder) => {
            observability::trace_commit_busy_advisory(&session_id, &holder);
            return match crate::store::commit_runtime_state_verified(store.as_ref(), commit).await {
                Ok(result) => Ok(result),
                Err(error) => {
                    trace_commit_cas_rejected(&session_id, None, owner, executor_id, &error);
                    Err(error)
                }
            };
        }
    };
    let evidence = lease.commit_evidence();
    let commit = commit.releasing_session_execution_lease(lease.completion());
    match crate::store::commit_runtime_state_verified(store.as_ref(), commit).await {
        Ok(result) => {
            lease.mark_released();
            Ok(result)
        }
        Err(error) => {
            trace_commit_cas_rejected(&session_id, Some(&evidence), owner, executor_id, &error);
            match lease.release_if_live().await {
                Ok(()) => {}
                Err(release_error) => {
                    tracing::warn!(
                        error = %release_error,
                        original_error = %error,
                        session_id,
                        "failed to release fresh session execution lease after rejected commit"
                    );
                }
            }
            Err(error)
        }
    }
}

/// Commit under a lane already held by the caller.
///
/// The ordinary store fence validates the guard's current owner, generation,
/// token, and expiry inside the commit transaction. No claim, rotation, or
/// release occurs on either outcome; the outer guard therefore remains the
/// sole owner of renewal and eventual release.
pub(super) async fn commit_runtime_state_with_borrowed_lease(
    lease: &BorrowedLaneAuthority,
    store: Arc<dyn RuntimePersistence>,
    commit: RuntimeCommit,
    owner: &crate::LeaseOwnerIdentity,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let session_id = commit.session_id.clone();
    debug_assert_eq!(lease.fence().session_id, session_id);
    let evidence = lease.commit_evidence();
    let commit = commit.borrowing_session_execution_lease(lease.fence());
    match crate::store::commit_runtime_state_verified(store.as_ref(), commit).await {
        Ok(result) => Ok(result),
        Err(error) => {
            trace_commit_cas_rejected(
                &session_id,
                Some(&evidence),
                owner,
                &evidence.executor_id,
                &error,
            );
            Err(error)
        }
    }
}

impl Drop for SessionExecutionLeaseGuard {
    fn drop(&mut self) {
        self.renew_task.abort();
        let state = self.release_state.load(Ordering::Acquire);
        if state == release_state::RELEASED
            || self.loss_cause.load(Ordering::Acquire) == loss_cause::STORE_VERDICT
        {
            return;
        }
        let lease = self.lease.lock_recover().clone();
        let completion = lease.completion();
        let observed_at_epoch_ms = self.clock.timestamp_ms();
        let store = Arc::clone(&self.store);
        let lease_lost = self.is_lost();
        tracing::debug!(
            session_id = %completion.session_id,
            owner_id = %completion.owner.owner_id,
            incarnation_id = %completion.owner.incarnation_id,
            executor_id = %completion.executor_id,
            fencing_token = completion.fencing_token,
            lease_lost,
            expires_at_epoch_ms = lease.expires_at_epoch_ms,
            observed_at_epoch_ms,
            remaining_ttl_ms = lease.expires_at_epoch_ms.saturating_sub(observed_at_epoch_ms),
            consulted = if state == release_state::RELEASING {
                "release_unacknowledged"
            } else {
                "release_not_requested"
            },
            outcome = "best_effort_release_spawned",
            event = "session_execution_lease.release",
            "dropped session execution lease guard; spawned token-scoped best-effort release"
        );
        crate::task::spawn(async move {
            match store.release_session_execution_lease(&completion).await {
                Ok(()) => tracing::debug!(
                    session_id = %completion.session_id,
                    owner_id = %completion.owner.owner_id,
                    incarnation_id = %completion.owner.incarnation_id,
                    executor_id = %completion.executor_id,
                    fencing_token = completion.fencing_token,
                    outcome = "released",
                    event = "session_execution_lease.release",
                    "best-effort drop release completed"
                ),
                Err(StoreError::SessionExecutionLeaseReleaseRefused { .. }) => tracing::debug!(
                    session_id = %completion.session_id,
                    owner_id = %completion.owner.owner_id,
                    incarnation_id = %completion.owner.incarnation_id,
                    executor_id = %completion.executor_id,
                    fencing_token = completion.fencing_token,
                    outcome = "stale_refused",
                    event = "session_execution_lease.release",
                    "best-effort drop release was stale and left the successor lease untouched"
                ),
                Err(error) => tracing::warn!(
                    error = %error,
                    session_id = %completion.session_id,
                    owner_id = %completion.owner.owner_id,
                    incarnation_id = %completion.owner.incarnation_id,
                    executor_id = %completion.executor_id,
                    fencing_token = completion.fencing_token,
                    outcome = "failed_ttl_fallback",
                    event = "session_execution_lease.release",
                    "best-effort drop release failed; lease expiry remains the fallback"
                ),
            }
        });
    }
}

fn spawn_renewal_task(
    store: Arc<dyn RuntimePersistence>,
    lease: Arc<StdMutex<SessionExecutionLease>>,
    release_state: Arc<AtomicU8>,
    loss_cause: Arc<AtomicU8>,
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
            let presented = lease.lock_recover().clone();
            let fence = presented.fence();
            let renewal = match store
                .renew_session_execution_lease(&fence, timings.ttl_ms())
                .await
            {
                Ok(renewed) => {
                    match validate_renewed_session_execution_lease(&presented, &renewed) {
                        Ok(()) => {
                            tracing::debug!(
                                session_id = %renewed.session_id,
                                owner_id = %renewed.owner.owner_id,
                                incarnation_id = %renewed.owner.incarnation_id,
                                executor_id = %renewed.executor_id,
                                fencing_token = renewed.fencing_token,
                                event = "session_execution_lease.renewed",
                                "renewed session execution lease"
                            );
                            *lease.lock_recover() = renewed;
                            Ok(())
                        }
                        Err(mismatch) => {
                            trace_renewal_install_refused(
                                &presented,
                                &renewed,
                                mismatch,
                                clock.timestamp_ms(),
                            );
                            Err(StoreError::SessionExecutionLeaseRenewalInstallRefused {
                                session_id: presented.session_id.clone(),
                                mismatch,
                            })
                        }
                    }
                }
                Err(err) => Err(err),
            };
            if let Err(err) = renewal {
                // A store verdict proves that owner-side release is obsolete.
                // A refused response also ends local continuity, but may leave
                // the durable lease ours and therefore still requires release.
                // A transient failure proves neither kind of loss.
                let established_loss_cause = match &err {
                    StoreError::SessionExecutionLeaseExpired { .. }
                    | StoreError::SessionExecutionLeaseRenewalRefused { .. } => {
                        loss_cause::STORE_VERDICT
                    }
                    StoreError::SessionExecutionLeaseRenewalInstallRefused { .. } => {
                        loss_cause::RESPONSE_REFUSED
                    }
                    _ => loss_cause::NONE,
                };
                if established_loss_cause != loss_cause::NONE {
                    loss_cause.store(established_loss_cause, Ordering::Release);
                    tracing::warn!(
                        error = %err,
                        session_id = %fence.session_id,
                        owner_id = %fence.owner.owner_id,
                        incarnation_id = %fence.owner.incarnation_id,
                        executor_id = %fence.executor_id,
                        fencing_token = fence.fencing_token,
                        consulted = if established_loss_cause == loss_cause::STORE_VERDICT {
                            "renewal_fence_rejected"
                        } else {
                            "renewal_response_refused"
                        },
                        outcome = "lease_lost",
                        event = "session_execution_lease.lost",
                        "lost session execution lease"
                    );
                } else {
                    tracing::warn!(
                        error = %err,
                        session_id = %fence.session_id,
                        owner_id = %fence.owner.owner_id,
                        incarnation_id = %fence.owner.incarnation_id,
                        executor_id = %fence.executor_id,
                        fencing_token = fence.fencing_token,
                        consulted = "renewal_error_transient",
                        outcome = "renewal_stopped_release_still_required",
                        event = "session_execution_lease.renewal_failed",
                        "session execution lease renewal failed transiently; \
                         the lease is still ours to release"
                    );
                }
                break;
            }
        }
    })
}

/// Require a renewal response to preserve the resident lease's identity and
/// monotonically extend (or retain) its expiry before replacing it.
///
/// This is the runner-side counterpart of the store-side renewal predicate;
/// FIG-1072 tracks core-owning that predicate so both sides share one
/// implementation.
fn validate_renewed_session_execution_lease(
    presented: &SessionExecutionLease,
    renewed: &SessionExecutionLease,
) -> Result<(), crate::SessionExecutionLeaseRenewalInstallMismatch> {
    use crate::SessionExecutionLeaseRenewalInstallMismatch as Mismatch;

    if renewed.session_id != presented.session_id {
        Err(Mismatch::Session)
    } else if !renewed.owner.same_incarnation(&presented.owner) {
        Err(Mismatch::OwnerIncarnation)
    } else if renewed.executor_id != presented.executor_id {
        Err(Mismatch::Executor)
    } else if renewed.lease_token != presented.lease_token {
        Err(Mismatch::LeaseToken)
    } else if renewed.fencing_token != presented.fencing_token {
        Err(Mismatch::FencingToken)
    } else if renewed.expires_at_epoch_ms < presented.expires_at_epoch_ms {
        Err(Mismatch::ExpiryRegressed)
    } else {
        Ok(())
    }
}

fn trace_renewal_install_refused(
    presented: &SessionExecutionLease,
    renewed: &SessionExecutionLease,
    mismatch: crate::SessionExecutionLeaseRenewalInstallMismatch,
    observed_at_epoch_ms: u64,
) {
    crate::store_backend_support::trace_session_execution_lease_refusal(
        crate::store_backend_support::SessionExecutionLeaseRefusalOperation::RenewalInstall,
        "core_renewal_install_validation",
        "backend_renewal_response",
        &presented.fence(),
        crate::store_backend_support::SessionExecutionLeaseRefusalFacts {
            current_owner: Some(&renewed.owner),
            current_executor_id: Some(&renewed.executor_id),
            current_token: Some(&renewed.lease_token),
            current_fencing_token: Some(renewed.fencing_token),
            current_expires_at_epoch_ms: Some(renewed.expires_at_epoch_ms),
            observed_at_epoch_ms: Some(observed_at_epoch_ms),
            minimum_expires_at_epoch_ms: Some(presented.expires_at_epoch_ms),
            requested_session_id: Some(&renewed.session_id),
            refusal_cause: Some(mismatch.label()),
        },
    );
}

#[cfg(test)]
mod renewal_install_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::in_memory_store::InMemorySessionStore;
    use crate::store::{SessionCommitStore, SessionExecutionLeaseStore};

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
            "acquire-gated-guard-executor",
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
                "lease-is-held-executor",
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("peer claim attempt");
        matches!(outcome, SessionExecutionLeaseClaimOutcome::Busy { .. })
    }

    fn borrowed_commit(session_id: &str) -> RuntimeCommit {
        RuntimeCommit::persisted_state_for_test(
            &crate::RuntimeSessionState {
                session_id: session_id.to_string(),
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                ))
            },
            &[],
        )
    }

    #[tokio::test]
    async fn borrowed_commit_leaves_outer_guard_fence_valid() {
        let store = Arc::new(InMemorySessionStore::new());
        let persistence: Arc<dyn RuntimePersistence> = store.clone();
        let owner = crate::LeaseOwnerIdentity::opaque("borrow-owner", "borrow-incarnation");
        store
            .admit_and_bind_session(&crate::SessionBinding::root("borrow-valid"))
            .await
            .expect("bind borrowed-commit session");
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&persistence),
            "borrow-valid",
            &owner,
            "borrowed-commit-leaves-outer-guard-fence-valid-executor",
            LeaseTimings::default(),
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim outer lane")
        .expect("outer lane acquired");

        commit_runtime_state_with_borrowed_lease(
            &guard.borrowed_authority(),
            persistence,
            borrowed_commit("borrow-valid"),
            &owner,
        )
        .await
        .expect("borrowed commit succeeds");

        let renewed = store
            .renew_session_execution_lease(&guard.fence(), LeaseTimings::default().ttl_ms())
            .await
            .expect("outer guard remains current after borrowed commit");
        assert_eq!(renewed.lease_token, guard.fence().lease_token);
        guard.release_if_live().await.expect("release outer lane");
    }

    #[tokio::test]
    async fn lapsed_guard_cannot_authorize_borrowed_commit() {
        let store = Arc::new(InMemorySessionStore::new());
        let persistence: Arc<dyn RuntimePersistence> = store.clone();
        let owner = crate::LeaseOwnerIdentity::opaque("lapsed-owner", "lapsed-incarnation");
        store
            .admit_and_bind_session(&crate::SessionBinding::root("borrow-lapsed"))
            .await
            .expect("bind lapsed borrowed-commit session");
        let timings = LeaseTimings::from_ttl(std::time::Duration::from_millis(30))
            .expect("valid short lease timings");
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&persistence),
            "borrow-lapsed",
            &owner,
            "lapsed-guard-cannot-authorize-borrowed-commit-executor",
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim outer lane")
        .expect("outer lane acquired");
        guard.renew_task.abort();
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        let error = commit_runtime_state_with_borrowed_lease(
            &guard.borrowed_authority(),
            persistence,
            borrowed_commit("borrow-lapsed"),
            &owner,
        )
        .await
        .expect_err("lapsed guard must fail the ordinary execution fence");
        assert!(matches!(
            error,
            StoreError::SessionExecutionLeaseExpired { .. }
        ));
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
            store.session_execution_lease_release_attempt_count(),
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

        assert_eq!(store.session_execution_lease_release_attempt_count(), 1);
        // The peer claim above holds the lease now; assert against the guard's
        // own state instead: a released guard short-circuits further releases.
        gate.admit_one();
        guard
            .release_if_live()
            .await
            .expect("acknowledged release is terminal");
        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            1,
            "an acknowledged release must not be repeated"
        );
    }

    /// A dropped guard releases out of band with the token of its own claim.
    /// When a same-incarnation successor has already rotated the token, the
    /// backend refuses the stale release and leaves the successor untouched.
    #[tokio::test]
    async fn guard_dropped_mid_release_cannot_release_a_successors_lease() {
        let (store, guard, gate) = acquire_gated_guard().await;
        let owner = crate::LeaseOwnerIdentity::opaque("owner", "incarnation");

        let mut release = Box::pin(guard.release_if_live());
        tokio::select! {
            _ = gate.wait_entered() => {}
            result = release.as_mut() => panic!("gated release must not complete: {result:?}"),
        }
        drop(release);

        // The same runtime drives again and re-claims the still-live lease.
        let successor_nonce = crate::LeaseClaimNonce::for_testing("drop-race-successor-token");
        let successor = store
            .try_claim_session_execution_lease_with_token(
                SESSION_ID,
                &owner,
                &guard.completion().executor_id,
                &successor_nonce,
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("same-incarnation re-claim")
            .acquired()
            .expect("re-claim refreshes the live lease");
        assert_ne!(
            successor.lease_token,
            guard.completion().lease_token,
            "the successor must rotate the lock-lifecycle token"
        );
        assert_eq!(successor.fencing_token, guard.completion().fencing_token);
        assert_eq!(successor.owner, guard.completion().owner);

        gate.admit_one();
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.session_execution_lease_release_attempt_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("best-effort drop release attempted");
        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            1,
            "drop must make one best-effort release attempt"
        );
        assert!(
            lease_is_held(&store).await,
            "the successor's live lease must survive the stale guard's drop"
        );

        // The successor still owns the release.
        gate.admit_one();
        store
            .release_session_execution_lease(&successor.completion())
            .await
            .expect("successor releases its own lease");
        assert!(!lease_is_held(&store).await);
    }

    /// A claim rotation or TTL failover may race the successful turn's in-band
    /// cleanup. The named refusal proves cleanup is already terminal; surfacing
    /// it as `StoreCommitFailed` would falsely report a committed turn as failed.
    #[tokio::test]
    async fn stale_in_band_release_refusal_is_terminal_and_benign() {
        let store = Arc::new(InMemorySessionStore::new());
        let owner = crate::LeaseOwnerIdentity::opaque("owner", "incarnation");
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &owner,
            "stale-in-band-release-refusal-is-terminal-and-benign-executor",
            LeaseTimings::default(),
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim predecessor guard")
        .expect("predecessor guard acquired");
        let successor = store
            .try_claim_session_execution_lease_with_token(
                SESSION_ID,
                &owner,
                &guard.completion().executor_id,
                &crate::LeaseClaimNonce::for_testing("in-band-successor-token"),
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("rotate same-incarnation claim")
            .acquired()
            .expect("same-incarnation successor acquired");
        assert_ne!(successor.lease_token, guard.completion().lease_token);

        guard
            .release_if_live()
            .await
            .expect("stale named refusal is terminal and benign");
        assert_eq!(store.session_execution_lease_release_attempt_count(), 1);
        guard
            .release_if_live()
            .await
            .expect("terminal benign refusal is not retried");
        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            1,
            "a terminal named refusal must be acknowledged exactly once"
        );
        assert!(lease_is_held(&store).await);
        store
            .release_session_execution_lease(&successor.completion())
            .await
            .expect("successor releases its own lease");
    }

    #[tokio::test]
    async fn clean_guard_drop_releases_before_ttl_for_immediate_peer_reclaim() {
        let store = Arc::new(InMemorySessionStore::new());
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            "clean-guard-drop-releases-before-ttl-for-immediate-peer-reclaim-executor",
            LeaseTimings::default(),
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");

        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.session_execution_lease_release_attempt_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("best-effort release completed");

        let peer = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &crate::LeaseOwnerIdentity::opaque("peer", "peer-incarnation"),
                "clean-guard-drop-releases-before-ttl-for-immediate-peer-reclaim-executor",
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("peer claim")
            .acquired()
            .expect("clean drop must make the lane immediately reclaimable");
        store
            .release_session_execution_lease(&peer.completion())
            .await
            .expect("peer release");
    }

    #[tokio::test]
    async fn stalled_drop_release_falls_back_to_ttl_without_freeing_the_reclaimer() {
        let clock = Arc::new(crate::testing::TestClock::new(1_000));
        let store_clock: Arc<dyn crate::Clock> = clock.clone();
        let store = Arc::new(InMemorySessionStore::with_clock(store_clock));
        let guard_clock: Arc<dyn crate::Clock> = clock.clone();
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor",
            LeaseTimings::default(),
            guard_clock,
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");
        let gate = store.gate_session_execution_lease_release();

        drop(guard);
        gate.wait_entered().await;
        let peer_owner = crate::LeaseOwnerIdentity::opaque("peer", "peer-incarnation");
        assert!(matches!(
            store
                .try_claim_session_execution_lease(
                    SESSION_ID,
                    &peer_owner,
                    "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor",
                    LeaseTimings::default().ttl_ms(),
                )
                .await
                .expect("peer before expiry"),
            SessionExecutionLeaseClaimOutcome::Busy { .. }
        ));

        clock.advance(LeaseTimings::default().ttl_ms() + 1);
        let peer = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &peer_owner,
                "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor-2",
                LeaseTimings::default().ttl_ms(),
            )
            .await
            .expect("peer after expiry")
            .acquired()
            .expect("TTL remains the fallback when drop release cannot complete");

        gate.admit_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.session_execution_lease_release_attempt_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late predecessor release was attempted");
        assert!(
            matches!(
                store
                    .try_claim_session_execution_lease(
                        SESSION_ID,
                        &crate::LeaseOwnerIdentity::opaque("observer", "observer-incarnation"),
                        "stalled-drop-release-falls-back-to-ttl-without-freeing-the-reclaimer-executor-3",
                        LeaseTimings::default().ttl_ms(),
                    )
                    .await
                    .expect("observer claim after stale release"),
                SessionExecutionLeaseClaimOutcome::Busy { .. }
            ),
            "the late predecessor release must not free the TTL reclaimer"
        );

        gate.admit_one();
        store
            .release_session_execution_lease(&peer.completion())
            .await
            .expect("peer release");
    }

    /// A transient renewal failure does not prove the lease stopped being ours,
    /// so it must not be recorded as release completion: the owner still has to
    /// ask the backend to release, or a successor waits out the whole TTL.
    #[tokio::test]
    async fn transient_renewal_failure_still_requires_a_backend_release() {
        let store = Arc::new(InMemorySessionStore::new());
        let timings = LeaseTimings::new(
            std::time::Duration::from_millis(30),
            std::time::Duration::from_millis(10),
        )
        .expect("test lease timings");
        store.fail_next_session_execution_lease_renewal_with(StoreError::Contended);
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            "transient-renewal-failure-still-requires-a-backend-release-executor",
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");

        // Wait for the renewal task to observe the transient failure.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.session_execution_lease_renewal_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("renewal attempt observed");
        assert!(
            !guard.is_lost(),
            "a transient renewal failure must not mark the lease lost"
        );

        guard.release_if_live().await.expect("release");

        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            1,
            "the owner must still ask the backend to release the lease"
        );
        assert!(
            !lease_is_held(&store).await,
            "a successor must not wait out the TTL after a transient renewal failure"
        );
    }

    /// Refusing a malformed renewal response also leaves the durable lease
    /// potentially ours: every backend has already extended its row before
    /// returning success, so cleanup must make one token-fenced release attempt.
    #[tokio::test]
    async fn renewal_install_refusal_still_requires_a_backend_release() {
        let store = Arc::new(InMemorySessionStore::new());
        let timings = LeaseTimings::new(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_millis(10),
        )
        .expect("test lease timings");
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            "renewal-install-refusal-still-requires-a-backend-release-executor",
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");
        let mut malformed = guard.lease.lock_recover().clone();
        malformed.lease_token.push_str("-malformed");
        store.respond_to_next_session_execution_lease_renewal_with(malformed);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !guard.is_lost() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the refused renewal response marks continuity lost");

        guard.release_if_live().await.expect("release");

        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            1,
            "the owner must release after refusing the backend response"
        );
        assert!(
            !lease_is_held(&store).await,
            "a successor must not wait out the TTL after an install refusal"
        );
    }

    /// The other half of the same rule: a definitive fence rejection *is* loss,
    /// and then there is no owner-side release left to perform.
    #[tokio::test]
    async fn definitive_renewal_fence_rejection_skips_the_owner_side_release() {
        let store = Arc::new(InMemorySessionStore::new());
        let timings = LeaseTimings::new(
            std::time::Duration::from_millis(30),
            std::time::Duration::from_millis(10),
        )
        .expect("test lease timings");
        store.fail_next_session_execution_lease_renewal_with(
            StoreError::SessionExecutionLeaseExpired {
                session_id: SESSION_ID.to_string(),
            },
        );
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            "definitive-renewal-fence-rejection-skips-the-owner-side-release-executor",
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim lease")
        .expect("lease acquired");

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !guard.is_lost() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a rejected renewal fence marks the lease lost");

        guard.release_if_live().await.expect("release");

        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            0,
            "a definitively lost lease has no owner-side release to perform"
        );
    }

    #[tokio::test]
    async fn named_refusals_trace_typed_redacted_decision_evidence() {
        use crate::runtime::tests::trace_capture::CapturedFieldKind;

        fn assert_refusal_event(
            event: &crate::runtime::tests::trace_capture::CapturedEvent,
            operation: &str,
            decision_basis: &str,
            presented: &SessionExecutionLeaseAuthority,
            current: &SessionExecutionLease,
        ) {
            assert_eq!(event.target, "lash_core::session_execution_lease");
            assert_eq!(event.level, "WARN");
            assert_eq!(event.field_count(), 27);
            for field in [
                "event",
                "operation",
                "decision_basis",
                "session_id",
                "presented_owner_id",
                "presented_incarnation_id",
                "presented_executor_id",
                "current_owner_id",
                "current_incarnation_id",
                "current_executor_id",
                "current_token_identity",
                "presented_token_identity",
                "consulted_state",
                "observation_freshness",
                "outcome",
                "refusal_cause",
            ] {
                assert_eq!(event.field_kind(field), CapturedFieldKind::Str, "{field}");
            }
            for field in ["owner_matched", "executor_matched", "token_matched"] {
                assert_eq!(event.field_kind(field), CapturedFieldKind::Bool, "{field}");
            }
            for field in [
                "session_matched",
                "current_fencing_token",
                "generation_matched",
                "current_expires_at_epoch_ms",
                "observed_at_epoch_ms",
                "minimum_expires_at_epoch_ms",
                "expiry_matched",
            ] {
                assert_eq!(event.field_kind(field), CapturedFieldKind::Debug, "{field}");
                assert_eq!(event.field(field), "None", "{field}");
            }
            assert_eq!(
                event.field_kind("presented_fencing_token"),
                CapturedFieldKind::U64
            );
            assert_eq!(event.field("operation"), operation);
            assert_eq!(event.field("decision_basis"), decision_basis);
            assert_eq!(event.field("refusal_cause"), decision_basis);
            assert_eq!(event.field("session_id"), presented.session_id);
            assert_eq!(event.field("presented_owner_id"), presented.owner.owner_id);
            assert_eq!(
                event.field("presented_incarnation_id"),
                presented.owner.incarnation_id
            );
            assert_eq!(event.field("presented_executor_id"), presented.executor_id);
            assert_eq!(event.field("current_owner_id"), current.owner.owner_id);
            assert_eq!(
                event.field("current_incarnation_id"),
                current.owner.incarnation_id
            );
            assert_eq!(event.field("current_executor_id"), current.executor_id);
            assert_eq!(event.field("owner_matched"), "false");
            assert_eq!(event.field("executor_matched"), "false");
            assert_eq!(event.field("token_matched"), "false");
            assert_eq!(
                event.field("consulted_state"),
                "session_execution_lease_row"
            );
            assert_eq!(
                event.field("observation_freshness"),
                "in_memory_write_transaction"
            );
            assert_eq!(event.field("outcome"), "refused");

            let current_identity = format!(
                "sha256:{}",
                crate::stable_hash::sha256_hex(current.lease_token.as_bytes())
            );
            let presented_identity = format!(
                "sha256:{}",
                crate::stable_hash::sha256_hex(presented.lease_token.as_bytes())
            );
            assert_eq!(event.field("current_token_identity"), current_identity);
            assert_eq!(event.field("presented_token_identity"), presented_identity);
            assert_ne!(event.field("current_token_identity"), current.lease_token);
            assert_ne!(
                event.field("presented_token_identity"),
                presented.lease_token
            );
        }

        let store = Arc::new(InMemorySessionStore::new());
        let owner = crate::LeaseOwnerIdentity::opaque("current-owner", "current-incarnation");
        let current = store
            .try_claim_session_execution_lease(
                SESSION_ID,
                &owner,
                "assert-refusal-event-executor",
                60_000,
            )
            .await
            .expect("claim current lease")
            .acquired()
            .expect("current lease acquired");
        let presented = SessionExecutionLeaseAuthority {
            session_id: SESSION_ID.to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("presented-owner", "presented-incarnation"),
            executor_id: "presented-executor".to_string(),
            lease_token: "presented-stale-token".to_string(),
            fencing_token: current.fencing_token,
        };

        let (renewal_error, renewal_capture) =
            crate::runtime::tests::trace_capture::capturing(|| async {
                store
                    .renew_session_execution_lease(&presented, 60_000)
                    .await
                    .expect_err("stale renewal refused")
            })
            .await;
        assert!(matches!(
            renewal_error,
            StoreError::SessionExecutionLeaseRenewalRefused { .. }
        ));
        assert_refusal_event(
            &renewal_capture.exactly_one("session_execution_lease.renewal_refused"),
            "renewal",
            "owner_or_token_mismatch",
            &presented,
            &current,
        );

        let (_, execution_capture) = crate::runtime::tests::trace_capture::capturing(|| async {
            crate::store_backend_support::require_current_session_execution_lease(
                SESSION_ID,
                Some(
                    crate::store_backend_support::SessionExecutionLeaseFenceFacts {
                        owner: Some(&current.owner),
                        executor_id: Some(&current.executor_id),
                        lease_token: Some(current.lease_token.as_str()),
                        fencing_token: current.fencing_token,
                        expires_at_epoch_ms: current.expires_at_epoch_ms,
                    },
                ),
                &presented,
                current.expires_at_epoch_ms - 1,
            )
            .expect_err("stale execution fence refused")
        })
        .await;
        let execution_event =
            execution_capture.exactly_one("session_execution_lease.execution_fence_refused");
        assert_eq!(execution_event.field_count(), 27);
        assert_eq!(execution_event.field("operation"), "execution_fence");
        assert_eq!(
            execution_event.field("decision_basis"),
            "core_execution_fence_predicate"
        );
        assert_eq!(execution_event.field("refusal_cause"), "owner_mismatch");
        assert_eq!(execution_event.field("session_matched"), "Some(true)");
        assert_eq!(execution_event.field("owner_matched"), "false");
        assert_eq!(execution_event.field("executor_matched"), "false");
        assert_eq!(execution_event.field("token_matched"), "false");
        assert_eq!(execution_event.field("generation_matched"), "Some(true)");
        assert_eq!(execution_event.field("expiry_matched"), "Some(true)");

        let (release_error, release_capture) =
            crate::runtime::tests::trace_capture::capturing(|| async {
                store
                    .release_session_execution_lease(&presented)
                    .await
                    .expect_err("stale release refused")
            })
            .await;
        assert!(matches!(
            release_error,
            StoreError::SessionExecutionLeaseReleaseRefused { .. }
        ));
        let release_event = release_capture.exactly_one("session_execution_lease.release_refused");
        assert_refusal_event(
            &release_event,
            "release",
            "token_scoped_release_did_not_match",
            &presented,
            &current,
        );

        store
            .release_session_execution_lease(&current.completion())
            .await
            .expect("release current lease");
    }

    /// Exercise the real FIG-924-shaped classification path: a same-owner claim
    /// rotates durable identity, the old renewal loop receives the new named
    /// refusal, marks itself lost, and Drop does not make a doomed release call.
    #[tokio::test]
    async fn rotated_token_refusal_marks_the_old_renewal_loop_lost() {
        let store = Arc::new(InMemorySessionStore::new());
        let owner = crate::LeaseOwnerIdentity::opaque("owner", "incarnation");
        let timings = LeaseTimings::new(
            std::time::Duration::from_millis(60),
            std::time::Duration::from_millis(10),
        )
        .expect("test lease timings");
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            SESSION_ID,
            &owner,
            "rotated-token-refusal-marks-the-old-renewal-loop-lost-executor",
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim predecessor guard")
        .expect("predecessor guard acquired");
        let predecessor_token = guard.completion().lease_token;
        let successor = store
            .try_claim_session_execution_lease_with_token(
                SESSION_ID,
                &owner,
                &guard.completion().executor_id,
                &crate::LeaseClaimNonce::for_testing("renewal-successor-token"),
                60_000,
            )
            .await
            .expect("rotate durable lease token")
            .acquired()
            .expect("same-incarnation successor acquired");
        assert_ne!(successor.lease_token, predecessor_token);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !guard.is_lost() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old renewal loop observes named refusal");
        assert!(store.session_execution_lease_renewal_count() >= 1);
        let release_gate = store.gate_session_execution_lease_release();
        drop(guard);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                release_gate.wait_entered()
            )
            .await
            .is_err(),
            "Drop must not even start a release for a definitively lost guard"
        );
        assert_eq!(
            store.session_execution_lease_release_attempt_count(),
            0,
            "Drop must skip a release for a definitively lost guard"
        );
        assert!(lease_is_held(&store).await);
        release_gate.admit_one();
        store
            .release_session_execution_lease(&successor.completion())
            .await
            .expect("successor releases its own lease");
    }
}
