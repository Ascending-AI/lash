use super::*;

#[cfg(feature = "rlm")]
#[test]
pub(super) fn leaf_bearing_rlm_append_stale_branch_rolls_back_projection() -> Result<()> {
    run_async_test_on_stack_budget("rlm-leaf-append-stale-rollback-test", || async {
        let retained_payload =
            "x".repeat(lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES * 2);
        let source = format!(
            "const retained = [{{ payload: {retained_payload:?} }}];\nfinish(\"committed\");"
        );
        let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
            .provider(queued_text_provider(vec![typescript_block(&source)]))
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())?;
        serve_processes(&core);
        let session = core
            .session("rlm-leaf-append-stale-rollback")
            .open()
            .await?;
        session
            .send(TurnInput::text("commit leaf-bearing state"))
            .output()
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
        let result = Box::pin(
            runtime.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: "leaf-bearing-stale-append".to_string(),
                nodes: vec![lash_core::SessionAppendNode::message(
                    lash_core::PluginMessage::text(
                        lash_core::MessageRole::User,
                        ROLLED_BACK_MARKER,
                    )
                    .with_id("leaf-bearing-stale-append-message"),
                )],
                requires_ancestor_node_id: Some("inactive-ancestor".to_string().into()),
            }),
        )
        .await?;
        assert!(matches!(
            result,
            lash_core::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
                if required_node_id == "inactive-ancestor"
        ));
        assert!(
            runtime
                .read_view()
                .expect("test runtime frame scope resolves")
                .messages()
                .iter()
                .all(|message| message
                    .parts
                    .iter()
                    .all(|part| part.content() != ROLLED_BACK_MARKER)),
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
    #[serde(with = "serde_bytes")]
    state_header: Vec<u8>,
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
        let fragments = self
            .globals
            .iter()
            .map(|(global, persisted)| {
                let body = match persisted {
                    RlmPersistedValueProbe::Inline { body } => body.as_slice(),
                    RlmPersistedValueProbe::Leaf { component } => {
                        state.components.get(component)?.as_ref()
                    }
                };
                Some((global.as_str(), body))
            })
            .collect::<Option<Vec<_>>>()?;
        let (reloaded, _) = lashlang::State::from_durable_parts(
            &self.state_header,
            fragments,
            lash_core::FleetFormat::current(),
        )
        .ok()?;
        reloaded.globals().get(name).cloned()
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
    const SEED: u64 = 0x0c01_d4e0;
    let double = restate_double(SEED).await;
    let sqlite_store_factory = double.stores().session_store_factory();
    let checkpoint_writes =
        lash_core::testing::checkpoint_observer::CheckpointWriteCollector::default();
    let observed_writes = checkpoint_writes.clone();
    let backend =
        DecoratedBackend::over(double.lash_backend()).session_store_factory(move |inner| {
            Arc::new(
                lash_core::testing::checkpoint_observer::ObservedSessionStoreFactory::new(
                    inner,
                    observed_writes,
                ),
            )
        });
    // The retired surface inlined the whole abandoned payload as a literal; the
    // TypeScript compiler refuses a cell over 64 KiB of source (ADR 0096), so
    // the same global is built at runtime instead of spelled out.
    let switch_source = format!(
        r#"const abandoned_global = "x".repeat({abandoned_global_bytes});
const probe_result = await fixture.probe({{}});
await control.continue_as({{ task: "finish after cold reopen", seed: {{ frame_seed: "seed:survives" }} }});"#
    );
    let first_factory = rlm_factory(&backend.clone().into())
        .with_deferred_tool_resolver(Arc::new(FrameStateDeferredResolver));
    let (follow_on_started_tx, follow_on_started_rx) = oneshot::channel::<()>();
    let follow_on_started_tx = Arc::new(StdMutex::new(Some(follow_on_started_tx)));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let follow_on_requests = Arc::new(StdMutex::new(Vec::<LlmRequest>::new()));
    let captured_follow_on_requests = Arc::clone(&follow_on_requests);
    // The first follow-on call is held until its attempt dies; the redriven
    // attempt's call answers.
    let first_provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let source = switch_source.clone();
            let started = Arc::clone(&follow_on_started_tx);
            let calls = Arc::clone(&provider_calls);
            let captured = Arc::clone(&captured_follow_on_requests);
            async move {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => return Ok(text_response(&typescript_block(&source))),
                    1 => {
                        if let Some(tx) = started.lock_recover().take() {
                            let _ = tx.send(());
                        }
                        std::future::pending::<()>().await;
                        unreachable!("the held follow-on call dies with its attempt")
                    }
                    _ => {}
                }
                captured.lock_recover().push(request);
                Ok(text_response(&typescript_block(
                    r#"finish("completed after real SQLite cold reopen");"#,
                )))
            }
        })
        .build()
        .into_handle();
    let first_core = explicit_ephemeral_facets(LashCore::rlm_builder(
        backend.clone().into(),
        crate::TurnBudget::Unbounded,
        first_factory,
    ))
    .provider(first_provider)
    .model(mock_model_spec())
    .tools(Arc::new(FrameStateDeferredTools))
    .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;

    let root_id = format!("{session_id}:switch-root");
    let switched = first_session
        .send(TurnInput::text("switch away from the abandoned frame"))
        .id(root_id.clone())
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), follow_on_started_rx)
        .await
        .expect("the drive reaches the follow-on provider call")
        .expect("follow-on provider signal");
    drop(switched);
    let switch_turn_index = 1;
    // The switch commit is the durable head while the follow-on call is held.
    let store_request = lash_core::SessionStoreCreateRequest {
        owning_process_id: None,
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

    // The attempt dies while the follow-on model call is held; the engine
    // redrives the root, which replays its journal up to the follow-on call
    // and answers it.
    let turn_invocation = double
        .server()
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation.target.starts_with("LashTurn/")
                && invocation.target.contains(session_id.as_str())
                && invocation.status == "running"
        })
        .expect("the switch root's turn invocation runs");
    assert!(
        double.server().crash(&turn_invocation.id),
        "the held attempt dies"
    );
    let redriven = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        first_session.root(root_id.clone()).outcome(),
    )
    .await
    .expect("the redriven root settles")?;
    assert_eq!(redriven.status, crate::TurnStatus::Answered);

    let resident_execution_state = first_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("resident switched RLM has an execution snapshot");

    drop(first_session);
    drop(first_core);

    let reopened_core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let reopened_session = reopened_core.session(session_id).open().await?;
    let execution_state = reopened_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("reopened RLM has an execution snapshot");

    let follow_on = reopened_session.root(root_id.clone()).output().await?;
    assert_eq!(
        follow_on.final_value(),
        Some(&serde_json::json!(
            "completed after real SQLite cold reopen"
        ))
    );
    let settled_again = reopened_session.root(root_id).outcome().await?;
    assert_eq!(settled_again.status, crate::TurnStatus::Answered);
    let follow_on_requests = follow_on_requests.lock_recover();
    assert_eq!(follow_on_requests.len(), 1);
    let follow_on_json = serde_json::to_string(&follow_on_requests[0])?;
    assert_eq!(
        follow_on_json.matches("finish after cold reopen").count(),
        1,
        "the committed continuation task must enter the reopened request exactly once: {follow_on_json}"
    );
    assert!(
        follow_on_json.contains("seed:survives"),
        "the explicit frame seed must enter the reopened request: {follow_on_json}"
    );
    assert!(
        !follow_on_json.contains("switch away from the abandoned frame"),
        "the previous frame's input must not enter the reopened request: {follow_on_json}"
    );

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
        let small = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Box::pin(frame_switch_state_after_cold_reopen(
                &SessionId::from("frame-clear-small"),
                16,
            )),
        )
        .await
        .expect("small frame switch settles")?;
        let large = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Box::pin(frame_switch_state_after_cold_reopen(
                &SessionId::from("frame-clear-large"),
                128 * 1024,
            )),
        )
        .await
        .expect("large frame switch settles")?;

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

