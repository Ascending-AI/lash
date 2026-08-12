//! Cross-backend laws for the aliveness-aware durable queued-drain wait.
//!
//! The policy itself
//! ([`queued_lane_wait`](crate::runtime::session_execution_lease::queued_lane_wait))
//! is pure, and the runtime-level regressions in `runtime::tests::turns` drive
//! it through a real turn on the in-memory store. What those cannot show is that
//! the *backend* cooperates: that a crashed holder's row really becomes
//! claimable once its own TTL elapses, and that a renewing holder really keeps
//! publishing a later expiry under an unchanged identity triple. Both facts come
//! from the store, one of them from the store's own clock, so they are laws every
//! durable backend owes rather than in-memory trivia.

use std::sync::Arc;

use super::runtime_persistence::RuntimePersistenceLeaseTiming;
use crate::runtime::session_execution_lease::queued_lane_wait::{
    QueuedLaneGiveUp, QueuedLaneWait, QueuedLaneWaitStep,
};
use crate::store::{RuntimePersistence, SessionExecutionLeaseClaimOutcome};

/// Drive the durable queued-drain wait policy against `store`.
///
/// Vector 1 (crashed holder): a foreign holder that never renews is waited out
/// and then displaced, within the policy's own budget of twice the observed TTL.
/// Vector 2 (live holder): a holder that renews is detected as alive on the very
/// next observation, the drain gives up, and the holder row is left byte-identical
/// to the renewal the holder itself installed.
pub async fn durable_queued_drain_wait_contract(
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
    let mut rounds = 0_usize;
    let acquisition = loop {
        rounds += 1;
        assert!(
            rounds <= 16,
            "the crashed-holder wait must terminate; it ran {rounds} rounds"
        );
        match store
            .try_claim_session_execution_lease(session_id, &drain, "drain-executor", ttl_ms)
            .await
            .expect("durable queued drain claim attempt")
        {
            SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => break acquisition,
            SessionExecutionLeaseClaimOutcome::Busy { holder: observed } => {
                assert_eq!(observed.executor_id, "crashed-holder-executor");
                match wait.observe(&observed) {
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
    let holder = store
        .try_claim_session_execution_lease(session_id, &live, "live-holder-executor", ttl_ms)
        .await
        .expect("claim the live holder's lane")
        .acquired()
        .expect("the live holder takes the lane");

    let mut wait = QueuedLaneWait::default();
    let first = busy_holder(store, session_id, &drain, ttl_ms).await;
    assert_eq!(first.executor_id, "live-holder-executor");
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
        renewed.expires_at_epoch_ms > first.expires_at_epoch_ms,
        "a renewal must publish a strictly later expiry: {} then {}",
        first.expires_at_epoch_ms,
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
) -> crate::store::SessionExecutionLease {
    match store
        .try_claim_session_execution_lease(session_id, drain, "drain-executor", ttl_ms)
        .await
        .expect("durable queued drain claim attempt")
    {
        SessionExecutionLeaseClaimOutcome::Busy { holder } => holder,
        SessionExecutionLeaseClaimOutcome::Acquired(_) => {
            panic!("a live holder's lane must not be granted to the drain")
        }
    }
}
