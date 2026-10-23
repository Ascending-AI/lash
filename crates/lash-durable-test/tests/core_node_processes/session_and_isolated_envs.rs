//! What a process runs under when it is not an engine start a policy chose
//! for (FIG-6034): a session-turn process has no environment (D5), and an
//! isolated tool's process runs under exactly the environment its provider
//! states (D6).

use super::*;

/// One boot of a node over `stores`, named `boot` so that each boot is a
/// new incarnation: a cold worker that holds nothing of an earlier one.
pub(super) fn booted(
    stores: &Arc<dyn StoreSet>,
    engines: Vec<Arc<dyn lash_execution_contracts::process::ProcessEngine>>,
    boot: &str,
    build: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
) -> (lash::Backend, lash::LashCore) {
    let backend = engines
        .into_iter()
        .fold(
            lash::durable::DurableBackendBuilder::new(Arc::clone(stores)),
            lash::durable::DurableBackendBuilder::process_engine,
        )
        .build()
        .expect("the backend assembles");
    let core = build(&backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(format!("core-node-{boot}")),
            lash::persistence::LeaseIncarnationId::new(format!("core-node-{boot}-boot")),
        ))
        .expect("the core builds");
    (backend, core)
}

/// Every process the registry holds.
async fn all_processes(
    backend: &lash::Backend,
) -> Vec<lash_execution_contracts::process::ProcessRecord> {
    lash_core::testing::process_roster_records_for_fixture(
        backend.process_registry().as_ref(),
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            ..lash_execution_contracts::process::ProcessListFilter::default()
        },
    )
    .await
    .expect("the registry lists its processes")
}

/// Poll until `found` answers within a minute.
async fn eventually<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut found: impl FnMut() -> F,
) -> T {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(found) = found().await {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} within a minute"))
}

// --- SESSION-TURN-WITHOUT-ENV -------------------------------------------------

const TURN_PARENT: &str = "session-turn-env-parent";
const HOST_CHILD: &str = "session-turn-env-host-child";
const HOST_CHILD_REPLY: &str = "the host-started child answered";
const TOOL_CHILD_TASK: &str = "session-turn-env tool child: hold";

/// A host's detached session-turn start of the child `HOST_CHILD` of
/// `TURN_PARENT`, its creation stating `plugin_options`.
fn host_session_turn(
    plugin_options: lash_core::PluginOptions,
) -> lash_execution_contracts::process::ProcessStartRequest {
    let policy = lash_core::SessionPolicy {
        model: Some(lash::LlmProfileConfig::new(lash::RecordedLlmProfile::mint(
            lash::LlmProfileKey::new(MODEL),
            metadata(),
        ))),
        ..lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash_core::NoProgressBudget::bounded(12),
        )
    };
    let create_request = lash_execution_contracts::session_types::SessionCreateRequest::child(
        TURN_PARENT,
        policy,
        plugin_options,
    )
    .with_session_id(SessionId::fixture(HOST_CHILD.to_owned()));
    lash_execution_contracts::process::ProcessStartRequest::session_turn(
        "host-child-turn",
        create_request,
        lash::TurnInput::text("answer the host"),
        lash_execution_contracts::process::SessionTurnOutcome::Turn,
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::LifetimeDecision::Detached,
    )
}

