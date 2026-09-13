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

use lash_sansio::SessionId;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::runtime_persistence::RuntimePersistenceLeaseTiming;
use crate::store::{RuntimePersistence, SessionExecutionLeaseClaimOutcome};
use crate::{
    AwaitEventResolver, QueuedLaneAcquisition, QueuedLaneHolder, QueuedLaneProbe, RuntimeError,
    RuntimeErrorCode,
};
use lash_core::testing::conformance_support::{
    QueuedLaneGiveUp, QueuedLaneWait, QueuedLaneWaitStep,
};
use pretty_assertions::assert_eq;

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

struct ScriptedQueuedLaneProbe {
    attempts: Mutex<VecDeque<crate::QueuedLaneAttempt>>,
    try_calls: AtomicUsize,
    pause_calls: AtomicUsize,
}

impl ScriptedQueuedLaneProbe {
    fn new(attempts: impl IntoIterator<Item = crate::QueuedLaneAttempt>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into_iter().collect()),
            try_calls: AtomicUsize::new(0),
            pause_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl QueuedLaneProbe for ScriptedQueuedLaneProbe {
    async fn try_acquire(&self) -> Result<crate::QueuedLaneAttempt, RuntimeError> {
        self.try_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .expect("queued-lane probe attempt"))
    }

    async fn pause(&self, _slice: std::time::Duration) {
        self.pause_calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// Certify the engine-paced and deployment-host queued-lane policies.
///
/// The backend supplies only the two resolver implementations. The shared law
/// owns the scripted lane probes and all outcome and call-count assertions.
pub async fn durable_queued_drain_wait_resolver_laws<Engine, Deployment>(
    make_engine: Engine,
    make_deployment: Deployment,
) where
    Engine: FnOnce() -> Arc<dyn AwaitEventResolver>,
    Deployment: FnOnce() -> Arc<dyn AwaitEventResolver>,
{
    let controller_probe = Arc::new(ScriptedQueuedLaneProbe::new([
        crate::QueuedLaneAttempt::Busy(lash_core::testing::queued_lane_holder_for_testing(7_400)),
        crate::QueuedLaneAttempt::Busy(lash_core::testing::queued_lane_holder_for_testing(7_401)),
    ]));
    let controller = make_engine();
    let result = durable_queued_drain_wait_contract(
        controller.as_ref(),
        Arc::clone(&controller_probe) as Arc<dyn QueuedLaneProbe>,
    )
    .await;
    let Err(error) = result else {
        panic!("the engine-paced controller must use the typed retryable lane wait")
    };
    assert_eq!(error.code, RuntimeErrorCode::SessionExecutionLaneBusy);
    assert!(error.is_retryable());
    assert_eq!(controller_probe.try_calls.load(Ordering::SeqCst), 2);
    assert_eq!(controller_probe.pause_calls.load(Ordering::SeqCst), 1);

    let host_probe = Arc::new(ScriptedQueuedLaneProbe::new([
        crate::QueuedLaneAttempt::Busy(lash_core::testing::queued_lane_holder_for_testing(7_400)),
    ]));
    let host = make_deployment();
    let result = durable_queued_drain_wait_contract(
        host.as_ref(),
        Arc::clone(&host_probe) as Arc<dyn QueuedLaneProbe>,
    )
    .await
    .expect("deployment-host queued-lane default");
    assert!(matches!(result, QueuedLaneAcquisition::NotAcquired));
    assert_eq!(host_probe.try_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host_probe.pause_calls.load(Ordering::SeqCst), 0);
}

/// Drive the durable queued-drain wait policy against `store`.
///
/// Vector 1 (crashed holder): a foreign holder that never renews is waited out
/// and then displaced, within twice the first observed row's persisted lease term.
/// Vector 2 (live holder): the first observed row has already renewed three
/// times, then one more renewal is detected as alive on the next observation;
/// the drain gives up and leaves the holder row byte-identical to that renewal.
pub async fn durable_queued_drain_wait_store_laws(
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
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &crashed,
            "crashed-holder-executor",
            ttl_ms,
        )
        .await
        .expect("claim the crashed holder's lane")
        .acquired()
        .expect("the crashed holder takes the lane");

    let mut wait = QueuedLaneWait::default();
    let acquisition = loop {
        match store
            .try_claim_session_execution_lease(
                &SessionId::from(session_id),
                &drain,
                "drain-executor",
                ttl_ms,
            )
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
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &live,
            "live-holder-executor",
            ttl_ms,
        )
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
    let first = busy_holder(store, &SessionId::from(session_id), &drain, ttl_ms).await;
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

    let second = busy_holder(store, &SessionId::from(session_id), &drain, ttl_ms).await;
    assert_eq!(
        wait.observe(&second),
        QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::HolderIsAlive)
    );
    let after = store
        .get_session_execution_lease(&SessionId::from(session_id))
        .await
        .expect("read the live holder's row after the drain gave up")
        .lease
        .expect("the live holder still holds the lane");
    assert_eq!(after, renewed);
    store
        .release_session_execution_lease(&renewed.completion())
        .await
        .expect("release the live holder's lane");
}

async fn busy_holder(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
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
