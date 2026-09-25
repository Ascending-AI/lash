//! Durable session-config command conformance.

use super::*;
use crate::Clock;
use pretty_assertions::assert_eq;
use std::future::Future;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_coalesces_config_command_claims(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("config-command-coalescing"),
        "config-command-base-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create config-command conformance store");
    for model in ["config-a", "config-b", "config-c"] {
        store
            .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                &request.session_id,
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::SessionCommand::ApplyConfigPatch {
                    patch: Box::new(crate::runtime::ApplyConfigPatch {
                        model: Some(
                            crate::ModelSpec::builder(model)
                                .context_window_tokens(32_000)
                                .build()
                                .expect("model"),
                        ),
                        ..crate::runtime::ApplyConfigPatch::default()
                    }),
                },
            ))
            .await
            .expect("enqueue config command");
    }
    let owner = crate::LeaseOwnerIdentity::opaque(
        "config-command-coalescing",
        "config-command-coalescing:incarnation",
    );
    let lease = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &owner,
            "config-command-coalescing-executor",
            60_000,
        )
        .await
        .expect("claim config-command session lease")
        .acquired()
        .expect("config-command session lease");
    let claim = store
        .claim_leading_ready_session_command(&request.session_id, &lease.fence(), &owner)
        .await
        .expect("claim leading config commands")
        .expect("config command claim");

    assert_eq!(claim.batches.len(), 3);
    assert_eq!(
        claim
            .session_commands()
            .expect("claim contains only config commands")
            .len(),
        3,
        "all adjacent config commands must share one backend claim"
    );
    let completed_batch_ids = claim
        .batches
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect::<Vec<_>>();
    commit_session_command_claim(store.as_ref(), &request, claim).await;
    for batch_id in completed_batch_ids {
        assert!(
            store
                .queued_work_batch_completed(&request.session_id, &batch_id)
                .await
                .expect("read config-command completion marker"),
            "every batch in a coalesced command commit must leave completion evidence"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_bounds_config_command_claims(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("config-command-claim-bound"),
        "config-command-base-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create bounded config-command store");
    let total = crate::store::queued_work::MAX_SESSION_COMMAND_BATCHES_PER_CLAIM + 3;
    for index in 0..total {
        store
            .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                &request.session_id,
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::SessionCommand::ApplyConfigPatch {
                    patch: Box::new(crate::runtime::ApplyConfigPatch {
                        model: Some(
                            crate::ModelSpec::builder(format!("bounded-config-{index}"))
                                .context_window_tokens(32_000)
                                .build()
                                .expect("model"),
                        ),
                        ..crate::runtime::ApplyConfigPatch::default()
                    }),
                },
            ))
            .await
            .expect("enqueue bounded config command");
    }
    let owner = crate::LeaseOwnerIdentity::opaque(
        "config-command-claim-bound",
        "config-command-claim-bound:incarnation",
    );
    let lease = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &owner,
            "config-command-claim-bound-executor",
            60_000,
        )
        .await
        .expect("claim bounded config-command session lease")
        .acquired()
        .expect("bounded config-command session lease");
    let first = store
        .claim_leading_ready_session_command(&request.session_id, &lease.fence(), &owner)
        .await
        .expect("claim first bounded command prefix")
        .expect("first bounded command prefix");
    assert_eq!(
        first.batches.len(),
        crate::store::queued_work::MAX_SESSION_COMMAND_BATCHES_PER_CLAIM
    );
    commit_session_command_claim(store.as_ref(), &request, first).await;

    let second = store
        .claim_leading_ready_session_command(&request.session_id, &lease.fence(), &owner)
        .await
        .expect("claim remaining bounded command prefix")
        .expect("remaining bounded command prefix");
    assert_eq!(second.batches.len(), 3);
    commit_session_command_claim(store.as_ref(), &request, second).await;

    assert!(
        store
            .claim_leading_ready_session_command(&request.session_id, &lease.fence(), &owner)
            .await
            .expect("check bounded command queue exhaustion")
            .is_none(),
        "a longer config-command run must drain completely over multiple commits"
    );
}

