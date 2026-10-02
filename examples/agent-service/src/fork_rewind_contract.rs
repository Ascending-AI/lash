//! Deterministic embedding acceptance for the host-facing fork/rewind API.

use lash::SessionId;

use lash::persistence::{
    DeploymentStore, LeaseOwnerIdentity, RuntimeCommit, RuntimeSessionState, RuntimeStore,
    SessionCatalogStore, SessionCreationHead, SessionRelation, SessionStore,
    SessionStoreCreateRequest,
};
use std::sync::Arc;

/// Admit `request`'s session on the catalog and hold its view.
async fn admitted_view(
    stores: &Arc<dyn DeploymentStore>,
    request: &SessionStoreCreateRequest,
) -> SessionStore {
    SessionCatalogStore::admit_session(stores.as_ref(), request)
        .await
        .expect("admit the session");
    let runtime: Arc<dyn RuntimeStore> = stores.clone();
    SessionStore::new(runtime, request.session_id.clone()).expect("a valid session id")
}
use lash::process::{ProcessInput, ProcessObserverBy, ProcessProvenance, ProcessRegistration};
use lash::provider::LlmResponse;
use lash::runtime::SessionPolicy;
use lash::{CommitBudget, LashCore, LlmProfileMetadata, QueuedWorkBatchingConfig, TurnBudget};

