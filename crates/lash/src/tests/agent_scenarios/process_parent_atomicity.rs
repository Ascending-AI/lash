use super::super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{
    EffectHost, ExecutionScope, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_lashlang_runtime::ToolBinding;
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};
use sqlx::{Connection, PgConnection};

// "LASH_PGT" encoded as a positive i64. Agent scenarios sharing the configured
// database serialize their full interaction through this advisory lock.
const SHARED_DATABASE_LOCK_KEY: i64 = 0x4c41_5348_5f50_4754;

fn database_url() -> Option<String> {
    match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => Some(database_url),
        Ok(_) => {
            if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") {
                panic!("LASH_POSTGRES_DATABASE_URL must be non-empty when LASH_REQUIRE_POSTGRES=1");
            }
            None
        }
        Err(error) => {
            if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") {
                panic!(
                    "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1: {error}"
                );
            }
            None
        }
    }
}

struct SharedDatabaseLock {
    _connection: PgConnection,
}

impl SharedDatabaseLock {
    async fn acquire(database_url: &str) -> Self {
        let mut connection = PgConnection::connect(database_url)
            .await
            .expect("connect Postgres Agent Scenario advisory lock");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SHARED_DATABASE_LOCK_KEY)
            .execute(&mut connection)
            .await
            .expect("acquire Postgres Agent Scenario advisory lock");
        Self {
            _connection: connection,
        }
    }
}

const SESSION: &str = "pg-process-parent-law";
const SEGMENTED_PARENT: &str = "pg-segmented-process-parent";
const TOOL_PARENT: &str = "pg-tool-call-parent";
const PARENT_END_REASON: &str = "recorded start intent parent ended with cancel policy";

#[derive(Default)]
struct ParentEndFaultState {
    crash_before_record_remaining: AtomicUsize,
    crash_after_recorded_parent_end: AtomicUsize,
    recorded_parent_end_count: AtomicUsize,
    completed_local_side_effects: AtomicUsize,
    segment_boundaries: AtomicUsize,
    concurrent_parent_end_barrier: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    frames: Mutex<Vec<RuntimeEffectEnvelope>>,
    outcomes: Mutex<Vec<lash_core::ToolIntentParentEndOutcome>>,
}

struct ParentEndFaultController {
    inner: lash_core::ScopedEffectController<'static>,
    state: Arc<ParentEndFaultState>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for ParentEndFaultController {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> std::result::Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for ParentEndFaultController {
    async fn runtime_effect_failure_disposition(
        &self,
        code: lash_core::RuntimeErrorCode,
    ) -> std::result::Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError>
    {
        self.inner
            .controller()
            .runtime_effect_failure_disposition(code)
            .await
    }

    async fn turn_control_participation(
        &self,
    ) -> std::result::Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
        self.inner.controller().turn_control_participation().await
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        if progress.effects_executed == 0 {
            return None;
        }
        self.state.segment_boundaries.fetch_add(1, Ordering::SeqCst);
        Some(lash_core::BoundaryReason::JournalBudget)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.inner.controller().supports_concurrent_effects()
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let is_parent_end = matches!(
            &envelope.command,
            RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), lash_core::ProcessCommand::ParentEnd { .. })
        );
        if is_parent_end {
            self.state
                .frames
                .lock()
                .expect("PostgreSQL parent-end frame lock")
                .push(envelope.clone());
            let concurrent_barrier = self
                .state
                .concurrent_parent_end_barrier
                .lock()
                .expect("PostgreSQL concurrent parent-end barrier lock")
                .clone();
            if let Some(barrier) = concurrent_barrier {
                barrier.wait().await;
            }
        }
        let crash_before_record = is_parent_end
            && self
                .state
                .crash_before_record_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        let result = if crash_before_record {
            let state = Arc::clone(&self.state);
            let wrapped = local_executor.with_process_outcome_observer(Arc::new(move |outcome| {
                assert!(matches!(
                    outcome,
                    lash_core::ProcessEffectOutcome::ParentEnd { .. }
                ));
                state
                    .completed_local_side_effects
                    .fetch_add(1, Ordering::SeqCst);
                panic!("injected crash after PostgreSQL ParentEnd side effect and before outcome recording");
            }));
            self.inner
                .controller()
                .execute_effect(envelope, wrapped)
                .await
        } else {
            self.inner
                .controller()
                .execute_effect(envelope, local_executor)
                .await
        };
        if is_parent_end
            && let Ok(RuntimeEffectOutcome::Process {
                result: lash_core::ProcessEffectOutcome::ParentEnd { outcome },
            }) = &result
        {
            self.state
                .outcomes
                .lock()
                .expect("PostgreSQL parent-end outcome lock")
                .push((**outcome).clone());
            let recorded = self
                .state
                .recorded_parent_end_count
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            let crash_after = self
                .state
                .crash_after_recorded_parent_end
                .load(Ordering::SeqCst);
            if crash_after != 0 && recorded == crash_after {
                panic!(
                    "injected crash after a PostgreSQL ParentEnd outcome and before the next command"
                );
            }
        }
        result
    }
}

