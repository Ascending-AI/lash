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
            .serve_test_llm_profile(
                queued_text_provider(vec![typescript_block(&source)]),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())?;
        serve_processes(&core);
        let session = core
            .session(
                crate::SessionId::parse("rlm-leaf-append-stale-rollback")
                    .expect("nonblank host identity"),
            )
            .created()
            .await
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
        // A host's append is a session command the shift applies at a turn
        // boundary (FIG-4202); a stale ancestor settles it `StaleBranch`.
        let result = session
            .admin()
            .state()
            .append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: "leaf-bearing-stale-append".to_string(),
                nodes: vec![lash_core::SessionAppendNode::message(
                    lash_core::PluginMessage::text(
                        lash_core::MessageRole::User,
                        ROLLED_BACK_MARKER,
                    )
                    .with_id("leaf-bearing-stale-append-message"),
                )],
                requires_ancestor_node_id: Some(lash_core::NodeId::from("inactive-ancestor")),
            })
            .await?;
        assert!(matches!(
            result,
            lash_core::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
                if required_node_id == "inactive-ancestor"
        ));
        assert!(
            session.read_view().messages().iter().all(|message| message
                .parts
                .iter()
                .all(|part| part.content() != ROLLED_BACK_MARKER)),
            "the stale append must be absent from the reconciled RLM history projection"
        );

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
        component: lash_core::plugin::ExecutionLeafName,
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
        let mut reloaded = lashlang::VmInstance::pristine();
        reloaded
            .restore_durable_parts(
                &self.state_header,
                fragments,
                lash_core::FleetFormat::current(),
            )
            .ok()?;
        reloaded.state().globals().get(name).cloned()
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
                lash_core::testing::checkpoint_observer::ObservedDeploymentStore::new(
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
    let first_core =
        explicit_ephemeral_facets(LashCore::rlm_builder(backend.clone().into(), first_factory))
            .serve_test_llm_profile(first_provider, mock_llm_profile_spec())
            .tools(Arc::new(FrameStateDeferredTools))
            .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core
        .session((session_id).clone())
        .created()
        .await
        .open()
        .await?;

    let run_id = format!("{session_id}:switch-run");
    let switched = first_session
        .send(TurnInput::text("switch away from the abandoned frame"))
        .id(TurnId::fixture(run_id.clone()))
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), follow_on_started_rx)
        .await
        .expect("the shift reaches the follow-on provider call")
        .expect("follow-on provider signal");
    drop(switched);
    let switch_turn_index = 1;
    // The switch commit is the durable head while the follow-on call is held.
    let durable = lash_core::SessionHistoryStore::load_session_window(
        sqlite_store_factory.as_ref(),
        &SessionId::fixture(session_id.to_string()),
        lash_core::store::WindowSelector::Current,
    )
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
    drop(durable);

    // The attempt dies while the follow-on model call is held; the engine
    // redrives the run, which replays its journal up to the follow-on call
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
        .expect("the switch run's turn invocation runs");
    assert!(
        double.server().crash(&turn_invocation.id),
        "the held attempt dies"
    );
    let redriven = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        first_session
            .run(crate::RunId::parse(run_id.clone())?)
            .outcome(),
    )
    .await
    .expect("the redriven run settles")?;
    assert_eq!(redriven.status(), crate::TurnStatus::Answered);

    let resident_execution_state = first_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("resident switched RLM has an execution snapshot");

    drop(first_session);
    drop(first_core);

    let reopened_core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let reopened_session = reopened_core
        .session((session_id).clone())
        .created()
        .await
        .open()
        .await?;
    let execution_state = reopened_session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("reopened RLM has an execution snapshot");

    let follow_on = reopened_session
        .run(crate::RunId::parse(run_id.clone())?)
        .output()
        .await?;
    assert_eq!(
        follow_on.final_value(),
        Some(&serde_json::json!(
            "completed after real SQLite cold reopen"
        ))
    );
    let settled_again = reopened_session
        .run(crate::RunId::parse(run_id)?)
        .outcome()
        .await?;
    assert_eq!(settled_again.status(), crate::TurnStatus::Answered);
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

