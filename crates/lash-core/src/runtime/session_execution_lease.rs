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
//! | `session_execution_lease.frame_handoff_transferred` | INFO | the logical-turn handoff boundary | `owner_id`, `incarnation_id`, old/new fencing tokens, and `nested_commit` explain why a live retained guard was rotated before the next frame |
//! | `session_execution_lease.lost` | WARN | the loser | a renewal was fence-rejected; this runner no longer holds the lane |
//! | `session_execution_lease.renewal_refused` | WARN | the store | durable owner/token decision evidence for a refused renewal |
//! | `session_execution_lease.release_refused` | WARN | the store | durable owner/token decision evidence for a refused release |
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
//! Same-incarnation overlap is benign by design: rotating the lock-lifecycle
//! token refuses stale release and renewal, but either holder may still win the
//! session-head generation CAS and commit. Forced displacement must advance the
//! generation; it never overloads the lease-token nonce.
//!
//! `acquired` and `taken_over` are INFO rather than DEBUG because reconstructing
//! takeover order is an ordinary production question; requiring debug logging to
//! answer it would make the timeline unavailable exactly when it is needed.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use super::{Clock, NestedLeaseReleaseSignal};
use crate::LeaseTimings;
use crate::store::{
    RuntimeCommit, RuntimeCommitResult, RuntimePersistence, SessionExecutionLease,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome, StoreError,
};

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
        let claim_nonce = crate::LeaseClaimNonce::new();
        let acquisition = match store
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                &claim_nonce,
                timings.ttl_ms(),
            )
            .await?
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition,
            SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                trace_busy(session_id, owner, &holder);
                return Ok(None);
            }
        };
        Ok(Some(Self::from_acquisition(
            store,
            acquisition,
            timings,
            clock,
        )))
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
        let lease = acquisition.lease;
        tracing::info!(
            session_id = %lease.session_id,
            owner_id = %lease.owner.owner_id,
            incarnation_id = %lease.owner.incarnation_id,
            fencing_token = lease.fencing_token,
            expires_at_epoch_ms = lease.expires_at_epoch_ms,
            event = "session_execution_lease.acquired",
            "acquired session execution lease"
        );
        if let Some(displaced) = acquisition.displaced.as_ref() {
            trace_taken_over(&lease, displaced);
        }
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
        Self {
            store,
            lease,
            release_state,
            lost,
            clock,
            guard_id: NEXT_LEASE_GUARD_ID.fetch_add(1, Ordering::Relaxed),
            renew_task,
        }
    }

    pub(super) fn fence(&self) -> SessionExecutionLeaseAuthority {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fence()
    }

    /// Poison-tolerant on purpose: this is read on the `Drop` path, where a
    /// panic would escalate an unwind into an abort. The lease behind the mutex
    /// is only ever replaced wholesale, so a poisoned lock still holds a
    /// complete lease.
    pub(super) fn completion(&self) -> SessionExecutionLeaseAuthority {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completion()
    }

    /// Snapshot the holder facts a commit-rejection trace event reports.
    ///
    /// Returned boxed: the caller holds this across the commit await, and even an
    /// unboxed temporary inside that async body is enough to push the turn futures
    /// past the workspace's large-future budget.
    pub(super) fn commit_evidence(&self) -> Box<SessionExecutionLeaseCommitEvidence> {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Box::new(SessionExecutionLeaseCommitEvidence {
            owner: lease.owner.clone(),
            fencing_token: lease.fencing_token,
            lease_lost: self.lost.load(Ordering::Acquire),
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
            fencing_token = completion.fencing_token,
            outcome,
            event = "session_execution_lease.release",
            "finished in-band session execution lease release"
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
    released: NestedLeaseReleaseSignal,
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
    let evidence = lease.commit_evidence();
    let commit = commit.releasing_session_execution_lease(lease.completion());
    match crate::store::commit_runtime_state_verified(store.as_ref(), commit).await {
        Ok(result) => {
            lease.mark_released();
            released.mark_released();
            Ok(result)
        }
        Err(error) => {
            trace_commit_cas_rejected(&session_id, Some(&evidence), owner, &error);
            // A failed release leaves the signal unset on purpose: the outer guard was already
            // invalidated by reentry rotation, preserving the pre-fix loud failure path.
            match lease.release_if_live().await {
                Ok(()) => released.mark_released(),
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

impl Drop for SessionExecutionLeaseGuard {
    fn drop(&mut self) {
        self.renew_task.abort();
        let state = self.release_state.load(Ordering::Acquire);
        if state == release_state::RELEASED || self.is_lost() {
            return;
        }
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let completion = lease.completion();
        let observed_at_epoch_ms = self.clock.timestamp_ms();
        let store = Arc::clone(&self.store);
        tracing::debug!(
            session_id = %completion.session_id,
            owner_id = %completion.owner.owner_id,
            incarnation_id = %completion.owner.incarnation_id,
            fencing_token = completion.fencing_token,
            lease_lost = false,
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
                    fencing_token = completion.fencing_token,
                    outcome = "released",
                    event = "session_execution_lease.release",
                    "best-effort drop release completed"
                ),
                Err(StoreError::SessionExecutionLeaseReleaseRefused { .. }) => tracing::debug!(
                    session_id = %completion.session_id,
                    owner_id = %completion.owner.owner_id,
                    incarnation_id = %completion.owner.incarnation_id,
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
                    let fence_lost = matches!(
                        err,
                        StoreError::SessionExecutionLeaseExpired { .. }
                            | StoreError::SessionExecutionLeaseRenewalRefused { .. }
                    );
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

/// Report a takeover from the winning claim, naming the holder it displaced.
///
/// The fields describe the emitter, as they do on every other lease event:
/// `fencing_token`/`owner_id`/`incarnation_id` are the *new* holder, and the
/// `displaced_*` fields are the lapsed holder this claim took the lane from. Both
/// come from one atomic claim, so a log line here is true regardless of whether
/// the displaced runner is still alive to notice.
fn trace_taken_over(
    lease: &SessionExecutionLease,
    displaced: &crate::store::SessionExecutionLeaseDisplacement,
) {
    tracing::info!(
        session_id = %lease.session_id,
        owner_id = %lease.owner.owner_id,
        incarnation_id = %lease.owner.incarnation_id,
        fencing_token = lease.fencing_token,
        displaced_owner_id = %displaced.owner.owner_id,
        displaced_incarnation_id = %displaced.owner.incarnation_id,
        displaced_fencing_token = displaced.fencing_token,
        displaced_expired_at_epoch_ms = displaced.expired_at_epoch_ms,
        consulted = "session_execution_lease_claim",
        outcome = "taken_over",
        event = "session_execution_lease.taken_over",
        "took the session execution lane over from a lapsed holder"
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
    claimant: &crate::LeaseOwnerIdentity,
    err: &StoreError,
) {
    let StoreError::HeadRevisionConflict { expected, actual } = err else {
        return;
    };
    // The writer is always nameable: it is the lane holder when one was held, and
    // otherwise the runner that proceeded under the busy advisory. A rejection is
    // never anonymous.
    let owner = evidence.map_or(claimant, |evidence| &evidence.owner);
    tracing::warn!(
        session_id,
        fencing_token = evidence.map(|evidence| evidence.fencing_token),
        owner_id = %owner.owner_id,
        incarnation_id = %owner.incarnation_id,
        lane_held = evidence.is_some(),
        lease_lost = evidence.is_some_and(|evidence| evidence.lease_lost),
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
        let successor = store
            .try_claim_session_execution_lease(SESSION_ID, &owner, LeaseTimings::default().ttl_ms())
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
            LeaseTimings::default(),
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim predecessor guard")
        .expect("predecessor guard acquired");
        let successor = store
            .try_claim_session_execution_lease(SESSION_ID, &owner, LeaseTimings::default().ttl_ms())
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
            assert_eq!(event.field_count(), 15);
            for field in [
                "event",
                "operation",
                "decision_basis",
                "session_id",
                "presented_owner_id",
                "presented_incarnation_id",
                "current_owner_id",
                "current_incarnation_id",
                "current_token_identity",
                "presented_token_identity",
                "consulted_state",
                "observation_freshness",
                "outcome",
            ] {
                assert_eq!(event.field_kind(field), CapturedFieldKind::Str, "{field}");
            }
            for field in ["owner_matched", "token_matched"] {
                assert_eq!(event.field_kind(field), CapturedFieldKind::Bool, "{field}");
            }
            assert_eq!(event.field("operation"), operation);
            assert_eq!(event.field("decision_basis"), decision_basis);
            assert_eq!(event.field("session_id"), presented.session_id);
            assert_eq!(event.field("presented_owner_id"), presented.owner.owner_id);
            assert_eq!(
                event.field("presented_incarnation_id"),
                presented.owner.incarnation_id
            );
            assert_eq!(event.field("current_owner_id"), current.owner.owner_id);
            assert_eq!(
                event.field("current_incarnation_id"),
                current.owner.incarnation_id
            );
            assert_eq!(event.field("owner_matched"), "false");
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
            .try_claim_session_execution_lease(SESSION_ID, &owner, 60_000)
            .await
            .expect("claim current lease")
            .acquired()
            .expect("current lease acquired");
        let presented = SessionExecutionLeaseAuthority {
            session_id: SESSION_ID.to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("presented-owner", "presented-incarnation"),
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
            timings,
            Arc::new(crate::runtime::SystemClock),
        )
        .await
        .expect("claim predecessor guard")
        .expect("predecessor guard acquired");
        let predecessor_token = guard.completion().lease_token;
        let successor = store
            .try_claim_session_execution_lease(SESSION_ID, &owner, 60_000)
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
