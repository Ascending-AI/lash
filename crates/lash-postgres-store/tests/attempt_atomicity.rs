//! The PostgreSQL counterpart of the tool-attempt atomicity gate.
//!
//! On Restate the hazard is real: the journal is **ordinal-addressed**, so a
//! nested command emitted from inside a recorded `ToolAttempt` shifts every
//! later ordinal and redrive fails with `RT0016`.
//!
//! The PostgreSQL effect-replay tier is **key-addressed**: every effect claims
//! its own `(scope_id, replay_key)` row under a fenced lease
//! (`postgres/effect_replay.rs`, `runtime/effect/effect_replay_driver.rs`).
//! A nested effect claims its own key, so there is no ordinal to shift. That is
//! a claim, and this module proves it: a recorded attempt whose body emits a
//! nested effect is crashed and redriven on a second, independently-connected
//! host, and both the attempt and its nested effect replay their recorded
//! terminals byte-for-byte without re-executing either body.

use std::sync::Arc;

fn registry_local_executor(
    registry: Arc<dyn lash_core::ProcessRegistry>,
) -> lash_core::RuntimeEffectLocalExecutor<'static> {
    let process_work = Arc::new(lash_core::NativeProcessWork::for_registry(Arc::clone(
        &registry,
    )));
    lash_core::RuntimeEffectLocalExecutor::processes(registry, process_work)
}
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::{
    EffectHost, ExecutionScope, ProcessRegistry as _, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimeScope,
};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions, PostgresStorage};

// Keep subsequent lines stable for machine-checked public API evidence anchors.
// Shared test support now lives at the grouped integration-harness root.
use crate::support::{SharedDatabaseLock, database_url};

const SESSION: &str = "pg-attempt-atomicity-session";
const TURN: &str = "pg-attempt-atomicity-turn";
const ATTEMPT_KEY: &str = "pg-attempt-atomicity:attempt";
const NESTED_KEY: &str = "pg-attempt-atomicity:attempt:nested";

struct CrossingController {
    inner: Arc<dyn lash_core::RuntimeEffectController>,
    signal_frames: Arc<Mutex<Vec<Vec<u8>>>>,
    crash_after: Option<CrashAfter>,
    cancel_after_batch_failure: Option<tokio_util::sync::CancellationToken>,
    interrupt_after_batch_failure: bool,
    force_serial: bool,
    fired: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Copy)]
enum CrashAfter {
    SpawnAgentStart,
    FirstProtocolBatchChild,
}

struct CrashingEffectHost {
    inner: Arc<dyn EffectHost>,
    crash_after: CrashAfter,
    force_serial: bool,
    fired: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for CrashingEffectHost {
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
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inner.peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }
    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }
    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl EffectHost for CrashingEffectHost {
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
        let inner = self
            .inner
            .scoped_static(scope.clone())?
            .expect("PostgreSQL exposes static scopes");
        lash_core::ScopedEffectController::shared(
            Arc::new(CrossingController {
                inner: Arc::new(ScopedControllerAdapter(inner)),
                signal_frames: Arc::new(Mutex::new(Vec::new())),
                crash_after: Some(self.crash_after),
                cancel_after_batch_failure: None,
                interrupt_after_batch_failure: false,
                force_serial: self.force_serial,
                fired: Arc::clone(&self.fired),
            }),
            scope,
        )
    }
    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        let inner = self
            .inner
            .scoped_static(scope.clone())?
            .expect("PostgreSQL exposes static scopes");
        lash_core::ScopedEffectController::shared(
            Arc::new(CrossingController {
                inner: Arc::new(ScopedControllerAdapter(inner)),
                signal_frames: Arc::new(Mutex::new(Vec::new())),
                crash_after: Some(self.crash_after),
                cancel_after_batch_failure: None,
                interrupt_after_batch_failure: false,
                force_serial: self.force_serial,
                fired: Arc::clone(&self.fired),
            }),
            scope,
        )
        .map(Some)
    }
}