/// The engine's session shift runs a chained frame handoff through nested
/// commits (D5: the shift is the only executor).
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
    let observation_count = Arc::new(AtomicUsize::new(0));
    let double = restate_double(0x0036_68c3).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .serve_test_llm_profile(
            queued_text_provider(vec![
                typescript_block(r#"await control.continue_as({ task: "switch again" });"#),
                typescript_block(r#"await control.continue_as({ task: "finish chain" });"#),
                typescript_block(r#"finish("done after chained handoffs");"#),
            ]),
            mock_llm_profile_spec(),
        )
        .plugin(Arc::new(TurnPersistedObserverFactory {
            observation_count: Arc::clone(&observation_count),
            max_failures: 2,
        }))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("start chained frame handoff"))
        .id(crate::TurnId::parse("engine-chained-continue-as").expect("nonblank host identity"))
        .output()
        .await?;

    assert_eq!(observation_count.load(Ordering::SeqCst), 2);
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
    let run_turn_id = "agent-frame-root-turn";
    let double = restate_double(0x0a9e_f5a1).await;
    let core = LashCore::standard_builder(double.lash_backend())
        .serve_test_llm_profile(agent_frame_switch_provider(), mock_llm_profile_spec())
        .tools(Arc::new(AgentFrameSwitchTools))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let activities = RecordingEvents::default();
    let output = session
        .send(TurnInput::text("switch frames"))
        .id(crate::TurnId::parse(run_turn_id).expect("nonblank host identity"))
        .output_into(&activities)
        .await?;

    assert_eq!(output.assistant_message(), Some("done after frame switch"));
    let follow_turn_id = TurnId::fixture(format!("{run_turn_id}:agent-frame:1"));
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
        vec![run_turn_id, follow_turn_id.as_str()],
        "each physical frame turn must announce its own identity exactly once"
    );
    // Each frame turn journals its model calls under its own turn: the
    // follow turn's id extends the run's, so a root key is one that names
    // the run and not the follow turn.
    let llm_calls = journaled_llm_call_keys(&double);
    let follow_calls = llm_calls
        .iter()
        .filter(|key| key.contains(&format!("{session_id}:{follow_turn_id}:")))
        .count();
    let run_calls = llm_calls
        .iter()
        .filter(|key| {
            key.contains(&format!("{session_id}:{run_turn_id}:"))
                && !key.contains(follow_turn_id.as_str())
        })
        .count();
    assert!(
        run_calls > 0 && follow_calls > 0 && run_calls + follow_calls == llm_calls.len(),
        "every model call is journaled under the run or the follow turn: {llm_calls:?}"
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
        })
        // The run also publishes its plugin transition under its own scope
        // (FIG-4857); the frame turns' commits are the rest.
        .filter(|operation| !operation.key.starts_with("plugin-transition"))
        .map(|operation| {
            operation
                .scope
                .turn_id()
                .expect("turn-scoped commit")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        turn_commit_ids,
        vec![run_turn_id.to_string(), follow_turn_id.to_string()]
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
    let tracing = lash_core::trace::TraceRuntime::new(backend.clock())
        .with_product_observer(graph_store.clone());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        backend.clone(),
        rlm_factory(&backend),
    ))
    .trace_runtime(tracing)
    .serve_test_llm_profile(queued_text_provider(vec![typescript_block(
        r#"
const lookup = async () => {
    const value = await tools.app_lookup({});
    return value;
  };
const h = await processes.start({ definition: lookup });
const value = await h;
finish(value);"#,
    )]), mock_llm_profile_spec())
    .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
    // ADR 0095: the `processes` module is catalogue presence, so a cell that
    // authors `processes.start` needs this factory installed.
    .plugin(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core
        .session(
            crate::SessionId::parse("rlm-lashlang-graph-store").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let turn_session = session.clone();
    let mut turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("start tool"))
            .output()
            .await
    });

    tokio::select! {
        entered = entered_rx => entered.expect("tool provider entered"),
        result = &mut turn => panic!("turn completed before its process entered the tool: {result:?}"),
    };

    let processes = session.admin().processes().list().await?;
    // The trace names the source export; the run identity names its definition.
    // Resolve that export through the stored immutable process reference.
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
    let definition_id = running
        .identity
        .definition_id
        .as_ref()
        .expect("definition id");
    let bytes = core
        .backend()
        .definition_store()
        .get_process_definition(definition_id)
        .await
        .map_err(lash_core::PluginError::from)?
        .expect("retained descriptor");
    let draft = lash_core::ProcessDefinitionDraft::from_store_bytes(definition_id, &bytes)
        .expect("canonical descriptor");
    let definition =
        crate::rlm::lang::ProcessDefinitionIdentity::from_process_value(draft.value().as_json())
            .expect("stock definition");
    let artifact = crate::persistence::LashlangArtifacts::of_backend(core.backend())
        .get_module_artifact(&definition.module_ref)
        .await
        .map_err(lash_core::PluginError::from)?
        .expect("retained module");
    assert_eq!(
        Some(graph.entry_name.as_str()),
        artifact.process_name_for_ref(&definition.process_ref)
    );
    assert_eq!(
        graph.status,
        lash_lashlang_runtime::TraceLanguageExecutionStatus::Running
    );
    assert!(!graph.nodes.is_empty());
    assert!(
        graph_store
            .graphs()
            .iter()
            .any(|graph| graph.graph_key == graph_key)
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
pub(super) async fn rlm_failed_code_emits_failed_code_completion_without_fake_tools() -> Result<()>
{
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
        .serve_test_llm_profile(
            queued_text_provider(vec![
                typescript_block("this is not valid typescript"),
                typescript_block(r#"finish("recovered");"#),
            ]),
            mock_llm_profile_spec(),
        )
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core
        .session(crate::SessionId::parse("rlm-failed-code-event").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
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
                TurnEvent::CodeBlockCompleted { error: Some(_), .. }
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

/// The first started run blocks in `app_lookup` until the turn releases it, so
/// the `processes.list` calls in the cell observe live runs.
#[cfg(feature = "rlm")]
async fn definition_filtered_process_list(cell: &str) -> Result<serde_json::Value> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double_backend().await))
        .serve_test_llm_profile(
            queued_text_provider(vec![format!(
                "<typescript>\n{}\n</typescript>",
                cell.trim()
            )]),
            mock_llm_profile_spec(),
        )
        .tools(Arc::new(BlockingAppTools::new(entered_tx, release_rx)))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session = core
        .session(
            crate::SessionId::parse("rlm-process-definition-filter")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let turn_session = session.clone();
    let mut turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("start tool"))
            .output()
            .await
    });

    tokio::select! {
        entered = entered_rx => entered.expect("tool provider entered"),
        result = &mut turn => panic!("turn completed before its process entered the tool: {result:?}"),
    };
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
const matched = await processes.list({ definition_id: lookup.id, status: "any" });
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

