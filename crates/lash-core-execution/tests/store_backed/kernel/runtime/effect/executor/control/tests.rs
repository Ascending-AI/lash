//! The queued-lane acquisition laws that need a real session execution lease
//! guard, which only a store can grant.

use std::sync::Arc;

use crate::runtime::effect::executor::control::*;
use crate::support::prelude::*;
use crate::{
    AdmittedScope, AwaitEventResolver, CancellationToken, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError, SessionId,
};

/// A memory backend on a test clock at the epoch the holder fixtures use.
async fn queued_lane_backend() -> lash_sqlite_store::SqliteBackend {
    lash_sqlite_store::SqliteBackend::memory_with_clock(Arc::new(crate::testing::TestClock::new(
        1_000,
    )))
    .await
    .expect("open a SQLite memory backend")
}

/// A guard over a fresh session's execution lane, claimed in the backend's
/// store.
async fn queued_lane_guard(backend: &lash_sqlite_store::SqliteBackend) -> QueuedLaneGuard {
    let session_id = SessionId::from("queued-lane-test");
    let store = backend
        .session_store_factory()
        .create_store(&crate::testing::store_fixtures::session_store_request(
            &session_id,
            "model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("create the queued-lane session store");
    let guard = crate::runtime::session_execution_lease::SessionExecutionLeaseGuard::try_acquire(
        store,
        &session_id,
        &crate::LeaseOwnerIdentity::opaque("owner", "owner:incarnation"),
        "queued-lane-test-executor",
        crate::LeaseTimings::default(),
        crate::Backend::from(backend.clone()).clock(),
    )
    .await
    .expect("queued-lane test claim")
    .expect("queued-lane test guard");
    QueuedLaneGuard::new(guard)
}

struct TestResolver;

impl AwaitEventResolver for TestResolver {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for TestResolver {
    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("queued-lane controller tests do not execute effects")
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("TestResolver"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("TestResolver"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("TestResolver"))
    }
}

#[derive(Default)]
struct FakeQueuedLaneProbe {
    attempts: std::sync::Mutex<std::collections::VecDeque<QueuedLaneAttempt>>,
    try_calls: std::sync::atomic::AtomicUsize,
    pause_calls: std::sync::atomic::AtomicUsize,
}

impl FakeQueuedLaneProbe {
    fn new(attempts: impl IntoIterator<Item = QueuedLaneAttempt>) -> Self {
        Self {
            attempts: std::sync::Mutex::new(attempts.into_iter().collect()),
            try_calls: std::sync::atomic::AtomicUsize::new(0),
            pause_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn try_calls(&self) -> usize {
        self.try_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn pause_calls(&self) -> usize {
        self.pause_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl QueuedLaneProbe for FakeQueuedLaneProbe {
    async fn try_acquire(&self) -> Result<QueuedLaneAttempt, RuntimeError> {
        self.try_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .attempts
            .lock()
            .expect("fake queued-lane attempts")
            .pop_front()
            .expect("fake queued-lane attempt available"))
    }

    async fn pause(&self, _slice: std::time::Duration) {
        self.pause_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn queued_lane_holder(expires_at_epoch_ms: u64) -> QueuedLaneHolder {
    QueuedLaneHolder::new(crate::store::SessionExecutionLease {
        session_id: SessionId::from("queued-lane-test"),
        owner: crate::LeaseOwnerIdentity::opaque("holder", "holder:incarnation"),
        executor_id: "holder-executor".to_string(),
        lease_token: "holder-token".to_string(),
        fencing_token: 7,
        claimed_at_epoch_ms: 1_000,
        lease_term_ms: 6_400,
        expires_at_epoch_ms,
    })
}

async fn acquire_through_task_controller(
    controller: &TestResolver,
    probe: Arc<dyn QueuedLaneProbe>,
) -> Result<QueuedLaneAcquisition, RuntimeError> {
    let (scoped, mut requests) = EffectTaskController::scoped(
        controller,
        AdmittedScope::queue_drain("queued-lane-test", "drain"),
    )?;
    let acquire = scoped
        .controller()
        .acquire_queued_lane(probe, CancellationToken::new());
    let drive = async {
        crate::serve_effect_controller_task_request(
            requests.recv().await.expect("queued-lane task request"),
            controller,
        )
        .await;
    };
    let (result, ()) = tokio::join!(acquire, drive);
    result
}

#[tokio::test]
async fn provided_wait_retries_a_crashed_looking_holder_until_acquired() {
    let backend = queued_lane_backend().await;
    let probe = Arc::new(FakeQueuedLaneProbe::new([
        QueuedLaneAttempt::Busy(queued_lane_holder(7_400)),
        QueuedLaneAttempt::Acquired(queued_lane_guard(&backend).await),
    ]));

    let result = TestResolver
        .wait_out_crashed_lane_holder(
            Arc::clone(&probe) as Arc<dyn QueuedLaneProbe>,
            CancellationToken::new(),
        )
        .await
        .expect("provided queued-lane wait");

    assert!(
        matches!(result, QueuedLaneAcquisition::Acquired(_)),
        "expected acquisition after one crashed-looking holder; try_calls={}, pause_calls={}",
        probe.try_calls(),
        probe.pause_calls(),
    );
    assert_eq!(probe.try_calls(), 2);
    assert_eq!(probe.pause_calls(), 1);
}

#[tokio::test]
async fn queued_lane_acquisition_round_trips_through_the_task_controller() {
    let backend = queued_lane_backend().await;
    let controller = TestResolver;
    let busy = acquire_through_task_controller(
        &controller,
        Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
            queued_lane_holder(7_400),
        )])),
    )
    .await
    .expect("busy queued-lane proxy response");
    assert!(matches!(busy, QueuedLaneAcquisition::NotAcquired));

    let acquired = acquire_through_task_controller(
        &controller,
        Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Acquired(
            queued_lane_guard(&backend).await,
        )])),
    )
    .await
    .expect("acquired queued-lane proxy response");
    assert!(matches!(acquired, QueuedLaneAcquisition::Acquired(_)));
}