struct ParentEndFaultHost {
    inner: PostgresEffectHost,
    state: Arc<ParentEndFaultState>,
}

impl ParentEndFaultHost {
    /// Fault hosts run on the production lease window. An effect-replay lease
    /// is fenced against the PostgreSQL server clock — `finalize` requires
    /// `lease_expires_at_ms > transaction_timestamp()` — so a TTL trimmed to
    /// make a crashed worker's lease lapse quickly also turns every ordinary
    /// effect in this scenario into a wall-clock race: one scheduler or
    /// round-trip stall between the last renewal and the finalize commit and
    /// the driver reports `postgres_effect_replay_lease_lost` (FIG-2370).
    /// Lapsing an abandoned lease is expressed as a fact instead — see
    /// [`lapse_abandoned_effect_leases`].
    fn new(storage: &PostgresStorage, state: Arc<ParentEndFaultState>, replay: bool) -> Self {
        let inner =
            PostgresEffectHost::with_options(storage, PostgresEffectReplayOptions::default());
        if replay {
            inner.start_replay();
        }
        Self { inner, state }
    }

    fn scoped_controller(
        &self,
        scope: ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'static>, lash_core::RuntimeError>
    {
        let inner = self
            .inner
            .scoped_static(scope.clone())?
            .expect("PostgreSQL supplies static process controllers");
        lash_core::ScopedEffectController::shared(
            Arc::new(ParentEndFaultController {
                inner,
                state: Arc::clone(&self.state),
            }),
            scope,
        )
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for ParentEndFaultHost {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> std::result::Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
}

#[async_trait::async_trait]
impl EffectHost for ParentEndFaultHost {
    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a lash_core::ScopedEffectController<'_>,
    ) -> std::result::Result<lash_core::TurnControlBinding<'a>, lash_core::RuntimeError> {
        self.inner.turn_control_binding(scoped).await
    }

    async fn prepare_tool_intent(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> std::result::Result<lash_core::ToolIntentPreparation, lash_core::RuntimeError> {
        self.inner.prepare_tool_intent(sink, identity, intent).await
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.inner
            .record_tool_intent_outcome(sink, identity, submitted, outcome)
            .await
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.scoped_controller(scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> std::result::Result<
        Option<lash_core::ScopedEffectController<'static>>,
        lash_core::RuntimeError,
    > {
        self.scoped_controller(scope).map(Some)
    }
}

struct ProcessParentIntentTool {
    calls: Arc<AtomicUsize>,
}

impl ProcessParentIntentTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:pg_process_parent_intent",
            "pg_process_parent_intent",
            "Optionally start a child carrying a Cancel-at-parent-end policy.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "emit": {"type": "boolean"},
                    "child": {"type": "string"}
                },
                "required": ["emit", "child"],
                "additionalProperties": false
            }),
            serde_json::json!({"type": "object"}),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "pg_process_parent_intent"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProcessParentIntentTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "pg_process_parent_intent").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        panic!("the PostgreSQL process-parent law must use AttemptContext")
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let child = call
            .args
            .get("child")
            .and_then(serde_json::Value::as_str)
            .expect("literal process-parent child");
        let emit = call
            .args
            .get("emit")
            .and_then(serde_json::Value::as_bool)
            .expect("literal process-parent emit");
        let intents = if emit {
            lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: call.context.session_id().to_string(),
                    request: lash_core::ProcessStartRequest::external(
                        "ignored-derived-child-id",
                        lash_core::ProcessOriginator::host_scoped("postgres-process-parent-law"),
                        serde_json::json!({"process_parent_child": child}),
                    ),
                    on_parent_end: lash_core::ProcessParentEndPolicy::Cancel,
                },
            ))])
        } else {
            lash_core::ToolIntents::default()
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"child": child})),
            intents,
        )
    }
}

