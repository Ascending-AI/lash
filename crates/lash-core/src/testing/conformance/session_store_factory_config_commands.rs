//! Durable session-config command conformance.

use super::*;
#[cfg(test)]
use crate::{Clock, SessionStoreFactory};

pub(super) async fn session_store_factory_coalesces_config_command_claims(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "config-command-coalescing",
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
                vec![crate::QueuedWorkPayload::session_command(
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
                )],
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

pub(super) async fn session_store_factory_bounds_config_command_claims(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "config-command-claim-bound",
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
                vec![crate::QueuedWorkPayload::session_command(
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
                )],
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
    let mut state = crate::load_persisted_session_state(store)
        .await
        .expect("load config-command state")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.policy.clone(),
            ..crate::RuntimeSessionState::new(request.policy.clone())
        });
    state.ensure_agent_frame_initialized();
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

/// Virtual clock for runtime settlement tests. Its sleeps advance the same
/// epoch used by the in-memory store and yield once, so settlement deadlines
/// are exercised without waiting on wall time.
#[cfg(test)]
#[derive(Debug)]
struct ConfigSettlementClock {
    epoch_ms: std::sync::atomic::AtomicU64,
    monotonic_origin: std::time::Instant,
    epoch_origin_ms: u64,
}

#[cfg(test)]
impl ConfigSettlementClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
            monotonic_origin: std::time::Instant::now(),
            epoch_origin_ms: epoch_ms,
        }
    }

    fn advance(&self, duration: std::time::Duration) {
        let duration_ms = u64::try_from(duration.as_millis()).expect("clock duration fits u64");
        self.epoch_ms
            .fetch_add(duration_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
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

    fn timestamp_ms(&self) -> u64 {
        self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms()),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        self.advance(duration);
        tokio::task::yield_now().await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.sleep(deadline.saturating_duration_since(self.now()))
            .await;
    }
}

#[cfg(test)]
async fn config_settlement_store(
    clock: Arc<ConfigSettlementClock>,
    request: &crate::SessionStoreCreateRequest,
) -> Arc<dyn crate::RuntimePersistence> {
    let factory =
        crate::InMemorySessionStoreFactory::with_clock(Arc::clone(&clock) as Arc<dyn crate::Clock>);
    factory
        .create_store(request)
        .await
        .expect("create in-memory config-settlement store")
}

#[cfg(test)]
async fn runtime_for_config_settlement(
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
    let plugins = match state.plugin_snapshot() {
        Some(snapshot) => host.rematerialize_session(
            request.session_id.clone(),
            snapshot,
            crate::plugin::RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        ),
        None => host.build_session(request.session_id.clone()),
    }
    .expect("config-settlement plugins");
    let host = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(clock as Arc<dyn crate::Clock>);
    crate::LashRuntime::from_persistent_embedded_state(
        request.policy.clone(),
        crate::EmbeddedRuntimeHost::new(host),
        crate::PersistentRuntimeServices::new(plugins, store),
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("build config-settlement runtime")
}

#[cfg(test)]
async fn enqueue_config_settlement_blocker(
    store: &dyn crate::RuntimePersistence,
    session_id: &str,
) {
    store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            session_id,
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            vec![crate::QueuedWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id(session_id, "config-settlement-blocker"),
                "block the FIFO head",
                None,
            )],
        ))
        .await
        .expect("enqueue config-settlement blocker");
}

#[cfg(test)]
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

#[cfg(test)]
pub(super) async fn session_config_settlement_timeout_is_typed() {
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        "config-settlement-timeout",
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let store = config_settlement_store(Arc::clone(&clock), &request).await;
    enqueue_config_settlement_blocker(store.as_ref(), &request.session_id).await;
    let mut runtime =
        runtime_for_config_settlement(Arc::clone(&store), &request, Arc::clone(&clock)).await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let started = clock.now();
    let error = runtime
        .update_session_config(config_settlement_patch("must-remain-pending"))
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

#[cfg(test)]
pub(super) async fn cancelled_session_config_settlement_is_typed() {
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        "config-settlement-cancelled",
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let store = config_settlement_store(Arc::clone(&clock), &request).await;
    enqueue_config_settlement_blocker(store.as_ref(), &request.session_id).await;
    let runtime =
        runtime_for_config_settlement(Arc::clone(&store), &request, Arc::clone(&clock)).await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let setter = crate::task::spawn(async move {
        let mut runtime = runtime;
        let result = runtime
            .update_session_config(config_settlement_patch("must-be-cancelled"))
            .await;
        (result, runtime)
    });

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