struct ScopedControllerAdapter(lash_core::ScopedEffectController<'static>);

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for ScopedControllerAdapter {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.0
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.0.controller().await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.0
            .controller()
            .resolve_await_event(key, resolution)
            .await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.0.controller().peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.0
            .controller()
            .await_await_event(key, cancel, deadline)
            .await
    }
    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.0
            .controller()
            .revoke_await_events_for_session(session_id)
            .await
    }
    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.0
            .controller()
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for ScopedControllerAdapter {
    fn supports_concurrent_effects(&self) -> bool {
        self.0.controller().supports_concurrent_effects()
    }
    async fn runtime_effect_failure_disposition(
        &self,
        code: lash_core::RuntimeErrorCode,
    ) -> Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError> {
        self.0
            .controller()
            .runtime_effect_failure_disposition(code)
            .await
    }
    async fn turn_control_participation(
        &self,
    ) -> Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
        self.0.controller().turn_control_participation().await
    }
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.0
            .controller()
            .execute_effect(envelope, local_executor)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for CrossingController {
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

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for CrossingController {
    fn supports_concurrent_effects(&self) -> bool {
        !self.force_serial && self.inner.supports_concurrent_effects()
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: lash_core::RuntimeErrorCode,
    ) -> Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError> {
        self.inner.runtime_effect_failure_disposition(code).await
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
        self.inner.turn_control_participation().await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let crash_here = match self.crash_after {
            Some(CrashAfter::SpawnAgentStart) => matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(
                        command.as_ref(),
                        lash_core::ProcessCommand::Start { registration, .. }
                            if registration.id == "process:subagent:fig1293-spawn-agent"
                    )
            ),
            Some(CrashAfter::FirstProtocolBatchChild) => matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "fig1293_echo"
                        && call.args.get("value") == Some(&serde_json::json!("alpha"))
            ),
            None => false,
        };
        let cancel_here = self.cancel_after_batch_failure.is_some()
            && matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "fig1293_echo"
                        && call.args.get("value") == Some(&serde_json::json!("fail"))
            );
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), lash_core::ProcessCommand::Signal { .. })
        ) {
            self.signal_frames
                .lock()
                .expect("signal crossing frame lock")
                .push(serde_json::to_vec(&envelope).expect("serialize signal crossing frame"));
        }
        let outcome = self.inner.execute_effect(envelope, local_executor).await;
        if cancel_here && outcome.is_ok() {
            self.cancel_after_batch_failure
                .as_ref()
                .expect("checked cancellation token")
                .cancel();
            if self.interrupt_after_batch_failure && !self.fired.swap(true, Ordering::SeqCst) {
                std::future::pending::<()>().await;
                unreachable!("the host task is aborted after the failure commits cancellation")
            }
        }
        if crash_here && outcome.is_ok() && !self.fired.swap(true, Ordering::SeqCst) {
            std::future::pending::<()>().await;
            unreachable!("the host task is aborted after the selected child commit")
        }
        outcome
    }
}

struct PublicSignalIntentProvider {
    calls: Arc<AtomicUsize>,
    kind: PublicIntentKind,
}

#[derive(Clone, Copy)]
enum PublicIntentKind {
    Signal,
    ParentEnd,
}

fn public_signal_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:pg_public_signal_intent",
        "pg_public_signal_intent",
        "Signal a process through the recorded intent protocol.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for PublicSignalIntentProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![public_signal_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "pg_public_signal_intent").then(|| Arc::new(public_signal_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        panic!("the PostgreSQL public-caller law must use AttemptContext")
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let intent = match self.kind {
            PublicIntentKind::Signal => {
                lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
                    session_id: call.context.session_id().to_string(),
                    process_id: "pg-public-intent-target".to_string(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"source": "postgres-public-caller"}),
                })
            }
            PublicIntentKind::ParentEnd => {
                lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
                    session_id: call.context.session_id().to_string(),
                    request: lash_core::ProcessStartRequest::external(
                        "pg-public-parent-end-child",
                        lash_core::ProcessOriginator::host_scoped("pg-public-caller"),
                        serde_json::json!({"source": "parent-end"}),
                    ),
                    on_parent_end: lash_core::ProcessParentEndPolicy::Cancel,
                }))
            }
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"signal": "recorded"})),
            lash_core::ToolIntents::v1(vec![intent]),
        )
    }
}

struct PanicAtParentEnd;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAtParentEnd {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "tool_intent.parent_end" {
            panic!("injected crash after ToolBatch commit and before parent-end teardown");
        }
    }
}

fn public_runtime_policy() -> lash_core::SessionPolicy {
    let mut policy = lash_core::testing::mock_session_policy();
    policy.session_id = Some(SESSION.to_string());
    policy
}

fn public_runtime_state(policy: &lash_core::SessionPolicy) -> lash_core::RuntimeSessionState {
    lash_core::RuntimeSessionState {
        session_id: SESSION.to_string(),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn public_runtime_input() -> lash_core::TurnInput {
    let mut input = lash_core::TurnInput::text("run PostgreSQL signal intent");
    input.trace_turn_id = Some(TURN.to_string());
    input
}

fn postgres_public_turn_scope(
    storage: &PostgresStorage,
    signal_frames: Arc<Mutex<Vec<Vec<u8>>>>,
) -> lash_core::ScopedEffectController<'static> {
    let scope = ExecutionScope::turn(SESSION, TURN);
    let inner: Arc<dyn lash_core::RuntimeEffectController> =
        Arc::new(storage.runtime_effect_controller(scope.clone()));
    lash_core::ScopedEffectController::shared(
        Arc::new(CrossingController {
            inner,
            signal_frames,
            crash_after: None,
            cancel_after_batch_failure: None,
            interrupt_after_batch_failure: false,
            force_serial: false,
            fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        scope,
    )
    .expect("scope PostgreSQL public turn")
}

async fn public_signal_runtime(
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    provider_calls: Arc<AtomicUsize>,
    model_calls: Arc<AtomicUsize>,
    kind: PublicIntentKind,
) -> lash_core::facade_support::LashRuntime {
    let tool_provider: Arc<dyn lash_core::ToolProvider> = Arc::new(PublicSignalIntentProvider {
        calls: provider_calls,
        kind,
    });
    let tool_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "pg-public-signal-intent",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tool_provider),
        ));
    let model = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_| {
            let model_calls = Arc::clone(&model_calls);
            async move {
                Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::ToolCall {
                            call_id: "pg-public-signal-call".to_string(),
                            tool_name: "pg_public_signal_intent".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    },
                    1 => lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "signal intent complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    },
                    index => panic!("unexpected PostgreSQL model call {index}"),
                })
            }
        })
        .build()
        .into_handle();
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.control.effect_host = effect_host;
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(model),
    );
    let policy = public_runtime_policy();
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::facade_support::InMemorySessionStore::new());
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let registry = Arc::clone(watched.registry());
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_id(SESSION)
        .with_policy(policy.clone())
        .with_initial_state(public_runtime_state(&policy))
        .with_runtime_host(host)
        .with_plugin_factories(
            lash_core::testing::test_standard_protocol_factories()
                .into_iter()
                .chain([tool_plugin])
                .collect(),
        )
        .with_store(store)
        .with_process_work(lash_core::ProcessWorkWiring::new(
            watched,
            Arc::new(lash_core::NativeProcessWork::for_registry(registry)),
        ))
        .with_queued_work(Arc::new(lash_core::NoQueuedWork::new()))
        .build(),
    )
    .await
    .expect("build PostgreSQL public-caller runtime")
}

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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
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
    }
}