fn parent_end_plugin(calls: Arc<AtomicUsize>) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "postgres-process-parent-intent",
        lash_core::facade_support::PluginSpec::new()
            .with_tool_provider(Arc::new(ProcessParentIntentTool { calls })),
    ))
}

async fn segmented_registration(
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> lash_core::ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process main() {
          first = await tools.pg_process_parent_intent({ emit: true, child: "segmented-first" })?
          second = await tools.pg_process_parent_intent({ emit: true, child: "segmented-second" })?
          later = await tools.pg_process_parent_intent({ emit: false, child: "none" })?
          finish later.child
        }
        "#,
    )
    .expect("parse PostgreSQL segmented process-parent law");
    let contract = ProcessParentIntentTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "pg_process_parent_intent",
            "tool:pg_process_parent_intent",
            lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
        )
        .expect("link PostgreSQL process-parent law tool");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default().with_processes(),
        ),
    )
    .expect("link PostgreSQL segmented process-parent law");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store PostgreSQL segmented process-parent artifact");
    let input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: linked.module_ref,
        process_ref: linked
            .artifact
            .process_ref("main")
            .expect("PostgreSQL process-parent main ref")
            .clone(),
        host_requirements_ref: linked.host_requirements_ref,
        process_name: "main".to_string(),
        args: serde_json::Map::new(),
    };
    let identity = input.process_identity();
    lash_core::ProcessRegistration::new(
        SEGMENTED_PARENT,
        input
            .into_process_input()
            .expect("encode PostgreSQL Lashlang process input"),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(SESSION)),
    )
    .with_identity(identity)
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

fn process_worker(
    registry: Arc<dyn lash_core::ProcessRegistry>,
    effect_host: Arc<dyn EffectHost>,
    env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    plugin: Arc<dyn lash_core::facade_support::PluginFactory>,
) -> lash_core::facade_support::DurableProcessWorker {
    let mut runtime_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_env_store(env_store)
    .with_process_engine(Arc::new(
        lash_lashlang_runtime::LashlangProcessEngine::in_memory(
            lash_lashlang_runtime::LashlangSurface::default(),
        ),
    ));
    runtime_host.control.effect_host = effect_host;
    let watched = lash_core::facade_support::watch_process_registry(registry);
    lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(vec![
                Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
                    as Arc<dyn lash_core::facade_support::PluginFactory>,
                plugin,
            ])),
            runtime_host,
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(lash_core::testing::mock_session_policy()),
    )
    .expect("valid test native substrate config")
}

async fn reset(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list PostgreSQL parent-end tables");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .expect("reset PostgreSQL parent-end tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = 0",
    )
    .execute(storage.pool())
    .await
    .expect("reset PostgreSQL process change clock");
}

/// Lapse the effect-replay leases an injected crash abandoned.
///
/// A crashed worker's leases lapse when their TTL runs out; nothing in the
/// substrate hurries that along. Waiting for it is what forced this scenario
/// onto a sub-second lease window, so the scenario states the outcome instead:
/// after a crash, every row still `in_progress` belongs to a worker that is
/// gone, and its lease is expired as of now. The next claimant then takes the
/// row over on the first attempt, with no timing left in the path.
///
/// Returns the number of leases lapsed so callers assert what the crash
/// actually abandoned rather than trusting a statement that may have matched
/// nothing.
async fn lapse_abandoned_effect_leases(storage: &PostgresStorage) -> u64 {
    sqlx::query(
        "UPDATE lash_runtime_effect_replay
         SET lease_expires_at_ms = 0
         WHERE status = 'in_progress'",
    )
    .execute(storage.pool())
    .await
    .expect("lapse abandoned PostgreSQL effect-replay leases")
    .rows_affected()
}

