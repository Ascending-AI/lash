use super::*;

#[cfg(feature = "rlm")]
#[test]
pub(super) fn leaf_bearing_rlm_append_stale_branch_rolls_back_projection() -> Result<()> {
    run_async_test_on_stack_budget("rlm-leaf-append-stale-rollback-test", || async {
        let retained_payload =
            "x".repeat(lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES * 2);
        let source =
            format!("retained = [{{ payload: {retained_payload:?} }}]\nfinish \"committed\"");
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(queued_text_provider(vec![lashlang_block(&source)]))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session("rlm-leaf-append-stale-rollback")
            .open()
            .await?;
        session
            .turn(TurnInput::text("commit leaf-bearing state"))
            .run()
            .await?;

        let execution_before = session
            .admin()
            .state()
            .snapshot_execution()
            .await?
            .expect("RLM has live execution state after the committed turn");
        assert!(
            !execution_before.components.is_empty(),
            "the committed RLM state must contain at least one keyed leaf"
        );

        const ROLLED_BACK_MARKER: &str = "must-not-survive-stale-append";
        let writer = session.runtime.writer();
        let mut runtime = writer.lock().await;
        let result = runtime
            .append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: "leaf-bearing-stale-append".to_string(),
                nodes: vec![lash_core::SessionAppendNode::message(
                    lash_core::PluginMessage::text(
                        lash_core::MessageRole::User,
                        ROLLED_BACK_MARKER,
                    )
                    .with_id("leaf-bearing-stale-append-message"),
                )],
                requires_ancestor_node_id: Some("inactive-ancestor".to_string()),
            })
            .await?;
        assert!(matches!(
            result,
            lash_core::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
                if required_node_id == "inactive-ancestor"
        ));
        assert!(
            runtime.read_view().messages().iter().all(|message| message
                .parts
                .iter()
                .all(|part| part.content != ROLLED_BACK_MARKER)),
            "the stale append must be absent from the reconciled RLM history projection"
        );
        session.runtime.publish_from(&runtime);
        drop(runtime);

        let execution_after = session
            .admin()
            .state()
            .snapshot_execution()
            .await?
            .expect("RLM execution state survives the stale append rollback");
        assert_eq!(
            execution_after, execution_before,
            "the stale append rollback must preserve the live leaf-bearing RLM projection"
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct RlmExecutionSnapshotProbe {
    version: u32,
    engine: String,
    globals: std::collections::BTreeMap<String, RlmPersistedValueProbe>,
    deferred_resolutions: lash_lashlang_runtime::DeferredResolutionRecord,
}

#[cfg(feature = "rlm")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum RlmPersistedValueProbe {
    Inline {
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Leaf {
        component: String,
    },
}

#[cfg(feature = "rlm")]
impl RlmExecutionSnapshotProbe {
    fn global(
        &self,
        state: &lash_core::plugin::HydratedExecutionState,
        name: &str,
    ) -> Option<lashlang::Value> {
        let body = match self.globals.get(name)? {
            RlmPersistedValueProbe::Inline { body } => body.as_slice(),
            RlmPersistedValueProbe::Leaf { component } => {
                state.components.get(component)?.as_slice()
            }
        };
        lashlang::Snapshot::from_canonical_bytes(body)
            .ok()?
            .globals()
            .get("value")
            .cloned()
    }
}

#[cfg(feature = "rlm")]
pub(super) struct ColdReopenFrameState {
    switch_checkpoint_budget_bytes: usize,
    resident_execution_state: lash_core::plugin::HydratedExecutionState,
    execution_state: lash_core::plugin::HydratedExecutionState,
}

#[cfg(feature = "rlm")]
pub(super) async fn frame_switch_state_after_cold_reopen(
    session_id: &SessionId,
    abandoned_global_bytes: usize,
) -> Result<ColdReopenFrameState> {
    let dir = tempfile::tempdir().expect("tempdir");
    let sqlite_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let checkpoint_writes =
        lash_core::testing::checkpoint_observer::CheckpointWriteCollector::default();
    let store_factory = Arc::new(
        lash_core::testing::checkpoint_observer::ObservedSessionStoreFactory::new(
            sqlite_store_factory.clone() as Arc<dyn lash_core::SessionStoreFactory>,
            checkpoint_writes.clone(),
        ),
    );
    let abandoned_value = "x".repeat(abandoned_global_bytes);
    let switch_source = format!(
        r#"abandoned_global = {abandoned_value:?}
probe_result = await fixture.probe({{}})?
await control.continue_as({{ task: "finish after cold reopen", seed: {{ frame_seed: "seed:survives" }} }})?"#
    );
    let first_factory =
        rlm_factory().with_deferred_tool_resolver(Arc::new(FrameStateDeferredResolver));
    let first_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        first_factory,
    ))
    .provider(queued_text_provider(vec![lashlang_block(&switch_source)]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .tools(Arc::new(FrameStateDeferredTools))
    .plugin(Arc::new(StopAfterFrameSwitchCommitFactory))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;

    let switched = first_session
        .turn(TurnInput::text("switch away from the abandoned frame"))
        .run()
        .await?;
    assert!(matches!(
        switched.result.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(
        switched.result.errors.iter().any(|issue| issue
            .message
            .contains("stop after the accepted frame-switch commit")),
        "the test hook must stop automatic follow-through only after the switch commit: {switched:?}"
    );
    let switch_turn_index = switched.result.state.turn_index;

    let resident_execution_state = first_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("resident switched RLM has an execution snapshot");

    drop(switched);
    drop(first_session);
    drop(first_core);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let conn = rusqlite::Connection::open(sqlite_store_factory.catalog_path())
                .expect("open SQLite session catalog");
            let owner = conn
                .query_row(
                    "SELECT lease_owner_id FROM session_execution_leases WHERE session_id = ?1",
                    [session_id.as_str()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("read session execution lease row");
            if owner.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped runtime releases its session lane");

    let store_request = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
    };
    let store = lash_core::SessionStoreFactory::open_existing_store(
        sqlite_store_factory.as_ref(),
        &store_request,
    )
    .await
    .expect("open durable session store")
    .expect("frame-switch session is durable");
    let durable = store
        .load_session()
        .await?
        .expect("frame-switch session has a durable head");
    let checkpoint = durable
        .checkpoint
        .as_ref()
        .expect("frame-switch commit has a checkpoint");
    assert!(
        checkpoint
            .component_ref(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_none(),
        "the accepted switch commit and resident reset must agree on cleared execution state"
    );
    let switch_writes = checkpoint_writes
        .events()
        .into_iter()
        .filter(|event| event.session_id == session_id && event.turn_index == switch_turn_index)
        .collect::<Vec<_>>();
    assert_eq!(
        switch_writes.len(),
        1,
        "observe exactly one RuntimeCommit for the accepted frame-switch turn: {switch_writes:?}"
    );
    let switch_checkpoint_budget_bytes = checkpoint_writes
        .runtime_commit_budget(session_id, switch_writes[0].revision_before)
        .expect("observe the accepted frame-switch RuntimeCommit before its transaction")
        .checkpoint_bytes;
    drop(store);
    drop(durable);

    let reopened_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"finish "unused after state inspection""#,
    )]))
    .model(mock_model_spec())
    .store_factory(sqlite_store_factory)
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let reopened_session = reopened_core.session(session_id).open().await?;
    let execution_state = reopened_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("reopened RLM has an execution snapshot");

    Ok(ColdReopenFrameState {
        switch_checkpoint_budget_bytes,
        resident_execution_state,
        execution_state,
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn agent_frame_switch_clears_execution_state_across_cold_reopen() -> Result<()> {
    run_async_test_on_stack_budget("agent-frame-switch-cold-reopen-test", || async {
        let small =
            frame_switch_state_after_cold_reopen(&SessionId::from("frame-clear-small"), 16).await?;
        let large =
            frame_switch_state_after_cold_reopen(&SessionId::from("frame-clear-large"), 128 * 1024)
                .await?;

        for (geometry, state) in [
            ("resident", &large.resident_execution_state),
            ("cold-reopened", &large.execution_state),
        ] {
            let execution_state: RlmExecutionSnapshotProbe =
                rmp_serde::from_slice(&state.root).expect("decode canonical RLM execution root");
            assert!(
                execution_state.global(state, "abandoned_global").is_none(),
                "the old frame's globals must not survive in the {geometry} executor"
            );
            assert!(matches!(
                execution_state.global(state, "frame_seed"),
                Some(lashlang::Value::String(value)) if value.as_str() == "seed:survives"
            ));

            assert!(
                execution_state.deferred_resolutions.is_empty(),
                "the old frame's deferred resolutions must not survive in the {geometry} executor"
            );
        }

        let checkpoint_growth = large
            .switch_checkpoint_budget_bytes
            .abs_diff(small.switch_checkpoint_budget_bytes);
        assert!(
            checkpoint_growth < 1_024,
            "the budgeted RuntimeCommit checkpoint must not scale with 128 KiB of abandoned execution state: small={}, large={}, growth={checkpoint_growth}",
            small.switch_checkpoint_budget_bytes,
            large.switch_checkpoint_budget_bytes
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn durable_queued_chained_continue_as_survives_nested_commit_handoff() -> Result<()> {
    run_async_test_on_stack_budget("durable-queued-chained-continue-as-test", || {
        durable_queued_chained_continue_as_survives_nested_commit_handoff_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn durable_queued_chained_continue_as_survives_nested_commit_handoff_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "durable-queued-chained-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block(r#"await control.continue_as({ task: "switch again" })?"#),
        lashlang_block(r#"await control.continue_as({ task: "finish chain" })?"#),
        lashlang_block(r#"finish "done after chained handoffs""#),
    ]))
    .model(mock_model_spec())
    .store_factory(store_factory.clone())
    .plugin(Arc::new(TurnPersistedGraphAppendFactory {
        append_count: Arc::clone(&append_count),
        max_appends: 2,
    }))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    session
        .enqueue(TurnInput::text("start chained frame handoff"))
        .id("queued-chained-continue-as")
        .send()
        .await?;

    let output = session
        .queued_turn()
        .run()
        .await?
        .expect("queued chained turn should run");

    assert_eq!(append_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after chained handoffs"))
    );
    // The queued ingress admission and the outer chained turn each acquire
    // once; both nested handoffs borrow the outer fence.
    assert_sqlite_session_lane_free_at_generation(
        store_factory.as_ref(),
        &SessionId::from(session_id),
        2,
    );
    Ok(())
}

#[test]
pub(super) fn durable_agent_frame_follow_through_uses_distinct_turn_scopes_and_commits()
-> Result<()> {
    run_async_test_on_stack_budget("durable-agent-frame-follow-through-test", || {
        durable_agent_frame_follow_through_uses_distinct_turn_scopes_and_commits_inner()
    })
}

pub(super) async fn durable_agent_frame_follow_through_uses_distinct_turn_scopes_and_commits_inner()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "agent-frame-durable";
    let root_turn_id = "agent-frame-root-turn";
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.path().join("sessions"),
    ));
    let controller = Arc::new(RecordingDurableEffectController::default());
    let scoped_effect_controller = ScopedEffectController::borrowed(
        controller.as_ref(),
        lash_core::ExecutionScope::turn(session_id, root_turn_id),
    )
    .expect("scoped durable effect controller");
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(agent_frame_switch_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AgentFrameSwitchTools))
        .store_factory(store_factory.clone())
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )))
        .effect_host(Arc::new(
            lash_core::facade_support::NativeEffectHost::default(),
        ))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .process_env_store(Arc::new(DurableInMemoryProcessEnvStore::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    let activities = RecordingEvents::default();
    let output = session
        .turn(TurnInput::text("switch frames"))
        .turn_id(root_turn_id)
        .advanced()
        .stream_to_with_scope(&activities, scoped_effect_controller)
        .await?;

    assert_eq!(output.assistant_message(), Some("done after frame switch"));
    let follow_turn_id = TurnId::from(format!("{root_turn_id}:agent-frame:1"));
    let activities = activities.snapshot().await;
    let started = activities
        .iter()
        .enumerate()
        .filter_map(|(index, activity)| match &activity.event {
            TurnEvent::TurnStarted { turn_id } => Some((index, turn_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started.first().map(|(index, _)| *index), Some(0));
    assert_eq!(
        started
            .iter()
            .map(|(_, turn_id)| *turn_id)
            .collect::<Vec<_>>(),
        vec![root_turn_id, follow_turn_id.as_str()],
        "each physical frame turn must announce its own identity exactly once"
    );
    let mut llm_turn_ids = controller
        .invocations()
        .into_iter()
        .filter(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .map(|record| record.turn_id.expect("turn-scoped LLM effect"))
        .collect::<Vec<_>>();
    llm_turn_ids.sort();
    llm_turn_ids.dedup();
    assert_eq!(
        llm_turn_ids,
        vec![root_turn_id.to_string(), follow_turn_id.clone().to_string()]
    );
    let replay_keys = controller
        .invocations()
        .into_iter()
        .filter_map(|record| record.replay_key)
        .collect::<Vec<_>>();
    assert!(
        replay_keys.iter().any(|key| key.contains(root_turn_id)),
        "root turn replay keys should include {root_turn_id}: {replay_keys:?}"
    );
    assert!(
        replay_keys
            .iter()
            .any(|key| key.contains(follow_turn_id.as_str())),
        "follow turn replay keys should include {follow_turn_id}: {replay_keys:?}"
    );

    let conn = rusqlite::Connection::open(store_factory.catalog_path())
        .expect("open session sqlite store");
    let mut stmt = conn
        .prepare(
            "SELECT turn_id FROM runtime_turn_commits
             WHERE session_id = ?1 ORDER BY turn_id ASC",
        )
        .expect("prepare turn commits");
    let turn_commit_ids = stmt
        .query_map([session_id], |row| row.get::<_, String>(0))
        .expect("query turn commits")
        .map(|row| row.expect("read turn commit row"))
        .map(|encoded| {
            serde_json::from_str::<lash_core::OperationId>(&encoded)
                .expect("decode commit operation")
                .scope
                .turn_id()
                .expect("turn-scoped commit")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        turn_commit_ids,
        vec![root_turn_id.to_string(), follow_turn_id.to_string()]
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn processes_lists_started_lashlang_process_until_awaited() -> Result<()> {
    run_async_test_on_stack_budget("process-control-lashlang-process-test", || {
        processes_lists_started_lashlang_process_until_awaited_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn processes_lists_started_lashlang_process_until_awaited_inner() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"
process lookup(tools: Tools) {
  value = await tools.app_lookup({})?
  finish value
}
h = start lookup(tools: tools)
value = await h
finish value"#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
    // A started (`start lookup(...)`) process runs in the lease-protected
    // worker's rebuilt runtime, which needs a session store factory; the
    // explicit in-memory factory backs ephemeral process execution.
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-process-control-tool").open().await?;
    let turn_session = session.clone();
    let scoped_effect_controller = turn_scope(&SessionId::from(turn_session.session_id()));
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("start tool"))
            .advanced()
            .run_with_scope(scoped_effect_controller)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");

    let processes = session.admin().processes().list().await?;
    let running_app_lookup = processes.iter().any(|process| {
        process.kind == "lashlang" && process.label == "lookup" && !process.terminal
    });
    assert!(
        running_app_lookup,
        "expected running lookup lashlang process, got {processes:?}"
    );

    release_tx.send(()).expect("release tool provider");
    let result = turn.await.expect("turn task")?;
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!({
            "ok": true,
            "value": { "answer": "ready" },
        }))
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn lashlang_execution_graph_store_observes_lashlang_process_from_facade() -> Result<()> {
    run_async_test_on_stack_budget("lashlang-graph-store-facade-test", || {
        lashlang_execution_graph_store_observes_lashlang_process_from_facade_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn lashlang_execution_graph_store_observes_lashlang_process_from_facade_inner()
-> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let graph_store = Arc::new(crate::tracing::TraceLashlangGraphStore::default());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory().with_lashlang_execution_sink(
            Arc::clone(&graph_store) as Arc<dyn crate::tracing::TraceSink>
        ),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"
process lookup(tools: Tools) {
  value = await tools.app_lookup({})?
  finish value
}
h = start lookup(tools: tools)
value = await h
finish value"#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-lashlang-graph-store").open().await?;
    let turn_session = session.clone();
    let scoped_effect_controller = turn_scope(&SessionId::from(turn_session.session_id()));
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("start tool"))
            .advanced()
            .run_with_scope(scoped_effect_controller)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");

    let processes = session.admin().processes().list().await?;
    let running = processes
        .iter()
        .find(|process| process.label == "lookup")
        .expect("running lookup process");
    let graph = graph_store
        .graph(&format!("process:{}", running.process_id))
        .expect("Lashlang graph snapshot");
    assert_eq!(graph.graph_key, format!("process:{}", running.process_id));
    assert_eq!(graph.entry_kind, "process");
    assert_eq!(graph.entry_name, "lookup");
    assert_eq!(
        graph.status,
        lash_lashlang_runtime::TraceLanguageExecutionStatus::Running
    );
    assert!(!graph.nodes.is_empty());
    assert!(
        graph_store
            .graphs()
            .iter()
            .any(|graph| graph.entry_name == "lookup")
    );

    release_tx.send(()).expect("release tool provider");
    let _ = turn.await.expect("turn task")?;
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
pub(super) async fn natural_rlm_completion_emits_no_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec!["done in prose"]))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-prose-completion").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("answer directly"))
        .allow_prose_or_finish()?
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    assert!(!events.iter().any(|event| matches!(
        &event.event,
        TurnEvent::FinalValue { .. } | TurnEvent::ToolValue { .. }
    )));
    assert_eq!(assistant_prose(&events), "done in prose");
    let read_view = result.state.read_view();
    let assistant_messages = read_view
        .messages()
        .iter()
        .filter(|message| message.role == lash_core::MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].parts[0].content, "done in prose");
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
pub(super) async fn finish_required_rlm_completion_emits_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![lashlang_block(
        r#"finish "done via finish""#,
    )]))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("rlm-finish-required-completion")
        .open()
        .await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .turn(TurnInput::text("finish"))
        .require_finish()?
        .stream_to(events.as_ref())
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
    ));
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!("done via finish"))
    );
    let events = events.snapshot().await;
    let terminal_output = events
        .iter()
        .find(|event| matches!(&event.event, TurnEvent::FinalValue { .. }))
        .expect("terminal output");
    let TurnEvent::FinalValue { value } = &terminal_output.event else {
        unreachable!();
    };
    assert_eq!(value, &serde_json::json!("done via finish"));
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
pub(super) async fn rlm_failed_code_emits_failed_code_completion_without_fake_tools() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(queued_text_provider(vec![
        lashlang_block("this is not valid lashlang"),
        lashlang_block(r#"finish "recovered""#),
    ]))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-failed-code-event").open().await?;
    let events = RecordingEvents::default();

    let _result = session
        .turn(TurnInput::text("bad code"))
        .stream_to(&events)
        .await?;

    let events = events.snapshot().await;
    let failed = events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                TurnEvent::CodeBlockCompleted {
                    success: false,
                    error: Some(_),
                    ..
                }
            )
        })
        .expect("failed code completion");
    let next_code = events[failed + 1..]
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::CodeBlockStarted { .. }))
        .map(|offset| failed + 1 + offset)
        .unwrap_or(events.len());
    assert!(
        !events[failed + 1..next_code]
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
    );
    Ok(())
}