/// The model of the session-turn law: the parent spawns the tool child
/// once and ends when its call answers; the tool child's turn reaches the
/// model and holds there, announcing each arrival on `held`; the host
/// child answers when `answers` says so, and holds otherwise.
fn session_turn_model(answers: bool, held: &Arc<tokio::sync::Notify>) -> ProviderHandle {
    let held = Arc::clone(held);
    lash_core::testing::TestProvider::builder()
        .kind("session-turn-env")
        .complete(move |request: LlmRequest| {
            let held = Arc::clone(&held);
            async move {
                let session = request
                    .session_id()
                    .map(SessionId::as_str)
                    .map(str::to_owned);
                let transcript = format!("{:?}", request.messages);
                if session.as_deref() == Some(HOST_CHILD) {
                    if answers {
                        return Ok(text(&request, HOST_CHILD_REPLY));
                    }
                    std::future::pending::<()>().await;
                }
                if transcript.contains(TOOL_CHILD_TASK) && session.as_deref() != Some(TURN_PARENT) {
                    held.notify_one();
                    std::future::pending::<()>().await;
                }
                if results(&request).is_empty() {
                    return Ok(call(
                        "session-turn-env-spawn",
                        "agents.spawn",
                        serde_json::json!({ "task": TOOL_CHILD_TASK }),
                    ));
                }
                Ok(text(&request, "the parent saw its child end"))
            }
        })
        .build()
        .into_handle()
}

/// SESSION-TURN-WITHOUT-ENV (D5): a host-started and a tool-declared
/// session-turn process hold no environment. On a cold, fresh worker the
/// host-started one mails its turn and ends on its child's answer, and the
/// tool-declared one is cancelled; a host start whose plugin options do not
/// resolve is refused before anything registers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_turn_process_runs_without_an_environment_on_a_cold_worker() {
    let (stores, _keep) = stores(Tier::SqliteMemory).await;
    let held = Arc::new(tokio::sync::Notify::new());

    // The first worker runs the parent's turn to its tool-declared start,
    // whose child's turn holds at its model.
    let (_, first) = booted(&stores, Vec::new(), "first", |backend| {
        lash::LashCore::standard_builder(backend.clone())
            .serve_test_llm_profile(session_turn_model(false, &held), metadata())
            .plugin(Arc::new(delegation()))
    });
    let parent = first
        .session(SessionId::try_from(TURN_PARENT.to_owned()).unwrap())
        .create(lash::SessionCreation::root(spec()))
        .await
        .expect("the parent is created");
    let _turn = parent
        .send(lash::TurnInput::text("spawn the tool child"))
        .await
        .expect("the parent's turn is admitted");
    tokio::time::timeout(Duration::from_secs(60), held.notified())
        .await
        .expect("the tool child's turn reaches its model");
    first.shutdown().await.expect("the first worker stops");
    drop(first);

    // A core that serves nothing registers the host's start: no worker
    // has seen it when the fresh one boots.
    let (_, starter) = booted(&stores, Vec::new(), "starter", |backend| {
        lash::LashCore::standard_builder(backend.clone())
            .serve_test_llm_profile(session_turn_model(false, &held), metadata())
            .serve_sessions(false)
    });
    let host_started = starter
        .processes()
        .start(
            host_session_turn(lash_core::PluginOptions::default()),
            starter.effect_host(),
        )
        .await
        .expect("the host's session-turn start registers")
        .process_id;
    starter.shutdown().await.expect("the starter stops");
    drop(starter);

    let (backend, fresh) = booted(&stores, Vec::new(), "fresh", |backend| {
        lash::LashCore::standard_builder(backend.clone())
            .serve_test_llm_profile(session_turn_model(true, &held), metadata())
            .plugin(Arc::new(delegation()))
    });
    let processes = all_processes(&backend).await;
    assert_eq!(
        processes.len(),
        2,
        "two session-turn processes: {processes:#?}"
    );
    for process in &processes {
        assert!(
            matches!(
                process.input.as_ref(),
                lash_execution_contracts::process::ProcessInput::SessionTurn { .. }
            ),
            "{process:#?}"
        );
        assert_eq!(process.env_ref(), None, "no environment: {process:#?}");
    }
    let tool_declared = processes
        .iter()
        .map(|process| process.id.clone())
        .find(|id| *id != host_started)
        .expect("the tool-declared start");

    let host_output = success(&ended(&fresh, &host_started).await);
    assert!(
        format!("{host_output}").contains(HOST_CHILD_REPLY),
        "the host-started process mails its turn and ends on its child's answer: {host_output}"
    );
    fresh
        .processes()
        .cancel(&tool_declared, fresh.effect_host())
        .await
        .expect("the cancel is requested");
    let cancelled = tokio::time::timeout(
        Duration::from_secs(60),
        fresh.processes().await_output(&tool_declared),
    )
    .await
    .expect("the cancelled process ends within a minute")
    .expect("its end is read");
    assert!(
        matches!(&cancelled, lash_execution_contracts::process::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Cancelled(_))),
        "the tool-declared process ends cancelled: {cancelled:?}"
    );

    let unresolved = lash_core::PluginOptions::typed(
        "session-turn-env-no-such-plugin",
        serde_json::json!({ "x": 1 }),
    )
    .expect("the options encode");
    let refused = fresh
        .processes()
        .start(host_session_turn(unresolved), fresh.effect_host())
        .await;
    assert!(
        refused.is_err(),
        "options naming no installed plugin are refused: {refused:?}"
    );
    assert_eq!(
        all_processes(&backend).await.len(),
        2,
        "the refused start registers nothing"
    );
    fresh.shutdown().await.expect("the fresh worker stops");
}

