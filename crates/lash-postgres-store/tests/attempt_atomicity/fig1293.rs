use super::*;

struct Fig1293EchoTools;
static FIG1293_BLOCKING_CHILD_RUNS: AtomicUsize = AtomicUsize::new(0);

fn fig1293_echo_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:fig1293_echo",
        "fig1293_echo",
        "Return the supplied literal value.",
        serde_json::json!({
            "type": "object",
            "properties": {"value": {}},
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {"echo": {}},
            "required": ["echo"],
            "additionalProperties": false
        }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Fig1293EchoTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![fig1293_echo_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "fig1293_echo").then(|| Arc::new(fig1293_echo_tool().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            if call.args.get("value") == Some(&serde_json::json!("fail")) {
                return lash_core::ToolOutcome::err_fmt("fig1293 injected batch failure");
            }
            if call.args.get("value") == Some(&serde_json::json!("block"))
                && FIG1293_BLOCKING_CHILD_RUNS.fetch_add(1, Ordering::SeqCst) == 0
            {
                std::future::pending::<()>().await;
                unreachable!("FIG-1293 blocking child is dropped by cancellation")
            }
            lash_core::ToolOutcome::ok(serde_json::json!({
                "echo": call.args.get("value").cloned().unwrap_or_default(),
            }))
        })
        .await
        .into()
    }
}

fn fig1293_factories() -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    let echo: Arc<dyn lash_core::ToolProvider> = Arc::new(Fig1293EchoTools);
    vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
        Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
        Arc::new(lash_subagents::SubagentsPluginFactory::new(Arc::new(
            lash_subagents::CapabilityRegistry::new().with(Arc::new(
                lash_subagents::StaticCapability::new(
                    "default",
                    lash_core::facade_support::SessionSpec::inherit(),
                ),
            )),
        ))),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig1293-echo",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(echo),
        )),
    ]
}

fn fig1293_policy() -> lash_core::SessionPolicy {
    let mut policy = lash_core::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from("fig1293-restate-migrated-tools"));
    policy
}

fn fig1293_state(policy: &lash_core::SessionPolicy) -> lash_core::RuntimeSessionState {
    lash_core::RuntimeSessionState {
        session_id: SessionId::from("fig1293-restate-migrated-tools"),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn fig1293_input() -> lash_core::TurnInput {
    let mut input = lash_core::TurnInput::text("finish once");
    input.trace_turn_id = Some(TurnId::from("fig1293-restate-migrated-turn".to_string()));
    input
}

fn fig1293_model() -> (lash_core::facade_support::ProviderHandle, Arc<AtomicUsize>) {
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
                                            {"tool": "fig1293_echo", "parameters": {"value": "alpha"}},
                                            {"tool": "fig1293_echo", "parameters": {"value": "beta"}},
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
                        index => panic!("unexpected FIG-1293 PostgreSQL model call {index}"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    (provider, model_calls)
}

fn fig1293_fault_batch_model() -> lash_core::facade_support::ProviderHandle {
    let model_calls = Arc::new(AtomicUsize::new(0));
    lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_| {
            let model_calls = Arc::clone(&model_calls);
            async move {
                Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::ToolCall {
                            call_id: "fig1293-fault-batch".to_string(),
                            tool_name: "batch".to_string(),
                            input_json: serde_json::json!({
                                "tool_calls": [
                                    {"tool": "fig1293_echo", "parameters": {"value": "alpha"}},
                                    {"tool": "fig1293_echo", "parameters": {"value": "fail"}},
                                    {"tool": "fig1293_echo", "parameters": {"value": "block"}},
                                ]
                            })
                            .to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    },
                    _ => lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "fault batch complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    },
                })
            }
        })
        .build()
        .into_handle()
}

async fn fig1293_seed_control_target(registry: &Arc<dyn lash_core::ProcessRegistry>) {
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "fig1293-control-target",
                lash_core::ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "fig1293"}),
                },
                // The control target is a fixture-owned external process. It
                // must not enter the durable worker worklist, whose racing
                // `first_started` events would make the signal sequence depend
                // on scheduler timing instead of the law's literal journal.
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.stdin".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from("fig1293-restate-migrated-tools")],
        )
        .await
        .expect("register FIG-1293 control target");
}

