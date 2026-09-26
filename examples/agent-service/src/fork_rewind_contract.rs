//! Deterministic embedding acceptance for the host-facing fork/rewind API.

use lash::SessionId;
use std::sync::Arc;

use lash::persistence::{
    LeaseOwnerIdentity, RuntimeCommit, RuntimeSessionState, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory as _,
};
use lash::process::{
    ProcessInput, ProcessObserverBy, ProcessObserverRegistry as _, ProcessProvenance,
    ProcessRegistrar as _, ProcessRegistration, RecoveryContract,
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
    let backend = Arc::new(
        lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("SQLite memory backend"),
    );
    let stores = backend.session_store_factory();
    let processes = backend.process_registry();
    let core = LashCore::standard_builder(backend.into(), TurnBudget::Unbounded)
        .provider(provider)
        .model(model.clone())
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
        session_id: Some(SessionId::from(SOURCE_SESSION.to_string())),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    };
    let source = stores
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(SOURCE_SESSION.to_string()),
            relation: SessionRelation::Root,
            policy: source_policy.clone(),
        })
        .await
        .expect("create source session");
    stores
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(FOREIGN_TARGET.to_string()),
            relation: SessionRelation::Root,
            policy: SessionPolicy {
                session_id: Some(SessionId::from(FOREIGN_TARGET.to_string())),
                ..source_policy.clone()
            },
        })
        .await
        .expect("create unrelated target session");
    let mut source_state = RuntimeSessionState::new(source_policy);
    source_state.session_id = SessionId::from(SOURCE_SESSION.to_string());
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

    let observed_process = processes
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
            lash::process::Lifetime::Detached,
        ))
        .await
        .expect("register process observed by the source")
        .id;
    processes
        .add_observer(
            &SessionId::from(SOURCE_SESSION),
            &observed_process,
            ProcessObserverBy::host("fork-contract-source-observer"),
        )
        .await
        .expect("observe process from source session");

    let selected = processes
        .list_observed_by(
            &SessionId::from(SOURCE_SESSION),
            &lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("host selects exact observed runs")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    let first_branch = core
        .fork_at(lash::ForkRequest {
            session_id: (FIRST_BRANCH).into(),
            node_id: (&retained_node_id).into(),
            relation: SessionRelation::Fork {
                source_session_id: (SOURCE_SESSION).into(),
                source_node_id: (&retained_node_id).into(),
            },
            observed_processes: selected.clone(),
        })
        .await
        .expect("fork retained continuation");
    assert_eq!(first_branch.session_id, FIRST_BRANCH);
    assert_eq!(first_branch.source_session_id, SOURCE_SESSION);

    let foreign_target_error = core
        .fork_at(lash::ForkRequest {
            session_id: (FOREIGN_TARGET).into(),
            node_id: (&retained_node_id).into(),
            relation: SessionRelation::Fork {
                source_session_id: (SOURCE_SESSION).into(),
                source_node_id: (&retained_node_id).into(),
            },
            observed_processes: selected.clone(),
        })
        .await
        .expect_err("an existing target must win over later fork validation");
    assert!(matches!(
        foreign_target_error,
        lash::EmbedError::Store(lash::persistence::StoreError::ForkSessionAlreadyExists {
            session_id
        }) if session_id == FOREIGN_TARGET
    ));

    let explicit_branch = core
        .fork_at(lash::ForkRequest {
            session_id: (EXPLICIT_BRANCH).into(),
            node_id: (&retained_node_id).into(),
            relation: SessionRelation::Fork {
                source_session_id: (SOURCE_SESSION).into(),
                source_node_id: (&retained_node_id).into(),
            },
            observed_processes: selected.clone(),
        })
        .await
        .expect("fork with explicit selected runs");
    assert_eq!(explicit_branch.session_id, EXPLICIT_BRANCH);
    assert_eq!(explicit_branch.source_session_id, SOURCE_SESSION);
    let inherited = processes
        .list_observed_by(
            &SessionId::from(EXPLICIT_BRANCH),
            &lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("read inherited branch observations");
    assert_eq!(inherited[0].id, observed_process);

    // The surviving branches need not agree about which work to observe.
    let other = processes
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
            lash::process::Lifetime::Detached,
        ))
        .await
        .expect("register second branch's work");
    processes
        .remove_observer(
            &SessionId::from(FIRST_BRANCH),
            &selected[0],
            ProcessObserverBy::host("choose-other-work"),
        )
        .await
        .expect("replace first branch observer");
    processes
        .add_observer(
            &SessionId::from(FIRST_BRANCH),
            &other.id,
            ProcessObserverBy::host("choose-other-work"),
        )
        .await
        .expect("observe other work");
    let selected = processes
        .list_observed_by(
            &SessionId::from(EXPLICIT_BRANCH),
            &lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("select intended survivor before deletion")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();

    let administration = core.session_administration().await;
    let context = administration
        .delete_context(SOURCE_SESSION)
        .expect("source session delete context");
    let deleted = LashCore::delete_session(context)
        .await
        .expect("delete superseded source session");
    assert_eq!(deleted.session_id, SOURCE_SESSION);
    let process_delete = deleted.process.expect("process cleanup report");
    assert_eq!(process_delete.removed_observer_count, 1);
    assert!(
        core.session(SOURCE_SESSION)
            .durable()
            .await
            .expect("durable handle for the retired source session")
            .was_deleted()
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

    // Selection is a snapshot: deleting its source does not revoke it.
    let context = administration
        .delete_context(EXPLICIT_BRANCH)
        .expect("selected source delete context");
    LashCore::delete_session(context)
        .await
        .expect("delete selected source after selection");
    let rewound = core
        .fork_at(lash::ForkRequest {
            session_id: (REWOUND_BRANCH).into(),
            node_id: (&retained_node_id).into(),
            relation: SessionRelation::Fork {
                source_session_id: (EXPLICIT_BRANCH).into(),
                source_node_id: (&retained_node_id).into(),
            },
            observed_processes: selected.clone(),
        })
        .await
        .expect("re-fork retained anchor after source deletion");
    assert_eq!(rewound.session_id, REWOUND_BRANCH);
    assert_eq!(rewound.node_id, retained_node_id);
    assert_eq!(rewound.source_session_id, SOURCE_SESSION);
    let observed = processes
        .list_observed_by(
            &SessionId::from(REWOUND_BRANCH),
            &lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("read selected branch observations after writer deletion");
    assert_eq!(
        observed.len(),
        1,
        "rewind must preserve explicitly selected live branch observers"
    );
    assert_eq!(observed[0].id, selected[0]);
    let rewind_store = stores
        .open_existing_store(&SessionStoreCreateRequest {
            session_id: REWOUND_BRANCH.into(),
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: SessionPolicy::new(TurnBudget::Unbounded),
        })
        .await
        .expect("read rewound lineage")
        .expect("rewound store");
    assert!(
        matches!(rewind_store.load_session_meta().await.expect("metadata").expect("metadata exists").relation,
        SessionRelation::Fork { source_session_id, .. } if source_session_id == EXPLICIT_BRANCH)
    );
}