async fn wait_for_count(counter: &AtomicUsize, expected: usize, label: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

#[test]
pub(super) fn agent_scenario_public_process_parents_are_literal_and_crash_atomic_on_postgres()
-> Result<()> {
    run_async_test_on_stack_budget(
        "agent-scenario-postgres-process-parent-atomicity",
        || async {
            Box::pin(public_process_parents_are_literal_and_crash_atomic_on_postgres()).await;
            Ok(())
        },
    )
}

async fn public_process_parents_are_literal_and_crash_atomic_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping PostgreSQL process-parent law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect PostgreSQL process-parent law");
    reset(&storage).await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(storage.process_registry());
    let env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let env_ref = lash_core::runtime::persist_process_execution_env(
        env_store.as_ref(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::empty(),
            lash_core::testing::mock_session_policy(),
        ),
    )
    .await
    .expect("persist PostgreSQL process-parent env");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let plugin = parent_end_plugin(Arc::clone(&provider_calls));

    let first_state = Arc::new(ParentEndFaultState::default());
    first_state
        .crash_before_record_remaining
        .store(1, Ordering::SeqCst);
    let first_host: Arc<dyn EffectHost> = Arc::new(ParentEndFaultHost::new(
        &storage,
        Arc::clone(&first_state),
        false,
    ));
    let first_worker = process_worker(
        Arc::clone(&registry),
        first_host,
        Arc::clone(&env_store),
        Arc::clone(&plugin),
    );
    registry
        .register_process(segmented_registration(env_ref.clone()).await)
        .await
        .expect("register PostgreSQL segmented process parent");
    let _ = first_worker
        .drive_pending_processes()
        .await
        .expect("drive PostgreSQL segmented parent through public worker path");
    let terminal = lash_core::NativeProcessWork::for_registry(Arc::clone(&registry))
        .await_terminal(SEGMENTED_PARENT)
        .await
        .expect("await PostgreSQL segmented parent terminal");
    assert_eq!(
        terminal,
        lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!("none"),
        ))
    );
    wait_for_count(
        &first_state.completed_local_side_effects,
        1,
        "PostgreSQL ParentEnd crash interval",
    )
    .await;
    assert!(
        first_state.segment_boundaries.load(Ordering::SeqCst) >= 3,
        "the public PostgreSQL worker must cross real Lashlang segment boundaries"
    );
    let pending = registry
        .get_pending_parent_end_plan(SEGMENTED_PARENT)
        .await
        .expect("read PostgreSQL segmented parent-end plan")
        .expect("crash retains PostgreSQL segmented parent-end plan");
    let literal_segmented_plan = lash_core::ProcessParentEndPlan {
        process_id: "pg-segmented-process-parent".to_string(),
        actions: vec![
            lash_core::ToolIntentParentEndAction {
                identity: lash_core::ToolIntentIdentity {
                    session_id: "process-env:pg-segmented-process-parent".to_string(),
                    execution_scope_id: "pg-segmented-process-parent".to_string(),
                    tool_call_id: "lashlang:pg-segmented-process-parent:resource:tool:pg_process_parent_intent:resource_operation:a843718765fe3d88a33b88ad:1".to_string(),
                    intent_index: 0,
                    replay_key: "tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113".to_string(),
                },
                parent_end: lash_core::ToolIntentParentEnd {
                    process_id: "tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113".to_string(),
                    policy: lash_core::ProcessParentEndPolicy::Cancel,
                },
            },
            lash_core::ToolIntentParentEndAction {
                identity: lash_core::ToolIntentIdentity {
                    session_id: "process-env:pg-segmented-process-parent".to_string(),
                    execution_scope_id: "pg-segmented-process-parent".to_string(),
                    tool_call_id: "lashlang:pg-segmented-process-parent:resource:tool:pg_process_parent_intent:resource_operation:bafd89a81de63475643c98aa:1".to_string(),
                    intent_index: 0,
                    replay_key: "tool-intent:v1:blake3:4454df073582e09e241c6b424b24bcf90f9f6be7bd9e5eccf6e3ca3fc8191929".to_string(),
                },
                parent_end: lash_core::ToolIntentParentEnd {
                    process_id: "tool-intent:v1:blake3:4454df073582e09e241c6b424b24bcf90f9f6be7bd9e5eccf6e3ca3fc8191929".to_string(),
                    policy: lash_core::ProcessParentEndPolicy::Cancel,
                },
            },
        ],
    };
    assert_eq!(pending, literal_segmented_plan);
    let incomplete: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, outcome_json, error_json
         FROM lash_runtime_effect_replay
         WHERE replay_key LIKE '%:parent-end:%'",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read interrupted PostgreSQL ParentEnd row");
    assert_eq!(incomplete, ("in_progress".to_string(), None, None));
    assert_eq!(
        lapse_abandoned_effect_leases(&storage).await,
        1,
        "the crash abandons exactly the interrupted ParentEnd lease"
    );

    let already_cancelled = &literal_segmented_plan.actions[1].parent_end.process_id;
    registry
        .append_event(
            already_cancelled,
            lash_core::ProcessEventAppendRequest::cancel_requested(
                already_cancelled,
                Some(PARENT_END_REASON.to_string()),
            ),
        )
        .await
        .expect("another actor cancels the second child before parent-end redrive");
    for action in &literal_segmented_plan.actions {
        assert_eq!(
            registry
                .events_after(&action.parent_end.process_id, 0)
                .await
                .expect("read pre-redrive child cancellation")
                .iter()
                .filter(|event| event.event_type == "process.cancel_requested")
                .count(),
            1,
            "each child has exactly one cancellation before redrive"
        );
    }

    let before_clear_state = Arc::new(ParentEndFaultState::default());
    before_clear_state
        .crash_after_recorded_parent_end
        .store(1, Ordering::SeqCst);
    let before_clear_host: Arc<dyn EffectHost> = Arc::new(ParentEndFaultHost::new(
        &storage,
        Arc::clone(&before_clear_state),
        false,
    ));
    let before_clear_worker = process_worker(
        Arc::clone(&registry),
        before_clear_host,
        Arc::clone(&env_store),
        Arc::clone(&plugin),
    );
    let crashed = tokio::spawn(async move { before_clear_worker.drive_pending_processes().await })
        .await
        .expect_err("crash after first ParentEnd outcome and before the second command");
    assert!(crashed.is_panic());
    assert_eq!(
        before_clear_state
            .outcomes
            .lock()
            .expect("PostgreSQL pre-clear outcome lock")
            .as_slice(),
        [lash_core::ToolIntentParentEndOutcome::Cancelled {
            identity: literal_segmented_plan.actions[0].identity.clone(),
            process_id: literal_segmented_plan.actions[0]
                .parent_end
                .process_id
                .clone(),
        }]
    );
    assert_eq!(
        serde_json::to_value(
            &*before_clear_state
                .frames
                .lock()
                .expect("PostgreSQL pre-clear frame lock")
        )
        .expect("serialize PostgreSQL ParentEnd frames"),
        serde_json::json!([
            {
                "invocation": {
                    "scope": {"session_id": "process-env:pg-segmented-process-parent"},
                    "subject": {
                        "type": "effect",
                        "effect_id": "process:parent-end:tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113",
                        "kind": "process"
                    },
                    "caused_by": {
                        "type": "effect",
                        "session_id": "process-env:pg-segmented-process-parent",
                        "effect_id": "tool-intent-parent-end:0"
                    },
                    "replay": {
                        "key": "tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113:parent-end:process:parent-end:tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113",
                        "attribution": {
                            "kind": "tool_intent",
                            "identity": {
                                "session_id": "process-env:pg-segmented-process-parent",
                                "execution_scope_id": "pg-segmented-process-parent",
                                "tool_call_id": "lashlang:pg-segmented-process-parent:resource:tool:pg_process_parent_intent:resource_operation:a843718765fe3d88a33b88ad:1",
                                "intent_index": 0,
                                "replay_key": "tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113"
                            }
                        }
                    }
                },
                "command": {
                    "type": "process",
                    "command": {
                        "op": "parent_end",
                        "identity": {
                            "session_id": "process-env:pg-segmented-process-parent",
                            "execution_scope_id": "pg-segmented-process-parent",
                            "tool_call_id": "lashlang:pg-segmented-process-parent:resource:tool:pg_process_parent_intent:resource_operation:a843718765fe3d88a33b88ad:1",
                            "intent_index": 0,
                            "replay_key": "tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113"
                        },
                        "process_id": "tool-intent:v1:blake3:0fcee5e819176c296f7d29e6e0ce488d0ae0944e38078c3213310a61d2b3f113",
                        "policy": "cancel",
                        "reason": "recorded start intent parent ended with cancel policy"
                    }
                }
            }
        ])
    );
    assert_eq!(
        registry
            .get_pending_parent_end_plan(SEGMENTED_PARENT)
            .await
            .expect("read pre-clear PostgreSQL plan")
            .expect("pre-clear crash retains the plan"),
        literal_segmented_plan
    );

    // The second crash lands after the ParentEnd outcome is finalized and
    // propagates out of the drive, so it abandons no lease and the concurrent
    // redrive below claims the remaining action fresh.
    assert_eq!(
        lapse_abandoned_effect_leases(&storage).await,
        0,
        "a crash after the recorded outcome leaves no lease in progress"
    );

    let final_state_a = Arc::new(ParentEndFaultState::default());
    let final_state_b = Arc::new(ParentEndFaultState::default());
    let final_worker_a = process_worker(
        Arc::clone(&registry),
        Arc::new(ParentEndFaultHost::new(
            &storage,
            Arc::clone(&final_state_a),
            false,
        )),
        Arc::clone(&env_store),
        Arc::clone(&plugin),
    );
    let final_worker_b = process_worker(
        Arc::clone(&registry),
        Arc::new(ParentEndFaultHost::new(
            &storage,
            Arc::clone(&final_state_b),
            false,
        )),
        Arc::clone(&env_store),
        Arc::clone(&plugin),
    );
    let (scan_a, scan_b) = tokio::join!(
        final_worker_a.drive_pending_processes(),
        final_worker_b.drive_pending_processes()
    );
    let _ = scan_a.expect("first concurrent PostgreSQL startup scan");
    let _ = scan_b.expect("second concurrent PostgreSQL startup scan");
    assert!(
        registry
            .get_pending_parent_end_plan(SEGMENTED_PARENT)
            .await
            .expect("read cleared PostgreSQL plan")
            .is_none(),
        "a successful redrive clears the retained plan"
    );
    for action in &literal_segmented_plan.actions {
        assert_eq!(
            registry
                .events_after(&action.parent_end.process_id, 0)
                .await
                .expect("read redriven PostgreSQL child cancellation")
                .iter()
                .filter(|event| event.event_type == "process.cancel_requested")
                .count(),
            1,
            "concurrent startup scans and redrive preserve exactly one cancellation"
        );
    }
    let recorded_outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome_json
         FROM lash_runtime_effect_replay
         WHERE replay_key LIKE '%:parent-end:%'
         ORDER BY replay_key",
    )
    .fetch_all(storage.pool())
    .await
    .expect("read literal PostgreSQL ParentEnd outcomes");
    let recorded_outcomes = recorded_outcomes
        .into_iter()
        .map(|json| {
            let RuntimeEffectOutcome::Process {
                result: lash_core::ProcessEffectOutcome::ParentEnd { outcome },
            } = serde_json::from_str::<RuntimeEffectOutcome>(&json)
                .expect("decode PostgreSQL ParentEnd outcome")
            else {
                panic!("PostgreSQL ParentEnd row stored another outcome")
            };
            *outcome
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded_outcomes,
        vec![
            lash_core::ToolIntentParentEndOutcome::Cancelled {
                identity: literal_segmented_plan.actions[0].identity.clone(),
                process_id: literal_segmented_plan.actions[0]
                    .parent_end
                    .process_id
                    .clone(),
            },
            lash_core::ToolIntentParentEndOutcome::Cancelled {
                identity: literal_segmented_plan.actions[1].identity.clone(),
                process_id: literal_segmented_plan.actions[1]
                    .parent_end
                    .process_id
                    .clone(),
            },
        ]
    );
    let frames_before_clear_redrive = final_state_a
        .frames
        .lock()
        .expect("PostgreSQL final frame lock")
        .len();
    let _ = final_worker_a
        .drive_pending_processes()
        .await
        .expect("redrive after PostgreSQL plan clear");
    assert_eq!(
        final_state_a
            .frames
            .lock()
            .expect("PostgreSQL cleared frame lock")
            .len(),
        frames_before_clear_redrive,
        "redrive after plan clear issues no further ParentEnd command"
    );

    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                TOOL_PARENT,
                lash_core::ProcessInput::ToolCall {
                    call: lash_core::PreparedToolCall::from_parts(
                        "pg-tool-call-parent-call",
                        "tool:pg_process_parent_intent",
                        "pg_process_parent_intent",
                        serde_json::json!({"emit": true, "child": "tool-call"}),
                        None,
                        serde_json::Value::Null,
                    ),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::session(lash_core::SessionScope::new(SESSION)),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register PostgreSQL ToolCall process parent");
    let _ = final_worker_a
        .drive_pending_processes()
        .await
        .expect("drive PostgreSQL ToolCall parent through public worker path");
    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry))
            .await_terminal(TOOL_PARENT)
            .await
            .expect("await PostgreSQL ToolCall parent"),
        lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!({"child": "tool-call"}),
        ))
    );
    let _ = final_worker_a
        .drive_pending_processes()
        .await
        .expect("settle PostgreSQL ToolCall parent teardown");
    let literal_tool_identity = lash_core::ToolIntentIdentity {
        session_id: "process-env:pg-tool-call-parent".to_string(),
        execution_scope_id: "pg-tool-call-parent".to_string(),
        tool_call_id: "pg-tool-call-parent-call".to_string(),
        intent_index: 0,
        replay_key:
            "tool-intent:v1:blake3:1be9324a1697dfce44d6d9b2760bbd63c34736c5ca4c50d2b9243ed134e79458"
                .to_string(),
    };
    assert_eq!(
        registry
            .events_after(&literal_tool_identity.replay_key, 0)
            .await
            .expect("read PostgreSQL ToolCall child cancellation")
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        1
    );
    let tool_outcome_json: String = sqlx::query_scalar(
        "SELECT outcome_json
         FROM lash_runtime_effect_replay
         WHERE replay_key LIKE $1",
    )
    .bind(format!(
        "%{}:parent-end:%",
        literal_tool_identity.replay_key
    ))
    .fetch_one(storage.pool())
    .await
    .expect("read PostgreSQL ToolCall ParentEnd outcome");
    let RuntimeEffectOutcome::Process {
        result: lash_core::ProcessEffectOutcome::ParentEnd { outcome },
    } = serde_json::from_str::<RuntimeEffectOutcome>(&tool_outcome_json)
        .expect("decode PostgreSQL ToolCall ParentEnd outcome")
    else {
        panic!("PostgreSQL ToolCall ParentEnd row stored another outcome")
    };
    assert_eq!(
        *outcome,
        lash_core::ToolIntentParentEndOutcome::Cancelled {
            identity: literal_tool_identity.clone(),
            process_id: literal_tool_identity.replay_key,
        }
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        4,
        "redrive never re-enters the three-segment or ToolCall provider bodies"
    );

    reset(&storage).await;
}