// --- ISOLATED-EXPLICIT-ENV ----------------------------------------------------

const EXPLICIT_ENGINE: &str = "core-node-explicit-env-engine";
const EXPLICIT_TOOL: &str = "iso_explicit";
const EXPLICIT_OPERATION: &str = "iso.explicit";
const ORDINARY_TOOL: &str = "core_node_ordinary";
const ORDINARY_OPERATION: &str = "tools.core_node_ordinary";
const B_TOOL: &str = "core_node_b";
/// Cross-cutting: no tools, one result check.
const CHECK_COMPONENT: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "core-node-check",
    lash_vocabulary::providers::FormatVersion::ONE,
);
/// The component that provides `B_TOOL`.
const B_COMPONENT: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "core-node-b",
    lash_vocabulary::providers::FormatVersion::ONE,
);

/// The engine runs `B_TOOL` once and ends with its answer.
fn explicit_engine_advance(
    _state: &mut serde_json::Value,
    event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    match event {
        lash_execution_contracts::process::EngineEvent::Started { payload } => {
            lash_execution_contracts::process::EngineAction::Steps {
                steps: vec![lash_execution_contracts::process::StepRequest::Tool {
                    step: lash_execution_contracts::process::StepName("b".to_owned()),
                    use_: operations::use_for(&b_runs()[0]),
                    input: payload,
                    site: None,
                }],
                wake: None,
            }
        }
        lash_execution_contracts::process::EngineEvent::StepSettled { outcome, .. } => {
            answer(serde_json::json!({ "step": settled(&outcome) }))
        }
        lash_execution_contracts::process::EngineEvent::Cancelled { origin, .. } => {
            cancelled(origin)
        }
        _ => lash_execution_contracts::process::EngineAction::Idle,
    }
}

/// What the explicit engine performs: B's tool.
fn b_runs() -> Vec<lash_vocabulary::tool_contract::ToolDefinition> {
    vec![object_tool(B_TOOL, ["b", "run"])]
}

fn object_tool(name: &str, binding: [&str; 2]) -> lash_vocabulary::tool_contract::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        name,
        format!("{}.{}", binding[0], binding[1]),
        "Answers its arguments.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(Duration::from_secs(120))
}

/// The ordinary tool: repeatable, so that a worker stopping mid-body
/// redelivers it.
fn ordinary_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    object_tool(ORDINARY_TOOL, ["tools", ORDINARY_TOOL]).with_execution_policy(
        lash_vocabulary::tool_contract::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(3).unwrap(),
            1,
            1,
        ),
    )
}

fn explicit_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    object_tool(EXPLICIT_TOOL, ["iso", "explicit"])
        .isolated_in(EXPLICIT_ENGINE)
        .expect("an isolated tool names its engine")
}