/// FIG-1573: a hard-killed host leaves a live session-execution-lease row; the
/// reopened process must claim its queued turn within one lease TTL.
///
/// Field shape (hirsel, durable SQLite): `send_message` was accepted onto the
/// queued-work path and the host process was killed before the drain claimed
/// it. The lease row the dead boot left behind cannot be released by anyone,
/// so the store's expiry check is the only thing that frees the lane. The
/// reopened process then drains every 30s and reports "claimed nothing
/// (session execution lease busy)" indefinitely.
#[tokio::test]
pub(super) async fn fig1573_queued_turn_claims_after_a_hard_killed_boot_left_a_live_lane()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "fig1573-agent-g1";
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let store_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path().join("sessions"))
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );

    // Boot 1: the host accepts a queued turn, then is hard-killed.
    let first_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-boot-1")
                    .complete(|_request| async { Ok(text_response("boot one must not answer")) })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .without_queued_work()
            .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;
    first_session
        .enqueue(TurnInput::text("what is the status of the migration?"))
        .id("fig1573-queued-request")
        .send()
        .await?;
    drop(first_session);
    drop(first_core);

    // The lane a SIGTERM leaves behind: a live lease row owned by a boot that
    // will never renew and never release it. Taking it on a bare store handle
    // and dropping the handle reproduces that row exactly - an in-process guard
    // drop would spawn the best-effort release a killed process never performs.
    let dead_boot_store = lash_core::SessionStoreFactory::create_store(
        store_factory.as_ref(),
        &lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(session_id.to_string()),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
        },
    )
    .await?;
    let dead_lane = dead_boot_store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &lash_core::LeaseOwnerIdentity::opaque("fig1573-host", "fig1573-host:boot-1"),
            "fig1573-boot-1-executor",
            lash_core::facade_support::LeaseTimings::default().ttl_ms(),
        )
        .await?
        .acquired()
        .expect("the dying boot held the lane");
    let dead_lane_expiry = dead_lane.expires_at_epoch_ms;
    std::mem::forget(dead_lane);
    drop(dead_boot_store);

    // Boot 2 comes up after the dead boot's lease expires. Session recovery is
    // itself lease-fenced, so a successor cannot hydrate the session at 14s
    // and merely wait to acquire the lane later; it must first cross the same
    // expiry boundary that makes the queued turn drainable.
    clock.advance(dead_lane_expiry - lash_core::ClockWallTime::timestamp_ms(clock.as_ref()));
    let second_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-boot-2")
                    .complete(|_request| async { Ok(text_response("the migration is green")) })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .without_queued_work()
            .build(crate::testing::runtime_lease_owner())?;
    let second_session = second_core.session(session_id).open().await?;
    assert_eq!(
        second_session.pending_turn_inputs().await?.len(),
        1,
        "the queued turn is still pending after the reopen"
    );

    let mut claimed_at_ms = None;
    for _attempt in 0..8 {
        if let Some(output) = second_session.queued_turn().run().await?.ran() {
            assert_eq!(output.assistant_message(), Some("the migration is green"));
            claimed_at_ms = Some(lash_core::ClockWallTime::timestamp_ms(clock.as_ref()));
            break;
        }
        clock.advance(30_000);
    }

    let claimed_at_ms = claimed_at_ms.expect(
        "the queued turn must be claimed by the reopened process, not wedged behind the dead \
         boot's lease row",
    );
    assert!(
        claimed_at_ms >= dead_lane_expiry && claimed_at_ms < dead_lane_expiry + 60_000,
        "the drain must claim on the first probe after the dead boot's lease expires \
         (claimed at {claimed_at_ms}, dead lane expired at {dead_lane_expiry})"
    );
    Ok(())
}

