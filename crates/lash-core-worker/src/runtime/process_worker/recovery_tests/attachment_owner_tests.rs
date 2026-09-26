use super::*;
use crate::TurnId;

struct AttachmentWritingEngine;

/// A layer over the backend's catalog that hands every session the one store
/// bound to the parent session: the shape of a parent-bound catalog, which a
/// process runtime must never alias its own runtime state into.
struct ParentBoundSessionStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    store: Arc<dyn crate::RuntimePersistence>,
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for ParentBoundSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for ParentBoundSessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(Arc::clone(&self.store))
    }

    // The layer binds exactly one store, so a by-id lookup hands back that
    // store for the id it was bound to.
    async fn open_existing_store_by_id(
        &self,
        _session_id: &SessionId,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
        Ok(Some(Arc::clone(&self.store)))
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &crate::store::TurnParkQuery,
    ) -> Result<Vec<crate::store::TurnPark>, crate::StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: crate::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::ParkFeedPage<crate::store::TurnParkTarget>, crate::StoreError> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &crate::SessionId,
        root: &crate::TurnId,
    ) -> std::result::Result<Option<crate::store::RootTerminal>, crate::StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<crate::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<crate::store::ControlIntent>, crate::StoreError> {
        self.inner.list_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: crate::store::ParkFeedCursor,
    ) -> Result<(), crate::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

#[async_trait::async_trait]
impl crate::store::ControlIntentStore for ParentBoundSessionStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<crate::store::ControlIntent>, crate::StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: crate::store::ControlIntentId,
    ) -> std::result::Result<crate::store::IntentApplication, crate::StoreError> {
        self.inner.claim_intent_application(id).await
    }

    async fn acknowledge_intent(
        &self,
        id: crate::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<(), crate::StoreError> {
        self.inner.acknowledge_intent(id, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: crate::store::ControlIntentId,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> std::result::Result<crate::store::ControlIntent, crate::StoreError> {
        self.inner
            .record_intent_failure(id, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: crate::store::ControlIntentId,
    ) -> std::result::Result<Option<crate::store::ControlIntent>, crate::StoreError> {
        self.inner.load_intent(id).await
    }
}

/// The parent session's store on `backend`, bound and committed.
async fn parent_bound_session_store(
    backend: &crate::Backend,
    policy: crate::SessionPolicy,
) -> Arc<dyn crate::RuntimePersistence> {
    const PARENT_SESSION_ID: &str = "parent-bound-process-worker";
    let store = backend
        .session_store_factory()
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: SessionId::from(PARENT_SESSION_ID),
            relation: crate::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: policy.clone(),
        })
        .await
        .expect("create the parent session store");
    let owner = crate::LeaseOwnerIdentity::opaque("parent-owner", "parent-incarnation");
    let _lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(PARENT_SESSION_ID),
            &owner,
            "parent-bound-session-store-executor",
            60_000,
        )
        .await
        .expect("claim parent session lease")
        .acquired()
        .expect("parent session lease acquired");
    let state = crate::RuntimeSessionState {
        session_id: SessionId::from(PARENT_SESSION_ID.to_string()),
        policy,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .admit_and_bind_session(&crate::SessionBinding::root(state.session_id.clone()))
        .await
        .expect("bind parent session");
    store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("persist parent session state");
    store
}

#[async_trait::async_trait]
impl crate::ProcessEngine for AttachmentWritingEngine {
    fn kind(&self) -> &'static str {
        "attachment-writing-engine"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        let catalog = context
            .resolved_tool_catalog()
            .expect("resolve process catalog");
        let runtime = context
            .into_runtime_context(catalog)
            .expect("build process runtime context");
        let attachment_store = runtime.context().attachment_store();
        attachment_store
            .put(
                b"process-attachment-before-nested-turn".to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("image/png").unwrap(),
                    Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    Some("before.png".to_string()),
                ),
            )
            .await
            .expect("process attachment before nested turn");

        let provider = crate::testing::TestProvider::builder()
            .kind("mock")
            .requires_streaming(true)
            .complete(|_| async {
                Ok(crate::llm::types::LlmResponse {
                    parts: vec![crate::llm::types::LlmOutputPart::Text {
                        text: "nested turn complete".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..Default::default()
                })
            })
            .build();
        // The nested embedded runtime stands on a memory backend of its own;
        // what it shares with the process is the attachment store, whose
        // owner scope the nested turn must not leave rebound.
        let backend = memory_backend().await;
        let mut nested_host = test_host_config(&backend);
        nested_host.durability.attachment_store = Arc::clone(&attachment_store);
        let mut nested_runtime =
            lash_core::testing::runtime_helpers::runtime_with_plugins_and_tools_and_host(
                Vec::new(),
                Arc::new(lash_core::testing::runtime_helpers::EmptyTools),
                provider,
                crate::EmbeddedRuntimeHost::new(nested_host),
            )
            .await;
        nested_runtime
            .stream_turn(
                crate::TurnInput::text("run nested turn"),
                crate::TurnOptions::new(
                    CancellationToken::new(),
                    lash_core::testing::runtime_helpers::backend_turn_scope(
                        &backend,
                        &SessionId::from("root"),
                        &TurnId::from("nested-engine-turn"),
                    ),
                ),
            )
            .await
            .expect("nested engine turn");

        attachment_store
            .put(
                b"process-attachment-after-nested-turn".to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("image/png").unwrap(),
                    Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    Some("after.png".to_string()),
                ),
            )
            .await
            .expect("process attachment after nested turn");
        drop(runtime);
        Ok(
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
            .into(),
        )
    }
}

