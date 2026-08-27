use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{
    EffectHost, ExecutionScope, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt as _};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};

use crate::support::{SharedDatabaseLock, database_url};

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
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
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
    ) -> Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError> {
        self.inner
            .controller()
            .runtime_effect_failure_disposition(code)
            .await
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
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
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
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
    fn new(storage: &PostgresStorage, state: Arc<ParentEndFaultState>, replay: bool) -> Self {
        let inner = PostgresEffectHost::with_options(
            storage,
            PostgresEffectReplayOptions {
                lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(
                    Duration::from_millis(120),
                )
                .expect("valid PostgreSQL parent-end lease timings"),
            },
        );
        if replay {
            inner.start_replay();
        }
        Self { inner, state }
    }

    fn scoped_controller(
        &self,
        scope: ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'static>, lash_core::RuntimeError> {
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
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
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
    ) -> Result<lash_core::TurnControlBinding<'a>, lash_core::RuntimeError> {
        self.inner.turn_control_binding(scoped).await
    }

    async fn prepare_tool_intent(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> Result<lash_core::ToolIntentPreparation, lash_core::RuntimeError> {
        self.inner.prepare_tool_intent(sink, identity, intent).await
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .record_tool_intent_outcome(sink, identity, submitted, outcome)
            .await
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.scoped_controller(scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
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

fn process_worker(
    registry: Arc<dyn lash_core::ProcessRegistry>,
    effect_host: Arc<dyn EffectHost>,
    env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    plugin: Arc<dyn lash_core::facade_support::PluginFactory>,
) -> lash_core::facade_support::DurableProcessWorker {
    let watched = lash_core::facade_support::watch_process_registry(registry);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_parent_end_scanners_cancel_once_on_postgres() {
    const PARENT: &str = "pg-concurrent-scanner-parent";
    const CHILD: &str = "pg-concurrent-scanner-child";

    let Some(database_url) = database_url() else {
        eprintln!("skipping PostgreSQL concurrent parent-end law: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect PostgreSQL concurrent parent-end law");
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
    .expect("persist concurrent parent-end execution env");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let plugin = parent_end_plugin(Arc::clone(&provider_calls));

    registry
        .register_process(lash_core::ProcessRegistration::new(
            CHILD,
            lash_core::ProcessInput::External {
                metadata: serde_json::json!({"role": "concurrent-scanner-child"}),
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register concurrent parent-end child");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                PARENT,
                lash_core::ProcessInput::ToolCall {
                    call: lash_core::PreparedToolCall::from_parts(
                        "pg-concurrent-scanner-parent-call",
                        "tool:pg_process_parent_intent",
                        "pg_process_parent_intent",
                        serde_json::json!({"emit": false, "child": "unused"}),
                        None,
                        serde_json::Value::Null,
                    ),
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register concurrent parent-end parent");
    let identity = lash_core::derive_tool_intent_identity(
        "pg-concurrent-scanner-session",
        PARENT,
        Some("pg-concurrent-scanner-call"),
        0,
    )
    .expect("derive concurrent parent-end identity");
    let action = lash_core::ToolIntentParentEndAction {
        identity: identity.clone(),
        parent_end: lash_core::ToolIntentParentEnd {
            process_id: CHILD.to_string(),
            policy: lash_core::ProcessParentEndPolicy::Cancel,
        },
    };
    registry
        .complete_process_with_parent_end(
            PARENT,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({"parent": "done"}),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
            vec![action.clone()],
        )
        .await
        .expect("commit concurrent parent-end plan");
    assert_eq!(
        registry
            .get_pending_parent_end_plan(PARENT)
            .await
            .expect("read concurrent parent-end plan"),
        Some(lash_core::ProcessParentEndPlan {
            process_id: PARENT.to_string(),
            actions: vec![action],
        })
    );

    let state = Arc::new(ParentEndFaultState::default());
    *state
        .concurrent_parent_end_barrier
        .lock()
        .expect("install concurrent parent-end barrier") =
        Some(Arc::new(tokio::sync::Barrier::new(2)));
    let worker_a = process_worker(
        Arc::clone(&registry),
        Arc::new(ParentEndFaultHost::new(&storage, Arc::clone(&state), false)),
        Arc::clone(&env_store),
        Arc::clone(&plugin),
    );
    let worker_b = process_worker(
        Arc::clone(&registry),
        Arc::new(ParentEndFaultHost::new(&storage, Arc::clone(&state), false)),
        Arc::clone(&env_store),
        plugin,
    );
    let (scan_a, scan_b) = tokio::join!(
        worker_a.drive_pending_processes(),
        worker_b.drive_pending_processes()
    );
    let _ = scan_a.expect("first synchronized PostgreSQL parent-end scan");
    let _ = scan_b.expect("second synchronized PostgreSQL parent-end scan");

    assert_eq!(
        state
            .frames
            .lock()
            .expect("read concurrent parent-end frames")
            .len(),
        2,
        "both workers must redrive the same pending plan"
    );
    let literal_outcome = lash_core::ToolIntentParentEndOutcome::Cancelled {
        identity,
        process_id: CHILD.to_string(),
    };
    {
        let outcomes = state
            .outcomes
            .lock()
            .expect("read concurrent parent-end outcomes");
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes.iter().all(|outcome| outcome == &literal_outcome),
            "both scanners must observe the literal recorded cancellation outcome"
        );
    }
    assert!(
        registry
            .get_pending_parent_end_plan(PARENT)
            .await
            .expect("read settled concurrent parent-end plan")
            .is_none()
    );
    let cancellations = registry
        .events_after(CHILD, 0)
        .await
        .expect("read concurrent parent-end child events")
        .into_iter()
        .filter(|event| event.event_type == "process.cancel_requested")
        .map(|event| event.payload)
        .collect::<Vec<_>>();
    assert_eq!(
        cancellations,
        vec![serde_json::json!({"reason": PARENT_END_REASON})],
        "two synchronized scanners must append exactly one literal cancellation"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "startup scanners must not enter a provider while settling a retained plan"
    );

    reset(&storage).await;
}
