use super::*;
use crate::AttachmentManifest;
use crate::store::{SessionCommitStore, SessionExecutionLeaseStore};

struct AttachmentWritingEngine;

struct ParentBoundSessionStoreFactory {
    store: Arc<InMemorySessionStore>,
}

// This test factory owns one attachment-aware store, so its explicit root-set
// capability projects that store's manifest rather than asserting emptiness.
#[async_trait::async_trait]
impl crate::AttachmentRootSet for ParentBoundSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        self.store
            .forget_aged_uncommitted_intents(intent_grace_cutoff_epoch_ms)?;
        Ok(self.store.list_all_refs()?.into_iter().collect())
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        self.store
            .has_live_ref_for_id(id, intent_grace_cutoff_epoch_ms)
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for ParentBoundSessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(self.store.clone())
    }

    async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

async fn parent_bound_session_store(policy: crate::SessionPolicy) -> Arc<InMemorySessionStore> {
    const PARENT_SESSION_ID: &str = "parent-bound-process-worker";
    let store = Arc::new(InMemorySessionStore::default());
    let owner = crate::LeaseOwnerIdentity::opaque("parent-owner", "parent-incarnation");
    let _lease = store
        .try_claim_session_execution_lease(PARENT_SESSION_ID, &owner, 60_000)
        .await
        .expect("claim parent session lease")
        .acquired()
        .expect("parent session lease acquired");
    let state = crate::RuntimeSessionState {
        session_id: PARENT_SESSION_ID.to_string(),
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
                    full_text: "nested turn complete".to_string(),
                    parts: vec![crate::llm::types::LlmOutputPart::Text {
                        text: "nested turn complete".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..Default::default()
                })
            })
            .build();
        let mut nested_host = RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        nested_host.durability.attachment_store = Arc::clone(&attachment_store);
        let mut nested_runtime =
            crate::runtime::tests::helpers::runtime_with_plugins_and_tools_and_host(
                Vec::new(),
                Arc::new(crate::runtime::tests::helpers::EmptyTools),
                provider,
                crate::EmbeddedRuntimeHost::new(nested_host),
            )
            .await;
        nested_runtime
            .stream_turn(
                crate::TurnInput::text("run nested turn"),
                crate::TurnOptions::new(
                    CancellationToken::new(),
                    crate::runtime::tests::helpers::named_turn_scope("root", "nested-engine-turn"),
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
        Ok(crate::ProcessAwaitOutput::Success {
            value: serde_json::Value::Null,
            control: None,
        }
        .into())
    }
}

#[tokio::test]
async fn process_runtime_keeps_state_separate_from_parent_bound_attachment_manifest() {
    const PROCESS_ID: &str = "parent-bound-process";
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::builder("test-model")
            .context_window_tokens(16_384)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let parent_store = parent_bound_session_store(policy.clone()).await;
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(ParentBoundSessionStoreFactory {
        store: Arc::clone(&parent_store),
    });
    let attachment_backend = Arc::new(crate::InMemoryAttachmentStore::new());
    let mut runtime_host = RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    runtime_host.durability.attachment_store =
        Arc::new(crate::SessionAttachmentStore::ephemeral(attachment_backend));
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(Vec::new())),
            runtime_host,
            factory,
            Arc::new(TestLocalProcessRegistry::default()),
        )
        .with_session_policy(policy.clone()),
    );

    let runtime = Box::pin(worker.build_process_runtime(
        format!("process-env:{PROCESS_ID}"),
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
        .bind_process_scoped(PROCESS_ID);
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
        .expect("list parent-bound process intents");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, format!("process-env:{PROCESS_ID}"));
    assert_eq!(
        entries[0].owner_kind,
        Some(crate::AttachmentOwnerKind::Process)
    );
    assert_eq!(entries[0].owner_id.as_deref(), Some(PROCESS_ID));
}

#[tokio::test]
async fn engine_put_after_nested_turn_restores_the_durable_process_owner() {
    const PROCESS_ID: &str = "attachment-owner-recovered-engine";
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let factory = Arc::new(crate::InMemorySessionStoreFactory::new());
    let attachment_backend = Arc::new(crate::InMemoryAttachmentStore::new());
    let mut runtime_host = RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    runtime_host.durability.attachment_store = Arc::new(crate::SessionAttachmentStore::ephemeral(
        attachment_backend.clone(),
    ));
    runtime_host.process_engines =
        crate::ProcessEngineRegistry::new().with_engine(Arc::new(AttachmentWritingEngine));
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::builder("test-model")
            .context_window_tokens(16_384)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let env_ref = crate::persist_process_execution_env(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(Vec::new())),
            runtime_host,
            factory.clone() as Arc<dyn SessionStoreFactory>,
            Arc::clone(&registry),
        )
        .with_session_policy(policy)
        .with_lease_owner(local_owner("attachment-worker", "host-a", "start-a")),
    );
    registry
        .register_process(
            ProcessRegistration::new(
                PROCESS_ID,
                ProcessInput::Engine {
                    kind: "attachment-writing-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                RecoveryDisposition::Rerunnable,
                crate::ProcessProvenance::host(),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register process");

    worker
        .drive_pending_processes()
        .await
        .expect("recover process");
    await_terminal(&registry, PROCESS_ID).await;

    let request = crate::SessionStoreCreateRequest {
        session_id: format!("process-env:{PROCESS_ID}"),
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
        .expect("list process intents");
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| {
        entry.owner_kind == Some(crate::AttachmentOwnerKind::Process)
            && entry.owner_id.as_deref() == Some(PROCESS_ID)
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
