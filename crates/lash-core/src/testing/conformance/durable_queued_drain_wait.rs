//! Cross-backend laws for the aliveness-aware durable queued-drain wait.
//!
//! The policy itself
//! ([`lane_wait`](crate::runtime::native_substrate::lane_wait))
//! is pure, and the runtime-level regressions in `runtime::tests::turns` drive
//! it through a real turn on the in-memory store. What those cannot show is that
//! the *backend* cooperates: that a crashed holder's row really becomes
//! claimable once its own TTL elapses, and that a renewing holder really keeps
//! publishing a later expiry under an unchanged identity triple. Both facts come
//! from the store, one of them from the store's own clock, so they are laws every
//! durable backend owes rather than in-memory trivia.

use std::sync::Arc;

use super::runtime_persistence::RuntimePersistenceLeaseTiming;
use crate::runtime::native_substrate::lane_wait::{
    QueuedLaneGiveUp, QueuedLaneWait, QueuedLaneWaitStep,
};
use crate::store::{RuntimePersistence, SessionExecutionLeaseClaimOutcome};
use crate::{
    AwaitEventResolver, QueuedLaneAcquisition, QueuedLaneHolder, QueuedLaneProbe, RuntimeError,
    RuntimeErrorCode,
};

/// Certify one queued-lane admission through the supplied effect boundary.
///
/// The probe owns the persistence and timing details. This contract observes
/// only the substrate seam's tagged outcome: acquisition, a one-shot refusal,
/// or the engine-paced typed retryable busy error. Any other error is a
/// conformance failure.
pub async fn durable_queued_drain_wait_contract(
    resolver: &dyn AwaitEventResolver,
    lane: Arc<dyn QueuedLaneProbe>,
) -> Result<QueuedLaneAcquisition, RuntimeError> {
    let outcome = resolver
        .acquire_queued_lane(lane, tokio_util::sync::CancellationToken::new())
        .await;
    match &outcome {
        Ok(QueuedLaneAcquisition::Acquired(_)) | Ok(QueuedLaneAcquisition::NotAcquired) => {}
        Err(error) if error.code == RuntimeErrorCode::SessionExecutionLaneBusy => {
            assert!(
                error.is_retryable(),
                "SessionExecutionLaneBusy must remain retryable end to end"
            );
        }
        Err(error) => panic!(
            "queued-lane admission returned an unruled error {}: {}",
            error.code, error.message
        ),
    }
    outcome
}

/// Drive the durable queued-drain wait policy against `store`.
///
/// Vector 1 (crashed holder): a foreign holder that never renews is waited out
/// and then displaced, within twice the first observed row's persisted lease term.
/// Vector 2 (live holder): the first observed row has already renewed three
/// times, then one more renewal is detected as alive on the next observation;
/// the drain gives up and leaves the holder row byte-identical to that renewal.
pub(super) async fn durable_queued_drain_wait_store_laws(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    let ttl_ms = lease_timing.scaffolding_lease_ttl_ms();
    waits_out_a_crashed_holder_then_claims(&store, lease_timing, ttl_ms).await;
    gives_up_on_a_renewing_holder_without_touching_its_row(&store, lease_timing, ttl_ms).await;
}