/// FIG-1573: an active-turn-scoped input orphaned by a hard kill must become
/// drainable again in the reopened process.
///
/// A host that routes `send_message` into the turn currently running writes a
/// `pending_active` row scoped to that turn id. The only thing that ever moves
/// such a row back to `deferred_next_turn` is the interrupted-input re-defer
/// carried by that same turn's own final commit
/// (`RuntimeCommit::deferring_interrupted_turn_inputs`, applied by
/// `commit_runtime_turn`). A hard kill skips that commit, and nothing at
/// session reopen re-defers the row: the next-turn drain matches only
/// `state = 'deferred_next_turn'`, and an active-turn claim would have to name
/// a turn id that can never exist again. Before the fix the row stayed visible
/// to `pending_turn_inputs` forever while every drain claimed nothing - the
/// field signature in FIG-1573.
///
/// The regression law: the drain-time backstop repairs the row and the reopened
/// process delivers it in that same drain. Remove
/// `defer_orphaned_turn_inputs_before_drain` from `stream_queued_work` and this
/// test goes red again - the dying boot released its lease on the way out, so
/// the successor observes no displacement and nothing else in the reopened
/// process ever reaches those rows.
#[tokio::test]
pub(super) async fn fig1573_active_turn_input_orphaned_by_a_hard_kill_is_drained_after_reopen()
-> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "fig1573-orphaned-active-input";
    let interrupted_turn_id = "fig1573-interrupted-turn";
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let store_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path().join("sessions"))
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );

    // Boot 1: a turn is running and the host routes an input into it. The
    // provider never answers, so the turn never reaches its final commit - the
    // only writer of the interrupted-input re-defer.
    let provider_entered = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::clone(&provider_entered);
    let first_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-hung-boot-1")
                    .complete(move |_request| {
                        let entered = Arc::clone(&entered);
                        async move {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                            unreachable!("the killed boot never answers")
                        }
                    })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .without_queued_work()
            .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;
    first_session
        .enqueue(TurnInput::text("what is the status of the migration?"))
        .id("fig1573-queued-request")
        .ingress(lash_core::TurnInputIngress::active_turn(
            interrupted_turn_id,
            lash_core::TurnInputCheckpointBoundary::default(),
        ))
        .send()
        .await?;
    {
        let running = first_session
            .turn(TurnInput::text("start the long turn"))
            .turn_id(interrupted_turn_id)
            .run();
        let mut running = std::pin::pin!(running);
        tokio::select! {
            _ = &mut running => panic!("the hung provider must not complete the turn"),
            () = provider_entered.notified() => {}
        }
        // Dropping the in-flight turn and the core is the hard kill: no final
        // commit, so no interrupted-input re-defer is ever written.
    }
    drop(first_session);
    drop(first_core);

    // Boot 2 cannot admit the session until the hard-killed turn's execution
    // lease expires. Advance to that boundary before reopening; admission now
    // fences recovery itself rather than letting a successor hydrate early and
    // wait to acquire the lane only when it starts draining.
    clock.advance(lash_core::facade_support::LeaseTimings::default().ttl_ms());
    let second_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(
                crate::testing::TestProvider::builder()
                    .kind("fig1573-boot-2")
                    .complete(|_request| async { Ok(text_response("the migration is green")) })
                    .build()
                    .into_handle(),
            )
            .model(mock_model_spec())
            .clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .store_factory(store_factory.clone())
            .without_queued_work()
            .build(crate::testing::runtime_lease_owner())?;
    let second_session = second_core.session(session_id).open().await?;
    assert_eq!(
        second_session.pending_turn_inputs().await?.len(),
        2,
        "the hard kill leaves both the orphaned routed input and the killed turn's own \
         acceptance pending after the reopen"
    );

    // Two rows, two drains: the killed turn's own acceptance is claimable
    // immediately, while the input routed into that dead turn only becomes
    // claimable once a drain that finds nothing runs the FIG-1573 backstop.
    let mut claimed = None;
    for _attempt in 0..10 {
        if let Some(output) = second_session.queued_turn().run().await?.ran() {
            claimed = Some(output);
            if second_session.pending_turn_inputs().await?.is_empty() {
                break;
            }
            continue;
        }
        clock.advance(30_000);
    }

    let output = claimed.expect(
        "the reopened process must drain the orphaned input; without the drain-time backstop \
         every drain claims nothing, because the row is stuck in pending_active with a turn id \
         that no longer exists",
    );
    assert_eq!(output.assistant_message(), Some("the migration is green"));
    assert!(
        second_session.pending_turn_inputs().await?.is_empty(),
        "the drained input must leave the pending queue"
    );
    Ok(())
}

