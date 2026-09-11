use super::*;

#[tokio::test]
pub(super) async fn fig1293_public_migrated_tools_redrive_with_literal_restate_outcomes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "fig1293-restate-migrated-tools";
    let turn_id = "fig1293-restate-migrated-turn";
    let model_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => lash_core::LlmResponse {
                            parts: vec![
                                lash_core::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-process-cancel".to_string(),
                                    tool_name: "cancel_process".to_string(),
                                    input_json: serde_json::json!({
                                        "process_id": "fig1293-control-target",
                                    })
                                    .to_string(),
                                    replay: None,
                                },
                                lash_core::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-spawn-agent".to_string(),
                                    tool_name: "spawn_agent".to_string(),
                                    input_json: serde_json::json!({
                                        "capability": "default",
                                        "task": "Return the literal child result.",
                                    })
                                    .to_string(),
                                    replay: None,
                                },
                                lash_core::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-batch".to_string(),
                                    tool_name: "batch".to_string(),
                                    input_json: serde_json::json!({
                                        "tool_calls": [
                                            {"tool": lash_core::testing::FIXTURE_ECHO_TOOL, "parameters": {"value": "alpha"}},
                                            {"tool": lash_core::testing::FIXTURE_ECHO_TOOL, "parameters": {"value": "beta"}},
                                        ]
                                    })
                                    .to_string(),
                                    replay: None,
                                },
                            ],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        1 => lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text: "child literal".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        2 => lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text: "migrated tools complete".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        index => panic!("unexpected FIG-1293 model call {index}"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open FIG-1293 session store"),
    );
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
    let policy = replay_test_policy(&SessionId::from(session_id));
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    let process_registry = process_registry();
    fig1293_seed_control_target(&process_registry, &SessionId::from(session_id)).await;
    let plugin_factories = fig1293_migrated_tool_factories();
    let watched = lash_core::facade_support::watch_process_registry(Arc::clone(&process_registry));
    context.install_process_worker(
        DurableProcessWorker::new(lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                plugin_factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        ))
        .expect("valid test native substrate config"),
    );

    let mut first = replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
        plugin_factories.clone(),
        Some(Arc::clone(&process_registry)),
    )
    .await;
    first.set_turn_phase_probe(Arc::new(PanicAtToolIntentParentEnd));
    let first_context = Arc::clone(&context);
    let crashed = tokio::spawn(async move {
        run_restate_replay_turn(
            &mut first,
            first_context,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    })
    .await
    .expect_err("FIG-1293 first turn must crash after its ToolBatch commit");
    assert!(crashed.is_panic());

    let before = context.recorded_runtime_effect_envelopes();
    let attempt_names = before
        .iter()
        .filter_map(|(_, envelope)| match &envelope.command {
            RuntimeEffectCommand::ToolAttempt { call, .. } => Some(call.tool_name.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attempt_names,
        vec![
            "cancel_process".to_string(),
            lash_core::testing::FIXTURE_ECHO_TOOL.to_string(),
            lash_core::testing::FIXTURE_ECHO_TOOL.to_string(),
        ],
        "cancel_process and batch children are attempts; batch and spawn_agent are not"
    );
    let outer_batch = before
        .iter()
        .find(|(_, envelope)| {
            envelope.invocation.caused_by.is_none()
                && matches!(
                    &envelope.command,
                    RuntimeEffectCommand::ToolBatch { batch }
                        if batch.calls.iter().any(|child| child.call.tool_name == "spawn_agent")
                )
        })
        .expect("outer FIG-1293 Restate tool-batch frame");
    let outer_causal_ref = outer_batch
        .1
        .invocation
        .causal_ref()
        .expect("outer FIG-1293 Restate batch causal ref");
    let outer_recorded: RecordedRuntimeEffect = serde_json::from_slice(
        context
            .records
            .lock_recover()
            .get(outer_batch.0.as_str())
            .expect("outer FIG-1293 Restate recorded outcome"),
    )
    .expect("decode outer FIG-1293 Restate outcome");
    let outer_outcome_json = serde_json::to_string(&outer_recorded.outcome)
        .expect("encode outer FIG-1293 Restate outcome");
    assert!(
        !outer_outcome_json.contains(r#""status":"refused""#),
        "every migrated Restate public intent must execute: {outer_outcome_json}",
    );
    assert!(
        outer_outcome_json.contains(r#""kind":"cancel_process""#),
        "the retained process-controls cancel intent must execute: {outer_outcome_json}",
    );
    let direct_orchestration_children = before
        .iter()
        .map(|(_, envelope)| envelope)
        .filter(|envelope| {
            let is_spawn_command = match &envelope.command {
                RuntimeEffectCommand::Process { command } => match command.as_ref() {
                    ProcessCommand::Start { registration, .. } => {
                        registration.id == "process:subagent:fig1293-spawn-agent"
                    }
                    ProcessCommand::Await { process_ref } => {
                        process_ref.process_id == "process:subagent:fig1293-spawn-agent"
                    }
                    _ => false,
                },
                _ => false,
            };
            let is_nested_batch = matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolBatch { batch }
                    if batch.calls.iter().any(|child| child.call.tool_name == lash_core::testing::FIXTURE_ECHO_TOOL)
            );
            (is_spawn_command || is_nested_batch)
                && envelope.invocation.caused_by.as_ref() == Some(&outer_causal_ref)
        })
        .count();
    assert_eq!(
        direct_orchestration_children, 1,
        "the recorded protocol batch must be a direct child; Restate process service-call frames are asserted by the PostgreSQL envelope law and the endpoint E2E",
    );

    assert!(
        context.live_process_workflow_starts.load(Ordering::SeqCst) > 0,
        "the first invocation must cross the production process-workflow start path"
    );
    // Fixture-only replay substitution: captured Restate service-call results
    // stand in for the substrate journal on the second invocation. The live
    // assertion above prevents this flag from masking production-path coverage.
    context.replay_process_workflow_starts_from_journal();
    context.start_replay_allowing_journal_extension();
    let mut replay = replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        runtime_store,
        plugin_factories,
        Some(process_registry),
    )
    .await;
    let turn = run_restate_replay_turn(
        &mut replay,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(model_calls.load(Ordering::SeqCst), 3);
    let outputs = turn
        .tool_calls
        .iter()
        .map(|record| (record.tool.clone(), record.output.value_for_projection()))
        .collect::<Vec<_>>();
    assert_eq!(
        outputs,
        vec![
            (
                "cancel_process".to_string(),
                serde_json::json!({
                    "process_id": "fig1293-control-target",
                    "status": "cancelled",
                }),
            ),
            (
                "spawn_agent".to_string(),
                serde_json::json!("child literal")
            ),
            (
                "batch".to_string(),
                serde_json::json!({
                    "results": [
                        {
                            "duration_ms": 0,
                            "index": 0,
                            "result": {"echo": "alpha"},
                            "success": true,
                            "tool": lash_core::testing::FIXTURE_ECHO_TOOL,
                        },
                        {
                            "duration_ms": 0,
                            "index": 1,
                            "result": {"echo": "beta"},
                            "success": true,
                            "tool": lash_core::testing::FIXTURE_ECHO_TOOL,
                        },
                    ]
                }),
            ),
        ]
    );
}

#[tokio::test]
pub(super) async fn restate_handler_replay_retries_final_lash_commit_idempotently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-final-commit-replay";
    let turn_id = "restate-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    let call_index = provider_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        call_index, 0,
                        "Restate replay should return the recorded LLM effect"
                    );
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "committed once".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
            DurableMemoryAttachmentStore::default(),
        )),
    );
    host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let policy = replay_test_policy(&SessionId::from(session_id));
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());

    let mut first = replay_test_runtime(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
    )
    .await;
    let first_turn = run_restate_replay_turn(
        &mut first,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        first_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    let first_runs = context.runs();
    assert!(!first_runs.is_empty());

    context.start_replay();
    let retry_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(CommitRetryStore::new(Arc::clone(&runtime_store)));
    let mut replay = replay_test_runtime(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        retry_store,
    )
    .await;
    let replay_turn = run_restate_replay_turn(
        &mut replay,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(first_turn.llm_calls.len(), 1);
    assert_eq!(replay_turn.llm_calls, first_turn.llm_calls);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let conn = rusqlite::Connection::open(dir.path().join("session.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .expect("count turn commit stamps");
    assert_eq!(rows, 1);
}

#[tokio::test]
pub(super) async fn restate_public_parent_end_cancel_survives_crash_after_tool_batch_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-parent-end-replay";
    let turn_id = "restate-parent-end-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RestateParentEndIntentProvider {
        calls: Arc::clone(&provider_calls),
    });
    let tool_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "restate-parent-end-tools",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
        ));
    let plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> =
        lash_core::testing::test_standard_protocol_factories()
            .into_iter()
            .chain([tool_plugin])
            .collect();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::ToolCall {
                                call_id: "restate-parent-end-call".to_string(),
                                tool_name: "restate_parent_end_intent".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        1 => lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text: "parent end complete".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        index => panic!("unexpected Restate parent-end model call {index}"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    let host = host.with_process_engine(Arc::new(RestateParentEndLawEngine));
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open parent-end session store"),
    );
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
    let policy = replay_test_policy(&SessionId::from(session_id));
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    context.defer_process_workflows();
    let process_registry = process_registry();
    let watched = lash_core::facade_support::watch_process_registry(Arc::clone(&process_registry));
    context.install_process_worker(
        DurableProcessWorker::new(lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                plugin_factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        ))
        .expect("valid test native substrate config"),
    );

    let mut first = replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
        plugin_factories.clone(),
        Some(Arc::clone(&process_registry)),
    )
    .await;
    first.set_turn_phase_probe(Arc::new(PanicAtToolIntentParentEnd));
    let first_context = Arc::clone(&context);
    let crashed = tokio::spawn(async move {
        run_restate_replay_turn(
            &mut first,
            first_context,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    })
    .await
    .expect_err("the phase probe crashes after the Restate ToolBatch commit");
    assert!(crashed.is_panic());
    let before = context.recorded_runtime_effect_envelopes();
    assert!(
        before.iter().any(|(_, envelope)| {
            matches!(envelope.command, RuntimeEffectCommand::ToolBatch { .. })
        }),
        "recorded effects before parent end: {:?}",
        before
            .iter()
            .map(|(name, envelope)| (name, format!("{:?}", envelope.command)))
            .collect::<Vec<_>>()
    );
    assert!(!before.iter().any(|(_, envelope)| {
        matches!(
            &envelope.command,
            RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
        )
    }));
    let recorded_parent_end = context
        .records
        .lock_recover()
        .values()
        .filter_map(|bytes| serde_json::from_slice::<RecordedRuntimeEffect>(bytes).ok())
        .any(|recorded| {
            matches!(
                recorded.outcome,
                Ok(RuntimeEffectOutcome::ToolBatch { launches, .. })
                    if launches.iter().any(|launch| matches!(
                        launch,
                        lash_core::runtime::ToolCallLaunch::Done { result }
                            if result.intent_outcomes.iter().any(|outcome| matches!(
                                outcome,
                                lash_core::ToolIntentExecutionOutcome::Executed {
                                    parent_end: Some(_),
                                    ..
                                }
                            ))
                    ))
            )
        });
    assert!(
        recorded_parent_end,
        "the Restate ToolBatch outcome durably carries parent-end metadata before the crash; records: {:?}",
        context
            .records
            .lock_recover()
            .values()
            .filter_map(|bytes| serde_json::from_slice::<RecordedRuntimeEffect>(bytes).ok())
            .map(|recorded| recorded.outcome)
            .collect::<Vec<_>>()
    );

    context.start_replay_allowing_journal_extension();
    let mut parent_end_fault_replay = replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
        plugin_factories.clone(),
        Some(Arc::clone(&process_registry)),
    )
    .await;
    let fault_state = Arc::new(RestateParentEndFaultState::default());
    fault_state
        .crash_before_record_remaining
        .store(1, Ordering::SeqCst);
    let fault_context = Arc::clone(&context);
    let task_fault_state = Arc::clone(&fault_state);
    let fault_result = tokio::spawn(async move {
        run_restate_replay_turn_with_parent_end_fault(
            &mut parent_end_fault_replay,
            fault_context,
            task_fault_state,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    })
    .await;
    let crashed = fault_result.expect_err("crash after the first Restate ParentEnd side effect");
    assert!(crashed.is_panic());
    assert_eq!(
        fault_state
            .completed_local_side_effects
            .load(Ordering::SeqCst),
        1,
        "the Restate fault lands after the first side effect and before its outcome record"
    );
    assert_eq!(
        fault_state.outcomes.lock_recover().as_slice(),
        [],
        "the crash prevents the first typed Restate outcome from returning"
    );
    assert_eq!(
        fault_state
            .frames
            .lock_recover()
            .iter()
            .map(|envelope| serde_json::json!({
                "replay_key": envelope.invocation.replay_key(),
                "command": &envelope.command,
            }))
            .collect::<Vec<_>>(),
        vec![serde_json::json!({
            "replay_key": "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244:parent-end:process:parent-end:tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244",
            "command": {
                "type": "process",
                "command": {
                    "op": "parent_end",
                    "identity": {
                        "session_id": "restate-parent-end-replay",
                        "execution_scope_id": "restate-parent-end-turn-1",
                        "tool_call_id": "restate-parent-end-call",
                        "intent_index": 0,
                        "minting_emission_replay_key": "restate-parent-end-replay:restate-parent-end-turn-1:1:0:tool_batch:2:child:0:restate-parent-end-call:attempt:1",
                        "replay_key": "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244"
                    },
                    "process_id": "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244",
                    "policy": "cancel",
                    "reason": "recorded start intent parent ended with cancel policy"
                }
            }
        })]
    );
    let after_interval_crash = context.recorded_runtime_effect_envelopes();
    assert_eq!(
        after_interval_crash
            .iter()
            .filter(|(_, envelope)| matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
            ))
            .count(),
        0,
        "the interrupted first ParentEnd has no Restate outcome record"
    );

    context.start_replay_allowing_journal_extension();
    let mut between_commands_replay = replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
        plugin_factories.clone(),
        Some(Arc::clone(&process_registry)),
    )
    .await;
    let between_commands_state = Arc::new(RestateParentEndFaultState::default());
    between_commands_state
        .crash_after_recorded_parent_end
        .store(1, Ordering::SeqCst);
    let between_commands_context = Arc::clone(&context);
    let task_between_commands_state = Arc::clone(&between_commands_state);
    let crashed = tokio::spawn(async move {
        run_restate_replay_turn_with_parent_end_fault(
            &mut between_commands_replay,
            between_commands_context,
            task_between_commands_state,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    })
    .await
    .expect_err("crash after the first Restate outcome and before the second command");
    assert!(crashed.is_panic());
    assert_eq!(
        between_commands_state.outcomes.lock_recover().as_slice(),
        [lash_core::ToolIntentParentEndOutcome::Cancelled {
            identity: lash_core::ToolIntentIdentity {
                session_id: SessionId::from("restate-parent-end-replay"),
                execution_scope_id: "restate-parent-end-turn-1".to_string(),
                tool_call_id: "restate-parent-end-call".to_string(),
                intent_index: 0,
                replay_key: "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244".to_string(),
                minting_emission_replay_key: Some(
                    "restate-parent-end-replay:restate-parent-end-turn-1:1:0:tool_batch:2:child:0:restate-parent-end-call:attempt:1".to_string(),
                ),
            },
            process_id: ProcessId::from("tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244"),
        }]
    );
    assert_eq!(
        context
            .recorded_runtime_effect_envelopes()
            .iter()
            .filter(|(_, envelope)| matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
            ))
            .count(),
        1,
        "the between-command crash records only the first ParentEnd outcome"
    );

    context.start_replay_allowing_journal_extension();
    let mut replay = replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        Arc::clone(&runtime_store),
        plugin_factories,
        Some(Arc::clone(&process_registry)),
    )
    .await;
    let redriven = run_restate_replay_turn(
        &mut replay,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        redriven.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    let after = context.recorded_runtime_effect_envelopes();
    let parent_end_frames = after
        .iter()
        .filter(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
            )
        })
        .count();
    assert_eq!(
        parent_end_frames, 2,
        "redrive journals both ParentEnd commands"
    );
    let literal_parent_end_frames = after
        .iter()
        .filter(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
            )
        })
        .map(|(_, envelope)| {
            serde_json::json!({
                "replay_key": envelope.invocation.replay_key(),
                "command": &envelope.command,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        literal_parent_end_frames,
        vec![
            serde_json::json!({
                "replay_key": "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244:parent-end:process:parent-end:tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244",
                "command": {
                    "type": "process",
                    "command": {
                        "op": "parent_end",
                        "identity": {
                            "session_id": "restate-parent-end-replay",
                            "execution_scope_id": "restate-parent-end-turn-1",
                            "tool_call_id": "restate-parent-end-call",
                            "intent_index": 0,
                            "minting_emission_replay_key": "restate-parent-end-replay:restate-parent-end-turn-1:1:0:tool_batch:2:child:0:restate-parent-end-call:attempt:1",
                            "replay_key": "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244"
                        },
                        "process_id": "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244",
                        "policy": "cancel",
                        "reason": "recorded start intent parent ended with cancel policy"
                    }
                }
            }),
            serde_json::json!({
                "replay_key": "tool-intent:v2:blake3:7c74c379f68bf3c63191e0a04e564bb08f62f20226c21029bd2b276cd7771cb9:parent-end:process:parent-end:tool-intent:v2:blake3:7c74c379f68bf3c63191e0a04e564bb08f62f20226c21029bd2b276cd7771cb9",
                "command": {
                    "type": "process",
                    "command": {
                        "op": "parent_end",
                        "identity": {
                            "session_id": "restate-parent-end-replay",
                            "execution_scope_id": "restate-parent-end-turn-1",
                            "tool_call_id": "restate-parent-end-call",
                            "intent_index": 1,
                            "minting_emission_replay_key": "restate-parent-end-replay:restate-parent-end-turn-1:1:0:tool_batch:2:child:0:restate-parent-end-call:attempt:1",
                            "replay_key": "tool-intent:v2:blake3:7c74c379f68bf3c63191e0a04e564bb08f62f20226c21029bd2b276cd7771cb9"
                        },
                        "process_id": "tool-intent:v2:blake3:7c74c379f68bf3c63191e0a04e564bb08f62f20226c21029bd2b276cd7771cb9",
                        "policy": "cancel",
                        "reason": "recorded start intent parent ended with cancel policy"
                    }
                }
            }),
        ]
    );
    let recorded = context.records.lock_recover().clone();
    let literal_parent_end_outcomes = after
        .iter()
        .filter(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
            )
        })
        .map(|(name, _)| {
            let recorded: RecordedRuntimeEffect = serde_json::from_slice(
                recorded
                    .get(name)
                    .expect("recorded Restate ParentEnd bytes"),
            )
            .expect("decode recorded Restate ParentEnd");
            let Ok(RuntimeEffectOutcome::Process {
                result: ProcessEffectOutcome::ParentEnd { outcome },
            }) = recorded.outcome
            else {
                panic!("Restate ParentEnd frame stored another outcome")
            };
            *outcome
        })
        .collect::<Vec<_>>();
    assert_eq!(
        literal_parent_end_outcomes,
        vec![
            lash_core::ToolIntentParentEndOutcome::Cancelled {
                identity: lash_core::ToolIntentIdentity {
                    session_id: SessionId::from("restate-parent-end-replay"),
                    execution_scope_id: "restate-parent-end-turn-1".to_string(),
                    tool_call_id: "restate-parent-end-call".to_string(),
                    intent_index: 0,
                    replay_key: "tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244".to_string(),
                    minting_emission_replay_key: Some(
                        "restate-parent-end-replay:restate-parent-end-turn-1:1:0:tool_batch:2:child:0:restate-parent-end-call:attempt:1".to_string(),
                    ),
                },
                process_id: ProcessId::from("tool-intent:v2:blake3:a651abf6867eb51ffbdf30909c5b19e4b11c8ebd6e224fda98e6fe562cb73244"),
            },
            lash_core::ToolIntentParentEndOutcome::Cancelled {
                identity: lash_core::ToolIntentIdentity {
                    session_id: SessionId::from("restate-parent-end-replay"),
                    execution_scope_id: "restate-parent-end-turn-1".to_string(),
                    tool_call_id: "restate-parent-end-call".to_string(),
                    intent_index: 1,
                    replay_key: "tool-intent:v2:blake3:7c74c379f68bf3c63191e0a04e564bb08f62f20226c21029bd2b276cd7771cb9".to_string(),
                    minting_emission_replay_key: Some(
                        "restate-parent-end-replay:restate-parent-end-turn-1:1:0:tool_batch:2:child:0:restate-parent-end-call:attempt:1".to_string(),
                    ),
                },
                process_id: ProcessId::from("tool-intent:v2:blake3:7c74c379f68bf3c63191e0a04e564bb08f62f20226c21029bd2b276cd7771cb9"),
            },
        ]
    );
    let processes = process_registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("list Restate parent-end processes");
    let children = processes
        .iter()
        .filter(|record| {
            matches!(
                record.input.as_ref(),
                ProcessInput::Engine { kind, payload }
                    if kind == "restate-parent-end-law"
                        && payload.get("source")
                            == Some(&serde_json::json!("restate-parent-end-law"))
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2, "find both Restate parent-end children");
    for child in children {
        let cancel_count = process_registry
            .events_after(&child.id, 0)
            .await
            .expect("read Restate parent-end child events")
            .into_iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count();
        assert_eq!(cancel_count, 1, "Cancel applies exactly once after redrive");
    }
}

/// FIG-460: a dropped suspended handler leaves its advisory lease live, but a
/// fresh durable worker re-enters before TTL and still makes progress under the
/// authoritative final-commit CAS fence.
#[tokio::test]
pub(super) async fn restate_replay_lease_acquisition_takes_recorded_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-replay-lease-branch";
    let turn_id = "restate-replay-lease-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let first_provider_started = Arc::new(tokio::sync::Notify::new());
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let first_provider_started = Arc::clone(&first_provider_started);
            move |_| {
                let provider_calls = Arc::clone(&provider_calls);
                let first_provider_started = Arc::clone(&first_provider_started);
                async move {
                    let call_index = provider_calls.fetch_add(1, Ordering::SeqCst);
                    if call_index == 0 {
                        first_provider_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "fresh worker progressed".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
            DurableMemoryAttachmentStore::default(),
        )),
    );
    host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());

    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let underlying_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let lease_claim_count = Arc::new(AtomicUsize::new(0));
    let probed_store = Arc::new(CommitRetryStore {
        inner: Arc::clone(&underlying_store),
        lease_claim_count: Arc::clone(&lease_claim_count),
    });
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = probed_store;
    let policy = replay_test_policy(&SessionId::from(session_id));
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());

    let mut suspended = replay_test_runtime(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
    )
    .await;
    let suspended_context = Arc::clone(&context);
    let suspended_turn = tokio::spawn(async move {
        run_restate_replay_turn(
            &mut suspended,
            suspended_context,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        first_provider_started.notified(),
    )
    .await
    .expect("first durable worker reaches provider suspension");
    assert_eq!(lease_claim_count.load(Ordering::SeqCst), 1);
    assert!(
        !context.runs().is_empty(),
        "the suspended handler reached the real Restate run boundary"
    );
    suspended_turn.abort();
    assert!(
        suspended_turn
            .await
            .expect_err("dropped suspended handler future")
            .is_cancelled()
    );

    let mut fresh_worker = replay_test_runtime(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        runtime_store,
    )
    .await;
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scoped_effect_controller = controller
        .scoped_effect_controller(durable_turn_scope(session_id, turn_id))
        .expect("scoped replay controller");
    let replay_turn = fresh_worker
        .stream_turn(
            replay_test_input(&TurnId::from(turn_id)),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await;

    let replay_turn = replay_turn.unwrap_or_else(|error| {
        panic!(
            "fresh durable worker must treat pre-TTL lease busy as advisory and \
             progress under CAS: {error:?}; total_lease_store_acquisitions={}",
            lease_claim_count.load(Ordering::SeqCst)
        )
    });
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        replay_turn.assistant_output.safe_text,
        "fresh worker progressed"
    );
    assert_eq!(
        lease_claim_count.load(Ordering::SeqCst),
        2,
        "the fresh worker re-enters before TTL and observes the advisory busy lease"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        2,
        "the dropped provider attempt is retried by the fresh worker"
    );

    let conn = rusqlite::Connection::open(dir.path().join("session.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .expect("count liveness turn commit stamps");
    assert_eq!(rows, 1, "the fresh worker commits exactly once");
}

pub(super) struct ReplayScalarPendingTools {
    scalar_invocations: Arc<AtomicUsize>,
    completion_key_tx:
        Mutex<Option<tokio::sync::oneshot::Sender<Result<lash_core::AwaitEventKey, String>>>>,
}

impl ReplayScalarPendingTools {
    fn scalar_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:replay_scalar_counter",
            "replay_scalar_counter",
            "Increment a non-idempotent replay probe counter.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "value": {} },
                "required": ["value"],
                "additionalProperties": false
            }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "replay_scalar_counter"))
    }

    fn pending_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:replay_pending_input",
            "replay_pending_input",
            "Wait for an externally supplied replay-test value.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "answer": {} },
                "required": ["answer"],
                "additionalProperties": true
            }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "replay_pending_input"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ReplayScalarPendingTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            Self::scalar_definition().manifest(),
            Self::pending_definition().manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "replay_scalar_counter" => Some(Arc::new(Self::scalar_definition().contract())),
            "replay_pending_input" => Some(Arc::new(Self::pending_definition().contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        match call.name {
            "replay_scalar_counter" => {
                self.scalar_invocations.fetch_add(1, Ordering::SeqCst);
                lash_core::ToolOutcome::ok(serde_json::json!({ "value": "counted" }))
            }
            "replay_pending_input" => {
                let key = match call.context.completion_key() {
                    Ok(key) => key,
                    Err(err) => return lash_core::ToolOutcome::err_fmt(err),
                };
                if let Some(tx) = self.completion_key_tx.lock_recover().take() {
                    let _ = tx.send(Ok(key));
                }
                lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new())
            }
            other => lash_core::ToolOutcome::err_fmt(format!("unknown replay tool `{other}`")),
        }
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == Self::pending_definition().id()
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        if call.name != "replay_scalar_counter" {
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(err) => {
                    return lash_core::ToolAttemptOutcome::done_without_intents(
                        lash_core::ToolOutcomeDone::failure(lash_core::ToolFailure::runtime(
                            lash_core::ToolFailureClass::Internal,
                            "replay_pending_input_completion_key",
                            err.to_string(),
                        )),
                    );
                }
            };
            if let Some(tx) = self.completion_key_tx.lock_recover().take() {
                let _ = tx.send(Ok(key));
            }
            return lash_core::ToolAttemptOutcome::pending(lash_core::PendingCompletion::new());
        }
        self.scalar_invocations.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "value": "counted" })),
            lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::SignalProcess(
                lash_core::SignalProcessIntent {
                    session_id: SessionId::from(call.context.session_id()),
                    process_id: ProcessId::from("restate-recorded-intent-target"),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"source": "recorded-scalar-attempt"}),
                },
            )]),
        )
    }
}