#[tokio::test]
async fn process_runtime_keeps_state_separate_from_parent_bound_attachment_manifest() {
    let process_id = crate::ProcessId::fixture("parent-bound-process");
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::builder("test-model")
            .context_window_tokens(16_384)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let backend = memory_backend().await;
    let parent_store = parent_bound_session_store(&backend, policy.clone()).await;
    let layer_store = Arc::clone(&parent_store);
    let backend = crate::testing::runtime_helpers::LayeredBackend::over(backend)
        .map_session_store_factory(|inner| {
            Arc::new(ParentBoundSessionStoreFactory {
                inner,
                store: layer_store,
            })
        })
        .into_backend();
    let runtime_host = test_host_config(&backend);
    let worker = DurableProcessWorker::new({
        let watched = crate::watch_process_registry(backend.process_registry());
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            runtime_host,
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            local_owner("attachment-parent-worker", "host-a", "parent-start-a"),
        )
        .with_session_policy(policy.clone())
    })
    .expect("valid test native substrate config");

    let runtime = Box::pin(worker.build_process_runtime(
        SessionId::from(format!("process-env:{process_id}")),
        policy,
        crate::PluginOptions::default(),
        "parent-bound regression",
    ))
    .await
    .expect("build process runtime with parent-bound session factory");
    let _owner = runtime
        .host
        .core
        .durability
        .attachment_store
        .bind_process_scoped(process_id.clone());
    runtime
        .host
        .core
        .durability
        .attachment_store
        .put(
            b"parent-bound-process-attachment".to_vec(),
            crate::AttachmentCreateMeta::new(
                crate::MediaType::parse("image/png").unwrap(),
                Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                Some("parent-bound.png".to_string()),
            ),
        )
        .await
        .expect("persist process-owned attachment");

    let entries = parent_store
        .list_uncommitted(u64::MAX)
        .await
        .expect("list parent-bound process intents");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, format!("process-env:{process_id}"));
    assert!(matches!(
        &entries[0].owner,
        Some(crate::AttachmentOwner::Process { process_id: owner }) if *owner == process_id
    ));
}

#[tokio::test]
async fn engine_put_after_nested_turn_restores_the_durable_process_owner() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let factory = backend.session_store_factory();
    let attachment_backend = backend.attachment_store();
    let mut runtime_host = test_host_config(&backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(AttachmentWritingEngine)),
    );
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::builder("test-model")
            .context_window_tokens(16_384)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ArtifactOwner::host("attachment-owner-test"),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let worker = DurableProcessWorker::new({
        let watched =
            crate::watch_process_registry(Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>);
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            runtime_host,
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            local_owner("attachment-worker", "host-a", "start-a"),
        )
        .with_session_policy(policy)
    })
    .expect("valid test native substrate config");
    let process_id = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "attachment-writing-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                RecoveryContract::Rerunnable,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register process")
        .id;

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("recover process");
    await_terminal(&registry, &process_id).await;

    let request = crate::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(format!("process-env:{process_id}")),
        relation: crate::SessionRelation::default(),
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    };
    let store = factory
        .open_existing_store(&request)
        .await
        .expect("open process owner store")
        .expect("process owner store exists");
    let entries = store
        .list_uncommitted(u64::MAX)
        .await
        .expect("list process intents");
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| {
        matches!(
            &entry.owner,
            Some(crate::AttachmentOwner::Process { process_id: owner }) if *owner == process_id
        )
    }));
    let attachment_ids = entries
        .iter()
        .map(|entry| entry.attachment_id.clone())
        .collect::<Vec<_>>();
    let report = crate::reclaim_unreferenced_attachments(
        &*factory,
        &*attachment_backend,
        crate::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: crate::EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("GC with recovered process row");
    assert_eq!(report.reclaimed_count, 0);
    for attachment_id in attachment_ids {
        attachment_backend
            .get(&attachment_id)
            .await
            .expect("recovered process attachment survives");
    }
}

