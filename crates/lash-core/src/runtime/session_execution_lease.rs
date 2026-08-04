//! Session-execution-lease acquisition, renewal, and the trace timeline an
//! operator reconstructs a takeover from.
//!
//! The lease is advisory: it serializes the common case so two runners do not
//! duplicate work, but the commit's head compare-and-set is the only authority
//! on who publishes (ADR 0029). That makes a handful of transitions decisive when
//! a turn looks stuck, and each emits a structured event carrying the session id,
//! the lease `fencing_token` (ADR 0029 calls it the generation), and the holder
//! identity:
//!
//! | event | level | emitted by | meaning |
//! |---|---|---|---|
//! | `session_execution_lease.acquired` | INFO | the claimant | this runner acquired the lane |
//! | `session_execution_lease.taken_over` | INFO | the *winner* | this claim displaced a named lapsed holder |
//! | `session_execution_lease.lost` | WARN | the loser | a renewal was fence-rejected; this runner no longer holds the lane |
//! | `session_execution_lease.renewal_failed` | WARN | the holder | renewal stopped on a transient error; the lease is still ours to release |
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
//! They are trace events, not durable session events, on purpose: lease churn is
//! per-attempt telemetry about which runner tried what, not session history. A
//! lost lease is not a turn failure: the turn may still commit, and the
//! `commit_cas_rejected` event is what proves it did not.
//!
//! `acquired` and `taken_over` are INFO rather than DEBUG because reconstructing
//! takeover order is an ordinary production question; requiring debug logging to
//! answer it would make the timeline unavailable exactly when it is needed.

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
/// Release completion is recorded only on backend acknowledgement, never on
/// intent. A cancelled or failed release therefore stays in `Releasing` with its
/// completion token retained, so the owner can retry the same release in band
/// instead of reporting a durable release that never happened. Only the guard
/// itself may retry: see the `Drop` impl for why an out-of-band release is
/// unsafe, and what a dropped `Releasing` guard costs instead.
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

/// What a commit-time trace event says about the authority the writer had.
///
/// Captured before the commit so a rejection reports the generation that lost,
/// not whatever the row holds afterwards. Both paths that reach a commit are
/// represented, because both can lose the head CAS:
///
/// * the runner holds the lane, and `fencing_token` is its own; or
/// * the lane was busy and the runner proceeded anyway under the advisory
///   (ADR 0029 makes the CAS the authority, so this is legal), in which case
///   `fencing_token` is the holder it knowingly raced and `lane_held` is false.
///
/// Either way the event carries the writer's identity and a generation, so a
/// rejection is never anonymous.
#[derive(Clone, Debug)]
pub(super) struct SessionExecutionLeaseCommitEvidence {
    /// Identity of the runner attempting the commit, always present.
    owner: crate::LeaseOwnerIdentity,
    /// The lane generation in play for this attempt: this runner's when it holds
    /// the lane, otherwise the generation it observed holding it.
    fencing_token: u64,
    /// Whether `fencing_token` belongs to this runner. False on the busy-advisory
    /// path, where no lane was ever held and `lease_lost` is meaningless.
    lane_held: bool,
    /// Whether this runner had already observed its own lease loss before the
    /// commit. Repeated rejections with `lane_held` true and this false are
    /// livelock, not takeover.
    lease_lost: bool,
}

impl SessionExecutionLeaseCommitEvidence {
    /// Evidence for a writer that proceeded under the busy advisory without the
    /// lane, naming itself and the generation it chose to race.
    pub(super) fn without_lane(
        claimant: &crate::LeaseOwnerIdentity,
        observed_holder: &SessionExecutionLease,
    ) -> Self {
        Self {
            owner: claimant.clone(),
            fencing_token: observed_holder.fencing_token,
            lane_held: false,
            lease_lost: false,
        }
    }
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
        tracing::info!(
            session_id = %lease.session_id,
            owner_id = %lease.owner.owner_id,
            incarnation_id = %lease.owner.incarnation_id,
            fencing_token = lease.fencing_token,
            expires_at_epoch_ms = lease.expires_at_epoch_ms,
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
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fence()
    }

    /// Poison-tolerant on purpose: this is read on the `Drop` path, where a
    /// panic would escalate an unwind into an abort. The lease behind the mutex
    /// is only ever replaced wholesale, so a poisoned lock still holds a
    /// complete lease.
    pub(super) fn completion(&self) -> SessionExecutionLeaseCompletion {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completion()
    }