async fn commit_session_command_claim(
    store: &dyn crate::RuntimePersistence,
    request: &crate::SessionStoreCreateRequest,
    claim: crate::QueuedWorkClaim,
) {
    commit_session_command_claim_with(store, request, claim, |_| {}).await;
}

/// [`commit_session_command_claim`], with `adjust` applied to the state the
/// settling commit writes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_session_command_claim_with(
    store: &dyn crate::RuntimePersistence,
    request: &crate::SessionStoreCreateRequest,
    claim: crate::QueuedWorkClaim,
    adjust: impl FnOnce(&mut crate::RuntimeSessionState),
) {
    let mut state = crate::load_persisted_session_state(store)
        .await
        .expect("load config-command state")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.policy.clone(),
            ..crate::RuntimeSessionState::new(request.policy.clone())
        });
    state.ensure_agent_frame_initialized();
    adjust(&mut state);
    let first_batch_id = claim
        .batches
        .first()
        .expect("command claim has a batch")
        .batch_id
        .clone();
    let commit = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(
            crate::ExecutionScope::queue_drain(&request.session_id, first_batch_id),
            "session-command",
        ),
    )
    .completing_queue_claim(claim.completion());
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit config-command claim");
}

tokio::task_local! {
    /// Marks the facade writer's task: only its sleeps move the virtual clock.
    static DRIVES_SETTLEMENT_CLOCK: ();
}

/// Virtual clock for runtime settlement tests.
///
/// Only the facade writer drives it: a sleep inside [`Self::driving`] advances
/// the clock by its duration and yields once, so the settlement deadline is
/// exercised without waiting on wall time. Every other sleeper (the session
/// lease renewer the settlement loop spawns, for one) waits for the writer to
/// move the clock past its deadline. A background sleeper therefore never
/// pushes the writer's deadline forward, and the writer answers at exactly
/// the bound.
#[derive(Debug)]
struct ConfigSettlementClock {
    epoch_ms: std::sync::atomic::AtomicU64,
    monotonic_origin: std::time::Instant,
    epoch_origin_ms: u64,
    advanced: tokio::sync::Notify,
}

impl ConfigSettlementClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
            monotonic_origin: std::time::Instant::now(),
            epoch_origin_ms: epoch_ms,
            advanced: tokio::sync::Notify::new(),
        }
    }

    /// Run `writer` as the task whose sleeps drive the clock.
    async fn driving<T>(writer: impl Future<Output = T>) -> T {
        DRIVES_SETTLEMENT_CLOCK.scope((), writer).await
    }

    fn duration_ms(duration: std::time::Duration) -> u64 {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    }

    fn advance(&self, duration: std::time::Duration) {
        self.epoch_ms.fetch_add(
            Self::duration_ms(duration),
            std::sync::atomic::Ordering::SeqCst,
        );
        self.advanced.notify_waiters();
    }

    async fn wait_until_ms(&self, deadline_ms: u64) {
        loop {
            let advanced = self.advanced.notified();
            tokio::pin!(advanced);
            advanced.as_mut().enable();
            if self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst) >= deadline_ms {
                return;
            }
            advanced.await;
        }
    }
}

#[async_trait::async_trait]
impl crate::Clock for ConfigSettlementClock {
    fn now(&self) -> std::time::Instant {
        self.monotonic_origin
            + std::time::Duration::from_millis(
                self.epoch_ms
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .saturating_sub(self.epoch_origin_ms),
            )
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        if DRIVES_SETTLEMENT_CLOCK.try_with(|()| ()).is_ok() {
            self.advance(duration);
            tokio::task::yield_now().await;
            return;
        }
        let deadline_ms = self
            .epoch_ms
            .load(std::sync::atomic::Ordering::SeqCst)
            .saturating_add(Self::duration_ms(duration));
        self.wait_until_ms(deadline_ms).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.sleep(deadline.saturating_duration_since(self.now()))
            .await;
    }
}