fn fig1293_factories() -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
    let echo: Arc<dyn lash_core::ToolProvider> = Arc::new(Fig1293EchoTools);
    vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
        Arc::new(lash_tools::shell::StandardShellPluginFactory::new()),
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
    policy.session_id = Some("fig1293-restate-migrated-tools".to_string());
    policy
}

fn fig1293_state(policy: &lash_core::SessionPolicy) -> lash_core::RuntimeSessionState {
    lash_core::RuntimeSessionState {
        session_id: "fig1293-restate-migrated-tools".to_string(),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn fig1293_input() -> lash_core::TurnInput {
    let mut input = lash_core::TurnInput::text("finish once");
    input.trace_turn_id = Some("fig1293-restate-migrated-turn".to_string());
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
                                    call_id: "fig1293-shell-start".to_string(),
                                    tool_name: "start_command".to_string(),
                                    input_json: serde_json::json!({"cmd": "printf tracked"})
                                        .to_string(),
                                    replay: None,
                                },
                                lash_core::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-shell-detach".to_string(),
                                    tool_name: "start_command".to_string(),
                                    input_json: serde_json::json!({"cmd": "true", "detach": true})
                                        .to_string(),
                                    replay: None,
                                },
                                lash_core::LlmOutputPart::ToolCall {
                                    call_id: "fig1293-shell-write".to_string(),
                                    tool_name: "write_stdin".to_string(),
                                    input_json: serde_json::json!({
                                        "process_id": "fig1293-control-target",
                                        "chars": "fig1293\n",
                                        "close_stdin": false,
                                    })
                                    .to_string(),
                                    replay: None,
                                },
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
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.stdin".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &["fig1293-restate-migrated-tools".to_string()],
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
    host.control.effect_host = effect_host;
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    let worker = lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched.clone()),
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
        .scoped(ExecutionScope::turn(
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
    let scope = ExecutionScope::turn(
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

fn assert_fig1293_literal_outputs(
    turn: &lash_core::facade_support::AssembledTurn,
    signal_sequence: u64,
) {
    let outputs = fig1293_literal_outputs(turn);
    assert_eq!(
        outputs,
        vec![
            (
                "start_command".to_string(),
                serde_json::json!({
                    "__handle__": "process",
                    "done": false,
                    "id": "tool-intent:v1:blake3:38fe75a23e6a480ef35585ffe4f231bc3df10b2626da8f6da26740b0bfb715ea",
                    "process_id": "tool-intent:v1:blake3:38fe75a23e6a480ef35585ffe4f231bc3df10b2626da8f6da26740b0bfb715ea",
                    "running": true,
                    "status": "running",
                }),
            ),
            (
                "start_command".to_string(),
                serde_json::json!({
                    "__handle__": "process",
                    "done": true,
                    "id": "tool-intent:v1:blake3:46535e8757014700cdfcf2a2bab0a5fa1bb0c7f170326057729832f833347f55:detached",
                    "process_id": "tool-intent:v1:blake3:46535e8757014700cdfcf2a2bab0a5fa1bb0c7f170326057729832f833347f55:detached",
                    "running": false,
                    "status": "detached",
                }),
            ),
            (
                "write_stdin".to_string(),
                serde_json::json!({
                    "process_id": "fig1293-control-target",
                    "sequence": signal_sequence,
                    "status": "signalled",
                }),
            ),
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

fn attempt_invocation() -> RuntimeInvocation {
    RuntimeInvocation::effect(
        RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "pg-attempt-atomicity-attempt",
        RuntimeEffectKind::ToolAttempt,
        ATTEMPT_KEY,
    )
}

/// The nested effect the attempt body emits. It carries its own replay key,
/// derived from the attempt's key exactly as `process_effect_invocation` derives
/// a nested process command's key in production.
fn nested_invocation() -> RuntimeInvocation {
    RuntimeInvocation::effect(
        RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "pg-attempt-atomicity-nested",
        RuntimeEffectKind::ToolAttempt,
        NESTED_KEY,
    )
}

/// A recorded `ToolAttempt` — the unit whose body must not be re-entered on
/// redrive. Both the outer attempt and the nested command it emits are journaled
/// as attempts here so each one's body execution is observable.
fn attempt_envelope(invocation: RuntimeInvocation, call_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        invocation,
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core::PreparedToolCall {
                call_id: call_id.to_string(),
                tool_id: lash_core::ToolId::from("tool:pg_attempt_atomicity".to_string()),
                tool_name: "pg_attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                replay: None,
                prepared_payload: serde_json::Value::Null,
            },
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

fn attempt_outcome(call_id: &str, value: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(lash_core::ToolCallRecord {
                call_id: Some(call_id.to_string()),
                tool: "pg_attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                output: lash_core::ToolCallOutput::success(serde_json::json!(value)),
                duration_ms: 0,
            }),
            intents: lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::StartProcess(
                Box::new(lash_core::StartProcessIntent {
                    session_id: SESSION.to_string(),
                    request: lash_core::ProcessStartRequest::external(
                        format!("{call_id}:recorded-child"),
                        lash_core::ProcessOriginator::host_scoped("pg-attempt-atomicity"),
                        serde_json::json!({"value": value}),
                    ),
                    on_parent_end: lash_core::ProcessParentEndPolicy::Abandon,
                }),
            )]),
        }),
        triggers: Vec::new(),
    }
}

fn projected_output(outcome: &RuntimeEffectOutcome) -> String {
    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("expected a tool-attempt outcome");
    };
    let lash_core::ToolAttemptLaunch::Done { record, .. } = launch.as_ref() else {
        panic!("expected a completed tool attempt");
    };
    record.output.value_for_projection().to_string()
}

async fn reset(storage: &PostgresStorage) {
    for statement in [
        "DELETE FROM lash_runtime_effect_replay WHERE scope_id LIKE '%pg-attempt-atomicity%'",
        "DELETE FROM lash_processes WHERE process_id = 'pg-public-intent-target' OR record_json LIKE '%pg-public-caller%'",
    ] {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset the PostgreSQL attempt-atomicity effect rows");
    }
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
        inline_registry,
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
    assert_fig1293_literal_outputs(&inline_turn, 2);
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
    first.set_turn_phase_probe(Arc::new(PanicAtParentEnd));
    let crashed =
        tokio::spawn(async move { run_fig1293_turn(&mut first, first_effect_host.as_ref()).await })
            .await
            .expect_err("FIG-1293 PostgreSQL turn must crash after ToolBatch commit");
    assert!(crashed.is_panic());

    let replay_effect_host: Arc<dyn EffectHost> = Arc::new(storage.effect_host());
    let mut replay = fig1293_runtime(
        Arc::clone(&replay_effect_host),
        postgres_registry,
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
    assert_fig1293_literal_outputs(&postgres_turn, 2);
    assert_eq!(postgres_model_calls.load(Ordering::SeqCst), 3);

    let envelope_json: Vec<String> = sqlx::query_scalar(
        "SELECT envelope_json FROM lash_runtime_effect_replay
         WHERE session_id = $1 ORDER BY replay_key",
    )
    .bind("fig1293-restate-migrated-tools")
    .fetch_all(storage.pool())
    .await
    .expect("read FIG-1293 PostgreSQL journal rows");
    let envelopes = envelope_json
        .into_iter()
        .map(|json| {
            let canonical: serde_json::Value =
                serde_json::from_str(&json).expect("decode FIG-1293 PostgreSQL canonical envelope");
            serde_json::from_str::<RuntimeEffectEnvelope>(
                canonical
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .expect("FIG-1293 canonical envelope json"),
            )
            .expect("decode FIG-1293 PostgreSQL envelope")
        })
        .collect::<Vec<_>>();
    let outer_batch = envelopes
        .iter()
        .find(|envelope| {
            envelope.invocation.caused_by.is_none()
                && matches!(
                    &envelope.command,
                    RuntimeEffectCommand::ToolBatch { batch }
                        if batch.calls.iter().any(|child| child.call.tool_name == "spawn_agent")
                )
        })
        .expect("outer FIG-1293 tool-batch frame");
    let outer_causal_ref = outer_batch
        .invocation
        .causal_ref()
        .expect("outer FIG-1293 batch causal ref");
    let outer_outcome_json: String = sqlx::query_scalar(
        "SELECT outcome_json FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND replay_key = $2",
    )
    .bind("fig1293-restate-migrated-tools")
    .bind(
        outer_batch
            .invocation
            .replay_key()
            .expect("outer FIG-1293 batch replay key"),
    )
    .fetch_one(storage.pool())
    .await
    .expect("read outer FIG-1293 PostgreSQL outcome");
    let outer_outcome: RuntimeEffectOutcome =
        serde_json::from_str(&outer_outcome_json).expect("decode outer FIG-1293 outcome");
    let RuntimeEffectOutcome::ToolBatch { launches, .. } = outer_outcome else {
        panic!("outer FIG-1293 PostgreSQL outcome must be a tool batch")
    };
    assert_eq!(launches.len(), 6);
    assert!(
        !outer_outcome_json.contains(r#""status":"refused""#),
        "every migrated PostgreSQL public intent must execute: {outer_outcome_json}",
    );
    let executed_intent_kinds = [
        (
            "start_process",
            outer_outcome_json
                .matches(r#""kind":"start_process""#)
                .count(),
        ),
        (
            "signal_process",
            outer_outcome_json
                .matches(r#""kind":"signal_process""#)
                .count(),
        ),
        (
            "cancel_process",
            outer_outcome_json
                .matches(r#""kind":"cancel_process""#)
                .count(),
        ),
    ];
    assert_eq!(
        executed_intent_kinds,
        [
            ("start_process", 2),
            ("signal_process", 1),
            ("cancel_process", 1),
        ],
    );
    let direct_orchestration_children = envelopes
        .iter()
        .filter(|envelope| {
            let is_spawn_command = match &envelope.command {
                RuntimeEffectCommand::Process { command } => match command.as_ref() {
                    lash_core::ProcessCommand::Start { registration, .. } => {
                        registration.id == "process:subagent:fig1293-spawn-agent"
                    }
                    lash_core::ProcessCommand::Await { process_id } => {
                        process_id == "process:subagent:fig1293-spawn-agent"
                    }
                    _ => false,
                },
                _ => false,
            };
            let is_nested_batch = matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolBatch { batch }
                    if batch.calls.iter().any(|child| child.call.tool_name == "fig1293_echo")
            );
            (is_spawn_command || is_nested_batch)
                && envelope.invocation.caused_by.as_ref() == Some(&outer_causal_ref)
        })
        .count();
    assert_eq!(
        direct_orchestration_children, 3,
        "spawn start/await and protocol batch must be direct children of the process-replayed outer invocation",
    );

    let mut attempt_names = envelopes
        .into_iter()
        .filter_map(|envelope| match envelope.command {
            RuntimeEffectCommand::ToolAttempt { call, .. } => Some(call.tool_name),
            _ => None,
        })
        .collect::<Vec<_>>();
    attempt_names.sort();
    assert_eq!(
        attempt_names,
        vec![
            "cancel_process".to_string(),
            "fig1293_echo".to_string(),
            "fig1293_echo".to_string(),
            "start_command".to_string(),
            "start_command".to_string(),
            "write_stdin".to_string(),
        ],
        "batch, spawn_agent, and the internal shell process body have no PostgreSQL ToolAttempt frame",
    );
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
    let base_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::with_options(
        &storage,
        PostgresEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(
                std::time::Duration::from_millis(300),
            )
            .expect("valid short FIG-1293 crash lease"),
        },
    ));
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let effect_host: Arc<dyn EffectHost> = Arc::new(CrashingEffectHost {
        inner: base_effect_host,
        crash_after,
        force_serial,
        fired: Arc::clone(&fired),
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
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !fired.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the selected child boundary must commit before host interruption");
    first_run.abort();
    let interrupted = first_run.await.expect_err("aborted host task");
    assert!(interrupted.is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let replay_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::with_options(
        &storage,
        PostgresEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(
                std::time::Duration::from_millis(300),
            )
            .expect("valid short FIG-1293 replay lease"),
        },
    ));
    let mut replay = fig1293_runtime(
        Arc::clone(&replay_effect_host),
        registry,
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
    assert_fig1293_literal_outputs(&redriven, 2);
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
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_spawn_agent_redrives_after_child_start_before_await_on_postgres() {
    assert_fig1293_postgres_crash_boundary(CrashAfter::SpawnAgentStart, false).await;
}

/// PostgreSQL redrive law for a protocol batch after its first child commits
/// but before the next serial child begins. Serial scheduling is the binding
/// substrate geometry used by ordinal journals and remains valid on the
/// key-addressed PostgreSQL controller.
#[tokio::test(flavor = "multi_thread")]
async fn fig1293_protocol_batch_redrives_between_children_on_postgres() {
    assert_fig1293_postgres_crash_boundary(CrashAfter::FirstProtocolBatchChild, true).await;
}

/// PostgreSQL redrive law for a serial protocol batch interrupted after one
/// committed success and one committed failure request cancellation, before
/// the third child starts. Redrive must recover the two recorded children and
/// record a literal cancelled terminal for the third without entering it.
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
    let first_effect_host: Arc<dyn EffectHost> = Arc::new(PostgresEffectHost::with_options(
        &storage,
        PostgresEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(
                std::time::Duration::from_millis(300),
            )
            .expect("valid short FIG-1293 batch-cancel lease"),
        },
    ));
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
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let replay_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect redriving batch-cancel host");
    let replay_host = PostgresEffectHost::with_options(
        &replay_storage,
        PostgresEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(
                std::time::Duration::from_millis(300),
            )
            .expect("valid short FIG-1293 batch-cancel replay lease"),
        },
    );
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
    assert_eq!(
        batch_envelope.invocation.replay_key(),
        Some(batch_replay_key.as_str())
    );
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
        .scoped(ExecutionScope::turn(
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

/// Runs the hazard shape on one host: a recorded attempt whose body emits a
/// nested journal command through the *same* controller.
///
/// Returns how many times each body actually executed.
async fn run_attempt_with_nested_command(host: &PostgresEffectHost) -> (usize, usize, String) {
    let scoped = host
        .scoped(ExecutionScope::turn(SESSION, TURN))
        .expect("scoped PostgreSQL effect controller");
    let attempt_body_runs = Arc::new(AtomicUsize::new(0));
    let nested_body_runs = Arc::new(AtomicUsize::new(0));
    let outcome = {
        let attempt_body_runs = Arc::clone(&attempt_body_runs);
        let nested_body_runs = Arc::clone(&nested_body_runs);
        let controller = scoped.controller();
        controller
            .execute_effect(
                attempt_envelope(attempt_invocation(), "pg-attempt-atomicity-outer"),
                RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                    attempt_body_runs.fetch_add(1, Ordering::SeqCst);
                    // The nested emission: a second journal command issued from
                    // inside the recorded body, through the same controller.
                    let nested_body_runs = Arc::clone(&nested_body_runs);
                    let nested = controller
                        .execute_effect(
                            attempt_envelope(nested_invocation(), "pg-attempt-atomicity-nested"),
                            RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                                nested_body_runs.fetch_add(1, Ordering::SeqCst);
                                Ok(attempt_outcome("pg-attempt-atomicity-nested", "nested"))
                            }),
                        )
                        .await;
                    assert!(
                        nested.is_ok(),
                        "the nested command must execute on the key-addressed tier: {nested:?}"
                    );
                    Ok(attempt_outcome("pg-attempt-atomicity-outer", "outer"))
                }),
            )
            .await
            .expect("recorded attempt completes on the PostgreSQL tier")
    };
    (
        attempt_body_runs.load(Ordering::SeqCst),
        nested_body_runs.load(Ordering::SeqCst),
        projected_output(&outcome),
    )
}

/// The key-addressed tier law: crash after a recorded attempt emitted a nested
/// command, redrive on a fresh host, and both effects replay their recorded
/// terminals without re-entering either body. No ordinal exists, so nothing can
/// shift.
#[tokio::test(flavor = "multi_thread")]
async fn attempt_with_nested_command_redrives_identically_on_the_key_addressed_tier() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL attempt-atomicity law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;

    let first_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect the first PostgreSQL attempt-atomicity host");
    reset(&first_storage).await;
    // First execution runs in normal (non-strict) mode: nothing is recorded yet.
    let first_host = first_storage.effect_host();
    let (attempt_runs, nested_runs, first_outcome) =
        run_attempt_with_nested_command(&first_host).await;
    assert_eq!(
        attempt_runs, 1,
        "the recorded attempt body runs once on first execution"
    );
    assert_eq!(
        nested_runs, 1,
        "the nested command body runs once on first execution"
    );
    assert_eq!(
        first_outcome, "\"outer\"",
        "first execution records the attempt terminal"
    );

    // The crash: drop the first host and its pool entirely, then redrive the
    // identical work on a second, independently-connected host — a different
    // process as far as the effect journal is concerned.
    drop(first_host);
    drop(first_storage);

    let second_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect the redriving PostgreSQL attempt-atomicity host");
    // Strict replay: the redriving host refuses to execute anything it does not
    // find recorded, so a re-executed body would fail loudly rather than pass
    // silently.
    let second_host = second_storage.effect_host();
    second_host.start_replay();
    let (redriven_attempt_runs, redriven_nested_runs, redriven_outcome) =
        run_attempt_with_nested_command(&second_host).await;
    assert_eq!(
        redriven_attempt_runs, 0,
        "redrive replays the recorded attempt terminal without re-entering the body"
    );
    assert_eq!(
        redriven_nested_runs, 0,
        "the nested command replays from its own key without re-executing; a \
         key-addressed journal has no ordinal for it to shift"
    );
    assert_eq!(
        redriven_outcome, "\"outer\"",
        "redrive yields the identical recorded terminal"
    );

    // Each effect owns its own row, keyed independently: that is *why* nesting
    // is safe here rather than an accident of ordering.
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT replay_key FROM lash_runtime_effect_replay
         WHERE scope_id LIKE '%pg-attempt-atomicity%' ORDER BY replay_key",
    )
    .fetch_all(second_storage.pool())
    .await
    .expect("read the PostgreSQL attempt-atomicity effect rows");
    assert_eq!(
        keys,
        vec![ATTEMPT_KEY.to_string(), NESTED_KEY.to_string()],
        "the attempt and its nested command each claimed their own replay key"
    );

    reset(&second_storage).await;
}