    /// Snapshot the holder facts a commit-rejection trace event reports.
    pub(super) fn commit_evidence(&self) -> SessionExecutionLeaseCommitEvidence {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SessionExecutionLeaseCommitEvidence {
            owner: lease.owner.clone(),
            fencing_token: lease.fencing_token,
            lane_held: true,
            lease_lost: self.lost.load(Ordering::Acquire),
        }
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
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                fencing_token = completion.fencing_token,
                consulted = "renewal_fence_lost",
                outcome = "skipped",
                event = "session_execution_lease.release",
                "skipped owner-side release: the lease fence was definitively lost"
            );
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
    match crate::store::commit_runtime_state_verified(store.as_ref(), commit).await {
        Ok(result) => {
            lease.mark_released();
            Ok(result)
        }
        Err(error) => {
            if let Err(release_error) = lease.release_if_live().await {
                tracing::warn!(
                    error = %release_error,
                    original_error = %error,
                    session_id,
                    "failed to release fresh session execution lease after rejected commit"
                );
            }
            Err(error)
        }
    }
}

impl Drop for SessionExecutionLeaseGuard {
    fn drop(&mut self) {
        self.renew_task.abort();
        // A dropped guard never releases out of band, in either state, and the
        // lease is left to expire by TTL. This is deliberate and load-bearing:
        // a retained completion does **not** identify one grant. A
        // same-incarnation re-claim is a refresh in place that returns the
        // identical owner, lease token and fencing token on every backend
        // (pinned by `session_execution_lease_contract` in the store conformance
        // suite), so releasing a completion this guard has stopped tracking
        // would clear a *successor's live* lease — the successor's next fenced
        // call would then fail as `SessionExecutionLeaseLost` while the row sits
        // free for a peer to claim. No backend predicate can distinguish the
        // two, so the only safe owner of a release is a guard that still tracks
        // the lease, in band, via `release_if_live`.
        //
        // Cost of the choice: a guard dropped in `Releasing` (a cancelled
        // release await) or in `Live` (a turn torn down without asking to
        // release) leaves the lease held until its TTL elapses, delaying queued
        // work for that session by up to `LeaseTimings::ttl`. That is the
        // pre-existing behavior for `Live` and no worse than before this fix for
        // `Releasing`; making a cancelled release land promptly needs claim-time
        // token rotation (so a stale completion is distinguishable), which is a
        // durable-contract change for a separate ticket.
        let state = self.release_state.load(Ordering::Acquire);
        if state == release_state::RELEASED {
            return;
        }
        let completion = self.completion();
        let expires_at_epoch_ms = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expires_at_epoch_ms;
        let observed_at_epoch_ms = self.clock.timestamp_ms();
        tracing::debug!(
            session_id = %completion.session_id,
            owner_id = %completion.owner.owner_id,
            incarnation_id = %completion.owner.incarnation_id,
            fencing_token = completion.fencing_token,
            consulted = if state == release_state::RELEASING {
                "release_unacknowledged"
            } else {
                "release_not_requested"
            },
            lease_lost = self.is_lost(),
            expires_at_epoch_ms,
            observed_at_epoch_ms,
            remaining_ttl_ms = expires_at_epoch_ms.saturating_sub(observed_at_epoch_ms),
            outcome = "left_to_ttl",
            event = "session_execution_lease.release",
            "dropped session execution lease guard without an acknowledged \
             release; the lease expires by TTL"
        );
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
            let fence = lease
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fence();
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
                    *lease
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = renewed;
                }
                Err(err) => {
                    // Only a definitive fence result proves the lease stopped
                    // being ours. A transient failure (contention, backend
                    // unavailability) leaves the durable lease live, so it must
                    // not mark the lease lost: `release_if_live` would then
                    // record completion without ever asking the backend to
                    // release, blocking a successor until TTL.
                    let fence_lost = matches!(err, StoreError::SessionExecutionLeaseExpired { .. });
                    if fence_lost {
                        lost.store(true, Ordering::Release);
                        tracing::warn!(
                            error = %err,
                            session_id = %fence.session_id,
                            owner_id = %fence.owner.owner_id,
                            incarnation_id = %fence.owner.incarnation_id,
                            fencing_token = fence.fencing_token,
                            consulted = "renewal_fence_rejected",
                            outcome = "lease_lost",
                            event = "session_execution_lease.lost",
                            "lost session execution lease"
                        );
                        trace_takeover(store.as_ref(), &fence).await;
                    } else {
                        tracing::warn!(
                            error = %err,
                            session_id = %fence.session_id,
                            owner_id = %fence.owner.owner_id,
                            incarnation_id = %fence.owner.incarnation_id,
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
        }
    })
}