#[test]
fn config_settlement_clock_wall_clock_faces_agree() {
    let clock = ConfigSettlementClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

/// The law's store: a fresh backend, and `request`'s store from its
/// factory. The backend is returned so the runtime takes its ports from it.
///
/// The backend keeps its own clock. Only the runtime's settlement wait runs
/// on the law's virtual clock: a backend on it would advance that clock with
/// every lease-renewal sleep of its own and blur the bound the law measures.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn config_settlement_store<M, Fut>(
    make: &M,
    request: &crate::SessionStoreCreateRequest,
) -> (crate::Backend, Arc<dyn crate::RuntimePersistence>)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let backend = make().await;
    let store = backend
        .session_store_factory()
        .create_store(request)
        .await
        .expect("create config-settlement store");
    (backend, store)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn runtime_for_config_settlement(
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimePersistence>,
    request: &crate::SessionStoreCreateRequest,
    clock: Arc<ConfigSettlementClock>,
) -> crate::LashRuntime {
    let mut state = crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("load config-settlement state")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.policy.clone(),
            ..crate::RuntimeSessionState::new(request.policy.clone())
        });
    state.ensure_agent_frame_initialized();
    let host = crate::PluginHost::new(crate::testing::test_standard_protocol_factories());
    let plugins = match state.plugin_state() {
        Some(snapshot) => host.rematerialize_session(
            request.session_id.clone(),
            snapshot,
            crate::plugin::RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        ),
        None => host.build_session(request.session_id.clone()),
    }
    .expect("config-settlement plugins");
    let host = crate::RuntimeHostConfig::new(
        backend,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(clock as Arc<dyn crate::Clock>);
    let runtime_host = crate::EmbeddedRuntimeHost::new(host);
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        store,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    crate::LashRuntime::from_persistent_embedded_state(
        request.policy.clone(),
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("build config-settlement runtime")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn enqueue_config_settlement_blocker(
    store: &dyn crate::RuntimePersistence,
    session_id: &SessionId,
) {
    store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            session_id,
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::TurnWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id(session_id, "config-settlement-blocker"),
                "block the FIFO head",
                None,
            ),
        ))
        .await
        .expect("enqueue config-settlement blocker");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn config_settlement_patch(model_id: &str) -> crate::SessionConfigPatch {
    crate::SessionConfigPatch {
        model: Some(
            crate::ModelSpec::builder(model_id)
                .context_window_tokens(32_000)
                .build()
                .expect("config-settlement model"),
        ),
        ..crate::SessionConfigPatch::default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_config_settlement_timeout_is_typed<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        &SessionId::from("config-settlement-timeout"),
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let (backend, store) = config_settlement_store(&make, &request).await;
    enqueue_config_settlement_blocker(store.as_ref(), &request.session_id).await;
    let mut runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&store),
        &request,
        Arc::clone(&clock),
    )
    .await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let started = clock.now();
    let error = ConfigSettlementClock::driving(
        runtime.update_session_config(config_settlement_patch("must-remain-pending")),
    )
    .await
    .expect_err("blocked config setter must return a typed pending error");
    assert!(
        matches!(error, crate::SessionError::SessionCommandPending(_)),
        "blocked config setter returned {error:?}"
    );
    assert!(
        clock.now().saturating_duration_since(started) == std::time::Duration::from_secs(30),
        "the injected 30s settlement bound must not hang the facade writer"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_model
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn cancelled_session_config_settlement_is_typed<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        &SessionId::from("config-settlement-cancelled"),
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let (backend, store) = config_settlement_store(&make, &request).await;
    enqueue_config_settlement_blocker(store.as_ref(), &request.session_id).await;
    let runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&store),
        &request,
        Arc::clone(&clock),
    )
    .await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let setter = crate::task::spawn(ConfigSettlementClock::driving(async move {
        let mut runtime = runtime;
        let result = runtime
            .update_session_config(config_settlement_patch("must-be-cancelled"))
            .await;
        (result, runtime)
    }));

    let command_batch = loop {
        if let Some(batch) = store
            .list_queued_work(&request.session_id)
            .await
            .expect("list queued config command")
            .into_iter()
            .find(crate::QueuedWorkBatch::is_session_command_work)
        {
            break batch;
        }
        tokio::task::yield_now().await;
    };
    let cancelled = store
        .cancel_queued_work_batch(&request.session_id, &command_batch.batch_id)
        .await
        .expect("cancel queued config command")
        .expect("config command cancellation wins before claim");
    assert_eq!(cancelled.batch_id, command_batch.batch_id);
    assert!(
        !store
            .queued_work_batch_completed(&request.session_id, &command_batch.batch_id)
            .await
            .expect("read cancelled config marker"),
        "cancellation must not manufacture completion evidence"
    );

    let (result, runtime) = setter.await.expect("cancelled setter task");
    let error = result.expect_err("cancelled config setter must be typed");
    assert!(
        matches!(
            &error,
            crate::SessionError::SessionCommandCancelled(receipt)
                if receipt.batch_id == command_batch.batch_id
        ),
        "cancelled config setter returned {error:?}"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_model
    );
}