async fn fig1293_runtime(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    provider: lash_core::facade_support::ProviderHandle,
    store: Arc<dyn lash_core::RuntimePersistence>,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
) -> lash_core::facade_support::LashRuntime {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let factories = fig1293_factories();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host = host.with_effect_host(effect_host);
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core_worker::WorkerProcessWork::SelfNative(watched.clone()),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        ),
    )
    .expect("valid test native substrate config");
    let process_work = lash_core::ProcessWorkWiring::new(
        watched.clone(),
        Arc::new(lash_core::NativeProcessWork::new(&watched, worker)),
    );
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_id("fig1293-restate-migrated-tools")
        .with_policy(policy)
        .with_initial_state(initial_state)
        .with_runtime_host(host)
        .with_plugin_factories(factories)
        .with_store(store)
        .with_process_work(process_work)
        .build(),
    )
    .await
    .expect("build FIG-1293 tier runtime")
}

async fn run_fig1293_turn(
    runtime: &mut lash_core::facade_support::LashRuntime,
    effect_host: &dyn EffectHost,
) -> lash_core::facade_support::AssembledTurn {
    let controller = effect_host
        .scoped(lash_core::AdmittedScope::turn(
            "fig1293-restate-migrated-tools",
            "fig1293-restate-migrated-turn",
        ))
        .expect("scope FIG-1293 tier controller");
    run_fig1293_turn_with_controller(runtime, controller).await
}

async fn run_fig1293_turn_with_controller(
    runtime: &mut lash_core::facade_support::LashRuntime,
    controller: lash_core::ScopedEffectController<'_>,
) -> lash_core::facade_support::AssembledTurn {
    runtime
        .stream_turn(
            fig1293_input(),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                controller,
            ),
        )
        .await
        .expect("run FIG-1293 tier turn")
}

fn fig1293_cancelling_scope(
    effect_host: &dyn EffectHost,
    cancellation: tokio_util::sync::CancellationToken,
    interrupt_after_batch_failure: bool,
    fired: Arc<std::sync::atomic::AtomicBool>,
) -> lash_core::ScopedEffectController<'static> {
    let scope = lash_core::AdmittedScope::turn(
        "fig1293-restate-migrated-tools",
        "fig1293-restate-migrated-turn",
    );
    let inner = effect_host
        .scoped_static(scope.clone())
        .expect("scope cancelling host")
        .expect("cancelling host exposes static scopes");
    lash_core::ScopedEffectController::shared(
        Arc::new(CrossingController {
            inner: Arc::new(ScopedControllerAdapter(inner)),
            signal_frames: Arc::new(Mutex::new(Vec::new())),
            crash_after: None,
            cancel_after_batch_failure: Some(cancellation),
            interrupt_after_batch_failure,
            force_serial: true,
            fired,
        }),
        scope,
    )
    .expect("scope FIG-1293 cancelling PostgreSQL controller")
}

fn fig1293_literal_outputs(
    turn: &lash_core::facade_support::AssembledTurn,
) -> Vec<(String, serde_json::Value)> {
    turn.tool_calls
        .iter()
        .map(|record| (record.tool.clone(), record.output.value_for_projection()))
        .collect()
}

