use super::*;
use crate::ActorContext;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn fork_inherits_history_without_execution_queues_waits_or_journals(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    host: ActorContext,
) {
    lash_core::testing::process_execution_env_fixture(host.backend().process_env_store().as_ref())
        .await;
    let source_id = SessionId::from("fork-isolated-source");
    let fork_id = SessionId::from("fork-isolated-branch");
    let source = factory
        .admit_view(&session_store_request(
            &source_id,
            "fork-model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("admit source");
    let mut state = crate::RuntimeSessionState {
        session_id: source_id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    state.ensure_agent_frame_initialized();
    append_conformance_event_node(&mut state, "shared-prefix", "shared historical prefix");
    commit_conformance_state(source.store(), &mut state)
        .await
        .expect("commit source prefix");
    let leaf = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source leaf");
    source
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            source_id.clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("source private input"),
        ))
        .await
        .expect("enqueue source input");
    source
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            &source_id,
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "source private work".into(),
            },
        ))
        .await
        .expect("enqueue source work");
    let source_head_meta = source.load_session_head_meta().await.expect("source head");
    let source_revision = source_head_meta
        .as_ref()
        .expect("the source has a head")
        .head_revision;
    let source_head = format!("{source_head_meta:?}");
    factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: fork_id.clone(),
            source_session_id: source_id.clone(),
            head_revision: source_revision,
            relation: crate::SessionRelation::Fork {
                source_session_id: source_id.clone(),
                source_node_id: Some(leaf.clone()),
            },
            config: state.policy.clone().into(),
        })
        .await
        .expect("fork source leaf");
    let branch = factory
        .live_view(&fork_id)
        .await
        .expect("open fork")
        .expect("fork exists");
    let read = branch
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .expect("fork history")
        .expect("fork window");
    assert_eq!(read.window.leaf_node_id, Some(leaf));
    assert_eq!(read.head_revision, 0);
    assert!(
        branch
            .list_pending_turn_inputs()
            .await
            .expect("fork input queue")
            .is_empty()
    );
    assert!(
        branch
            .list_queued_work()
            .await
            .expect("fork work queue")
            .is_empty()
    );
    assert_eq!(
        source
            .list_pending_turn_inputs()
            .await
            .expect("source input survives")
            .len(),
        1
    );
    assert_eq!(
        source
            .list_queued_work()
            .await
            .expect("source work survives")
            .len(),
        1
    );
    assert_eq!(
        format!(
            "{:?}",
            source
                .load_session_head_meta()
                .await
                .expect("source still independent")
        ),
        source_head
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn reclaim_races_fork_and_unpin_without_using_process_roots(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    host: ActorContext,
) {
    lash_core::testing::process_execution_env_fixture(host.backend().process_env_store().as_ref())
        .await;
    let id = SessionId::from("reclaim-race-source");
    let source = factory
        .admit_view(&session_store_request(
            &id,
            "reclaim-model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("source");
    let mut state = crate::RuntimeSessionState {
        session_id: id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    state.ensure_agent_frame_initialized();
    append_conformance_event_node(&mut state, "race-prefix", "retained prefix");
    commit_conformance_state(source.store(), &mut state)
        .await
        .expect("prefix");
    let leaf = state.session_graph.leaf_node_id.clone().expect("leaf");
    let prefix = crate::Target::Revision(state.head_revision);
    factory.pin(&id, &prefix).await.expect("pin prefix");
    let process = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            crate::ProcessProvenance::host().with_caused_by(Some(crate::CausalRef::SessionNode {
                session_id: id.clone(),
                node_id: leaf.to_string(),
            })),
            crate::Lifetime::Detached,
        ))
        .await
        .expect("independent live process reference");
    let request = crate::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("reclaim-race-fork"),
        source_session_id: id.clone(),
        head_revision: state.head_revision,
        relation: crate::SessionRelation::Root,
        config: state.policy.clone().into(),
    };
    let (fork, unpin, delete) = tokio::join!(
        factory.fork_session(&request),
        factory.unpin(&id, &prefix),
        factory.delete_session(&id)
    );
    // The delete takes the pin with the session, so an unpin that loses the
    // race finds the session gone.
    assert!(
        matches!(
            unpin,
            Ok(()) | Err(crate::StoreError::SessionDeleted { .. })
        ),
        "remove pin during reclamation: {unpin:?}"
    );
    delete.expect("delete producer during fork");
    match fork {
        Ok(_) => {
            let branch = factory
                .live_view(&request.session_id)
                .await
                .expect("branch")
                .expect("branch remains");
            assert!(
                crate::conformance::helpers::node_readable(&branch, &leaf)
                    .await
                    .expect("branch edge retains prefix")
            );
            factory
                .delete_session(&request.session_id)
                .await
                .expect("delete last graph root");
        }
        Err(crate::StoreError::SessionDeleted { session_id }) => assert_eq!(session_id, id),
        Err(error) => panic!("unexpected reclamation race refusal: {error:?}"),
    }
    assert!(
        registry
            .get_process(&process.id)
            .await
            .expect("process is not a graph root")
            .is_some()
    );
    assert!(matches!(
        crate::conformance::helpers::node_readable(&source, &leaf).await,
        Err(crate::StoreError::SessionDeleted { .. })
    ));
    assert!(
        matches!(
            factory.revisions(&id).await,
            Err(crate::StoreError::SessionDeleted { .. })
        ),
        "the reclaimed session retains no revision"
    );
}
