//! Deterministic embedding acceptance for the host-facing fork/rewind API.

use std::sync::Arc;

use lash::persistence::{
    InMemoryAttachmentStore, InMemoryProcessExecutionEnvStore, InMemorySessionStoreFactory,
    LeaseOwnerIdentity, RuntimeCommit, RuntimeSessionState, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory as _,
};
use lash::process::{
    ProcessInput, ProcessObserverBy, ProcessProvenance, ProcessRegistration, ProcessRegistry as _,
    RecoveryContract,
};
use lash::provider::LlmResponse;
use lash::runtime::SessionPolicy;
use lash::{CommitBudget, LashCore, ModelSpec, QueuedWorkBatchingConfig, TurnBudget};

#[tokio::test]
async fn host_can_rewind_from_a_retained_anchor_after_deleting_its_source() {
    const SOURCE_SESSION: &str = "fork-contract-source";
    const FOREIGN_TARGET: &str = "fork-contract-foreign-target";
    const FIRST_BRANCH: &str = "fork-contract-first-branch";
    const EXPLICIT_BRANCH: &str = "fork-contract-explicit-branch";
    const REWOUND_BRANCH: &str = "fork-contract-rewound-branch";

    let provider = lash::testing::TestProvider::builder()
        .kind("agent-service-fork-contract")
        .complete(|_request| async { Ok(LlmResponse::default()) })
        .build()
        .into_handle();
    let model = ModelSpec::builder("fork-contract-model")
        .context_window_tokens(8_192)
        .build()
        .expect("valid test model");
    let stores = Arc::new(InMemorySessionStoreFactory::new());
    let processes = Arc::new(lash::testing::TestLocalProcessRegistry::default());
    let core = LashCore::standard_builder(TurnBudget::Unbounded)
        .with_native_queued_work()
        .provider(provider)
        .model(model.clone())
        .store_factory(Arc::clone(&stores) as Arc<dyn lash::persistence::SessionStoreFactory>)
        .process_registry(Arc::clone(&processes) as Arc<dyn lash::process::ProcessRegistry>)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(InMemoryProcessExecutionEnvStore::new()))
        .commit_budget(CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(QueuedWorkBatchingConfig::new(1024))
        .build(LeaseOwnerIdentity::opaque(
            "agent-service-fork-contract",
            "test-boot",
        ))
        .expect("fork contract core");

    let source_policy = SessionPolicy {
        provider_id: "agent-service-fork-contract".to_string(),
        model,
        session_id: Some(SOURCE_SESSION.to_string()),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    };
    let source = stores
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SOURCE_SESSION.to_string(),
            relation: SessionRelation::Root,
            policy: source_policy.clone(),
        })
        .await
        .expect("create source session");
    stores
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: FOREIGN_TARGET.to_string(),
            relation: SessionRelation::Root,
            policy: SessionPolicy {
                session_id: Some(FOREIGN_TARGET.to_string()),
                ..source_policy.clone()
            },
        })
        .await
        .expect("create unrelated target session");
    let mut source_state = RuntimeSessionState::new(source_policy);
    source_state.session_id = SOURCE_SESSION.to_string();
    source_state.ensure_agent_frame_initialized();
    source
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&source_state, &[]))
        .await
        .expect("commit retained continuation frame");
    let retained_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("committed frame has a leaf");

    let pinned = core
        .pin(&retained_node_id)
        .await
        .expect("pin live source continuation");
    assert_eq!(pinned.node_id, retained_node_id);
    assert!(
        pinned.pinned,
        "pin must return an explicitly retained point"
    );

    let points = core
        .fork_points()
        .await
        .expect("enumerate retained host fork points");
    assert_eq!(points, vec![pinned.clone()]);

    processes
        .register_process(ProcessRegistration::new(
            "fork-contract-observed-process",
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register process observed by the source");
    processes
        .add_observer(
            SOURCE_SESSION,
            "fork-contract-observed-process",
            ProcessObserverBy::host("fork-contract-source-observer"),
        )
        .await
        .expect("observe process from source session");

    let first_branch = core
        .fork_at(&retained_node_id, FIRST_BRANCH)
        .await
        .expect("fork retained continuation");
    assert_eq!(first_branch.session_id, FIRST_BRANCH);
    assert_eq!(first_branch.source_session_id, SOURCE_SESSION);

    let foreign_target_error = core
        .fork_at(&retained_node_id, FOREIGN_TARGET)
        .await
        .expect_err("an existing target must win over later fork validation");
    assert!(matches!(
        foreign_target_error,
        lash::EmbedError::Store(lash::persistence::StoreError::ForkSessionAlreadyExists {
            session_id
        }) if session_id == FOREIGN_TARGET
    ));

    let explicit_branch = core
        .fork_at_with_observer_inheritance(&retained_node_id, EXPLICIT_BRANCH, Default::default())
        .await
        .expect("fork with explicit observer-inheritance policy");
    assert_eq!(explicit_branch.session_id, EXPLICIT_BRANCH);
    assert_eq!(explicit_branch.source_session_id, SOURCE_SESSION);
    let inherited = processes
        .list_observed_by(EXPLICIT_BRANCH)
        .await
        .expect("read inherited branch observations");
    assert_eq!(inherited[0].id, "fork-contract-observed-process");

    let delete_scope = core
        .session_delete_scope(SOURCE_SESSION)
        .await
        .expect("source session delete scope");
    let effect_host = core.effect_host();
    let scoped_effects = effect_host
        .scoped(delete_scope)
        .expect("matching source session delete scope");
    let deleted = core
        .delete_session(SOURCE_SESSION, scoped_effects)
        .await
        .expect("delete superseded source session");
    assert_eq!(deleted.session_id, SOURCE_SESSION);
    let process_delete = deleted.process.expect("process cleanup report");
    assert_eq!(process_delete.removed_observer_count, 1);
    assert!(
        core.session_was_deleted(SOURCE_SESSION)
            .await
            .expect("read retirement fence")
    );

    let retained_after_delete = core
        .fork_points()
        .await
        .expect("enumerate retained anchor after deleting its source")
        .into_iter()
        .find(|point| point.node_id == retained_node_id)
        .expect("pin survives deletion of its source session");
    assert_eq!(retained_after_delete.source_session_id, SOURCE_SESSION);
    assert!(
        retained_after_delete.pinned,
        "deleted-source anchor remains explicitly retained"
    );

    let rewound = core
        .fork_at(&retained_node_id, REWOUND_BRANCH)
        .await
        .expect("re-fork retained anchor after source deletion");
    assert_eq!(rewound.session_id, REWOUND_BRANCH);
    assert_eq!(rewound.node_id, retained_node_id);
    assert_eq!(rewound.source_session_id, SOURCE_SESSION);
}