/// The environment the isolated tool's provider states: B's tool, from
/// `[check, B]` in that order.
fn explicit_environment() -> lash::process::ProcessEnv {
    environment_of(
        vec![(B_COMPONENT, object_tool(B_TOOL, ["b", "run"]))],
        &[CHECK_COMPONENT, B_COMPONENT],
    )
}

/// What the law's tools and hooks saw. It outlives every node.
#[derive(Default)]
struct Seen {
    /// Each ordinary-body delivery: the process environments stored then.
    ordinary: Mutex<Vec<i64>>,
    /// Runs of `check`'s result hook.
    checks: AtomicUsize,
    /// The call results the model read when it ended the turn.
    answered: Mutex<Vec<String>>,
}

/// The session's tools: the isolated tool, and an ordinary tool whose first
/// delivery holds until its worker stops.
struct SessionTools {
    seen: Arc<Seen>,
    database: String,
}

/// The process environments the database stores.
fn stored_envs(database: &str) -> i64 {
    rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("the database opens for inspection")
    .query_row(
        "SELECT COUNT(*) FROM artifact_refs WHERE namespace = 'process_execution_env'",
        [],
        |row| row.get(0),
    )
    .expect("the stored environments are counted")
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SessionTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok({
            vec![
                explicit_definition().manifest(),
                ordinary_definition().manifest(),
            ]
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        match name {
            EXPLICIT_OPERATION => Some(Arc::new(explicit_definition().contract())),
            ORDINARY_OPERATION => Some(Arc::new(ordinary_definition().contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        assert_eq!(
            call.name(),
            ORDINARY_OPERATION,
            "only the ordinary tool runs a body"
        );
        let first = {
            let mut ordinary = self.seen.ordinary.lock().unwrap();
            ordinary.push(stored_envs(&self.database));
            ordinary.len() == 1
        };
        if first {
            std::future::pending::<()>().await;
        }
        lash_core::ToolOutcome::ok(serde_json::json!("ordinary answered")).into()
    }

    /// The call's process runs under `[check, B]` (D6), whatever the
    /// calling session's tools.
    fn isolated_start(
        &self,
        call: lash_core::IsolatedProcessRequest<'_>,
    ) -> Result<lash_core::IsolatedStart, lash_core::ProcessEnvRefusal> {
        Ok(lash_core::IsolatedStart {
            payload: call.args.clone(),
            env: lash_core::ProcessEnvChoice::New(explicit_environment()),
            engine_settings: None,
        })
    }
}

struct AnswerB;

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for AnswerB {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "b": call.args })).into()
    }
}

/// A core whose model calls the ordinary tool, then the isolated one, and
/// then ends; the `[check, B]` components are installed on it.
fn explicit_core(backend: &lash::Backend, tools: &Arc<SessionTools>) -> lash::LashCoreBuilder {
    let seen = Arc::clone(&tools.seen);
    let model = scripted(move |request, _| match results(request).len() {
        0 => call(
            "explicit-ordinary",
            ORDINARY_OPERATION,
            serde_json::json!({ "n": 1 }),
        ),
        1 => call(
            "explicit-isolated",
            EXPLICIT_OPERATION,
            serde_json::json!({ "label": "explicit" }),
        ),
        _ => {
            *seen.answered.lock().unwrap() = results(request);
            text(request, "both calls answered")
        }
    });
    let seen = Arc::clone(&tools.seen);
    let check = lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(CHECK_COMPONENT.component()),
        lash_core::facade_support::PluginSpec::new().with_tool_result_check(
            lash_core_execution::hook_key!("check"),
            Arc::new(move |_input| {
                seen.checks.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(lash_core::facade_support::AfterToolContributions::default()) })
            }),
        ),
    )
    .as_component(CHECK_COMPONENT.format())
    .expect("the check is a process component");
    let b = lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(B_COMPONENT.component()),
        lash_core::testing::tool_spec(
            B_COMPONENT.component(),
            lash::tool_provider_key!("tools"),
            Arc::new(lash::tools::StaticToolProvider::new(
                vec![object_tool(B_TOOL, ["b", "run"])],
                AnswerB,
            )),
        ),
    )
    .as_component(B_COMPONENT.format())
    .expect("B is a process component");
    lash::LashCore::standard_builder(backend.clone())
        .serve_test_llm_profile(model, metadata())
        .plugin(packages::tools(
            lash::tool_provider_key!("tools"),
            Arc::clone(tools) as Arc<dyn lash_core::ToolProvider>,
        ))
        .process_component(Arc::new(check), CHECK_COMPONENT)
        .process_component(Arc::new(b), B_COMPONENT)
}