async fn assert_fig1293_literal_outputs(turn: &lash_core::facade_support::AssembledTurn) {
    let outputs = fig1293_literal_outputs(turn);
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
                serde_json::json!("child literal"),
            ),
            (
                "batch".to_string(),
                serde_json::json!({
                    "results": [
                        {"duration_ms": 0, "index": 0, "result": {"echo": "alpha"}, "success": true, "tool": "fig1293_echo"},
                        {"duration_ms": 0, "index": 1, "result": {"echo": "beta"}, "success": true, "tool": "fig1293_echo"},
                    ]
                }),
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fig1293_public_migrated_tools_are_literal_on_inline_and_postgres_redrive() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping FIG-1293 PostgreSQL tier law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect FIG-1293 PostgreSQL host");
    for statement in [
        "DELETE FROM lash_await_event_waits WHERE session_id LIKE '%fig1293%'",
        "DELETE FROM lash_runtime_effect_replay WHERE envelope_json LIKE '%fig1293%' OR session_id LIKE '%fig1293%'",
        "DELETE FROM lash_processes WHERE process_id LIKE '%fig1293%' OR record_json LIKE '%fig1293%'",
    ] {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset FIG-1293 PostgreSQL rows");
    }

    let inline_registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    fig1293_seed_control_target(&inline_registry).await;
    let (inline_model, inline_model_calls) = fig1293_model();
    let native_effect_host: Arc<dyn EffectHost> =
        Arc::new(lash_core::facade_support::NativeEffectHost::default());
    let inline_policy = fig1293_policy();
    let mut native = fig1293_runtime(
        Arc::clone(&native_effect_host),
        Arc::clone(&inline_registry),
        inline_model,
        Arc::new(lash_core::facade_support::InMemorySessionStore::new()),
        inline_policy.clone(),
        fig1293_state(&inline_policy),
    )
    .await;
    let inline_turn = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_fig1293_turn(&mut native, native_effect_host.as_ref()),
    )
    .await
    .expect("native FIG-1293 substrate turn timed out");
    assert_fig1293_literal_outputs(&inline_turn).await;
    assert_eq!(inline_model_calls.load(Ordering::SeqCst), 3);

    let postgres_registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(storage.process_registry());
    fig1293_seed_control_target(&postgres_registry).await;
    let (postgres_model, postgres_model_calls) = fig1293_model();
    let first_effect_host: Arc<dyn EffectHost> = Arc::new(storage.effect_host());
    let postgres_policy = fig1293_policy();
    let postgres_state = fig1293_state(&postgres_policy);
    let postgres_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::facade_support::InMemorySessionStore::new());
    let mut first = fig1293_runtime(
        Arc::clone(&first_effect_host),
        Arc::clone(&postgres_registry),
        postgres_model.clone(),
        Arc::clone(&postgres_store),
        postgres_policy.clone(),
        postgres_state.clone(),
    )
    .await;
    first.set_turn_phase_probe(Arc::new(PanicBeforeTurnCommit));
    let crashed =
        tokio::spawn(async move { run_fig1293_turn(&mut first, first_effect_host.as_ref()).await })
            .await
            .expect_err("FIG-1293 PostgreSQL turn must crash after ToolBatch commit");
    assert!(crashed.is_panic());

    let replay_effect_host: Arc<dyn EffectHost> = Arc::new(storage.effect_host());
    let mut replay = fig1293_runtime(
        Arc::clone(&replay_effect_host),
        Arc::clone(&postgres_registry),
        postgres_model,
        postgres_store,
        postgres_policy,
        postgres_state,
    )
    .await;
    let postgres_turn = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_fig1293_turn(&mut replay, replay_effect_host.as_ref()),
    )
    .await
    .expect("PostgreSQL FIG-1293 redrive timed out");
    assert_fig1293_literal_outputs(&postgres_turn).await;
    assert_eq!(postgres_model_calls.load(Ordering::SeqCst), 3);

    // A batch is a durable effect group now (FIG-3397): each call is a group
    // child with its own journal rows, and no parent-level ToolBatch frame,
    // launch list or nested-batch causal edge exists to pin. What the journal
    // still states is the attempt set and that the redrive refused none of the
    // migrated public intents.
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT envelope_json, outcome_json FROM lash_runtime_effect_replay
         WHERE session_id = $1 ORDER BY replay_key",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_all(storage.pool())
    .await
    .expect("read FIG-1293 PostgreSQL journal rows");
    for (_, outcome_json) in &rows {
        if let Some(outcome_json) = outcome_json {
            assert!(
                !outcome_json.contains(r#""status":"refused""#),
                "every migrated PostgreSQL public intent must execute: {outcome_json}",
            );
        }
    }
    let mut attempt_names = rows
        .into_iter()
        .filter_map(|(json, _)| {
            let canonical: serde_json::Value =
                serde_json::from_str(&json).expect("decode FIG-1293 PostgreSQL canonical envelope");
            let envelope = serde_json::from_str::<RuntimeEffectEnvelope>(
                canonical
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .expect("FIG-1293 canonical envelope json"),
            )
            .expect("decode FIG-1293 PostgreSQL envelope");
            match envelope.command {
                RuntimeEffectCommand::ToolAttempt { call, .. } => Some(call.tool_name),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    attempt_names.sort();
    assert_eq!(
        attempt_names,
        vec![
            "cancel_process".to_string(),
            "fig1293_echo".to_string(),
            "fig1293_echo".to_string(),
        ],
        "cancel_process and the protocol batch's leaves are attempts; spawn_agent and the groups are not",
    );
}

// Call only after aborting and joining the interrupted host: no live executor
// may still renew these rows. Completed child terminals remain untouched.
async fn expire_fig1293_abandoned_effect_rows(storage: &PostgresStorage) {
    sqlx::query(
        "UPDATE lash_runtime_effect_replay
         SET lease_expires_at_ms = 0
         WHERE session_id = $1 AND status = 'in_progress'",
    )
    .bind("fig1293-restate-migrated-tools")
    .execute(storage.pool())
    .await
    .expect("expire only the interrupted host's abandoned replay rows");
}

async fn assert_fig1293_postgres_crash_boundary(crash_after: CrashAfter, force_serial: bool) {
    let Some(database_url) = database_url() else {
        eprintln!("skipping FIG-1293 PostgreSQL crash law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect FIG-1293 PostgreSQL crash host");
    for statement in [
        "DELETE FROM lash_await_event_waits WHERE session_id LIKE '%fig1293%'",
        "DELETE FROM lash_runtime_effect_replay WHERE envelope_json LIKE '%fig1293%' OR session_id LIKE '%fig1293%'",
        "DELETE FROM lash_processes WHERE process_id LIKE '%fig1293%' OR record_json LIKE '%fig1293%'",
    ] {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset FIG-1293 PostgreSQL crash rows");
    }

    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(storage.process_registry());
    fig1293_seed_control_target(&registry).await;
    let (model, model_calls) = fig1293_model();
    let base_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::new(&storage));
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let effect_host: Arc<dyn EffectHost> = Arc::new(CrossingEffectHost {
        inner: base_effect_host,
        crash_after: Some(crash_after),
        force_serial,
        fired: Arc::clone(&fired),
        signal_frames: Arc::new(Mutex::new(Vec::new())),
    });
    let policy = fig1293_policy();
    let state = fig1293_state(&policy);
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::facade_support::InMemorySessionStore::new());
    let mut first = fig1293_runtime(
        Arc::clone(&effect_host),
        Arc::clone(&registry),
        model.clone(),
        Arc::clone(&store),
        policy.clone(),
        state.clone(),
    )
    .await;
    let first_run =
        tokio::spawn(async move { run_fig1293_turn(&mut first, effect_host.as_ref()).await });
    // CI run 33656939416 attempt 1 exhausted the former 10s boundary wait under load.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !fired.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the selected child boundary must commit before host interruption");
    first_run.abort();
    let interrupted = first_run.await.expect_err("aborted host task");
    assert!(interrupted.is_cancelled());
    expire_fig1293_abandoned_effect_rows(&storage).await;

    let replay_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::new(&storage));
    let mut replay = fig1293_runtime(
        Arc::clone(&replay_effect_host),
        Arc::clone(&registry),
        model,
        store,
        policy,
        state,
    )
    .await;
    let redriven = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_fig1293_turn(&mut replay, replay_effect_host.as_ref()),
    )
    .await
    .expect("FIG-1293 child-boundary redrive timed out");
    assert_fig1293_literal_outputs(&redriven).await;
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        3,
        "redrive must replay the recorded provider calls"
    );

    let child_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lash_runtime_effect_replay
         WHERE session_id = $1
           AND (envelope_json LIKE '%process:subagent:fig1293-spawn-agent%'
                OR envelope_json LIKE '%fig1293_echo%')",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_one(storage.pool())
    .await
    .expect("count FIG-1293 durable child rows");
    assert_eq!(
        child_rows, 8,
        "the interrupted spawn boundary and nested batch children retain stable durable identities"
    );
}

