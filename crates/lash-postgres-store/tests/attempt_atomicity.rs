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

use lash_core_execution::ProcessLifecycle as _;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::sync::Arc;

fn registry_local_executor(
    registry: Arc<dyn lash_core_execution::ProcessRegistry>,
) -> lash_core_execution::RuntimeEffectLocalExecutor<'static> {
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry),
    ));
    lash_core_execution::RuntimeEffectLocalExecutor::processes(registry, process_work)
}
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core_execution::{
    EffectHost, ExecutionScope, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_postgres_store::{PostgresEffectHost, PostgresStorage};

// Keep subsequent lines stable for machine-checked public API evidence anchors.
// Shared test support now lives at the grouped integration-harness root.
use crate::support::{SharedDatabaseLock, database_url};

const SESSION: &str = "pg-attempt-atomicity-session";
const TURN: &str = "pg-attempt-atomicity-turn";
const ATTEMPT_KEY: &str = "pg-attempt-atomicity:attempt";
const NESTED_KEY: &str = "pg-attempt-atomicity:attempt:nested";

struct CrossingController {
    inner: Arc<dyn lash_core_execution::RuntimeEffectController>,
    signal_frames: Arc<Mutex<Vec<Vec<u8>>>>,
    crash_after: Option<CrashAfter>,
    fired: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Copy)]
enum CrashAfter {
    /// After the durable `spawn_agent` child start, before its await.
    SpawnAgentStart,
    /// After the protocol batch's first child attempt (`alpha`) commits.
    FirstProtocolBatchChild,
    /// After the fault batch's failing child attempt (`fail`) commits.
    FailingProtocolBatchChild,
}

/// A PostgreSQL host whose every scoped controller — the turn's and each
/// group child's — is a [`CrossingController`]: it captures signal crossings
/// into `signal_frames` and, when `crash_after` names a boundary, parks the
/// host task after that commit. A group child's commands cross the host's
/// bound child controller, so the crossing is observed here rather than on
/// the turn's controller alone (FIG-3397).
struct CrossingEffectHost {
    inner: Arc<dyn EffectHost>,
    crash_after: Option<CrashAfter>,
    fired: Arc<std::sync::atomic::AtomicBool>,
    signal_frames: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl lash_core_execution::AwaitEventResolver for CrossingEffectHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.inner.turn_control_binding_id())
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core_execution::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core_execution::CompletionKeyPreparation, lash_core_execution::RuntimeError>
    {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core_execution::AwaitEventWaitIdentity,
    ) -> Result<lash_core_execution::AwaitEventKey, lash_core_execution::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
        resolution: lash_core_execution::Resolution,
    ) -> Result<lash_core_execution::ResolveOutcome, lash_core_execution::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
    ) -> Result<Option<lash_core_execution::Resolution>, lash_core_execution::RuntimeError> {
        self.inner.peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core_execution::Resolution, lash_core_execution::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }
    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }
    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl EffectHost for CrossingEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    fn await_event_resolver(&self) -> &dyn lash_core_execution::AwaitEventResolver {
        self
    }

    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a lash_core_execution::ScopedEffectController<'_>,
    ) -> Result<lash_core_execution::TurnControlBinding<'a>, lash_core_execution::RuntimeError>
    {
        self.inner.turn_control_binding(scoped).await
    }

    async fn prepare_tool_intent(
        &self,
        sink: &dyn lash_core_execution::ToolIntentOutcomeSink,
        identity: &lash_core_execution::ToolIntentIdentity,
        intent: lash_core_execution::ToolIntent,
    ) -> Result<lash_core_execution::ToolIntentPreparation, lash_core_execution::RuntimeError> {
        self.inner.prepare_tool_intent(sink, identity, intent).await
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core_execution::ToolIntentOutcomeSink,
        identity: &lash_core_execution::ToolIntentIdentity,
        submitted: lash_core_execution::ToolIntent,
        outcome: lash_core_execution::ToolIntentExecutionOutcome,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.inner
            .record_tool_intent_outcome(sink, identity, submitted, outcome)
            .await
    }

    fn scoped<'run>(
        &'run self,
        admitted: lash_core_execution::AdmittedScope,
    ) -> Result<lash_core_execution::ScopedEffectController<'run>, lash_core_execution::RuntimeError>
    {
        let inner = self
            .inner
            .scoped_static(admitted.clone())?
            .expect("PostgreSQL exposes static scopes");
        lash_core_execution::ScopedEffectController::shared(
            Arc::new(CrossingController {
                inner: Arc::new(ScopedControllerAdapter(inner)),
                signal_frames: Arc::clone(&self.signal_frames),
                crash_after: self.crash_after,
                fired: Arc::clone(&self.fired),
            }),
            admitted,
        )
    }
    fn scoped_static(
        &self,
        admitted: lash_core_execution::AdmittedScope,
    ) -> Result<
        Option<lash_core_execution::ScopedEffectController<'static>>,
        lash_core_execution::RuntimeError,
    > {
        let inner = self
            .inner
            .scoped_static(admitted.clone())?
            .expect("PostgreSQL exposes static scopes");
        lash_core_execution::ScopedEffectController::shared(
            Arc::new(CrossingController {
                inner: Arc::new(ScopedControllerAdapter(inner)),
                signal_frames: Arc::clone(&self.signal_frames),
                crash_after: self.crash_after,
                fired: Arc::clone(&self.fired),
            }),
            admitted,
        )
        .map(Some)
    }

    /// The tool-child resolver registers on the PostgreSQL host's driver,
    /// where this wrapper's group operations land.
    fn install_tool_child_host(
        &self,
        candidate: Arc<lash_core_execution::facade_support::ToolChildHost>,
    ) -> Option<Arc<lash_core_execution::facade_support::ToolChildHost>> {
        self.inner.install_tool_child_host(candidate)
    }

    fn scoped_for_group_child(
        &self,
        admitted: lash_core_execution::AdmittedScope,
        binding: lash_core_execution::GroupChildBinding,
    ) -> Result<
        Option<lash_core_execution::ScopedEffectController<'static>>,
        lash_core_execution::RuntimeError,
    > {
        let inner = self
            .inner
            .scoped_for_group_child(admitted.clone(), binding)?
            .expect("PostgreSQL exposes static scopes");
        lash_core_execution::ScopedEffectController::shared(
            Arc::new(CrossingController {
                inner: Arc::new(ScopedControllerAdapter(inner)),
                signal_frames: Arc::clone(&self.signal_frames),
                crash_after: self.crash_after,
                fired: Arc::clone(&self.fired),
            }),
            admitted,
        )
        .map(Some)
    }
}

