use super::*;

struct Fig1293EchoTools;
static FIG1293_BLOCKING_CHILD_RUNS: AtomicUsize = AtomicUsize::new(0);

fn fig1293_echo_tool() -> lash_core_execution::ToolDefinition {
    lash_core_execution::ToolDefinition::raw(
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
impl lash_core_execution::ToolProvider for Fig1293EchoTools {
    fn tool_manifests(&self) -> Vec<lash_core_execution::ToolManifest> {
        vec![fig1293_echo_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core_execution::ToolContract>> {
        (name == "fig1293_echo").then(|| Arc::new(fig1293_echo_tool().contract()))
    }

    async fn execute(
        &self,
        call: lash_core_execution::ToolCall<'_>,
    ) -> lash_core_execution::ToolAttemptOutcome {
        (async {
            if call.args.get("value") == Some(&serde_json::json!("fail")) {
                return lash_core_execution::ToolOutcome::err_fmt("fig1293 injected batch failure");
            }
            if call.args.get("value") == Some(&serde_json::json!("block"))
                && FIG1293_BLOCKING_CHILD_RUNS.fetch_add(1, Ordering::SeqCst) == 0
            {
                std::future::pending::<()>().await;
                unreachable!("FIG-1293 blocking child is dropped by cancellation")
            }
            lash_core_execution::ToolOutcome::ok(serde_json::json!({
                "echo": call.args.get("value").cloned().unwrap_or_default(),
            }))
        })
        .await
        .into()
    }
}

fn fig1293_factories() -> Vec<Arc<dyn lash_core_execution::facade_support::PluginFactory>> {
    let echo: Arc<dyn lash_core_execution::ToolProvider> = Arc::new(Fig1293EchoTools);
    vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
        Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
        Arc::new(lash_subagents::SubagentsPluginFactory::new(Arc::new(
            lash_subagents::CapabilityRegistry::new().with(Arc::new(
                lash_subagents::StaticCapability::new(
                    "default",
                    lash_core_execution::facade_support::SessionSpec::inherit(),
                ),
            )),
        ))),
        Arc::new(lash_core_execution::plugin::StaticPluginFactory::new(
            "fig1293-echo",
            lash_core_execution::facade_support::PluginSpec::new().with_tool_provider(echo),
        )),
    ]
}

fn fig1293_policy() -> lash_core_execution::SessionPolicy {
    let mut policy = lash_core_execution::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from("fig1293-restate-migrated-tools"));
    policy
}

fn fig1293_state(
    policy: &lash_core_execution::SessionPolicy,
) -> lash_core_execution::RuntimeSessionState {
    lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from("fig1293-restate-migrated-tools"),
        policy: policy.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    }
}

fn fig1293_input() -> lash_core_execution::TurnInput {
    let mut input = lash_core_execution::TurnInput::text("finish once");
    input.trace_turn_id = Some(TurnId::from("fig1293-restate-migrated-turn".to_string()));
    input
}

fn fig1293_model() -> (
    lash_core_execution::facade_support::ProviderHandle,
    Arc<AtomicUsize>,
) {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core_execution::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => lash_core_execution::LlmResponse {
                            parts: vec![
                                lash_core_execution::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-process-cancel".to_string(),
                                    tool_name: "cancel_process".to_string(),
                                    input_json: serde_json::json!({
                                        "process_id": "fig1293-control-target",
                                    })
                                    .to_string(),
                                    replay: None,
                                },
                                lash_core_execution::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-spawn-agent".to_string(),
                                    tool_name: "spawn_agent".to_string(),
                                    input_json: serde_json::json!({
                                        "capability": "default",
                                        "task": "Return the literal child result.",
                                    })
                                    .to_string(),
                                    replay: None,
                                },
                                lash_core_execution::LlmOutputPart::ToolCall {
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
                            ..lash_core_execution::LlmResponse::default()
                        },
                        1 => lash_core_execution::LlmResponse {
                            parts: vec![lash_core_execution::LlmOutputPart::Text {
                                text: "child literal".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core_execution::LlmResponse::default()
                        },
                        2 => lash_core_execution::LlmResponse {
                            parts: vec![lash_core_execution::LlmOutputPart::Text {
                                text: "migrated tools complete".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core_execution::LlmResponse::default()
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

fn fig1293_fault_batch_model() -> lash_core_execution::facade_support::ProviderHandle {
    let model_calls = Arc::new(AtomicUsize::new(0));
    lash_core_execution::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_| {
            let model_calls = Arc::clone(&model_calls);
            async move {
                Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => lash_core_execution::LlmResponse {
                        parts: vec![lash_core_execution::LlmOutputPart::ToolCall {
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
                        ..lash_core_execution::LlmResponse::default()
                    },
                    _ => lash_core_execution::LlmResponse {
                        parts: vec![lash_core_execution::LlmOutputPart::Text {
                            text: "fault batch complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core_execution::LlmResponse::default()
                    },
                })
            }
        })
        .build()
        .into_handle()
}

async fn fig1293_seed_control_target(registry: &Arc<dyn lash_core_execution::ProcessRegistry>) {
    registry
        .register_process_with_observers(
            lash_core_execution::ProcessRegistration::new(
                "fig1293-control-target",
                lash_core_execution::ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "fig1293"}),
                },
                // The control target is a fixture-owned external process. It
                // must not enter the durable worker worklist, whose racing
                // `first_started` events would make the signal sequence depend
                // on scheduler timing instead of the law's literal journal.
                lash_core_execution::RecoveryContract::ExternallyOwned,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core_execution::ProcessEventType {
                name: "signal.stdin".to_string(),
                payload_schema: lash_core_execution::LashSchema::any(),
                semantics: lash_core_execution::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from("fig1293-restate-migrated-tools")],
        )
        .await
        .expect("register FIG-1293 control target");
}

async fn fig1293_runtime(
    backend: Arc<dyn lash_core_execution::Backend>,
    provider: lash_core_execution::facade_support::ProviderHandle,
    store: Arc<dyn lash_core_execution::RuntimePersistence>,
    policy: lash_core_execution::SessionPolicy,
    initial_state: lash_core_execution::RuntimeSessionState,
) -> lash_core::facade_support::LashRuntime {
    let watched =
        lash_core_execution::facade_support::watch_process_registry(backend.process_registry());
    let factories = fig1293_factories();
    let mut host = lash_core_execution::facade_support::RuntimeHostConfig::new(
        backend,
        lash_core_execution::CommitBudget::bounded(1024 * 1024, 512),
        lash_core_execution::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver =
        Arc::new(lash_core_execution::facade_support::SingleProviderResolver::new(provider));
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(lash_core_execution::facade_support::PluginHost::new(
                factories.clone(),
            )),
            host.clone(),
            lash_core_worker::WorkerProcessWork::SelfNative(watched.clone()),
            Arc::new(lash_core_execution::NoQueuedWork::new()),
            lash_core_execution::testing::runtime_lease_owner(),
        ),
    )
    .expect("valid test native substrate config");
    let process_work = lash_core_execution::ProcessWorkWiring::new(
        watched.clone(),
        Arc::new(lash_core_execution::NativeProcessWork::new(
            &watched, worker,
        )),
    );
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core_execution::testing::runtime_lease_owner(),
        )
        .with_session_id("fig1293-restate-migrated-tools")
        .with_policy(policy)
        .with_initial_state(initial_state)
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
) -> lash_core_execution::facade_support::AssembledTurn {
    let controller = effect_host
        .scoped(lash_core_execution::AdmittedScope::turn(
            "fig1293-restate-migrated-tools",
            "fig1293-restate-migrated-turn",
        ))
        .expect("scope FIG-1293 tier controller");
    run_fig1293_turn_with_controller(runtime, controller).await
}

async fn run_fig1293_turn_with_controller(
    runtime: &mut lash_core::facade_support::LashRuntime,
    controller: lash_core_execution::ScopedEffectController<'_>,
) -> lash_core_execution::facade_support::AssembledTurn {
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

/// Clears every row a FIG-1293 turn leaves, the retained effect groups
/// included: each law's turn opens its tool calls as a group keyed by the same
/// session and turn, so a sibling law's retained group would otherwise be
/// reopened under a different shape.
async fn reset_fig1293_rows(storage: &PostgresStorage) {
    for statement in [
        "DELETE FROM lash_await_event_waits WHERE session_id LIKE '%fig1293%'",
        "DELETE FROM lash_runtime_effect_replay WHERE envelope_json LIKE '%fig1293%' OR session_id LIKE '%fig1293%'",
        "DELETE FROM lash_runtime_effect_group_child WHERE group_key IN (
             SELECT group_key FROM lash_runtime_effect_group WHERE scope_id LIKE '%fig1293%')",
        "DELETE FROM lash_runtime_effect_group WHERE scope_id LIKE '%fig1293%'",
        "DELETE FROM lash_processes WHERE process_id LIKE '%fig1293%' OR record_json LIKE '%fig1293%'",
    ] {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset FIG-1293 PostgreSQL rows");
    }
}

fn fig1293_literal_outputs(
    turn: &lash_core_execution::facade_support::AssembledTurn,
) -> Vec<(String, serde_json::Value)> {
    turn.tool_calls
        .iter()
        .map(|record| (record.tool.clone(), record.output.value_for_projection()))
        .collect()
}

async fn assert_fig1293_literal_outputs(turn: &lash_core_execution::facade_support::AssembledTurn) {
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
    reset_fig1293_rows(&storage).await;

    // The inline tier: the same law over a SQLite memory backend.
    let inline_backend: Arc<dyn lash_core_execution::Backend> = Arc::new(
        lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open a SQLite memory backend"),
    );
    let inline_registry = inline_backend.process_registry();
    fig1293_seed_control_target(&inline_registry).await;
    let (inline_model, inline_model_calls) = fig1293_model();
    let native_effect_host = inline_backend.effect_host();
    let inline_policy = fig1293_policy();
    let mut native = fig1293_runtime(
        Arc::clone(&inline_backend),
        inline_model,
        detached_session_store().await,
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

    let postgres_registry: Arc<dyn lash_core_execution::ProcessRegistry> =
        Arc::new(storage.process_registry());
    fig1293_seed_control_target(&postgres_registry).await;
    let (postgres_model, postgres_model_calls) = fig1293_model();
    let first_effect_host: Arc<dyn EffectHost> = Arc::new(storage.effect_host());
    let postgres_policy = fig1293_policy();
    let postgres_state = fig1293_state(&postgres_policy);
    let postgres_store = detached_session_store().await;
    let mut first = fig1293_runtime(
        pg_law_backend(
            &storage,
            Arc::clone(&first_effect_host),
            Arc::clone(&postgres_registry),
        )
        .await,
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
        pg_law_backend(
            &storage,
            Arc::clone(&replay_effect_host),
            Arc::clone(&postgres_registry),
        )
        .await,
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
/// Waits until the session's in-progress journal rows stop changing: the
/// first host's unparked children have settled and only the parked work —
/// which never progresses — still holds rows.
async fn wait_for_fig1293_quiescent_journal(storage: &PostgresStorage) {
    let snapshot = || async {
        let mut rows: Vec<(String,)> = sqlx::query_as(
            "SELECT replay_key FROM lash_runtime_effect_replay
             WHERE session_id = $1 AND status = 'in_progress'",
        )
        .bind("fig1293-restate-migrated-tools")
        .fetch_all(storage.pool())
        .await
        .expect("read FIG-1293 in-progress rows");
        rows.sort();
        rows
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut previous = snapshot().await;
        let mut stable_polls = 0;
        while stable_polls < 10 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let current = snapshot().await;
            if current == previous {
                stable_polls += 1;
            } else {
                stable_polls = 0;
                previous = current;
            }
        }
    })
    .await
    .expect("the first host's unparked children settle");
}

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

/// One journal row of the FIG-1293 session, classified by its command: the
/// kind, and the identity it is keyed on inside that kind. Hash-bearing replay
/// keys are deliberately not the label, so the classification is a literal.
fn fig1293_row_identity(envelope: &RuntimeEffectEnvelope) -> (String, String) {
    let kind = envelope.command.kind().as_str().to_string();
    let label = match &envelope.command {
        RuntimeEffectCommand::ToolAttempt { call, attempt, .. } => format!(
            "{}:{}#{attempt}",
            call.tool_name,
            call.args
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
        ),
        RuntimeEffectCommand::ToolInvocation { request } => format!(
            "{}:{}",
            request.call.tool_name,
            request
                .call
                .args
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
        ),
        RuntimeEffectCommand::PresentToolResult { call_id, .. } => call_id.clone(),
        RuntimeEffectCommand::Process { command } => match command.as_ref() {
            lash_core_execution::ProcessCommand::Start { registration, .. } => {
                format!("start:{}", registration.id)
            }
            _ => envelope.invocation.effect_id().to_string(),
        },
        _ => envelope.invocation.effect_id().to_string(),
    };
    (kind, label)
}

/// Every FIG-1293 journal row of the session, classified and sorted
/// (FIG-3550). A redrive that duplicated an identity
/// would show here as a repeated `(kind, label)` pair, which a bare row count
/// cannot tell from a new recorded step such as the presentation boundary.
async fn fig1293_session_rows(storage: &PostgresStorage) -> Vec<(String, String)> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT envelope_json FROM lash_runtime_effect_replay WHERE session_id = $1",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_all(storage.pool())
    .await
    .expect("read FIG-1293 durable child rows");
    let mut identities = rows
        .into_iter()
        .map(|(envelope_json,)| {
            let canonical: serde_json::Value =
                serde_json::from_str(&envelope_json).expect("decode FIG-1293 canonical envelope");
            let envelope = serde_json::from_str::<RuntimeEffectEnvelope>(
                canonical
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .expect("FIG-1293 canonical envelope json"),
            )
            .expect("decode FIG-1293 envelope");
            fig1293_row_identity(&envelope)
        })
        .collect::<Vec<_>>();
    identities.sort();
    let mut distinct = identities.clone();
    distinct.dedup();
    assert_eq!(
        distinct, identities,
        "a redrive re-keys nothing: every journaled identity appears exactly once"
    );
    identities
}

fn fig1293_identities(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut identities = rows
        .iter()
        .map(|(kind, label)| ((*kind).to_string(), (*label).to_string()))
        .collect::<Vec<_>>();
    identities.sort();
    identities
}

/// Runs one FIG-1293 turn on PostgreSQL, parks the host after the commit
/// `crash_after` names, aborts it, and redrives the turn on a second host over
/// the same journal. The children the first host never settled re-drive from
/// the group's retained membership; the ones it did replay their records.
async fn fig1293_crash_and_redrive(
    database_url: &str,
    crash_after: CrashAfter,
    model: lash_core_execution::facade_support::ProviderHandle,
    // What else must have happened on the first host before it is aborted.
    first_host_ready: fn() -> bool,
) -> (
    PostgresStorage,
    lash_core_execution::facade_support::AssembledTurn,
) {
    let storage = PostgresStorage::connect(database_url)
        .await
        .expect("connect FIG-1293 PostgreSQL crash host");
    reset_fig1293_rows(&storage).await;

    let registry: Arc<dyn lash_core_execution::ProcessRegistry> =
        Arc::new(storage.process_registry());
    fig1293_seed_control_target(&registry).await;
    let base_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::new(&storage));
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let effect_host: Arc<dyn EffectHost> = Arc::new(CrossingEffectHost {
        inner: base_effect_host,
        crash_after: Some(crash_after),
        fired: Arc::clone(&fired),
        signal_frames: Arc::new(Mutex::new(Vec::new())),
    });
    let policy = fig1293_policy();
    let state = fig1293_state(&policy);
    let store = detached_session_store().await;
    let mut first = fig1293_runtime(
        pg_law_backend(&storage, Arc::clone(&effect_host), Arc::clone(&registry)).await,
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
        while !fired.load(Ordering::SeqCst) || !first_host_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the selected child boundary must commit before host interruption");
    first_run.abort();
    let interrupted = first_run.await.expect_err("aborted host task");
    assert!(interrupted.is_cancelled());
    // A group's children run on host-owned tasks, not on the aborted turn
    // task, so the first host's siblings of the parked child are still live
    // executors. Expiring a lease one of them still holds would hand its row
    // to the redrive mid-attempt; wait until only the parked work is left.
    wait_for_fig1293_quiescent_journal(&storage).await;
    expire_fig1293_abandoned_effect_rows(&storage).await;

    let replay_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::new(&storage));
    let mut replay = fig1293_runtime(
        pg_law_backend(
            &storage,
            Arc::clone(&replay_effect_host),
            Arc::clone(&registry),
        )
        .await,
        model,
        store,
        policy,
        state,
    )
    .await;
    let redriven = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_fig1293_turn(&mut replay, replay_effect_host.as_ref()),
    )
    .await
    .expect("FIG-1293 child-boundary redrive timed out");
    (storage, redriven)
}

/// Every journal row a migrated-tools crash law leaves once the redrive
/// settles, whichever boundary the first host died at (FIG-3550). Classified
/// by command rather than counted by a `LIKE` match over envelope text, so a
/// recorded step — each call's presentation boundary (FIG-3420), the group's
/// incorporated prefix — reads as what it is, and a duplicated identity cannot
/// hide in a total: the three group children (`cancel_process`, `spawn_agent`
/// and the orchestrating `batch`), the spawn boundary's start and await, the
/// batch body's two echo attempts, and the turn's own steps (its acceptance and
/// the journaled initial drive set it claims, ADR 0069 §6), each exactly once.
const FIG1293_MIGRATED_SESSION_ROWS: &[(&str, &str)] = &[
    ("accept_turn_input", "fig1293-restate-migrated-turn.accept"),
    ("checkpoint", "4"),
    ("checkpoint", "7"),
    (
        "claim_accepted_turn_input",
        "fig1293-restate-migrated-turn.accept.claim_accepted_turn_input",
    ),
    (
        "incorporate_group_settlements",
        "effect-group-incorporate:fig1293-restate-migrated-turn:group:fig1293-restate-migrated-tools:fig1293-restate-migrated-turn:1:0:tool_batch:3:1-3",
    ),
    ("llm_call", "2"),
    ("llm_call", "6"),
    ("peek_await_event", "turn_cancel.after_llm.0"),
    ("peek_await_event", "turn_cancel.after_llm.1"),
    ("peek_await_event", "turn_cancel.after_step.0"),
    ("peek_await_event", "turn_cancel.start_gate"),
    ("present_tool_result", "fig1293-batch"),
    ("present_tool_result", "fig1293-process-cancel"),
    ("present_tool_result", "fig1293-spawn-agent"),
    (
        "process",
        "process:await:process:subagent:fig1293-spawn-agent",
    ),
    ("process", "process:cancel:fig1293-control-target"),
    ("process", "start:process:subagent:fig1293-spawn-agent"),
    ("sync_execution_environment", "1"),
    ("sync_execution_environment", "5"),
    ("tool_attempt", "cancel_process:-#1"),
    ("tool_attempt", "fig1293_echo:alpha#1"),
    ("tool_attempt", "fig1293_echo:beta#1"),
    ("tool_invocation", "batch:-"),
    ("tool_invocation", "cancel_process:-"),
    ("tool_invocation", "spawn_agent:-"),
];

async fn assert_fig1293_postgres_crash_boundary(crash_after: CrashAfter) {
    let Some(database_url) = database_url() else {
        eprintln!("skipping FIG-1293 PostgreSQL crash law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let (model, model_calls) = fig1293_model();
    let (storage, redriven) =
        fig1293_crash_and_redrive(&database_url, crash_after, model, || true).await;
    assert_fig1293_literal_outputs(&redriven).await;
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        3,
        "redrive must replay the recorded provider calls"
    );
    assert_eq!(
        fig1293_session_rows(&storage).await,
        fig1293_identities(FIG1293_MIGRATED_SESSION_ROWS),
        "the interrupted boundary and the group's children retain stable durable identities"
    );
}

/// PostgreSQL redrive law for the exact process-replay boundary between a
/// durable `spawn_agent` child start and its following await.
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_spawn_agent_redrives_after_child_start_before_await_on_postgres() {
    assert_fig1293_postgres_crash_boundary(CrashAfter::SpawnAgentStart).await;
}

/// PostgreSQL redrive law for a crash between a protocol batch's children:
/// the first child's attempt has committed and the host dies before the group
/// settles. The batch is a durable effect group (ADR 0099 §3), so the redrive
/// re-drives the group from its retained membership — the committed child
/// replays its record and the rest run — instead of re-entering a serial
/// batch body.
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_protocol_batch_redrives_between_children_on_postgres() {
    assert_fig1293_postgres_crash_boundary(CrashAfter::FirstProtocolBatchChild).await;
}

/// PostgreSQL redrive law for a group interrupted after one committed success
/// and one committed failure, while its third child is still running. The
/// redrive re-drives the group from its retained membership: the success and
/// the failure replay their recorded attempts byte for byte and never run
/// again, and the child that never settled runs to its own terminal.
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_protocol_batch_partial_failure_redrives_from_retained_membership_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping FIG-1293 PostgreSQL batch-failure law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    FIG1293_BLOCKING_CHILD_RUNS.store(0, Ordering::SeqCst);
    let (storage, redriven) = fig1293_crash_and_redrive(
        &database_url,
        CrashAfter::FailingProtocolBatchChild,
        fig1293_fault_batch_model(),
        // The third child is parked inside its first run when the host dies,
        // so the redrive is what settles it.
        || FIG1293_BLOCKING_CHILD_RUNS.load(Ordering::SeqCst) == 1,
    )
    .await;
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
                        "index": 2,
                        "result": {"echo": "block"},
                        "success": true,
                        "tool": "fig1293_echo",
                    },
                ],
            }),
        )],
        "the enclosing model-facing batch projects the literal three-child terminal oracle",
    );
    assert_eq!(
        FIG1293_BLOCKING_CHILD_RUNS.load(Ordering::SeqCst),
        2,
        "the unsettled child re-drives once from retained membership"
    );

    // The committed attempts are the durable fact: a strict replay of each
    // recorded echo attempt answers its record and never runs a body.
    let recorded_rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT envelope_json, outcome_json FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND envelope_json LIKE '%fig1293_echo%'",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_all(storage.pool())
    .await
    .expect("read FIG-1293 batch-failure rows");
    let mut attempts = recorded_rows
        .into_iter()
        .filter_map(|(envelope_json, outcome_json)| {
            let canonical: serde_json::Value = serde_json::from_str(&envelope_json).ok()?;
            let envelope =
                serde_json::from_str::<RuntimeEffectEnvelope>(canonical.get("json")?.as_str()?)
                    .ok()?;
            let RuntimeEffectCommand::ToolAttempt { call, .. } = &envelope.command else {
                return None;
            };
            let value = call.args["value"].as_str()?.to_string();
            Some((value, envelope, outcome_json?))
        })
        .collect::<Vec<_>>();
    attempts.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        attempts
            .iter()
            .map(|(value, _, _)| value.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "block", "fail"],
        "each child committed exactly one attempt",
    );
    let strict_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect strict FIG-1293 replay host");
    let strict_host = strict_storage.effect_host();
    strict_host.start_replay();
    for (value, envelope, recorded_outcome_json) in attempts {
        let strict_controller = strict_host
            .scoped(lash_core_execution::AdmittedScope::turn(
                "fig1293-restate-migrated-tools",
                "fig1293-restate-migrated-turn",
            ))
            .expect("scope strict FIG-1293 replay");
        let replayed = strict_controller
            .controller()
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async move {
                    panic!("a recorded FIG-1293 attempt must replay without running its body")
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("strictly replay the {value} attempt: {error}"));
        assert_eq!(
            serde_json::to_string(&replayed).expect("encode strictly replayed attempt"),
            recorded_outcome_json,
            "the {value} attempt replays its record byte for byte",
        );
    }
}