/// PostgreSQL redrive law for the exact process-replay boundary between a
/// durable `spawn_agent` child start and its following await.
#[ignore = "parked: rewritten in PR B (serial batch path deleted) (FIG-3397)"]
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_spawn_agent_redrives_after_child_start_before_await_on_postgres() {
    assert_fig1293_postgres_crash_boundary(CrashAfter::SpawnAgentStart, false).await;
}

/// PostgreSQL redrive law for a protocol batch after its first child commits
/// but before the next serial child begins. Serial scheduling is the binding
/// substrate geometry used by ordinal journals and remains valid on the
/// key-addressed PostgreSQL controller.
#[ignore = "parked: rewritten in PR B (serial batch path deleted) (FIG-3397)"]
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_protocol_batch_redrives_between_children_on_postgres() {
    assert_fig1293_postgres_crash_boundary(CrashAfter::FirstProtocolBatchChild, true).await;
}

/// PostgreSQL redrive law for a serial protocol batch interrupted after one
/// committed success and one committed failure request cancellation, before
/// the third child starts. Redrive must recover the two recorded children and
/// record a literal cancelled terminal for the third without entering it.
#[ignore = "parked: rewritten in PR B (serial batch path deleted) (FIG-3397)"]
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_protocol_batch_partial_failure_and_mid_batch_cancel_redrive_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping FIG-1293 PostgreSQL batch-cancel law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect FIG-1293 PostgreSQL batch-cancel host");
    for statement in [
        "DELETE FROM lash_await_event_waits WHERE session_id LIKE '%fig1293%'",
        "DELETE FROM lash_runtime_effect_replay WHERE envelope_json LIKE '%fig1293%' OR session_id LIKE '%fig1293%'",
        "DELETE FROM lash_processes WHERE process_id LIKE '%fig1293%' OR record_json LIKE '%fig1293%'",
    ] {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset FIG-1293 PostgreSQL batch-cancel rows");
    }

    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(storage.process_registry());
    FIG1293_BLOCKING_CHILD_RUNS.store(0, Ordering::SeqCst);
    let model = fig1293_fault_batch_model();
    let first_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::new(&storage));
    let policy = fig1293_policy();
    let state = fig1293_state(&policy);
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::facade_support::InMemorySessionStore::new());
    let mut first = fig1293_runtime(
        Arc::clone(&first_effect_host),
        Arc::clone(&registry),
        model.clone(),
        Arc::clone(&store),
        policy.clone(),
        state.clone(),
    )
    .await;
    let first_cancellation = tokio_util::sync::CancellationToken::new();
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_controller = fig1293_cancelling_scope(
        first_effect_host.as_ref(),
        first_cancellation.clone(),
        true,
        Arc::clone(&interrupted),
    );
    let first_run = tokio::spawn(async move {
        first
            .stream_turn(
                fig1293_input(),
                lash_core::facade_support::TurnOptions::new(first_cancellation, first_controller),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !interrupted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failure must commit and request cancellation before interruption");
    first_run.abort();
    let interrupted_run = first_run.await.expect_err("aborted batch host task");
    assert!(interrupted_run.is_cancelled());

    let before_redrive_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT envelope_json, status FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND envelope_json LIKE '%fig1293_echo%'",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_all(storage.pool())
    .await
    .expect("read interrupted FIG-1293 child rows");
    let mut before_redrive_children = before_redrive_rows
        .into_iter()
        .filter_map(|(envelope_json, status)| {
            let canonical: serde_json::Value = serde_json::from_str(&envelope_json).ok()?;
            let envelope =
                serde_json::from_str::<RuntimeEffectEnvelope>(canonical.get("json")?.as_str()?)
                    .ok()?;
            let RuntimeEffectCommand::ToolAttempt { call, .. } = envelope.command else {
                return None;
            };
            (call.tool_name == "fig1293_echo").then(|| {
                (
                    call.args["value"]
                        .as_str()
                        .unwrap_or("<non-string>")
                        .to_string(),
                    status,
                )
            })
        })
        .collect::<Vec<_>>();
    before_redrive_children.sort();
    assert_eq!(
        before_redrive_children,
        vec![
            ("alpha".to_string(), "completed".to_string()),
            ("fail".to_string(), "completed".to_string()),
        ],
        "the host is interrupted after success and failure commit but before child 3 starts",
    );
    assert_eq!(FIG1293_BLOCKING_CHILD_RUNS.load(Ordering::SeqCst), 0);
    expire_fig1293_abandoned_effect_rows(&storage).await;

    let replay_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect redriving batch-cancel host");
    let replay_host = PostgresEffectHost::new(&replay_storage);
    let replay_effect_host: Arc<dyn EffectHost> = Arc::new(replay_host);
    let mut replay = fig1293_runtime(
        Arc::clone(&replay_effect_host),
        registry,
        model,
        store,
        policy,
        state,
    )
    .await;
    let replay_cancellation = tokio_util::sync::CancellationToken::new();
    let replay_controller = fig1293_cancelling_scope(
        replay_effect_host.as_ref(),
        replay_cancellation.clone(),
        false,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let redriven = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        replay.stream_turn(
            fig1293_input(),
            lash_core::facade_support::TurnOptions::new(replay_cancellation, replay_controller),
        ),
    )
    .await
    .expect("FIG-1293 cancelling batch redrive timed out")
    .expect("FIG-1293 cancelling batch redrive completes as turn data");
    assert_eq!(
        fig1293_literal_outputs(&redriven),
        vec![(
            "batch".to_string(),
            serde_json::json!({
                "results": [
                    {
                        "duration_ms": 0,
                        "index": 0,
                        "result": {"echo": "alpha"},
                        "success": true,
                        "tool": "fig1293_echo",
                    },
                    {
                        "duration_ms": 0,
                        "error": {
                            "class": "execution",
                            "code": "tool_error",
                            "message": "fig1293 injected batch failure",
                            "source": "tool",
                            "retry": {"type": "never"},
                            "raw": "fig1293 injected batch failure",
                        },
                        "index": 1,
                        "success": false,
                        "tool": "fig1293_echo",
                    },
                    {
                        "duration_ms": 0,
                        "error": {
                            "message": "tool call cancelled",
                            "source": "cancellation",
                        },
                        "index": 2,
                        "success": false,
                        "tool": "fig1293_echo",
                    },
                ],
            }),
        )],
        "the enclosing model-facing batch projects the literal three-child terminal oracle",
    );
    assert_eq!(FIG1293_BLOCKING_CHILD_RUNS.load(Ordering::SeqCst), 0);

    let recorded_rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT replay_key, envelope_hash, envelope_json, outcome_json
         FROM lash_runtime_effect_replay WHERE session_id = $1",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_all(storage.pool())
    .await
    .expect("read FIG-1293 batch-cancel rows");
    let (batch_replay_key, stored_hash, batch_outcome, batch_envelope) = recorded_rows
        .iter()
        .find_map(|(replay_key, envelope_hash, envelope_json, outcome_json)| {
            let canonical: serde_json::Value = serde_json::from_str(envelope_json).ok()?;
            let envelope =
                serde_json::from_str::<RuntimeEffectEnvelope>(canonical.get("json")?.as_str()?)
                    .ok()?;
            let is_fault_batch = matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolBatch { batch }
                    if batch.calls.len() == 3
                        && batch.calls.iter().all(|child| child.call.tool_name == "fig1293_echo")
            );
            is_fault_batch.then_some((
                replay_key.clone(),
                envelope_hash.clone(),
                outcome_json.clone(),
                envelope,
            ))
        })
        .expect("recorded FIG-1293 nested fault ToolBatch");
    let recorded_outcome_json = batch_outcome.expect("nested fault batch is terminal");
    assert_eq!(batch_envelope.invocation.replay_key(), batch_replay_key);
    assert_eq!(
        batch_envelope
            .stable_hash()
            .expect("nested batch stable hash"),
        stored_hash
    );
    let RuntimeEffectCommand::ToolBatch { batch } = &batch_envelope.command else {
        unreachable!("selected nested ToolBatch")
    };
    assert_eq!(
        batch
            .calls
            .iter()
            .map(|child| (
                child.call.call_id.as_str(),
                child.call.tool_id.as_str(),
                child.call.args.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "fig1293-fault-batch:00",
                "tool:fig1293_echo",
                serde_json::json!({"value": "alpha"}),
            ),
            (
                "fig1293-fault-batch:01",
                "tool:fig1293_echo",
                serde_json::json!({"value": "fail"}),
            ),
            (
                "fig1293-fault-batch:02",
                "tool:fig1293_echo",
                serde_json::json!({"value": "block"}),
            ),
        ],
        "the nested durable frame pins all three child identities and arguments",
    );

    let recorded_outcome: RuntimeEffectOutcome =
        serde_json::from_str(&recorded_outcome_json).expect("decode recorded nested fault batch");
    let RuntimeEffectOutcome::ToolBatch {
        launches,
        triggers,
        settlement_order,
    } = &recorded_outcome
    else {
        panic!("nested fault frame must record a ToolBatch outcome")
    };
    assert!(triggers.is_empty());
    let mut settled = settlement_order.clone();
    settled.sort_unstable();
    assert_eq!(
        settled,
        (0..launches.len()).collect::<Vec<_>>(),
        "the recorded batch settles every child exactly once"
    );
    let terminal_oracle = launches
        .iter()
        .map(|launch| {
            let lash_core::runtime::ToolCallLaunch::Done { result } = launch else {
                panic!("all three nested children must be terminal")
            };
            let status = match result.output.outcome {
                lash_core::ToolCallOutcome::Success(_) => "success",
                lash_core::ToolCallOutcome::Failure(_) => "failure",
                lash_core::ToolCallOutcome::Cancelled(_) => "cancelled",
            };
            serde_json::json!({
                "call_id": result.call_id,
                "tool": result.tool_name,
                "status": status,
                "value": result.output.value_for_projection(),
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_oracle,
        vec![
            serde_json::json!({
                "call_id": "fig1293-fault-batch:00",
                "tool": "fig1293_echo",
                "status": "success",
                "value": {"echo": "alpha"},
            }),
            serde_json::json!({
                "call_id": "fig1293-fault-batch:01",
                "tool": "fig1293_echo",
                "status": "failure",
                "value": {
                    "class": "execution",
                    "code": "tool_error",
                    "message": "fig1293 injected batch failure",
                    "source": "tool",
                    "retry": {"type": "never"},
                    "raw": "fig1293 injected batch failure",
                },
            }),
            serde_json::json!({
                "call_id": "fig1293-fault-batch:02",
                "tool": "fig1293_echo",
                "status": "cancelled",
                "value": {
                    "message": "tool call cancelled",
                    "source": "cancellation",
                },
            }),
        ],
        "redrive must record the hard-coded success/failure/cancelled oracle",
    );

    let echo_attempt_rows = recorded_rows
        .iter()
        .filter(|(_, _, envelope_json, _)| envelope_json.contains("fig1293_echo"))
        .filter(|(_, _, envelope_json, _)| envelope_json.contains("tool_attempt"))
        .count();
    assert_eq!(
        echo_attempt_rows, 2,
        "child 3 is cancelled by the serial scheduler before a ToolAttempt frame exists",
    );

    let strict_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect strict cancelled-batch replay host");
    let strict_host = strict_storage.effect_host();
    strict_host.start_replay();
    let strict_controller = strict_host
        .scoped(lash_core::AdmittedScope::turn(
            "fig1293-restate-migrated-tools",
            "fig1293-restate-migrated-turn",
        ))
        .expect("scope strict cancelled batch replay");
    let replayed = strict_controller
        .controller()
        .execute_effect(
            batch_envelope,
            RuntimeEffectLocalExecutor::testing(|_| async move {
                Ok(RuntimeEffectOutcome::ToolBatch {
                    launches: Vec::new(),
                    triggers: Vec::new(),
                    settlement_order: Vec::new(),
                })
            }),
        )
        .await
        .expect("strictly replay recorded cancelled FIG-1293 ToolBatch");
    assert_eq!(
        serde_json::to_string(&replayed).expect("encode strictly replayed nested batch"),
        recorded_outcome_json,
    );
}