#[tokio::test]
pub(super) async fn restate_replay_does_not_reexecute_scalar_lashlang_tool_before_pending_wait() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-scalar-lashlang-replay";
    let turn_id = "restate-scalar-lashlang-turn-1";
    let scalar_invocations = Arc::new(AtomicUsize::new(0));
    let (completion_key_tx, completion_key_rx) = tokio::sync::oneshot::channel();
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(ReplayScalarPendingTools {
        scalar_invocations: Arc::clone(&scalar_invocations),
        completion_key_tx: Mutex::new(Some(completion_key_tx)),
    });
    let tool_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "restate-scalar-replay-tools",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
        ));
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let rlm_plugin: Arc<dyn lash_core::facade_support::PluginFactory> = Arc::new(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::clone(&artifact_store),
        )
        .with_process_lifecycle(true),
    );
    let plugin_factories = vec![rlm_plugin, tool_plugin];
    let llm_provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let llm_provider_calls = Arc::clone(&llm_provider_calls);
            move |_request| {
                let llm_provider_calls = Arc::clone(&llm_provider_calls);
                async move {
                    llm_provider_calls.fetch_add(1, Ordering::SeqCst);
                    let source = r#"<lashlang>
process replay_probe(tools: Tools) {
  counted = await tools.replay_scalar_counter({})?
  resumed = await tools.replay_pending_input({})?
  finish { counted: counted.value, answer: resumed.answer }
}
handle = start replay_probe(tools: tools)
finish (await handle)?
</lashlang>"#;
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: source.to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let corpus_clock: Arc<dyn lash_core::Clock> = Arc::new(ToolIntentCorpusClock);
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(Arc::clone(&corpus_clock));
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
            DurableMemoryAttachmentStore::default(),
        )),
    );
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(DurableMemoryProcessEnvStore::default());
    host.durability.process_env_store = Arc::clone(&process_env_store);
    host = host.with_process_engine(Arc::new(lash_lashlang_runtime::LashlangProcessEngine::new(
        Arc::clone(&artifact_store),
        lash_lashlang_runtime::LashlangSurface::default(),
    )));
    let store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("session.db"))
            .await
            .expect("open session store"),
    );
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
    let policy = replay_test_policy(&SessionId::from(session_id));
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    let process_registry = process_registry()
        .with_runtime_clock(corpus_clock)
        .expect("SQLite process registry accepts the fixed corpus clock");
    process_registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "restate-recorded-intent-target",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from(session_id.to_string())],
        )
        .await
        .expect("register the recorded-intent signal target");
    let watched = lash_core::facade_support::watch_process_registry(Arc::clone(&process_registry));
    let process_worker =
        DurableProcessWorker::new(lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                plugin_factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        ))
        .expect("valid test native substrate config");
    context.install_process_worker(process_worker);
    let signal_wait_controller =
        Arc::new(RestateRuntimeEffectController::new(Arc::clone(&context)));
    let signal_wait_key = signal_wait_controller
        .await_event_key(
            &ExecutionScope::process("restate-recorded-intent-target"),
            AwaitEventWaitIdentity::process_signal("restate-recorded-intent-target", "resume", 1),
        )
        .await
        .expect("mint captured-journal process-signal wait");
    let signal_wait = {
        let signal_wait_controller = Arc::clone(&signal_wait_controller);
        let signal_wait_key = signal_wait_key.clone();
        tokio::spawn(async move {
            signal_wait_controller
                .await_await_event(
                    &signal_wait_key,
                    tokio_util::sync::CancellationToken::new(),
                    None,
                )
                .await
        })
    };
    tokio::task::yield_now().await;

    let mut first = Box::pin(replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
        plugin_factories.clone(),
        Some(Arc::clone(&process_registry)),
    ))
    .await;
    let first_context = Arc::clone(&context);
    let mut first_turn = tokio::spawn(async move {
        run_restate_replay_turn(
            &mut first,
            first_context,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    });
    let completion_key = tokio::select! {
        completion_key = completion_key_rx => completion_key
            .expect("pending tool must publish its completion key")
            .expect("pending tool must obtain its completion key"),
        turn = &mut first_turn => panic!(
            "first turn completed before the pending tool published its completion key: {turn:?}"
        ),
    };
    let resolver = RestateRuntimeEffectController::new(Arc::clone(&context));
    assert_eq!(
        resolver
            .resolve_await_event(
                &completion_key,
                Resolution::Ok(serde_json::json!({ "answer": "resumed" })),
            )
            .await
            .expect("resolve pending replay-test tool"),
        ResolveOutcome::Accepted
    );
    let first_turn = first_turn.await.expect("first turn task");
    assert!(matches!(
        first_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), signal_wait)
            .await
            .expect("Restate SignalProcess intent must wake the parked wait")
            .expect("Restate signal wait task")
            .expect("Restate signal wait resolution"),
        Resolution::Ok(serde_json::json!({"source": "recorded-scalar-attempt"}))
    );
    assert_eq!(scalar_invocations.load(Ordering::SeqCst), 1);
    let first_recorded_envelopes = context.recorded_runtime_effect_envelopes();
    let scalar_tool_attempts = first_recorded_envelopes
        .iter()
        .filter(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "replay_scalar_counter"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        scalar_tool_attempts.len(),
        1,
        "the production Lashlang tool caller must emit one journaled ToolAttempt envelope"
    );
    let (scalar_effect_name, scalar_envelope) = scalar_tool_attempts[0];
    assert_eq!(
        scalar_envelope.invocation.effect_kind(),
        Some(RuntimeEffectKind::ToolAttempt)
    );
    let RuntimeEffectCommand::ToolAttempt {
        call,
        attempt,
        max_attempts,
        ..
    } = &scalar_envelope.command
    else {
        unreachable!("filtered to the scalar ToolAttempt");
    };
    assert_eq!(call.tool_name, "replay_scalar_counter");
    assert_eq!((*attempt, *max_attempts), (1, 1));
    assert_eq!(
        scalar_effect_name,
        &restate_effect_name(&scalar_envelope.invocation),
        "the real journaling host must derive its run identity from the caller-emitted envelope"
    );
    let scalar_envelope_hash = scalar_envelope.stable_hash().expect("scalar envelope hash");
    let (signal_effect_name, signal_envelope) = first_recorded_envelopes
        .iter()
        .find(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::Signal { .. })
            )
        })
        .expect("production SignalProcess command envelope");
    let signal_envelope_hash = signal_envelope
        .stable_hash()
        .expect("signal command envelope hash");
    let first_intent_events = process_registry
        .events_after(&ProcessId::from("restate-recorded-intent-target"), 0)
        .await
        .expect("read the first recorded-intent event set")
        .into_iter()
        .filter(|event| event.event_type == "signal.resume")
        .collect::<Vec<_>>();
    assert_eq!(first_intent_events.len(), 1, "one signal command drains");
    assert_eq!(first_intent_events[0].event_type, "signal.resume");
    assert_eq!(
        first_intent_events[0].payload,
        serde_json::json!({"source": "recorded-scalar-attempt"})
    );
    let first_intent_event_bytes =
        serde_json::to_vec(&first_intent_events).expect("serialize first intent events");
    process_registry
        .complete_process(
            &ProcessId::from("restate-recorded-intent-target"),
            process_success(serde_json::json!("live state mutated after drain")),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize the intent target before redrive");
    let recorded_effect_count = first_recorded_envelopes.len();

    context
        .events
        .reset_invocation_state_for_replay_preserving_durable_event(
            &RestateDurableWaitAddress::for_key(&completion_key).workflow_key,
        );
    context.start_replay();
    let retry_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(CommitRetryStore::new(Arc::clone(&runtime_store)));
    let mut replay = Box::pin(replay_test_runtime_with_plugins_and_registry(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        retry_store,
        plugin_factories,
        Some(Arc::clone(&process_registry)),
    ))
    .await;
    let replay_turn = run_restate_replay_turn(
        &mut replay,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(llm_provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        scalar_invocations.load(Ordering::SeqCst),
        1,
        "Restate replay must return the journaled scalar ToolAttempt instead of re-executing the provider"
    );
    let replayed_envelopes = context.recorded_runtime_effect_envelopes();
    assert_eq!(
        replayed_envelopes.len(),
        recorded_effect_count,
        "replay must consume the journal rather than append another ToolAttempt record"
    );
    let replayed_scalar = replayed_envelopes
        .iter()
        .find(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "replay_scalar_counter"
            )
        })
        .expect("replayed scalar ToolAttempt envelope");
    assert_eq!(
        replayed_scalar
            .1
            .stable_hash()
            .expect("replayed scalar envelope hash"),
        scalar_envelope_hash,
        "the caller must reconstruct the same ToolAttempt envelope on replay"
    );
    let replayed_signal = replayed_envelopes
        .iter()
        .find(|(_, envelope)| {
            matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(command.as_ref(), ProcessCommand::Signal { .. })
            )
        })
        .expect("redriven production SignalProcess command envelope");
    assert_eq!(
        replayed_signal
            .1
            .stable_hash()
            .expect("redriven signal command envelope hash"),
        signal_envelope_hash,
        "the redriven process-command frame must be byte-identical"
    );
    let replayed_intent_events = process_registry
        .events_after(&ProcessId::from("restate-recorded-intent-target"), 0)
        .await
        .expect("read redriven recorded-intent events")
        .into_iter()
        .filter(|event| event.event_type == "signal.resume")
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_vec(&replayed_intent_events).expect("serialize redriven intent events"),
        first_intent_event_bytes,
        "live terminal mutation cannot change, suppress, or duplicate the recorded signal outcome"
    );
    assert_eq!(
        context
            .runs()
            .iter()
            .filter(|effect_name| *effect_name == scalar_effect_name)
            .count(),
        2,
        "the production caller must cross the journaling host once live and once on replay"
    );
    assert_eq!(
        context
            .runs()
            .iter()
            .filter(|effect_name| *effect_name == signal_effect_name)
            .count(),
        2,
        "the production SignalProcess command must cross the Restate journal once live and once on redrive"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_schedules_process_workflow_without_running_executor() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let registration = external_registration("task-1");
    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: vec![SessionId::from("session")],
                    env_spec: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("start");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert_eq!(
        record
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some("LashProcessWorkflow/task-1")
    );
    assert_eq!(
        registry
            .get_process(&ProcessId::from("task-1"))
            .await
            .expect("read process")
            .expect("get")
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some("LashProcessWorkflow/task-1")
    );
    assert_eq!(
        registry
            .list_observed_by(
                &SessionId::from("session"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("observed")
            .into_iter()
            .next()
            .and_then(|record| record.external_ref)
            .map(|external| (
                external.backend,
                external
                    .metadata
                    .and_then(|metadata| metadata.get("invocation_id").cloned())
            )),
        Some((
            "restate".to_string(),
            Some(serde_json::json!("invocation-task-1"))
        ))
    );
    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| registration.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-1"]
    );
    assert!(
        context.runs.lock_recover().is_empty(),
        "process workflow scheduling must not call Restate context from inside ctx.run"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_replays_process_start_await_command_sequence() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-start-await-replay";

    let start = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-start-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: external_registration(process_id),
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        )
    };
    let terminal = process_success(serde_json::json!({ "done": true }));

    host.execute_effect(start(), registry_local_executor(registry.clone()))
        .await
        .expect("first start");
    let process_ref = registry
        .resolve_process_ref(&ProcessId::from(process_id))
        .await
        .expect("resolve started process incarnation");
    let await_terminal = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-await-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_ref: process_ref.clone(),
            }),
        )
    };
    registry
        .complete_process(
            &ProcessId::from(process_id),
            terminal.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete child process");
    context.resolve_process_terminal(&ProcessId::from(process_id), &terminal);
    host.execute_effect(await_terminal(), registry_local_executor(registry.clone()))
        .await
        .expect("first await");

    // Simulates Restate replay of the same parent handler after a later
    // suspension resumes. The already persisted registry record has an
    // external_ref at this point, but the handler must still issue the same
    // Restate send before the await call so the journal command sequence stays
    // send -> call -> ... on every replay.
    host.execute_effect(start(), registry_local_executor(registry.clone()))
        .await
        .expect("replay start");
    host.execute_effect(await_terminal(), registry_local_executor(registry.clone()))
        .await
        .expect("replay await");

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[
            format!("send:{process_id}"),
            format!("call:{process_id}"),
            format!("send:{process_id}"),
            format!("call:{process_id}"),
        ],
        "child process start/await must replay the same Restate command sequence"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_start_emits_send_when_external_ref_already_exists() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-start-existing-ref";
    let registration = external_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    registry
        .set_external_ref(
            &ProcessId::from(process_id),
            ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!("LashProcessWorkflow/{process_id}"),
                metadata: Some(serde_json::json!({
                    "invocation_id": format!("invocation-{process_id}")
                })),
            },
        )
        .await
        .expect("pre-set external ref");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-start-existing-ref"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration,
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        ),
        registry_local_executor(registry),
    )
    .await
    .expect("start with existing external ref");

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[format!("send:{process_id}")],
        "pre-existing external_ref must not suppress the journaled Restate send"
    );
}

pub(super) async fn run_parent_shaped_start_await_suspend_flow(
    host: &RestateRuntimeEffectController<'_, Arc<RecordingContext>>,
    registry: Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    suspend_key: AwaitEventKey,
) {
    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "parent-flow-start-child"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: external_registration(process_id),
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        ),
        registry_local_executor(registry.clone()),
    )
    .await
    .expect("parent flow start child");

    let process_ref = registry
        .resolve_process_ref(process_id)
        .await
        .expect("resolve parent-flow child incarnation");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "parent-flow-await-child"),
            RuntimeEffectCommand::process(ProcessCommand::Await { process_ref }),
        ),
        registry_local_executor(registry),
    )
    .await
    .expect("parent flow await child");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::AwaitEvent, "parent-flow-suspend"),
            RuntimeEffectCommand::AwaitEvent { key: suspend_key },
        ),
        RuntimeEffectLocalExecutor::await_event(tokio_util::sync::CancellationToken::new(), None)
            .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
    )
    .await
    .expect("parent flow await resume event");
}