struct ScopedControllerAdapter(lash_core_execution::ScopedEffectController<'static>);

#[async_trait::async_trait]
impl lash_core_execution::AwaitEventResolver for ScopedControllerAdapter {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.0.controller().await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core_execution::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core_execution::CompletionKeyPreparation, lash_core_execution::RuntimeError>
    {
        self.0
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core_execution::AwaitEventWaitIdentity,
    ) -> Result<lash_core_execution::AwaitEventKey, lash_core_execution::RuntimeError> {
        self.0.controller().await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
        resolution: lash_core_execution::Resolution,
    ) -> Result<lash_core_execution::ResolveOutcome, lash_core_execution::RuntimeError> {
        self.0
            .controller()
            .resolve_await_event(key, resolution)
            .await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
    ) -> Result<Option<lash_core_execution::Resolution>, lash_core_execution::RuntimeError> {
        self.0.controller().peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core_execution::Resolution, lash_core_execution::RuntimeError> {
        self.0
            .controller()
            .await_await_event(key, cancel, deadline)
            .await
    }
    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.0
            .controller()
            .revoke_await_events_for_session(session_id)
            .await
    }
    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.0
            .controller()
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::RuntimeEffectController for ScopedControllerAdapter {
    async fn drive_independent_effect_work<'work>(
        &self,
        work: Vec<lash_core_execution::IndependentEffectWork<'work>>,
    ) {
        self.0
            .controller()
            .drive_independent_effect_work(work)
            .await;
    }
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core_execution::RuntimeEffectControllerError> {
        self.0
            .controller()
            .execute_effect(envelope, local_executor)
            .await
    }

    async fn open_effect_group(
        &self,
        group: lash_core_execution::RuntimeEffectGroup,
    ) -> Result<
        lash_core_execution::EffectGroupHandle,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.0.controller().open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core_execution::GroupExecutors>,
    ) -> Result<(), lash_core_execution::RuntimeEffectControllerError> {
        self.0.controller().register_group_executors(executors)
    }

    fn group_child_scoped_controller(
        &self,
        admitted: lash_core_execution::AdmittedScope,
        binding: lash_core_execution::GroupChildBinding,
    ) -> Result<
        Option<lash_core_execution::ScopedEffectController<'static>>,
        lash_core_execution::RuntimeError,
    > {
        self.0
            .controller()
            .group_child_scoped_controller(admitted, binding)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core_execution::EffectGroupHandle,
        cancel: lash_core_execution::CancellationToken,
    ) -> Result<
        lash_core_execution::GroupSettlement,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.0
            .controller()
            .await_next_settlement(handle, cancel)
            .await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core_execution::EffectGroupHandle,
        disposition: lash_core_execution::LoserPolicy,
    ) -> Result<(), lash_core_execution::RuntimeEffectControllerError> {
        self.0
            .controller()
            .close_effect_group(handle, disposition)
            .await
    }
    async fn commit_group_child_final(
        &self,
        commit: lash_core_execution::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core_execution::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.0.controller().commit_group_child_final(commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), lash_core_execution::RuntimeEffectControllerError> {
        self.0
            .controller()
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::AwaitEventResolver for CrossingController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core_execution::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core_execution::CompletionKeyPreparation, lash_core_execution::RuntimeError>
    {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core_execution::AwaitEventWaitIdentity,
    ) -> Result<lash_core_execution::AwaitEventKey, lash_core_execution::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
        resolution: lash_core_execution::Resolution,
    ) -> Result<lash_core_execution::ResolveOutcome, lash_core_execution::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
    ) -> Result<Option<lash_core_execution::Resolution>, lash_core_execution::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core_execution::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core_execution::Resolution, lash_core_execution::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core_execution::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::RuntimeEffectController for CrossingController {
    async fn drive_independent_effect_work<'work>(
        &self,
        work: Vec<lash_core_execution::IndependentEffectWork<'work>>,
    ) {
        self.inner.drive_independent_effect_work(work).await;
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core_execution::RuntimeEffectControllerError> {
        let crash_here = match self.crash_after {
            Some(CrashAfter::SpawnAgentStart) => matches!(
                &envelope.command,
                RuntimeEffectCommand::Process { command }
                    if matches!(
                        command.as_ref(),
                        lash_core_execution::ProcessCommand::Start { registration, .. }
                            if registration.id == "process:subagent:fig1293-spawn-agent"
                    )
            ),
            Some(CrashAfter::FirstProtocolBatchChild) => matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "fig1293_echo"
                        && call.args.get("value") == Some(&serde_json::json!("alpha"))
            ),
            Some(CrashAfter::FailingProtocolBatchChild) => matches!(
                &envelope.command,
                RuntimeEffectCommand::ToolAttempt { call, .. }
                    if call.tool_name == "fig1293_echo"
                        && call.args.get("value") == Some(&serde_json::json!("fail"))
            ),
            None => false,
        };
        if matches!(
            &envelope.command,
            RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), lash_core_execution::ProcessCommand::Signal { .. })
        ) {
            self.signal_frames
                .lock()
                .expect("signal crossing frame lock")
                .push(serde_json::to_vec(&envelope).expect("serialize signal crossing frame"));
        }
        let outcome = self.inner.execute_effect(envelope, local_executor).await;
        if crash_here && outcome.is_ok() && !self.fired.swap(true, Ordering::SeqCst) {
            std::future::pending::<()>().await;
            unreachable!("the host task is aborted after the selected child commit")
        }
        outcome
    }

    async fn open_effect_group(
        &self,
        group: lash_core_execution::RuntimeEffectGroup,
    ) -> Result<
        lash_core_execution::EffectGroupHandle,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.inner.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: std::sync::Arc<dyn lash_core_execution::GroupExecutors>,
    ) -> Result<(), lash_core_execution::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    /// A group child's commands are still this turn's crossings: its bound
    /// controller is wrapped so its attempts reach the crash boundaries and
    /// its signal commands land in the same captured frames.
    fn group_child_scoped_controller(
        &self,
        admitted: lash_core_execution::AdmittedScope,
        binding: lash_core_execution::GroupChildBinding,
    ) -> Result<
        Option<lash_core_execution::ScopedEffectController<'static>>,
        lash_core_execution::RuntimeError,
    > {
        let Some(bound) = self
            .inner
            .group_child_scoped_controller(admitted, binding)?
        else {
            return Ok(None);
        };
        let admitted = bound.admitted_scope().clone();
        lash_core_execution::ScopedEffectController::shared(
            Arc::new(CrossingController {
                inner: Arc::new(ScopedControllerAdapter(bound)),
                signal_frames: Arc::clone(&self.signal_frames),
                crash_after: self.crash_after,
                fired: Arc::clone(&self.fired),
            }),
            admitted,
        )
        .map(Some)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core_execution::EffectGroupHandle,
        cancel: lash_core_execution::CancellationToken,
    ) -> Result<
        lash_core_execution::GroupSettlement,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.inner.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core_execution::runtime::effect::RankedGroupSettlement>,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core_execution::EffectGroupHandle,
        disposition: lash_core_execution::LoserPolicy,
    ) -> Result<(), lash_core_execution::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }
    async fn commit_group_child_final(
        &self,
        commit: lash_core_execution::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core_execution::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core_execution::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), lash_core_execution::RuntimeEffectControllerError> {
        self.inner
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
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

fn public_signal_tool() -> lash_core_execution::ToolDefinition {
    lash_core_execution::ToolDefinition::raw(
        "tool:pg_public_signal_intent",
        "pg_public_signal_intent",
        "Signal a process through the recorded intent protocol.",
        lash_core_execution::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl lash_core_execution::ToolProvider for PublicSignalIntentProvider {
    fn tool_manifests(&self) -> Vec<lash_core_execution::ToolManifest> {
        vec![public_signal_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core_execution::ToolContract>> {
        (name == "pg_public_signal_intent").then(|| Arc::new(public_signal_tool().contract()))
    }

    async fn execute(
        &self,
        call: lash_core_execution::ToolCall<'_>,
    ) -> lash_core_execution::ToolAttemptOutcome {
        let parent_scope = call
            .context
            .child_process_parent_scope()
            .expect("recorded attempt carries its parent scope");

        self.calls.fetch_add(1, Ordering::SeqCst);
        let intent = match self.kind {
            PublicIntentKind::Signal => lash_core_execution::ToolIntent::SignalProcess(
                lash_core_execution::SignalProcessIntent {
                    session_id: lash_core_execution::SessionId::from(call.context.session_id()),
                    process_id: ProcessId::from("pg-public-intent-target"),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"source": "postgres-public-caller"}),
                },
            ),
            PublicIntentKind::ParentEnd => lash_core_execution::ToolIntent::StartProcess(Box::new(
                lash_core_execution::StartProcessIntent {
                    session_id: lash_core_execution::SessionId::from(call.context.session_id()),
                    declaration: lash_core_execution::ProcessStartDeclaration::external(
                        lash_core_execution::ProcessOriginator::session(
                            lash_core_execution::SessionScope::new(
                                lash_core_execution::SessionId::from(call.context.session_id()),
                            ),
                        ),
                        serde_json::json!({"source": "parent-end"}),
                        lash_core_execution::ProcessLifecyclePolicy::new(
                            parent_scope.clone(),
                            lash_core_execution::OnParentEnd::Cancel,
                        ),
                    ),
                },
            )),
        };
        lash_core_execution::ToolAttemptOutcome::done(
            lash_core_execution::ToolOutcomeDone::ok(serde_json::json!({"signal": "recorded"})),
            lash_core_execution::ToolIntents::v3(vec![intent]),
        )
    }
}

struct PanicAtParentEnd;

impl lash_core_execution::runtime::RuntimeTurnPhaseProbe for PanicAtParentEnd {
    fn begin(&self, _phase: lash_core_execution::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core_execution::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "turn.parent_end" {
            panic!("injected crash after the turn commit and before the parent-end ledger row");
        }
    }
}

/// Crash between the tool group's settlement and the turn's own final commit:
/// the admission phase runs immediately before the commit the turn is redriven
/// for.
struct PanicBeforeTurnCommit;

impl lash_core_execution::runtime::RuntimeTurnPhaseProbe for PanicBeforeTurnCommit {
    fn begin(&self, _phase: lash_core_execution::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core_execution::runtime::RuntimeTurnPhase) {
        if phase == lash_core_execution::runtime::RuntimeTurnPhase::EffectLoop {
            panic!("injected crash after the tool group settled and before the turn commit");
        }
    }

    fn begin_named(&self, _phase: &str) {}
}

fn public_runtime_policy() -> lash_core_execution::SessionPolicy {
    let mut policy = lash_core_execution::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from(SESSION.to_string()));
    policy
}

fn public_runtime_state(
    policy: &lash_core_execution::SessionPolicy,
) -> lash_core_execution::RuntimeSessionState {
    lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from(SESSION.to_string()),
        policy: policy.clone(),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    }
}

fn public_runtime_input() -> lash_core_execution::TurnInput {
    let mut input = lash_core_execution::TurnInput::text("run PostgreSQL signal intent");
    input.trace_turn_id = Some(TurnId::from(TURN.to_string()));
    input
}

/// The public turn's scope, admitted on the host the runtime runs on: the
/// turn's tool calls open effect groups, and a group's children route through
/// the executors that host registered, so a controller minted on a separate
/// driver would refuse the open (`EffectGroupUnsupported`, ADR 0099 §3).
fn postgres_public_turn_scope(
    effect_host: &dyn EffectHost,
    signal_frames: Arc<Mutex<Vec<Vec<u8>>>>,
) -> lash_core_execution::ScopedEffectController<'static> {
    let scope = lash_core_execution::AdmittedScope::turn(SESSION, TURN);
    let inner: Arc<dyn lash_core_execution::RuntimeEffectController> = effect_host
        .scoped_static(scope.clone())
        .expect("scope PostgreSQL public turn on its host")
        .expect("the PostgreSQL host lends a 'static controller")
        .owned_controller()
        .expect("the PostgreSQL host's scoped controller is shared");
    lash_core_execution::ScopedEffectController::shared(
        Arc::new(CrossingController {
            inner,
            signal_frames,
            crash_after: None,
            fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        scope,
    )
    .expect("scope PostgreSQL public turn")
}

/// The one backend a law's runtime runs over: `storage`'s PostgreSQL ports,
/// with `effect_host` (a host over the same storage, possibly under a test
/// layer) and `registry` (the law's handle on the same storage's registry) in
/// place of the backend's own.
///
/// These laws certify the PostgreSQL effect journal and registry. The session
/// history a law runtime and its worker's child runtimes commit is outside what
/// they check, so each law runtime keeps it in a detached session catalog of
/// its own (see [`detached_session_store`]). A law "crashes" a host by
/// aborting its turn task while that host's child runtimes live on, so a
/// catalog shared across hosts would leave the redrive waiting on a child
/// session lease the crashed host still renews.
async fn pg_law_backend(
    storage: &PostgresStorage,
    effect_host: Arc<dyn EffectHost>,
    registry: Arc<dyn lash_core_execution::ProcessRegistry>,
) -> Arc<dyn lash_core_execution::Backend> {
    let detached = held_memory_backend().await;
    // The laws write no attachment; the backend's attachment port is the
    // detached memory backend's byte store.
    lash_core::testing::runtime_helpers::LayeredBackend::over(Arc::new(
        lash_postgres_store::PostgresBackend::new(
            storage,
            lash_core_execution::Backend::attachment_store(&detached),
        ),
    ))
    .map_effect_host(|_| effect_host)
    .map_process_registry(|_| registry)
    .map_session_store_factory(|_| lash_core_execution::Backend::session_store_factory(&detached))
    .into_backend()
}

/// A fresh SQLite memory backend, held for the life of the test binary: its
/// named in-memory databases live while any handle does, and the stores it
/// hands out reach sibling databases by name.
async fn held_memory_backend() -> lash_sqlite_store::SqliteBackend {
    static HELD: std::sync::Mutex<Vec<lash_sqlite_store::SqliteBackend>> =
        std::sync::Mutex::new(Vec::new());
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(backend.clone());
    backend
}

/// The law runtime's session store. These laws certify the PostgreSQL effect
/// journal and registry; the session history the runtime commits is outside
/// what they check, so it lives in a detached SQLite memory store rather than
/// in PostgreSQL session rows a rerun would inherit.
async fn detached_session_store() -> Arc<dyn lash_core_execution::RuntimePersistence> {
    Arc::new(
        held_memory_backend()
            .await
            .open_store()
            .await
            .expect("open a detached session store"),
    )
}

async fn public_signal_runtime(
    backend: Arc<dyn lash_core_execution::Backend>,
    provider_calls: Arc<AtomicUsize>,
    model_calls: Arc<AtomicUsize>,
    kind: PublicIntentKind,
) -> lash_core::facade_support::LashRuntime {
    let tool_provider: Arc<dyn lash_core_execution::ToolProvider> =
        Arc::new(PublicSignalIntentProvider {
            calls: provider_calls,
            kind,
        });
    let tool_plugin: Arc<dyn lash_core_execution::facade_support::PluginFactory> =
        Arc::new(lash_core_execution::plugin::StaticPluginFactory::new(
            "pg-public-signal-intent",
            lash_core_execution::facade_support::PluginSpec::new()
                .with_tool_provider(tool_provider),
        ));
    let model = lash_core_execution::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_| {
            let model_calls = Arc::clone(&model_calls);
            async move {
                Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => lash_core_execution::LlmResponse {
                        parts: vec![lash_core_execution::LlmOutputPart::ToolCall {
                            call_id: "pg-public-signal-call".to_string(),
                            tool_name: "pg_public_signal_intent".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core_execution::LlmResponse::default()
                    },
                    1 => lash_core_execution::LlmResponse {
                        parts: vec![lash_core_execution::LlmOutputPart::Text {
                            text: "signal intent complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core_execution::LlmResponse::default()
                    },
                    index => panic!("unexpected PostgreSQL model call {index}"),
                })
            }
        })
        .build()
        .into_handle();
    let registry = backend.process_registry();
    let mut host = lash_core_execution::facade_support::RuntimeHostConfig::new(
        backend,
        lash_core_execution::CommitBudget::bounded(1024 * 1024, 512),
        lash_core_execution::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver =
        Arc::new(lash_core_execution::facade_support::SingleProviderResolver::new(model));
    let policy = public_runtime_policy();
    let store = detached_session_store().await;
    let watched = lash_core_execution::facade_support::watch_process_registry(registry);
    let registry = Arc::clone(watched.registry());
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core_execution::testing::runtime_lease_owner(),
        )
        .with_session_id(SESSION)
        .with_policy(policy.clone())
        .with_initial_state(public_runtime_state(&policy))
        .with_plugin_factories(
            lash_core_execution::testing::test_standard_protocol_factories()
                .into_iter()
                .chain([tool_plugin])
                .collect(),
        )
        .with_store(store)
        .with_process_work(lash_core_execution::ProcessWorkWiring::new(
            watched,
            Arc::new(lash_core_execution::NativeProcessWork::for_registry(
                registry,
            )),
        ))
        .with_queued_work(Arc::new(lash_core_execution::NoQueuedWork::new()))
        .build(),
    )
    .await
    .expect("build PostgreSQL public-caller runtime")
}

fn attempt_invocation() -> lash_core_execution::RuntimeEffectInvocation {
    lash_core_execution::RuntimeEffectInvocation::new(
        lash_core_execution::EffectAddress::new(
            lash_core_execution::ExecutionScope::turn(SESSION, TURN),
            ATTEMPT_KEY,
        )
        .expect("valid attempt address"),
        lash_core_execution::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        "pg-attempt-atomicity-attempt",
    )
}

/// The nested effect the attempt body emits. It carries its own replay key,
/// derived from the attempt's key exactly as `process_effect_invocation` derives
/// a nested process command's key in production.
fn nested_invocation() -> lash_core_execution::RuntimeEffectInvocation {
    lash_core_execution::RuntimeEffectInvocation::new(
        lash_core_execution::EffectAddress::new(
            lash_core_execution::ExecutionScope::turn(SESSION, TURN),
            NESTED_KEY,
        )
        .expect("valid nested attempt address"),
        lash_core_execution::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        "pg-attempt-atomicity-nested",
    )
}

/// A recorded `ToolAttempt` — the unit whose body must not be re-entered on
/// redrive. Both the outer attempt and the nested command it emits are journaled
/// as attempts here so each one's body execution is observable.
fn attempt_envelope(
    invocation: lash_core_execution::RuntimeEffectInvocation,
    call_id: &str,
) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        invocation,
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core_execution::PreparedToolCall {
                call_id: call_id.to_string(),
                tool_id: lash_core_execution::ToolId::from("tool:pg_attempt_atomicity".to_string()),
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
        launch: Box::new(lash_core_execution::ToolAttemptLaunch::Done {
            record: Box::new(lash_core_execution::ToolCallRecord {
                call_id: Some(call_id.to_string()),
                tool: "pg_attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                output: lash_core_execution::ToolCallOutput::success(serde_json::json!(value)),
                duration_ms: 0,
            }),
            intents: lash_core_execution::ToolIntents::v3(vec![
                lash_core_execution::ToolIntent::StartProcess(Box::new(
                    lash_core_execution::StartProcessIntent {
                        session_id: SessionId::from(SESSION.to_string()),
                        declaration: lash_core_execution::ProcessStartDeclaration::external(
                            lash_core_execution::ProcessOriginator::host_scoped(
                                "pg-attempt-atomicity",
                            ),
                            serde_json::json!({"value": value}),
                            lash_core_execution::ProcessLifecyclePolicy::new(
                                lash_core_execution::ParentScope::Host,
                                lash_core_execution::OnParentEnd::Abandon,
                            ),
                        ),
                    },
                )),
            ]),
        }),
        triggers: Vec::new(),
        capture: None,
    }
}

fn projected_output(outcome: &RuntimeEffectOutcome) -> String {
    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("expected a tool-attempt outcome");
    };
    let lash_core_execution::ToolAttemptLaunch::Done { record, .. } = launch.as_ref() else {
        panic!("expected a completed tool attempt");
    };
    record.output.value_for_projection().to_string()
}