/// ISOLATED-EXPLICIT-ENV (D6): an isolated tool's process runs under the
/// environment its provider states, `[check, B]`, not the calling
/// session's tools: the row holds that environment's reference and its
/// `check` runs. An ordinary call in the same turn publishes no process
/// environment, and after its worker stops mid-body, its redelivery on a
/// fresh worker presents its answer under the facts its admission recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_isolated_tool_runs_its_process_under_its_providers_explicit_environment() {
    let sqlite = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let database = sqlite.database_uri().to_owned();
    let stores: Arc<dyn StoreSet> = sqlite;
    let seen = Arc::new(Seen::default());
    let tools = Arc::new(SessionTools {
        seen: Arc::clone(&seen),
        database: database.clone(),
    });
    let engine = || {
        Arc::new(ScriptEngine {
            kind: EXPLICIT_ENGINE,
            advance: explicit_engine_advance,
        }) as Arc<dyn lash_execution_contracts::process::ProcessEngine>
    };
    let before = stored_envs(&database);

    let (_, first) = booted(&stores, vec![engine()], "explicit-first", |backend| {
        explicit_core(backend, &tools)
    });
    let session_id = SessionId::try_from("explicit-env".to_owned()).unwrap();
    let session = first
        .session(session_id.clone())
        .create(lash::SessionCreation::root(spec()))
        .await
        .expect("the session is created");
    let _turn = session
        .send(lash::TurnInput::text("call both tools"))
        .await
        .expect("the turn is admitted");
    eventually("the ordinary body is delivered", || {
        let delivered = !seen.ordinary.lock().unwrap().is_empty();
        async move { delivered.then_some(()) }
    })
    .await;
    first
        .shutdown()
        .await
        .expect("the first worker stops mid-body");
    drop(first);

    let (backend, fresh) = booted(&stores, vec![engine()], "explicit-fresh", |backend| {
        explicit_core(backend, &tools)
    });
    let session = fresh
        .session(session_id)
        .open()
        .await
        .expect("the session opens on the fresh worker");
    eventually("the turn ends on the fresh worker", || {
        let session = &session;
        async move {
            session
                .current_turn()
                .await
                .expect("the turn is read")
                .is_none()
                .then_some(())
        }
    })
    .await;
    let calls = seen.answered.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|result| result.contains("ordinary answered")),
        "the redelivered ordinary call presents its answer: {calls:?}"
    );
    assert_eq!(
        *seen.ordinary.lock().unwrap(),
        vec![before, before],
        "each ordinary delivery found no process environment published for it"
    );

    let processes = all_processes(&backend).await;
    let [process] = processes.as_slice() else {
        panic!("one isolated process: {processes:#?}");
    };
    assert_eq!(
        process.env_ref(),
        Some(&explicit_environment().stable_ref().unwrap()),
        "the row holds the provider's environment"
    );
    assert_eq!(
        success(&ended(&fresh, &process.id).await),
        serde_json::json!({ "step": { "b": { "label": "explicit" } } }),
        "the process ran B's tool"
    );
    assert!(seen.checks.load(Ordering::SeqCst) > 0, "check's hook runs");
    fresh.shutdown().await.expect("the fresh worker stops");
}