pub(super) fn gated_app_lookup_provider(
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    released: Arc<std::sync::atomic::AtomicBool>,
    provider_calls: Arc<AtomicUsize>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let released = Arc::clone(&released);
            let provider_calls = Arc::clone(&provider_calls);
            async move {
                match provider_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        started.notify_one();
                        while !released.load(Ordering::SeqCst) {
                            let notified = release.notified();
                            if released.load(Ordering::SeqCst) {
                                break;
                            }
                            notified.await;
                        }
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "call-1".to_string(),
                                tool_name: "app_lookup".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        })
                    }
                    _ => Ok(text_response("finished after the stop")),
                }
            }
        })
        .build()
        .into_handle()
}

#[tokio::test]
pub(super) async fn cancel_running_turns_after_step_stops_at_the_step_boundary() -> Result<()> {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(gated_app_lookup_provider(
            Arc::clone(&started),
            Arc::clone(&release),
            Arc::clone(&released),
            Arc::clone(&provider_calls),
        ))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("stop-after-step").open().await?;
    let stopper = session.clone();

    let stream = session
        .turn(TurnInput::text("use the tool, then stop"))
        .stream()?;
    started.notified().await;
    assert_eq!(
        stopper.cancel_running_turns_with_origin_and_mode(
            Some("shutdown".to_string()),
            crate::TurnCancelMode::AfterStep
        ),
        1
    );
    released.store(true, Ordering::SeqCst);
    release.notify_one();

    let result = stream.finish().await?;
    let evidence = match &result.outcome {
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { evidence }) => {
            evidence.clone()
        }
        other => panic!("expected an after-step stop, got {other:?}"),
    };
    assert_eq!(evidence.mode, crate::TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(evidence.origin.as_deref(), Some("shutdown"));
    assert_eq!(
        result.tool_calls.len(),
        1,
        "the tool call of the closing step ran to completion"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "no further model call starts after the step boundary"
    );
    assert_eq!(stopper.cancel_running_turns(), 0);
    Ok(())
}