async fn reset(storage: &PostgresStorage) {
    for statement in [
        "DELETE FROM lash_runtime_effect_replay WHERE scope_id LIKE '%pg-attempt-atomicity%'",
        // Every public turn opens its tool calls as an effect group keyed by
        // the same SESSION/TURN scope and call id, so a sibling's retained
        // group would otherwise be reopened by the next test's turn.
        "DELETE FROM lash_runtime_effect_group_child WHERE group_key IN (
             SELECT group_key FROM lash_runtime_effect_group
             WHERE scope_id LIKE '%pg-attempt-atomicity%')",
        "DELETE FROM lash_runtime_effect_group WHERE scope_id LIKE '%pg-attempt-atomicity%'",
        "DELETE FROM lash_processes WHERE process_id = 'pg-public-intent-target' OR record_json LIKE '%pg-public-caller%'",
        // Every public turn in this binary ends the same SESSION/TURN scope, so
        // a sibling's turn-exit ledger row would otherwise be waiting for the
        // recovery test that asserts the crash preempted the write.
        "DELETE FROM lash_parent_end_plans WHERE parent_kind = 'turn' AND parent_id LIKE '%pg-attempt-atomicity%'",
    ] {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset the PostgreSQL attempt-atomicity effect rows");
    }
}

/// Runs the hazard shape on one host: a recorded attempt whose body emits a
/// nested journal command through the *same* controller.
async fn run_attempt_with_nested_command(host: &PostgresEffectHost) -> (usize, usize, String) {
    let scoped = host
        .scoped(lash_core_execution::AdmittedScope::turn(SESSION, TURN))
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
    let identity = lash_core_execution::derive_tool_intent_identity(
        &SessionId::from(SESSION),
        TURN,
        Some("pg-journal-first-call"),
        0,
    )
    .expect("literal PostgreSQL intent identity");
    let invocation = lash_core_execution::RuntimeEffectInvocation::new(
        lash_core_execution::EffectAddress::new(
            lash_core_execution::ExecutionScope::turn(SESSION, TURN),
            identity.replay_key.clone(),
        )
        .expect("valid recorded intent address"),
        lash_core_execution::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        "pg-recorded-intent-start",
    )
    .with_replay_attribution(lash_core_execution::RuntimeReplayAttribution::ToolIntent(
        identity.clone(),
    ));
    let registration = lash_core_execution::ProcessRegistration::new(
        identity.replay_key.clone(),
        lash_core_execution::ProcessInput::External {
            metadata: serde_json::json!({"source": "postgres-recorded-intent"}),
        },
        lash_core_execution::RecoveryContract::ExternallyOwned,
        lash_core_execution::ProcessProvenance::host(),
        lash_core_execution::ProcessLifecyclePolicy::new(
            lash_core_execution::ParentScope::Host,
            lash_core_execution::OnParentEnd::Abandon,
        ),
    );
    let envelope = RuntimeEffectEnvelope::new(
        invocation,
        RuntimeEffectCommand::process(lash_core_execution::ProcessCommand::Start {
            registration,
            observers: vec![SessionId::from(SESSION.to_string())],
            env_spec: None,
            execution_context: Box::default(),
        }),
    );
    let frame_hash = envelope.stable_hash().expect("intent command frame hash");
    let registry = Arc::new(first_storage.process_registry());
    let first_host = first_storage.effect_host();
    let first_scoped = first_host
        .scoped(lash_core_execution::AdmittedScope::turn(SESSION, TURN))
        .expect("scope first PostgreSQL intent host");
    let first = first_scoped
        .controller()
        .execute_effect(envelope.clone(), registry_local_executor(registry.clone()))
        .await
        .expect("execute recorded intent command");
    registry
        .complete_process(
            &ProcessId::from(identity.replay_key),
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::json!(
                    "terminal after the recorded drain"
                )),
            ),
            lash_core_execution::ProcessCompletionAuthority::external_owner(),
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
        .scoped(lash_core_execution::AdmittedScope::turn(SESSION, TURN))
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