#[tokio::test]
async fn host_can_rewind_from_a_surviving_branch_after_deleting_the_source() {
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
    let model = LlmProfileMetadata::builder("fork-contract-model")
        .context_window_tokens(8_192)
        .build()
        .expect("valid test model");
    let double = crate::state::test_support::test_double().await;
    let stores = double.engine_stores().session_store_factory();
    let processes = double.engine_stores().process_registry();
    let core = LashCore::standard_builder(double.lash_backend())
        .serve_test_llm_profile(provider, model.clone())
        .commit_budget(CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(QueuedWorkBatchingConfig::new(1024))
        .build(LeaseOwnerIdentity::opaque(
            "agent-service-fork-contract",
            "test-boot",
        ))
        .expect("fork contract core");
    // A session delete closes the session as a journaled effect, so it runs in
    // the service's own discard workflow, as the service deletes a chat.
    crate::state::test_support::serve_chat_discard(&double, &core).await;

    let source_policy = SessionPolicy {
        model: Some(lash::testing::test_llm_profile_config(
            model.wire_model.clone(),
            model.clone(),
        )),
        ..SessionPolicy::new(TurnBudget::Unbounded, lash::MaxToolCalls::new(1024))
    };
    let source = admitted_view(
        &stores,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::fixture(SOURCE_SESSION.to_string()),
            relation: SessionRelation::Root,
            config: source_policy.clone().into(),
            head: SessionCreationHead::Config,
        },
    )
    .await;
    admitted_view(
        &stores,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::fixture(FOREIGN_TARGET.to_string()),
            relation: SessionRelation::Root,
            config: source_policy.clone().into(),
            head: SessionCreationHead::Config,
        },
    )
    .await;
    let mut source_state = RuntimeSessionState::new(source_policy);
    source_state.session_id = SessionId::fixture(SOURCE_SESSION.to_string());
    source_state.ensure_agent_frame_initialized();
    source
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&source_state))
        .await
        .expect("commit retained continuation frame");
    let retained_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("committed frame has a leaf");

    // The committed head is a retained revision; the pin keeps it through
    // every collection for as long as the source session lives.
    let source_handle = core
        .session(SOURCE_SESSION)
        .durable()
        .await
        .expect("durable handle for the source session");
    let retained = source_handle
        .revisions()
        .await
        .expect("enumerate the source's retained revisions")
        .into_iter()
        .find(|revision| revision.head)
        .expect("the committed head is a retained revision");
    assert_eq!(
        retained.leaf_node_id.as_ref().map(|leaf| leaf.as_str()),
        Some(retained_node_id.as_str())
    );
    let target = lash::Target::Revision(retained.head_revision);
    source_handle
        .pin(target.clone())
        .await
        .expect("pin the source continuation");
    source_handle
        .pin(target.clone())
        .await
        .expect("pinning again changes nothing");
    let pinned = source_handle
        .revisions()
        .await
        .expect("enumerate retained host fork points")
        .into_iter()
        .find(|revision| revision.head_revision == retained.head_revision)
        .expect("the pinned revision is retained");
    assert_eq!(pinned.pinned_by, vec![target.clone()]);

    let observed_process = processes
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
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
        .fork_at(
            &SOURCE_SESSION.into(),
            target.clone(),
            lash::ForkRequest {
                session_id: (FIRST_BRANCH).into(),
                relation: SessionRelation::Fork {
                    source_session_id: (SOURCE_SESSION).into(),
                    source_node_id: Some((&retained_node_id).into()),
                },
                observed_processes: selected.clone(),
            },
        )
        .await
        .expect("fork retained continuation");
    assert_eq!(first_branch.session_id, FIRST_BRANCH);
    assert_eq!(first_branch.source_session_id, SOURCE_SESSION);

    let foreign_target_error = core
        .fork_at(
            &SOURCE_SESSION.into(),
            target.clone(),
            lash::ForkRequest {
                session_id: (FOREIGN_TARGET).into(),
                relation: SessionRelation::Fork {
                    source_session_id: (SOURCE_SESSION).into(),
                    source_node_id: Some((&retained_node_id).into()),
                },
                observed_processes: selected.clone(),
            },
        )
        .await
        .expect_err("an existing target must win over later fork validation");
    assert!(matches!(
        foreign_target_error,
        lash::EmbedError::Store(lash::persistence::StoreError::ForkSessionAlreadyExists {
            session_id
        }) if session_id == FOREIGN_TARGET
    ));

    let explicit_branch = core
        .fork_at(
            &SOURCE_SESSION.into(),
            target.clone(),
            lash::ForkRequest {
                session_id: (EXPLICIT_BRANCH).into(),
                relation: SessionRelation::Fork {
                    source_session_id: (SOURCE_SESSION).into(),
                    source_node_id: Some((&retained_node_id).into()),
                },
                observed_processes: selected.clone(),
            },
        )
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

    crate::chat_discard::discard_chat_session(&double.ingress(), SOURCE_SESSION)
        .await
        .expect("delete superseded source session");
    assert!(
        processes
            .list_observed_by(
                &SessionId::from(SOURCE_SESSION),
                &lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await
            .expect("read the deleted source's observations")
            .is_empty(),
        "the delete removes the source's process observer"
    );
    assert!(
        core.session(SOURCE_SESSION)
            .durable()
            .await
            .expect("durable handle for the retired source session")
            .was_deleted()
            .await
            .expect("read retirement fence")
    );

    // A session's pins and retained revisions go with it: the deleted source
    // names no state any more, and nothing is forked in its place.
    let deleted_source = core
        .fork_at(
            &SOURCE_SESSION.into(),
            target.clone(),
            lash::ForkRequest {
                session_id: (REWOUND_BRANCH).into(),
                relation: SessionRelation::Fork {
                    source_session_id: (SOURCE_SESSION).into(),
                    source_node_id: Some((&retained_node_id).into()),
                },
                observed_processes: Vec::new(),
            },
        )
        .await
        .expect_err("a deleted session has no revision to fork");
    assert!(matches!(
        deleted_source,
        lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { session_id })
            if session_id == SOURCE_SESSION
    ));

    // Selection is a snapshot: deleting its source does not revoke it.
    crate::chat_discard::discard_chat_session(&double.ingress(), EXPLICIT_BRANCH)
        .await
        .expect("delete selected source after selection");
    // The surviving branch's own head still names the same state: its
    // creation revision is an ordinary retained revision.
    let rewound = core
        .fork_at(
            &FIRST_BRANCH.into(),
            lash::Target::Revision(0),
            lash::ForkRequest {
                session_id: (REWOUND_BRANCH).into(),
                relation: SessionRelation::Fork {
                    source_session_id: (EXPLICIT_BRANCH).into(),
                    source_node_id: None,
                },
                observed_processes: selected.clone(),
            },
        )
        .await
        .expect("re-fork the surviving branch after the source's deletion");
    assert_eq!(rewound.session_id, REWOUND_BRANCH);
    assert_eq!(
        rewound.leaf_node_id.as_ref().map(|leaf| leaf.as_str()),
        Some(retained_node_id.as_str())
    );
    assert_eq!(rewound.source_session_id, FIRST_BRANCH);
    assert_eq!(rewound.head_revision, 0);
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
    assert!(
        matches!(
            SessionCatalogStore::lookup_session(stores.as_ref(), &REWOUND_BRANCH.into())
                .await
                .expect("read rewound lineage"),
            lash::persistence::SessionLookup::Live(_)
        ),
        "the rewound branch is live"
    );
    let runtime: Arc<dyn RuntimeStore> = stores.clone();
    let rewind_store =
        SessionStore::new(runtime, REWOUND_BRANCH.into()).expect("a valid session id");
    assert!(
        matches!(rewind_store.load_session_meta().await.expect("metadata").expect("metadata exists").relation,
        SessionRelation::Fork { source_session_id, source_node_id } if source_session_id == EXPLICIT_BRANCH
            && source_node_id.as_ref().map(|node| node.as_str()) == Some(retained_node_id.as_str()))
    );
}