#[tokio::test]
pub(super) async fn host_escalates_a_local_after_step_stop_to_an_immediate_abort() -> Result<()> {
    let started = Arc::new(tokio::sync::Notify::new());
    let provider_calls = Arc::new(AtomicUsize::new(0));
    // The response never arrives, so an after-step stop can never land by
    // itself; the host escalates after its own deadline.
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(gated_app_lookup_provider(
            Arc::clone(&started),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::clone(&provider_calls),
        ))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("escalate-after-step").open().await?;
    let stopper = session.clone();

    let stream = session
        .turn(TurnInput::text("hang, then escalate"))
        .stream()?;
    started.notified().await;
    assert_eq!(
        stopper.cancel_running_turns_with_mode(crate::TurnCancelMode::AfterStep),
        1
    );
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        stopper.cancel_running_turns_with_mode(crate::TurnCancelMode::AfterStep),
        1,
        "the after-step stop leaves the turn running until its step closes"
    );
    assert_eq!(
        stopper.cancel_running_turns_with_origin(Some("operator".to_string())),
        1,
        "escalation aborts the still-running turn"
    );

    let result = stream.finish().await?;
    let evidence = match &result.outcome {
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { evidence }) => {
            evidence.clone()
        }
        other => panic!("expected an aborted turn, got {other:?}"),
    };
    assert_eq!(evidence.mode, crate::TurnCancelMode::Immediate);
    assert_eq!(evidence.honoured_after_step, None);
    assert!(result.tool_calls.is_empty(), "the response never arrived");
    assert_eq!(stopper.cancel_running_turns(), 0);
    Ok(())
}