/// The redrive runs on a fresh runtime over a fresh in-memory session store:
/// only the PostgreSQL effect journal survives. Every journaled step replays
/// byte-identically, and the signal command crosses once. The commit then
/// cedes with `accepted_turn_input_ceded`, because the journaled initial drive
/// set claims a row this session store never held (ADR 0069 §6, FIG-3532). A
/// redrive never re-admits the accepted input to answer it a second time.
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
    let registry: Arc<dyn lash_core_execution::ProcessRegistry> =
        Arc::new(storage.process_registry());
    registry
        .register_process_with_observers(
            lash_core_execution::ProcessRegistration::new(
                "pg-public-intent-target",
                lash_core_execution::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core_execution::RecoveryContract::ExternallyOwned,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core_execution::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core_execution::LashSchema::any(),
                semantics: lash_core_execution::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from(SESSION.to_string())],
        )
        .await
        .expect("register PostgreSQL public signal target");

    let first_host = Arc::new(storage.effect_host());
    let wait_controller: Arc<dyn lash_core_execution::RuntimeEffectController> = Arc::new(
        storage.runtime_effect_controller(ExecutionScope::process("pg-public-intent-target")),
    );
    let wake_key = wait_controller
        .await_event_key(
            &ExecutionScope::process("pg-public-intent-target"),
            lash_core_execution::AwaitEventWaitIdentity::process_signal(
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
    // The signal command runs on the group child's bound controller, which
    // the runtime's host hands out, so the crossing is captured there.
    let crossing_host = |inner: Arc<dyn EffectHost>| -> Arc<dyn EffectHost> {
        Arc::new(CrossingEffectHost {
            inner,
            crash_after: None,
            fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            signal_frames: Arc::clone(&signal_crossing_frames),
        })
    };
    let mut first = public_signal_runtime(
        pg_law_backend(
            &storage,
            crossing_host(first_host.clone()),
            Arc::clone(&registry),
        )
        .await,
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::Signal,
    )
    .await;
    let first_scope =
        postgres_public_turn_scope(first_host.as_ref(), Arc::clone(&signal_crossing_frames));
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
        lash_core_execution::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), wake)
            .await
            .expect("PostgreSQL SignalProcess intent must wake the parked wait")
            .expect("PostgreSQL wake task")
            .expect("PostgreSQL wake resolution"),
        lash_core_execution::Resolution::Ok(serde_json::json!({
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
            &ProcessId::from("pg-public-intent-target"),
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::json!(
                    "terminal after public intent drain"
                )),
            ),
            lash_core_execution::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("terminalize PostgreSQL signal target before redrive");
    drop(first);

    let replay_host = Arc::new(storage.effect_host());
    replay_host.start_replay();
    let mut replay = public_signal_runtime(
        pg_law_backend(
            &storage,
            crossing_host(replay_host.clone()),
            Arc::clone(&registry),
        )
        .await,
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::Signal,
    )
    .await;
    let replay_scope =
        postgres_public_turn_scope(replay_host.as_ref(), Arc::clone(&signal_crossing_frames));
    let replay_error = replay
        .stream_turn(
            public_runtime_input(),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                replay_scope,
            ),
        )
        .await
        .expect_err("a redrive over a session store without the claimed row cedes");
    assert_eq!(
        replay_error.code,
        lash_core_execution::RuntimeErrorCode::AcceptedTurnInputCeded,
        "the journaled drive claims a row this store never held, so the redrive cedes \
         rather than re-admitting and answering the input again: {replay_error:?}"
    );
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
        // A batch is a durable effect group (FIG-3397): the redrive reopens
        // it and is served the child's retained settlement, so the signal
        // command crosses once, live. The journal rows above are what the
        // redrive reconstructs byte-identically.
        assert_eq!(
            crossing_frames.len(),
            1,
            "the production signal command crosses once, live; the redrive is served the retained settlement"
        );
        assert_eq!(
            crossing_frames[0], first_crossing_frame,
            "the live production signal command frame is the one captured"
        );
    }

    reset(&storage).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn public_provider_parent_end_row_is_recovered_after_a_crash_before_the_ledger_write_on_postgres()
 {
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
    let registry: Arc<dyn lash_core_execution::ProcessRegistry> =
        Arc::new(storage.process_registry());
    registry
        .register_process_with_observers(
            lash_core_execution::ProcessRegistration::new(
                "pg-public-intent-target",
                lash_core_execution::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core_execution::RecoveryContract::ExternallyOwned,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core_execution::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core_execution::LashSchema::any(),
                semantics: lash_core_execution::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from(SESSION.to_string())],
        )
        .await
        .expect("register PostgreSQL parent-end signal target");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let effect_host = Arc::new(storage.effect_host());
    let mut first = public_signal_runtime(
        pg_law_backend(&storage, effect_host.clone(), Arc::clone(&registry)).await,
        Arc::clone(&provider_calls),
        Arc::clone(&model_calls),
        PublicIntentKind::ParentEnd,
    )
    .await;
    first.set_turn_phase_probe(Arc::new(PanicAtParentEnd));
    let first_scope =
        postgres_public_turn_scope(effect_host.as_ref(), Arc::new(Mutex::new(Vec::new())));
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
    .expect_err("the phase probe crashes after the turn commit and before the ledger row");
    assert!(crashed.is_panic());

    let parent = lash_core_execution::ParentScope::turn(
        SessionId::from(SESSION.to_string()),
        TurnId::from(TURN.to_string()),
    );
    let page = std::num::NonZeroUsize::new(16).expect("page bound");

    // The crash lands in the exact window recovery exists for: the turn's own
    // commit is durable, the `Cancel` child the tool call started is registered,
    // and the ledger row that ends the scope was never written.
    assert!(
        registry
            .get_parent_end_plan(&parent)
            .await
            .expect("read the parent-end ledger row after the crash")
            .is_none(),
        "the crash preempts the turn-exit ledger write"
    );
    let processes = registry
        .list_processes(&lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            ..lash_core_execution::ProcessListFilter::default()
        })
        .await
        .expect("list PostgreSQL public parent-end processes");
    let child = processes
        .iter()
        .find(|record| {
            matches!(
                record.input.as_ref(),
                lash_core_execution::ProcessInput::External { metadata }
                    if metadata == &serde_json::json!({"source": "parent-end"})
            )
        })
        .expect("find the Cancel child the tool call started");
    assert!(
        child.cancel_request.is_none(),
        "no cancel is requested while the ledger row is missing"
    );
    assert_eq!(
        registry
            .list_unrecorded_opener_parents(None, page)
            .await
            .expect("page turn parents that still owe a ledger row"),
        vec![parent.clone()],
        "the PostgreSQL candidate query reports the turn whose row the crash lost"
    );

    // Recovery writes the row the turn owed, through the same idempotent
    // registry write the turn itself makes.
    registry
        .record_parent_end(&parent)
        .await
        .expect("recovery re-derives the missing ledger row");
    assert!(
        registry
            .get_parent_end_plan(&parent)
            .await
            .expect("read the re-derived ledger row")
            .is_some(),
        "the re-derived row is the scope-end fact"
    );
    assert_eq!(
        registry
            .list_parent_end_children(&parent, None, page)
            .await
            .expect("page the ended scope's cancel children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![child.id.clone()],
        "the sweep finds the child by its own parent scope"
    );
    assert!(
        registry
            .list_unrecorded_opener_parents(None, page)
            .await
            .expect("re-page turn parents after the row lands")
            .is_empty(),
        "a turn with a ledger row is no longer a recovery candidate"
    );

    reset(&storage).await;
}

#[path = "attempt_atomicity/fig1293.rs"]
mod fig1293;

#[path = "attempt_atomicity/rerunnable_signal.rs"]
mod rerunnable_signal;

#[path = "attempt_atomicity/host_ingress.rs"]
mod host_ingress;
