use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn compacted_process_tombstone_never_reopens_derived_sessions(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    _host: Arc<dyn crate::EffectHost>,
) {
    let id = registry
        .register_process(super::super::process_registry::registration(
            "compacted-derived-sessions",
        ))
        .await
        .expect("register process")
        .id;
    let sessions = crate::process_runtime_session_ids(&id);
    for session in &sessions {
        factory
            .admit_session(&session_store_request(
                session,
                "process-model",
                crate::SessionRelation::Root,
            ))
            .await
            .expect("materialize derived session");
    }
    registry
        .complete_process(
            &id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process");
    assert_eq!(
        registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune process")
            .pruned_processes,
        1
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, crate::ProjectionWatermark::NoProjector, None)
            .await
            .expect("compact process tombstone"),
        1
    );
    for session in sessions {
        assert!(
            factory
                .is_deleted(&session)
                .await
                .expect("session identity survives compaction")
        );
        assert!(
            factory
                .live_view(&session)
                .await
                .expect("noncreating lookup")
                .is_none()
        );
        assert!(matches!(
            factory
                .admit_session(&session_store_request(
                    &session,
                    "process-model",
                    crate::SessionRelation::Root
                ))
                .await,
            Err(crate::StoreError::SessionDeleted { .. })
        ));
        factory
            .vacuum(&session)
            .await
            .expect("vacuum deleted derived session");
        assert!(matches!(
            factory
                .admit_session(&session_store_request(
                    &session,
                    "process-model",
                    crate::SessionRelation::Root
                ))
                .await,
            Err(crate::StoreError::SessionDeleted { .. })
        ));
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn fork_inherits_history_without_execution_queues_waits_or_journals(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    host: Arc<dyn crate::EffectHost>,
) {
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
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
    let source_scope = crate::ExecutionScope::turn(source_id.clone(), "same-turn");
    let fork_scope = crate::ExecutionScope::turn(fork_id.clone(), "same-turn");
    let wait =
        crate::AwaitEventWaitIdentity::tool_completion(crate::ToolCallId::fixture("same-call"));
    let source_key = host
        .await_event_key(&source_scope, wait.clone())
        .await
        .expect("source wait");
    host.resolve_await_event(
        &source_key,
        crate::Resolution::Ok(serde_json::json!("source-only")),
    )
    .await
    .expect("settle source wait");
    let source_head = format!(
        "{:?}",
        source.load_session_head_meta().await.expect("source head")
    );
    factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: fork_id.clone(),
            node_id: leaf.clone(),
            relation: crate::SessionRelation::Fork {
                source_session_id: source_id.clone(),
                source_node_id: leaf.clone(),
            },
            policy: state.policy.clone(),
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
    assert!(read.usage.rows.is_empty());
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
    let fork_key = host
        .await_event_key(&fork_scope, wait)
        .await
        .expect("fork has its own wait");
    assert_ne!(fork_key, source_key);
    assert!(
        host.peek_await_event(&fork_key)
            .await
            .expect("fork wait was not copied")
            .is_none()
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
    _host: Arc<dyn crate::EffectHost>,
) {
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
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    append_conformance_event_node(&mut state, "race-prefix", "retained prefix");
    commit_conformance_state(source.store(), &mut state)
        .await
        .expect("prefix");
    let leaf = state.session_graph.leaf_node_id.clone().expect("leaf");
    factory.pin(&leaf).await.expect("pin prefix");
    let process = registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
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
        node_id: leaf.clone(),
        relation: crate::SessionRelation::Root,
        policy: state.policy.clone(),
    };
    let (fork, unpin, delete) = tokio::join!(
        factory.fork_session(&request),
        factory.unpin(&leaf),
        factory.delete_session(&id)
    );
    unpin.expect("remove pin during reclamation");
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
        Err(crate::StoreError::ForkPointNotRetained { node_id }) => assert_eq!(node_id, leaf),
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
        !factory
            .fork_points()
            .await
            .expect("reclaimed graph has no anchors")
            .iter()
            .any(|point| point.node_id == leaf)
    );
}