async fn waits_out_a_crashed_holder_then_claims(
    store: &Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
    ttl_ms: u64,
) {
    let session_id = "durable-queued-drain-wait-crashed";
    let crashed = crate::LeaseOwnerIdentity::opaque("crashed-host", "crashed-host:boot");
    let drain = crate::LeaseOwnerIdentity::opaque("drain-host", "drain-host:boot");
    let holder = store
        .try_claim_session_execution_lease(session_id, &crashed, "crashed-holder-executor", ttl_ms)
        .await
        .expect("claim the crashed holder's lane")
        .acquired()
        .expect("the crashed holder takes the lane");

    let mut wait = QueuedLaneWait::default();
    let acquisition = loop {
        match store
            .try_claim_session_execution_lease(session_id, &drain, "drain-executor", ttl_ms)
            .await
            .expect("durable queued drain claim attempt")
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => break acquisition,
            SessionExecutionLeaseClaimOutcome::Busy { holder: observed } => {
                assert_eq!(observed.executor_id, "crashed-holder-executor");
                match wait.observe(&QueuedLaneHolder::new(observed)) {
                    QueuedLaneWaitStep::Wait { slice_ms } => {
                        lease_timing.pass_wait_slice(slice_ms).await;
                    }
                    QueuedLaneWaitStep::GiveUp(give_up) => panic!(
                        "a crashed holder must be waited out, not given up on: {give_up:?} after \
                         {}ms",
                        wait.waited_ms()
                    ),
                }
            }
        }
    };
    assert!(
        wait.waited_ms() <= ttl_ms * 2,
        "waiting out a crashed holder must stay inside twice its TTL; waited {}ms of {}ms",
        wait.waited_ms(),
        ttl_ms * 2
    );
    let displaced = acquisition
        .displaced
        .as_ref()
        .expect("the drain's claim displaced the lapsed holder");
    assert_eq!(displaced.owner.owner_id, "crashed-host");
    assert_eq!(displaced.executor_id, "crashed-holder-executor");
    assert!(acquisition.lease.fencing_token > holder.fencing_token);
    store
        .release_session_execution_lease(&acquisition.lease.completion())
        .await
        .expect("release the drain's lane");
}

async fn gives_up_on_a_renewing_holder_without_touching_its_row(
    store: &Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
    ttl_ms: u64,
) {
    let session_id = "durable-queued-drain-wait-live";
    let live = crate::LeaseOwnerIdentity::opaque("live-host", "live-host:boot");
    let drain = crate::LeaseOwnerIdentity::opaque("drain-host", "drain-host:boot");
    let mut holder = store
        .try_claim_session_execution_lease(session_id, &live, "live-holder-executor", ttl_ms)
        .await
        .expect("claim the live holder's lane")
        .acquired()
        .expect("the live holder takes the lane");

    for _ in 0..3 {
        lease_timing.pass_wait_slice(25).await;
        holder = store
            .renew_session_execution_lease(&holder.fence(), ttl_ms)
            .await
            .expect("the live holder renews before its first observation");
    }
    match lease_timing {
        RuntimePersistenceLeaseTiming::Realtime => assert_eq!(holder.lease_term_ms, 500),
        RuntimePersistenceLeaseTiming::Controlled(_) => assert_eq!(holder.lease_term_ms, 50),
    }

    let mut wait = QueuedLaneWait::default();
    let first = busy_holder(store, session_id, &drain, ttl_ms).await;
    assert_eq!(first.lease().executor_id, "live-holder-executor");
    let slice_ms = match wait.observe(&first) {
        QueuedLaneWaitStep::Wait { slice_ms } => slice_ms,
        QueuedLaneWaitStep::GiveUp(give_up) => {
            panic!("the first observation carries no aliveness evidence yet: {give_up:?}")
        }
    };

    lease_timing.pass_wait_slice(slice_ms).await;
    let renewed = store
        .renew_session_execution_lease(&holder.fence(), ttl_ms)
        .await
        .expect("the live holder renews its own lane");
    assert!(
        renewed.expires_at_epoch_ms > first.lease().expires_at_epoch_ms,
        "a renewal must publish a strictly later expiry: {} then {}",
        first.lease().expires_at_epoch_ms,
        renewed.expires_at_epoch_ms
    );

    let second = busy_holder(store, session_id, &drain, ttl_ms).await;
    assert_eq!(
        wait.observe(&second),
        QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::HolderIsAlive)
    );
    let after = store
        .get_session_execution_lease(session_id)
        .await
        .expect("read the live holder's row after the drain gave up")
        .expect("the live holder still holds the lane");
    assert_eq!(after, renewed);
    store
        .release_session_execution_lease(&renewed.completion())
        .await
        .expect("release the live holder's lane");
}

async fn busy_holder(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    drain: &crate::LeaseOwnerIdentity,
    ttl_ms: u64,
) -> QueuedLaneHolder {
    match store
        .try_claim_session_execution_lease(session_id, drain, "drain-executor", ttl_ms)
        .await
        .expect("durable queued drain claim attempt")
    {
        SessionExecutionLeaseClaimOutcome::Busy { holder } => QueuedLaneHolder::new(holder),
        SessionExecutionLeaseClaimOutcome::Acquired(_) => {
            panic!("a live holder's lane must not be granted to the drain")
        }
    }
}