/// Head-authoritative settlement (FIG-1875): when another writer supersedes a
/// settled config command before the facade writer observes settlement, the
/// facade writer adopts the newer durable head as-is — it never re-publishes
/// its own older patch values over that head residently.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn superseded_config_settlement_adopts_the_newer_head<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        &SessionId::from("config-settlement-superseded"),
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let (backend, store) = config_settlement_store(&make, &request).await;
    let runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&store),
        &request,
        Arc::clone(&clock),
    )
    .await;

    // A second writer holds the session-execution lease for the whole test,
    // so the facade writer can neither drain inline nor drain from its
    // settlement wait loop.
    let owner =
        crate::LeaseOwnerIdentity::opaque("superseding-writer", "superseding-writer:incarnation");
    let lease = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &owner,
            "superseding-executor",
            600_000,
        )
        .await
        .expect("claim superseding session lease")
        .acquired()
        .expect("superseding session lease");

    let setter = crate::task::spawn(ConfigSettlementClock::driving(async move {
        let mut runtime = runtime;
        let result = runtime
            .update_session_config(config_settlement_patch("first-settled"))
            .await;
        (result, runtime)
    }));

    let command_batch = loop {
        if let Some(batch) = store
            .list_queued_work(&request.session_id)
            .await
            .expect("list queued config command")
            .into_iter()
            .find(crate::QueuedWorkBatch::is_session_command_work)
        {
            break batch;
        }
        tokio::task::yield_now().await;
    };

    // The second writer drains the facade writer's command...
    let claim = store
        .claim_leading_ready_session_command(&request.session_id, &lease.fence(), &owner)
        .await
        .expect("claim facade config command")
        .expect("facade config command claim");
    assert!(
        claim
            .batches
            .iter()
            .any(|batch| batch.batch_id == command_batch.batch_id),
        "the superseding writer drains the facade writer's command"
    );
    // ...and, in the same commit that settles it, advances the durable head
    // with a newer model. One commit keeps the law independent of when the
    // facade writer polls: it can only ever observe its command settled under
    // the newer head.
    let superseding_model = crate::ModelSpec::builder("second-newer")
        .context_window_tokens(32_000)
        .build()
        .expect("superseding model");
    let newer_model = superseding_model.clone();
    commit_session_command_claim_with(store.as_ref(), &request, claim, move |state| {
        state.policy.model = newer_model;
    })
    .await;

    let (result, runtime) = setter.await.expect("superseded setter task");
    result.expect("superseded config setter settles durably");
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        superseding_model,
        "settlement adopts the newer durable head instead of re-publishing \
         the settled command's older values"
    );
}