/// The engine's session drive runs a chained frame handoff through nested
/// commits (D5: the drive is the only executor).
#[cfg(feature = "rlm")]
#[test]
pub(super) fn engine_driven_chained_continue_as_survives_nested_commit_handoff() -> Result<()> {
    run_async_test_on_stack_budget("engine-chained-continue-as-test", || {
        engine_driven_chained_continue_as_survives_nested_commit_handoff_inner()
    })
}

#[cfg(feature = "rlm")]
pub(super) async fn engine_driven_chained_continue_as_survives_nested_commit_handoff_inner()
-> Result<()> {
    let session_id = "engine-chained-continue-as";
    let append_count = Arc::new(AtomicUsize::new(0));
    let double = restate_double(0x0036_68c3).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .provider(queued_text_provider(vec![
            typescript_block(r#"await control.continue_as({ task: "switch again" });"#),
            typescript_block(r#"await control.continue_as({ task: "finish chain" });"#),
            typescript_block(r#"finish("done after chained handoffs");"#),
        ]))
        .model(mock_model_spec())
        .plugin(Arc::new(TurnPersistedGraphAppendFactory {
            append_count: Arc::clone(&append_count),
            max_appends: 2,
        }))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session(session_id).open().await?;

    let output = session
        .send(TurnInput::text("start chained frame handoff"))
        .id("engine-chained-continue-as")
        .output()
        .await?;

    assert_eq!(append_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done after chained handoffs"))
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
    let session_id = "agent-frame-durable";
    let root_turn_id = "agent-frame-root-turn";
    let double = restate_double(0x0a9e_f5a1).await;
    let core = LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded)
        .provider(agent_frame_switch_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AgentFrameSwitchTools))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session(session_id).open().await?;
    let activities = RecordingEvents::default();
    let output = session
        .send(TurnInput::text("switch frames"))
        .id(root_turn_id)
        .output_into(&activities)
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
    // Each frame turn journals its model calls under its own turn: the
    // follow turn's id extends the root's, so a root key is one that names
    // the root and not the follow turn.
    let llm_calls = journaled_llm_call_keys(&double);
    let follow_calls = llm_calls
        .iter()
        .filter(|key| key.contains(&format!("{session_id}:{follow_turn_id}:")))
        .count();
    let root_calls = llm_calls
        .iter()
        .filter(|key| {
            key.contains(&format!("{session_id}:{root_turn_id}:"))
                && !key.contains(follow_turn_id.as_str())
        })
        .count();
    assert!(
        root_calls > 0 && follow_calls > 0 && root_calls + follow_calls == llm_calls.len(),
        "every model call is journaled under the root or the follow turn: {llm_calls:?}"
    );

    let conn = rusqlite::Connection::open(
        double
            .stores()
            .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
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
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
    .provider(queued_text_provider(vec![typescript_block(
        r#"
const lookup = async () => {
    const value = await tools.app_lookup({});
    return value;
  };
const h = await processes.start({ definition: lookup });
const value = await h;
finish(value);"#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
    // A started (`start lookup(...)`) process runs in the lease-protected
    // worker's rebuilt runtime, which needs a session store factory; the
    // explicit in-memory factory backs ephemeral process execution.
    // ADR 0095: the `processes` module is catalogue presence, so a cell that
    // authors `processes.start` needs this factory installed.
    .plugin(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("rlm-process-control-tool").open().await?;
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("start tool"))
            .output()
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");

    let processes = session.admin().processes().list().await?;
    // #1529 retired the `defineProcess` name: a lifted process literal's label
    // is the lift digest (`__process_<hash>`), so the running run is pinned by
    // kind and liveness, not by a source-level name the surface no longer has.
    let running_app_lookup = processes
        .iter()
        .filter(|process| process.kind() == "lashlang" && !process.terminal())
        .count();
    assert_eq!(
        running_app_lookup, 1,
        "expected exactly one running lashlang process, got {processes:?}"
    );

    release_tx.send(()).expect("release tool provider");
    let result = turn.await.expect("turn task")?;
    // `await handle` on the retired surface yielded the `{ ok, value }`
    // settlement envelope; TypeScript's await yields the process's own return
    // and throws on failure (ADR 0096). The process value reaching the turn's
    // final value is what this asserts, and that is unchanged.
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!({ "answer": "ready" }))
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
    let backend = double_backend().await;
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
        rlm_factory(&backend.clone()).with_lashlang_execution_sink(
            Arc::clone(&graph_store) as Arc<dyn crate::tracing::TraceSink>
        ),
    ))
    .provider(queued_text_provider(vec![typescript_block(
        r#"
const lookup = async () => {
    const value = await tools.app_lookup({});
    return value;
  };
const h = await processes.start({ definition: lookup });
const value = await h;
finish(value);"#,
    )]))
    .model(mock_model_spec())
    .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
    // ADR 0095: the `processes` module is catalogue presence, so a cell that
    // authors `processes.start` needs this factory installed.
    .plugin(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("rlm-lashlang-graph-store").open().await?;
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("start tool"))
            .output()
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");

    let processes = session.admin().processes().list().await?;
    // The lifted literal's label is the lift digest (#1529), so the run is
    // found by kind and liveness and the graph's entry name is compared against
    // the label the registry actually recorded.
    let running = processes
        .iter()
        .find(|process| process.kind() == "lashlang" && !process.terminal())
        .expect("running lashlang process");
    let attempt = running
        .first_started
        .as_ref()
        .expect("running process has a started fact")
        .attempt;
    let graph_key = format!("process:{}:attempt:{attempt}", running.process_id);
    let graph = graph_store
        .graph(&graph_key)
        .expect("Lashlang graph snapshot");
    assert_eq!(graph.graph_key, graph_key);
    assert_eq!(graph.entry_kind, "process");
    assert_eq!(graph.entry_name, running.label());
    assert_eq!(
        graph.status,
        lash_lashlang_runtime::TraceLanguageExecutionStatus::Running
    );
    assert!(!graph.nodes.is_empty());
    assert!(
        graph_store
            .graphs()
            .iter()
            .any(|graph| graph.entry_name == running.label())
    );

    let mut subscription = core
        .processes()
        .subscribe_observation(&running.process_id, None)
        .await?;
    let Some(crate::process::ProcessObservationItem::Snapshot { snapshot, .. }) =
        subscription.recv().await?
    else {
        panic!("facade process subscription must start with a graph snapshot");
    };
    let graph = snapshot
        .live
        .graph
        .expect("a routed running process snapshot carries its live graph");
    assert_eq!(graph.graph_key, graph_key);

    release_tx.send(()).expect("release tool provider");
    let _ = turn.await.expect("turn task")?;
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
pub(super) async fn natural_rlm_completion_emits_no_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
        .provider(queued_text_provider(vec!["done in prose"]))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("rlm-prose-completion").open().await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .send(TurnInput::text("answer directly"))
        .allow_prose_or_finish()?
        .output_into(events.as_ref())
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
    let read_view = result
        .state
        .read_view()
        .expect("test runtime frame scope resolves");
    let assistant_messages = read_view
        .messages()
        .iter()
        .filter(|message| message.role == lash_core::MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].parts[0].content(), "done in prose");
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
pub(super) async fn finish_required_rlm_completion_emits_terminal_output() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
        .provider(queued_text_provider(vec![typescript_block(
            r#"finish("done via finish");"#,
        )]))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core
        .session("rlm-finish-required-completion")
        .open()
        .await?;
    let events = Arc::new(RecordingEvents::default());

    let result = session
        .send(TurnInput::text("finish"))
        .require_finish()?
        .output_into(events.as_ref())
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
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
        .provider(queued_text_provider(vec![
            typescript_block("this is not valid typescript"),
            typescript_block(r#"finish("recovered");"#),
        ]))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("rlm-failed-code-event").open().await?;
    let events = RecordingEvents::default();

    let _result = session
        .send(TurnInput::text("bad code"))
        .output_into(&events)
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
pub(super) async fn an_after_step_cancel_stops_at_the_step_boundary() -> Result<()> {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(gated_app_lookup_provider(
        Arc::clone(&started),
        Arc::clone(&release),
        Arc::clone(&released),
        Arc::clone(&provider_calls),
    ))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("stop-after-step").open().await?;

    let handle = session
        .send(TurnInput::text("use the tool, then stop"))
        .await?;
    // This core runs no session work: a waiter drives the input in its own
    // task, so the events follower is what starts the turn.
    let mut events = handle.events();
    started.notified().await;
    assert!(matches!(
        handle
            .cancel()
            .origin("shutdown")
            .mode(crate::TurnCancelMode::AfterStep)
            .await?,
        crate::CancelReceipt::Requested { .. }
    ));
    released.store(true, Ordering::SeqCst);
    release.notify_one();

    // Drain the follower to its end: it settles with the live turn report
    // and leaves that answer on the handle, which output() then returns.
    while let Some(_activity) = events.next_activity().await {}
    let result = handle.output().await?.result;
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
    Ok(())
}

#[tokio::test]
pub(super) async fn host_escalates_an_after_step_cancel_to_an_immediate_abort() -> Result<()> {
    let started = Arc::new(tokio::sync::Notify::new());
    let provider_calls = Arc::new(AtomicUsize::new(0));
    // The response never arrives, so an after-step stop can never land by
    // itself; the host escalates after its own deadline.
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(gated_app_lookup_provider(
        Arc::clone(&started),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::clone(&provider_calls),
    ))
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("escalate-after-step").open().await?;

    let handle = session.send(TurnInput::text("hang, then escalate")).await?;
    // This core runs no session work: a waiter drives the input in its own
    // task, so the events follower is what starts the turn.
    let _events = handle.events();
    started.notified().await;
    let requested = |receipt: crate::CancelReceipt| match receipt {
        crate::CancelReceipt::Requested { receipt, .. } => receipt.outcome,
        other => panic!("the running root must receive the request, got {other:?}"),
    };
    assert!(matches!(
        requested(
            handle
                .cancel()
                .mode(crate::TurnCancelMode::AfterStep)
                .await?
        ),
        lash_core::facade_support::TurnCancelOutcome::Requested(_)
    ));
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(
        matches!(
            requested(
                handle
                    .cancel()
                    .mode(crate::TurnCancelMode::AfterStep)
                    .await?
            ),
            lash_core::facade_support::TurnCancelOutcome::AlreadyRequested(_)
        ),
        "the after-step stop leaves the turn running until its step closes"
    );
    assert!(
        matches!(
            requested(handle.cancel().origin("operator").await?),
            lash_core::facade_support::TurnCancelOutcome::Escalated(_)
        ),
        "escalation aborts the still-running turn"
    );

    let result = handle.output().await?.result;
    let evidence = match &result.outcome {
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { evidence }) => {
            evidence.clone()
        }
        other => panic!("expected an aborted turn, got {other:?}"),
    };
    assert_eq!(evidence.mode, crate::TurnCancelMode::Immediate);
    assert_eq!(evidence.honoured_after_step, None);
    assert!(result.tool_calls.is_empty(), "the response never arrived");
    Ok(())
}

/// The first started run blocks in `app_lookup` until the turn releases it, so
/// the `processes.list` calls in the cell observe live runs.
#[cfg(feature = "rlm")]
async fn definition_filtered_process_list(cell: &str) -> Result<serde_json::Value> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
        .provider(queued_text_provider(vec![format!(
            "<typescript>\n{}\n</typescript>",
            cell.trim()
        )]))
        .model(mock_model_spec())
        .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core.session("rlm-process-definition-filter").open().await?;
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("start tool"))
            .output()
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("tool process should start")
        .expect("tool provider entered");
    release_tx.send(()).expect("release tool provider");

    let result = turn.await.expect("turn task")?;
    Ok(result
        .final_value()
        .cloned()
        .expect("definition-filter cell finishes with a value"))
}

/// A cell filtering by a definition it started sees exactly that run.
///
/// This is the FIG-2989 regression. The cell-side `lookup` value and the
/// `ProcessIdentity.definition` the started run stores are one encoding now, so
/// the equality in `ProcessListFilter::matches_record` holds. Before the codec
/// cutover the stored definition was an unmarked record and `matched` was 0.
/// Two runs are live, so a filter that matched everything would fail too.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn process_list_matches_the_definition_the_cell_started() -> Result<()> {
    run_async_test_on_stack_budget("process-list-definition-match-test", || async {
        let value = definition_filtered_process_list(
            r#"
const lookup = async () => { return await tools.app_lookup({}); };
const probe = async () => { return await tools.app_lookup({}); };
const first = await processes.start({ definition: lookup });
const second = await processes.start({ definition: probe });
const matched = await processes.list({ definition: lookup, status: "any" });
const every = await processes.list({ status: "any" });
await first;
await second;
finish({
  matched: matched.length,
  every: every.length,
  matched_ids: matched.map((row) => row.process_id),
  every_ids: every.map((row) => row.process_id)
});"#,
        )
        .await?;

        assert_eq!(
            value.get("every").and_then(serde_json::Value::as_u64),
            Some(2),
            "precondition: both started runs are visible unfiltered: {value}"
        );
        assert_eq!(
            value.get("matched").and_then(serde_json::Value::as_u64),
            Some(1),
            "the definition filter must select exactly the `lookup` run: {value}"
        );
        let matched_ids = value
            .get("matched_ids")
            .and_then(serde_json::Value::as_array)
            .expect("matched ids");
        let every_ids = value
            .get("every_ids")
            .and_then(serde_json::Value::as_array)
            .expect("every id");
        assert!(
            every_ids.contains(&matched_ids[0]),
            "the matched row must be one of the started runs: {value}"
        );
        Ok(())
    })
}

/// A definition the cell never started must match nothing.
#[cfg(feature = "rlm")]
#[test]
pub(super) fn process_list_rejects_a_definition_that_was_not_started() -> Result<()> {
    run_async_test_on_stack_budget("process-list-definition-mismatch-test", || async {
        let value = definition_filtered_process_list(
            r#"
const lookup = async () => { return await tools.app_lookup({}); };
const idle = async () => { return await tools.app_lookup({}); };
const handle = await processes.start({ definition: lookup });
const matched = await processes.list({ definition: lookup, status: "any" });
const other = await processes.list({ definition: idle, status: "any" });
await handle;
finish({ matched: matched.length, other: other.length });"#,
        )
        .await?;

        assert_eq!(
            value.get("matched").and_then(serde_json::Value::as_u64),
            Some(1),
            "precondition: the started definition still matches: {value}"
        );
        assert_eq!(
            value.get("other").and_then(serde_json::Value::as_u64),
            Some(0),
            "a definition the cell never started must match nothing: {value}"
        );
        Ok(())
    })
}