/// Name the successor after a fence-rejected renewal so a log timeline
/// reconstructs takeover order. Diagnostics only: the read never fences anything,
/// and a failure to read is not allowed to escalate a lost lease into an error.
async fn trace_takeover(store: &dyn RuntimePersistence, fence: &SessionExecutionLeaseFence) {
    let current = match store.get_session_execution_lease(&fence.session_id).await {
        Ok(current) => current,
        Err(err) => {
            tracing::debug!(
                error = %err,
                session_id = %fence.session_id,
                consulted = "session_execution_lease_row",
                outcome = "successor_unknown",
                event = "session_execution_lease.takeover_unknown",
                "could not read the session execution lease row after renewal failure"
            );
            return;
        }
    };
    let Some(current) = current else {
        return;
    };
    if current.owner.same_incarnation(&fence.owner) && current.fencing_token == fence.fencing_token
    {
        return;
    }
    tracing::info!(
        session_id = %fence.session_id,
        owner_id = %fence.owner.owner_id,
        incarnation_id = %fence.owner.incarnation_id,
        fencing_token = fence.fencing_token,
        superseding_owner_id = %current.owner.owner_id,
        superseding_incarnation_id = %current.owner.incarnation_id,
        superseding_fencing_token = current.fencing_token,
        superseding_expires_at_epoch_ms = current.expires_at_epoch_ms,
        consulted = "session_execution_lease_row",
        outcome = "taken_over",
        event = "session_execution_lease.taken_over",
        "the session execution lane was taken over by a named successor"
    );
}

/// Report a commit whose head compare-and-set lost to a concurrent writer.
///
/// This is the authority speaking, not the advisory lease: a repeated rejection
/// while `lane_held` is true and `lease_lost` is false is livelock (two writers
/// racing the same head), while a rejection after `lost` / `taken_over` is an
/// ordinary handoff. Non-CAS store failures are left to their own error paths.
pub(super) fn trace_commit_cas_rejected(
    session_id: &str,
    evidence: Option<&SessionExecutionLeaseCommitEvidence>,
    err: &StoreError,
) {
    let StoreError::HeadRevisionConflict { expected, actual } = err else {
        return;
    };
    tracing::warn!(
        session_id,
        fencing_token = evidence.map(|evidence| evidence.fencing_token),
        owner_id = evidence.map(|evidence| evidence.owner.owner_id.as_str()),
        incarnation_id = evidence.map(|evidence| evidence.owner.incarnation_id.as_str()),
        lane_held = evidence.map(|evidence| evidence.lane_held),
        lease_lost = evidence.map(|evidence| evidence.lease_lost),
        expected_head_revision = expected,
        actual_head_revision = actual,
        consulted = "session_head_revision",
        outcome = "commit_rejected",
        event = "session_execution_lease.commit_cas_rejected",
        "the commit's head compare-and-set was rejected; another writer published first"
    );
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

    /// A dropped guard must never release out of band. A retained completion
    /// does not identify one grant — a same-incarnation re-claim refreshes in
    /// place with the identical token and fence — so an out-of-band release
    /// would clear a successor's live lease. The successor here is the one that
    /// releases; the stale guard's drop must be inert.
    ///
    /// This test is the enforcement half of a pair: the backend fact it rests on
    /// (that releasing a completion retained across a same-incarnation re-claim
    /// really does free the refreshed lease, on every backend) is pinned by
    /// `session_execution_lease_contract` in the store conformance suite. That
    /// law cannot host this prohibition — it never constructs a guard — so the
    /// two must be changed together.
    #[tokio::test]
    async fn guard_dropped_mid_release_never_releases_a_successors_lease() {
        let (store, guard, gate) = acquire_gated_guard().await;
        let owner = crate::LeaseOwnerIdentity::opaque("owner", "incarnation");

        let mut release = Box::pin(guard.release_if_live());
        tokio::select! {
            _ = gate.wait_entered() => {}
            result = release.as_mut() => panic!("gated release must not complete: {result:?}"),
        }
        drop(release);

        // The same runtime drives again and re-claims the still-live lease: the
        // successor's identity is byte-identical to the completion the dropped
        // guard retained, so nothing on the backend could tell them apart.
        let successor = store
            .try_claim_session_execution_lease(SESSION_ID, &owner, LeaseTimings::default().ttl_ms())
            .await
            .expect("same-incarnation re-claim")
            .acquired()
            .expect("re-claim refreshes the live lease");
        assert_eq!(
            (
                successor.lease_token.clone(),
                successor.fencing_token,
                successor.owner.clone()
            ),
            (
                guard.completion().lease_token,
                guard.completion().fencing_token,
                guard.completion().owner
            ),
            "a same-incarnation re-claim must be indistinguishable from the retained completion"
        );

        gate.admit_one();
        drop(guard);

        // Give any (forbidden) detached release time to land.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store.session_execution_lease_release_count(),
            0,
            "a dropped guard must not release the lease out of band"
        );
        assert!(
            lease_is_held(&store).await,
            "the successor's live lease must survive the stale guard's drop"
        );

        // The successor still owns the release.
        store
            .release_session_execution_lease(&successor.completion())
            .await
            .expect("successor releases its own lease");
        assert!(!lease_is_held(&store).await);
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
            store.session_execution_lease_release_count(),
            1,
            "the owner must still ask the backend to release the lease"
        );
        assert!(
            !lease_is_held(&store).await,
            "a successor must not wait out the TTL after a transient renewal failure"
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
            store.session_execution_lease_release_count(),
            0,
            "a definitively lost lease has no owner-side release to perform"
        );
    }
}
