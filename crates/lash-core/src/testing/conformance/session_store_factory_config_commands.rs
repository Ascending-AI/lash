//! Durable session-config command conformance.

use super::*;

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

async fn runtime_for_config_settlement(
    store: Arc<dyn crate::RuntimePersistence>,
    request: &crate::SessionStoreCreateRequest,
    settlement_timeout: std::time::Duration,
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
    .with_lease_timings(
        crate::LeaseTimings::from_ttl(settlement_timeout).expect("valid config-settlement timing"),
    );
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

async fn enqueue_config_settlement_blocker(
    store: &dyn crate::RuntimePersistence,
    session_id: &str,
) {
    store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            session_id,
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            vec![crate::QueuedWorkPayload::agent_frame_task(
                "config-settlement-blocker",
                "block the FIFO head",
                None,
            )],
        ))
        .await
        .expect("enqueue config-settlement blocker");
}

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

pub(super) async fn session_config_settlement_timeout_is_typed(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "config-settlement-timeout",
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create config-settlement timeout store");
    enqueue_config_settlement_blocker(store.as_ref(), &request.session_id).await;
    let mut runtime = runtime_for_config_settlement(
        Arc::clone(&store),
        &request,
        std::time::Duration::from_millis(300),
    )
    .await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.update_session_config(config_settlement_patch("must-remain-pending")),
    )
    .await
    .expect("config setter must return within its settlement bound")
    .expect_err("blocked config setter must return a typed pending error");
    assert!(
        matches!(error, crate::SessionError::SessionCommandPending(_)),
        "blocked config setter returned {error:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "300ms settlement timeout must not hang the facade writer"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_model
    );
}

pub(super) async fn cancelled_session_config_settlement_is_typed(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "config-settlement-cancelled",
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create config-settlement cancellation store");
    enqueue_config_settlement_blocker(store.as_ref(), &request.session_id).await;
    let runtime = runtime_for_config_settlement(
        Arc::clone(&store),
        &request,
        std::time::Duration::from_secs(3),
    )
    .await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let setter = crate::task::spawn(async move {
        let mut runtime = runtime;
        let result = runtime
            .update_session_config(config_settlement_patch("must-be-cancelled"))
            .await;
        (result, runtime)
    });

    let command_batch = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
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
        }
    })
    .await
    .expect("accepted config command must become visible");
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

    let (result, runtime) = tokio::time::timeout(std::time::Duration::from_secs(2), setter)
        .await
        .expect("cancelled setter must resolve promptly")
        .expect("cancelled setter task");
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