/// Journal-first law for the key-addressed tier: the exact command produced by
/// a recorded intent is replayed before any now-live process state can affect
/// the answer.
#[tokio::test(flavor = "multi_thread")]
async fn recorded_intent_command_replays_after_live_terminal_mutation_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL recorded-intent law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let first_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect first PostgreSQL intent host");
    reset(&first_storage).await;
    let identity =
        lash_core::derive_tool_intent_identity(SESSION, TURN, Some("pg-journal-first-call"), 0)
            .expect("literal PostgreSQL intent identity");
    let mut invocation = RuntimeInvocation::effect(
        RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "pg-recorded-intent-start",
        RuntimeEffectKind::Process,
        identity.replay_key.clone(),
    );
    invocation.replay = Some(lash_core::RuntimeReplay {
        key: identity.replay_key.clone(),
        attribution: Some(lash_core::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        )),
    });
    let registration = lash_core::ProcessRegistration::new(
        identity.replay_key.clone(),
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({"source": "postgres-recorded-intent"}),
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::host(),
    );
    let envelope = RuntimeEffectEnvelope::new(
        invocation,
        RuntimeEffectCommand::process(lash_core::ProcessCommand::Start {
            registration,
            observers: vec![SESSION.to_string()],
            env_spec: None,
            execution_context: Box::default(),
        }),
    );
    let frame_hash = envelope.stable_hash().expect("intent command frame hash");
    let registry = Arc::new(first_storage.process_registry());
    let first_host = first_storage.effect_host();
    let first_scoped = first_host
        .scoped(ExecutionScope::turn(SESSION, TURN))
        .expect("scope first PostgreSQL intent host");
    let first = first_scoped
        .controller()
        .execute_effect(envelope.clone(), registry_local_executor(registry.clone()))
        .await
        .expect("execute recorded intent command");
    registry
        .complete_process(
            &identity.replay_key,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("terminal after the recorded drain"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize the recorded intent target");
    drop(registry);
    drop(first_scoped);
    drop(first_host);
    drop(first_storage);

    let second_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect redriving PostgreSQL intent host");
    let second_host = second_storage.effect_host();
    second_host.start_replay();
    let second_scoped = second_host
        .scoped(ExecutionScope::turn(SESSION, TURN))
        .expect("scope redriving PostgreSQL intent host");
    assert_eq!(
        envelope
            .stable_hash()
            .expect("redriven intent command frame hash"),
        frame_hash,
        "the redriven command frame is byte-identical"
    );
    let redriven = second_scoped
        .controller()
        .execute_effect(
            envelope,
            registry_local_executor(Arc::new(second_storage.process_registry())),
        )
        .await
        .expect("replay recorded intent command after live mutation");
    assert_eq!(
        serde_json::to_vec(&redriven).expect("serialize redriven intent outcome"),
        serde_json::to_vec(&first).expect("serialize first intent outcome"),
        "the key-addressed recorded outcome is byte-identical after live terminal mutation"
    );

    reset(&second_storage).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn public_provider_signal_intent_wakes_and_redrives_byte_identically_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL public signal-intent law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect PostgreSQL public signal-intent host");
    reset(&storage).await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(storage.process_registry());
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "pg-public-intent-target",
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
            &[SESSION.to_string()],
        )
        .await
        .expect("register PostgreSQL public signal target");

    let first_host = Arc::new(storage.effect_host());
    let wait_controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
        storage.runtime_effect_controller(ExecutionScope::process("pg-public-intent-target")),
    );
    let wake_key = wait_controller
        .await_event_key(
            &ExecutionScope::process("pg-public-intent-target"),
            lash_core::AwaitEventWaitIdentity::process_signal(
                "pg-public-intent-target",
                "resume",
                1,
            ),
        )
        .await
        .expect("mint PostgreSQL process-signal wait");
    let wake = {
        let wait_controller = Arc::clone(&wait_controller);
        let wake_key = wake_key.clone();
        tokio::spawn(async move {
            wait_controller
                .await_await_event(&wake_key, tokio_util::sync::CancellationToken::new(), None)
                .await
        })
    };
    tokio::task::yield_now().await;

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let signal_crossing_frames = Arc::new(Mutex::new(Vec::new()));
    let mut first = public_signal_runtime(
        first_host.clone(),
        Arc::clone(&registry),
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::Signal,
    )
    .await;
    let first_scope = postgres_public_turn_scope(&storage, Arc::clone(&signal_crossing_frames));
    let first_turn = first
        .stream_turn(
            public_runtime_input(),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                first_scope,
            ),
        )
        .await
        .expect("run first PostgreSQL public signal-intent turn");
    assert!(matches!(
        first_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), wake)
            .await
            .expect("PostgreSQL SignalProcess intent must wake the parked wait")
            .expect("PostgreSQL wake task")
            .expect("PostgreSQL wake resolution"),
        lash_core::Resolution::Ok(serde_json::json!({
            "source": "postgres-public-caller"
        }))
    );
    let first_signal_frames: Vec<String> = sqlx::query_scalar(
        "SELECT envelope_json FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND replay_key LIKE '%process:signal:%'
         ORDER BY replay_key",
    )
    .bind(SESSION)
    .fetch_all(storage.pool())
    .await
    .expect("read first PostgreSQL signal command frames");
    assert_eq!(
        first_signal_frames.len(),
        1,
        "the provider/coordinator path emits one literal signal command"
    );
    let first_crossing_frame = signal_crossing_frames
        .lock()
        .expect("first signal crossing frame lock")
        .first()
        .cloned()
        .expect("the live provider/coordinator path crosses one signal command");

    registry
        .complete_process(
            "pg-public-intent-target",
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("terminal after public intent drain"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize PostgreSQL signal target before redrive");
    drop(first);

    let replay_host = Arc::new(storage.effect_host());
    replay_host.start_replay();
    let mut replay = public_signal_runtime(
        replay_host.clone(),
        Arc::clone(&registry),
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::Signal,
    )
    .await;
    let replay_scope = postgres_public_turn_scope(&storage, Arc::clone(&signal_crossing_frames));
    let replay_turn = replay
        .stream_turn(
            public_runtime_input(),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                replay_scope,
            ),
        )
        .await
        .expect("redrive PostgreSQL public signal-intent turn");
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    let replay_signal_frames: Vec<String> = sqlx::query_scalar(
        "SELECT envelope_json FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND replay_key LIKE '%process:signal:%'
         ORDER BY replay_key",
    )
    .bind(SESSION)
    .fetch_all(storage.pool())
    .await
    .expect("read redriven PostgreSQL signal command frames");
    assert_eq!(
        replay_signal_frames, first_signal_frames,
        "the public caller reconstructs byte-identical signal frames after live terminal mutation"
    );
    {
        let crossing_frames = signal_crossing_frames
            .lock()
            .expect("redriven signal crossing frame lock");
        assert_eq!(
            crossing_frames.len(),
            2,
            "the production signal command must cross once live and once on redrive"
        );
        assert_eq!(
            crossing_frames[1], first_crossing_frame,
            "the redriven production signal command frame must be byte-identical"
        );
    }

    reset(&storage).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn public_provider_parent_end_cancel_survives_crash_after_tool_batch_on_postgres() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL public parent-end law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect PostgreSQL public parent-end host");
    reset(&storage).await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(storage.process_registry());
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "pg-public-intent-target",
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
            &[SESSION.to_string()],
        )
        .await
        .expect("register PostgreSQL parent-end signal target");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let effect_host = Arc::new(storage.effect_host());
    let mut first = public_signal_runtime(
        effect_host.clone(),
        Arc::clone(&registry),
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::ParentEnd,
    )
    .await;
    first.set_turn_phase_probe(Arc::new(PanicAtParentEnd));
    let first_scope = postgres_public_turn_scope(&storage, Arc::new(Mutex::new(Vec::new())));
    let crashed = tokio::spawn(async move {
        first
            .stream_turn(
                public_runtime_input(),
                lash_core::facade_support::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    first_scope,
                ),
            )
            .await
    })
    .await
    .expect_err("the phase probe crashes after ToolBatch commit");
    assert!(crashed.is_panic());
    let before_parent_end: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND replay_key LIKE '%:parent-end:%'",
    )
    .bind(SESSION)
    .fetch_one(storage.pool())
    .await
    .expect("count parent-end frames before redrive");
    assert_eq!(
        before_parent_end, 0,
        "the injected crash lands before the parent-end command"
    );
    let committed_batches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_runtime_effect_replay
         WHERE session_id = $1
           AND replay_key ~ ':tool_batch:[0-9]+$'
           AND outcome_json LIKE '%parent_end%'",
    )
    .bind(SESSION)
    .fetch_one(storage.pool())
    .await
    .expect("count committed ToolBatch parent-end evidence");
    assert_eq!(
        committed_batches, 1,
        "the durable ToolBatch outcome commits the parent-end metadata before the crash"
    );

    let replay_host = Arc::new(storage.effect_host());
    let mut replay = public_signal_runtime(
        replay_host.clone(),
        Arc::clone(&registry),
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::ParentEnd,
    )
    .await;
    let replay_scope = postgres_public_turn_scope(&storage, Arc::new(Mutex::new(Vec::new())));
    let redriven = replay
        .stream_turn(
            public_runtime_input(),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                replay_scope,
            ),
        )
        .await
        .expect("redrive PostgreSQL parent-end turn");
    assert!(matches!(
        redriven.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
    let processes = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("list PostgreSQL public parent-end processes");
    let child = processes
        .iter()
        .find(|record| {
            matches!(
                record.input.as_ref(),
                lash_core::ProcessInput::External { metadata }
                    if metadata == &serde_json::json!({"source": "parent-end"})
            )
        })
        .expect("find the parent-end child reconstructed from ToolBatch outcome");
    let cancel_events = registry
        .events_after(&child.id, 0)
        .await
        .expect("read redriven parent-end cancellation")
        .into_iter()
        .filter(|event| event.event_type == "process.cancel_requested")
        .count();
    assert_eq!(
        cancel_events, 1,
        "redrive applies the recorded Cancel policy exactly once"
    );
    let after_parent_end: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_runtime_effect_replay
         WHERE session_id = $1 AND replay_key LIKE '%:parent-end:%'",
    )
    .bind(SESSION)
    .fetch_one(storage.pool())
    .await
    .expect("count parent-end frames after redrive");
    assert_eq!(after_parent_end, 1);

    reset(&storage).await;
}

#[path = "attempt_atomicity/rerunnable_signal.rs"]
mod rerunnable_signal;

#[path = "attempt_atomicity/host_ingress.rs"]
mod host_ingress;