/// FIG-2980, FIG-3611 L4: the worker binds the attachment owner from the
/// record the registry hands it. A start under a key whose process was pruned
/// starts a new process with a new id (ADR 0107), so its process-env session is
/// its own — never the pruned predecessor's tombstoned one — and its blobs root
/// under the id that is actually running. This drives the real lifecycle — run,
/// complete, prune, start again under the same key.
///
/// Red on the parent commit (it was ignored there): the re-registered name
/// reused the pruned process's tombstoned process-env session and could not
/// create it.
#[tokio::test]
async fn a_start_after_prune_binds_attachments_to_its_own_process() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let factory = backend.session_store_factory();
    let mut runtime_host = test_host_config(&backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(AttachmentWritingEngine)),
    );
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::builder("test-model")
            .context_window_tokens(16_384)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ArtifactOwner::host("attachment-owner-reincarnation-test"),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let worker = DurableProcessWorker::new({
        let watched =
            crate::watch_process_registry(Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>);
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            runtime_host,
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            local_owner("attachment-reincarnation-worker", "host-a", "start-a"),
        )
        .with_session_policy(policy)
    })
    .expect("valid test native substrate config");

    let registration = || {
        ProcessRegistration::new(
            ProcessInput::Engine {
                kind: "attachment-writing-engine".to_string(),
                payload: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
        .with_execution_env_ref(Some(env_ref.clone()))
        .with_start_key(Some(crate::StartKey::for_host(
            crate::StartKeyOwner::HOST,
            "attachment-owner-start",
        )))
    };

    let first = registry
        .register_process(registration())
        .await
        .expect("register the first process");
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive the first process");
    await_terminal(&registry, &first.id).await;

    let owners = |entries: &[crate::AttachmentManifestEntry]| {
        entries
            .iter()
            .map(|entry| match &entry.owner {
                Some(crate::AttachmentOwner::Process { process_id }) => process_id.clone(),
                other => panic!("every intent is process-owned: {other:?}"),
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    let process_env_store = |process_id: ProcessId| {
        let factory = Arc::clone(&factory);
        async move {
            let request = crate::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(format!("process-env:{process_id}")),
                relation: crate::SessionRelation::default(),
                policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            };
            factory
                .open_existing_store(&request)
                .await
                .expect("open process owner store")
                .expect("process owner store exists")
        }
    };

    let first_entries = process_env_store(first.id.clone())
        .await
        .list_uncommitted(u64::MAX)
        .await
        .expect("list the first process's intents");
    assert!(
        !first_entries.is_empty(),
        "precondition: the first run wrote process-owned intents"
    );
    assert_eq!(
        owners(&first_entries),
        std::collections::BTreeSet::from([first.id.clone()]),
        "the first run roots its attachments under its own id"
    );

    let terminal = registry
        .get_process(&first.id)
        .await
        .expect("read the terminal first process")
        .expect("the first process is still retained");
    registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the first process");
    let second = registry
        .register_process(registration())
        .await
        .expect("start again under the same key");
    assert_ne!(
        first.id, second.id,
        "a start after prune mints a new process, never the pruned id"
    );

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive the second process");
    await_terminal(&registry, &second.id).await;

    let second_entries = process_env_store(second.id.clone())
        .await
        .list_uncommitted(u64::MAX)
        .await
        .expect("list the second process's intents");
    assert!(
        !second_entries.is_empty(),
        "precondition: the second run wrote process-owned intents"
    );
    assert_eq!(
        owners(&second_entries),
        std::collections::BTreeSet::from([second.id.clone()]),
        "the new process binds under its own id, never the pruned predecessor's"
    );
}