/// What one deferred resolution was asked for: the owner's session, the
/// run, and the capability refs.
#[cfg(feature = "rlm")]
type ResolvedFor = (
    Option<SessionId>,
    Option<TurnId>,
    std::collections::BTreeMap<crate::SlotId, crate::CapabilityRef>,
);

/// [`FrameStateDeferredResolver`] that also records whom each resolution
/// was for.
#[cfg(feature = "rlm")]
struct RecordingContextResolver {
    resolved_for: Arc<StdMutex<Vec<ResolvedFor>>>,
}

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for RecordingContextResolver {
    async fn resolve(
        &self,
        cx: &lash_lashlang_runtime::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> std::collections::BTreeMap<String, lash_lashlang_runtime::Resolution> {
        self.resolved_for.lock_recover().push((
            cx.owner.require_session("deferred_resolve").ok().cloned(),
            cx.run.map(|run| run.turn_id.clone()),
            cx.capabilities.clone(),
        ));
        lash_lashlang_runtime::DeferredToolResolver::resolve(&FrameStateDeferredResolver, cx, paths)
            .await
    }
}

/// FIG-5093: a deployment-wide deferred resolver grants by the run that
/// links. It is asked in the context of the session, the run, and the
/// capability refs the run's spec named, which the run recorded with its
/// shape.
#[cfg(feature = "rlm")]
#[tokio::test]
pub(super) async fn the_deferred_resolver_resolves_for_the_run_and_its_recorded_capabilities()
-> Result<()> {
    let resolved_for = Arc::new(StdMutex::new(Vec::new()));
    let backend = double_backend().await;
    let factory =
        rlm_factory(&backend).with_deferred_tool_resolver(Arc::new(RecordingContextResolver {
            resolved_for: Arc::clone(&resolved_for),
        }));
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(
            queued_text_provider(vec![typescript_block(
                r#"const probed = await fixture.probe({});
finish({ probed });"#,
            )]),
            mock_llm_profile_spec(),
        )
        .tools(Arc::new(FrameStateDeferredTools))
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    let session_id = SessionId::from("deferred-resolve-context");
    let session = core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let capabilities = std::collections::BTreeMap::from([(
        crate::SlotId::new("toolbox"),
        crate::CapabilityRef {
            contract: crate::ContractRef::new("fixture.toolbox", 1),
            binding: crate::BindingId::new("toolbox-a"),
            args: serde_json::Value::Null,
        },
    )]);
    let run = TurnId::fixture("deferred-resolve-context-run");
    session
        .send(TurnInput::text("probe through a deferred grant"))
        .id(run.clone())
        .run(crate::RunSpec {
            capabilities: capabilities.clone(),
            ..crate::RunSpec::default()
        })
        .output()
        .await?;
    assert_eq!(
        resolved_for.lock_recover().clone(),
        vec![(Some(session_id), Some(run), capabilities)],
        "the resolver was asked once, for the session, the run and its recorded refs"
    );
    Ok(())
}
