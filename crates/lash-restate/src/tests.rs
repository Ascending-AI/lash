#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by protocol fixtures"
)]

//! The obsolete FIG-1127 nested-command refusal fixture was removed with the
//! journal-capable leaf surface. Current intent and process-replay laws cover
//! Restate at its sanctioned seams.

use super::*;
use crate::controller::RestateEffectControllerOptions;
use crate::controller::context::{ProcessCancelRace, guard_restate_context_future};
use crate::controller::effect_journal::JournaledEffectRecord;
use crate::controller::{
    RecordedRuntimeEffect, restate_await_event_turn_cancel_wait_request, restate_effect_name,
    restate_timer_turn_cancel_wait_request, validate_recorded_effect_envelope,
};
use crate::durable_wait::{
    DURABLE_WAIT_PROMISE_KEY, RestateDurableWaitIndexMetadata, RestateTurnCancelWake,
    durable_wait_address_from_state_key, durable_wait_index_state_key, restate_await_event_key,
    restate_await_event_key_for_authority, split_cancellable_waits,
};
use crate::process::{
    boundary_must_be_declined, handler_error_from_plugin, process_segment_workflow_key,
    restate_process_terminal_await_key, restate_process_terminal_resolution,
    workflow_key_authority,
};
use bytes::Bytes;
use lash_core::ProcessWorkSubstrate as _;
use lash_core::StoreSet as _;
use lash_core::testing::store_fixtures::{durable_admission, recorded_process_admission};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, Clock, EffectAddress, EffectHost,
    ExecutionScope, PluginError, ProcessAwaitOutput, ProcessCommand, ProcessEffectOutcome,
    ProcessExecutionContext, ProcessExecutionEnvStore, ProcessExternalRef, ProcessRegistry,
    Resolution, ResolveOutcome, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, ScopedEffectController, facade_support::TurnAddress,
    facade_support::TurnAttach,
};
use lash_core::{ProcessInput, ProcessRegistration, TriggerStore};
use lash_core_worker::DurableProcessWorker;
use lash_http_transport::HttpRequest;
use lash_http_transport::{HttpResponse, HttpResponseBody, HttpTransport, LlmTransportError};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::MutexExt;
use restate_sdk::context::macro_support::SealedDurableFuture;
use restate_sdk::context::{ContextClient, RequestTarget, RunRetryPolicy, WorkflowContext};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::prelude::Endpoint;
use restate_sdk::serde::Json;
use restate_sdk::service::Discoverable;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// The drain generation every test-built controller and host names: the
/// build the conformance harness's endpoint serves, so recorded owner work
/// uses a lane that endpoint binds (FIG-4454).
pub(crate) fn test_build_generation() -> lash_core::engine::BuildGeneration {
    lash_core::engine::BuildGeneration::for_test(conformance_harness::HARNESS_BUILD)
}

fn test_restate_authority_id() -> RestateAuthorityId {
    RestateAuthorityId::new("lash-restate-tests").expect("valid test Restate authority id")
}

fn test_restate_await_event_key(
    scope: &ExecutionScope,
    wait: AwaitEventWaitIdentity,
) -> Result<AwaitEventKey, lash_core::RuntimeError> {
    restate_await_event_key_for_authority(&test_restate_authority_id(), scope, wait)
}

/// A runtime host config over this engine on a fresh SQLite memory store set
/// (ADR 0104): the stores a Restate test's runtime stands on. The engine's
/// connection reaches no server; a test that journals under Restate installs
/// its own Restate host over this config's backend host.
pub(super) async fn memory_host_config() -> lash_core::facade_support::RuntimeHostConfig {
    lash_core::facade_support::RuntimeHostConfig::new(
        memory_engine_backend().await,
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
}

/// A backend of this engine over a fresh SQLite memory store set, connected
/// to no server: the substrate a test's Lashlang artifacts live in.
pub(super) async fn memory_engine_backend() -> lash_core::Backend {
    lash_core::Backend::new(Arc::new(memory_engine().await))
}

/// This engine over a fresh SQLite memory store set, connected to no server.
pub(super) async fn memory_engine() -> RestateEngine {
    let connection = RestateConnection::new("https://restate.invalid");
    RestateEngine::new(
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("open a SQLite memory store set"),
        ),
        RestateConfig::new(connection.clone(), connection, test_restate_authority_id()).stamped(
            lash_core::engine::BuildGeneration::for_test("lash-restate-tests"),
        ),
    )
}

/// Restate process work over `registry`, submitting to no server: the
/// wiring a test's process worker is built with. The worker runs the segments
/// the test hands it; nothing here schedules a process in-process.
pub(super) fn restate_process_work(
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
) -> lash_core::ProcessWorkWiring {
    RestateProcessDeployment::new_for_test("https://restate.invalid", registry, continuations)
        .process_work()
}

/// A session-store catalog over a fresh SQLite memory store set: the catalog
/// a process-worker test hands its worker.
pub(super) async fn memory_session_store_factory() -> Arc<dyn lash_core::DeploymentStore> {
    lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set")
        .session_store_factory()
}

/// The process ports of a fresh SQLite memory store set, with the fault
/// decorator over its registry: the registry a Restate process test writes
/// through and can fault.
pub(super) struct MemoryProcessStores {
    pub(super) registry: Arc<lash_core::testing::ProcessRegistryFaults>,
    pub(super) continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    pub(super) env_store: Arc<dyn ProcessExecutionEnvStore>,
    /// The `ProcessStart` obligation ledger `register_process` arms.
    pub(super) start_ledger: Arc<dyn lash_core::store::ObligationLedger>,
    pub(super) clock: Arc<dyn Clock>,
}

pub(super) async fn memory_process_stores() -> MemoryProcessStores {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set");
    let registry = stores.process_registry();
    MemoryProcessStores {
        registry: Arc::new(lash_core::testing::ProcessRegistryFaults::new(
            Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
        )),
        continuations: registry,
        env_store: stores.process_env_store(),
        start_ledger: stores.obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
        clock: stores.clock(),
    }
}

/// Deliver `process_id`'s armed `ProcessStart` obligation through the relay —
/// the one production delivery path's own-commit attempt (ADR 0109 §1.5).
pub(super) async fn deliver_process_start_now(
    start_ledger: &Arc<dyn lash_core::store::ObligationLedger>,
    registry: &Arc<dyn ProcessRegistry>,
    port: &Arc<dyn lash_core::ProcessWorkSubstrate>,
    clock: &Arc<dyn Clock>,
    process_id: &ProcessId,
) -> lash_core::runtime::shift::relay::RelayVerdict {
    let relay = lash_core::runtime::process_start::ProcessStartRelay::new(
        Arc::clone(start_ledger),
        Arc::clone(registry),
        Arc::clone(port),
        Arc::clone(clock),
    );
    relay
        .deliver_start(process_id)
        .await
        .expect("the start delivery's ledger settle")
}

/// The session-control recovery pass's park settle, over the paused `run`
/// invocations `admin` lists — what `RestateSessionControl::reconcile_parks`
/// runs each tick.
pub(super) async fn reconcile_parked_processes(
    admin: &RestateAdminClient,
    registry: &Arc<dyn ProcessRegistry>,
    continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
) -> crate::process::park_reconcile::ProcessParkReconcileReport {
    let paused = admin
        .paused_invocations(
            &crate::services::DEFAULT_NAMESPACE
                .stable(crate::services::LashService::ProcessWorkflow)
                .name(),
        )
        .await
        .expect("list paused process invocations");
    crate::process::park_reconcile::reconcile_process_invocations(
        admin,
        registry,
        continuations,
        paused,
        &Default::default(),
    )
    .await
    .expect("reconcile paused processes")
}

/// Admit `calls` as one round of `run`, as production admits every tool
/// call (a singleton is a one-member round), and progress the round's
/// recorded schedule until each call has decided or parked on its Deferred
/// source. Decisions return in `calls` order.
pub(super) async fn decide_round<'a>(
    run: &mut lash_core::tool_dispatch::RunCoordinator<'a>,
    calls: &[lash_core::tool_dispatch::SingletonToolCall],
    handlers: Arc<dyn lash_core::tool_dispatch::SingletonToolHandlers + 'a>,
    retry: lash_core::tool_run::RecordedRetryPolicy,
) -> Result<Vec<lash_core::tool_dispatch::DecidedCall>, lash_core::tool_dispatch::SingletonRunError>
{
    let mut decisions: std::collections::BTreeMap<_, _> = run
        .start_round(
            calls,
            lash_core::tool_run::CapacityScope::Held,
            handlers,
            retry,
        )
        .await?
        .into_iter()
        .collect();
    while calls
        .iter()
        .any(|call| !decisions.contains_key(&call.call_id))
    {
        if let Some((call_id, decision)) = run.progress().await? {
            decisions.insert(call_id, decision);
        }
    }
    Ok(calls
        .iter()
        .filter_map(|call| decisions.remove(&call.call_id))
        .collect())
}

/// How a one-member round ended, with the records its Run holds.
#[derive(Clone, Debug)]
pub(super) struct SingletonRunOutcome {
    pub terminal: lash_core::tool_dispatch::SingletonTerminal,
    pub records: Vec<lash_core::tool_run::RunRecord>,
}

/// Run `call` as a one-member round of its own Run, then drain its
/// presentation; a Deferred call ends parked on its source.
pub(super) async fn run_singleton<'a>(
    scoped: &'a lash_core::ScopedEffectController<'a>,
    call: &lash_core::tool_dispatch::SingletonToolCall,
    handlers: Arc<dyn lash_core::tool_dispatch::SingletonToolHandlers + 'a>,
) -> Result<SingletonRunOutcome, lash_core::tool_dispatch::SingletonRunError> {
    use lash_core::tool_dispatch::{DecidedCall, RunCoordinator, SingletonTerminal};
    let mut run = RunCoordinator::open(
        scoped,
        call.owner.clone(),
        call.segment,
        call.available.clone(),
    );
    let decided = decide_round(
        &mut run,
        std::slice::from_ref(call),
        handlers,
        Default::default(),
    )
    .await?;
    let terminal = match decided.into_iter().next() {
        Some(DecidedCall::Deferred { source }) => SingletonTerminal::Deferred { source },
        _ => run
            .drain()
            .await?
            .pop()
            .map(|(_, terminal)| terminal)
            .ok_or_else(|| lash_core::tool_run::RunEventRefusal::BoundaryOrder {
                call_id: call.call_id.clone(),
            })?,
    };
    Ok(SingletonRunOutcome {
        terminal,
        records: run.into_records(),
    })
}

/// The view of `session_id` on `store`, a catalog store.
pub(super) fn session_view(
    store: Arc<dyn lash_core::RuntimeStore>,
    session_id: impl Into<SessionId>,
) -> lash_core::store::SessionStore {
    lash_core::store::SessionStore::new(store, session_id.into()).expect("a valid session id")
}

/// `view`'s session seen through the store decorator `decorate` builds over
/// `view`'s catalog.
pub(super) fn decorated_view<D>(
    view: &lash_core::store::SessionStore,
    decorate: impl FnOnce(Arc<dyn lash_core::RuntimeStore>) -> D,
) -> lash_core::store::SessionStore
where
    D: lash_core::RuntimeStore + 'static,
{
    session_view(
        Arc::new(decorate(Arc::clone(view.store()))),
        view.session_id().clone(),
    )
}

/// A root session store for `session_id` over a fresh SQLite memory backend.
pub(super) async fn memory_session_store(session_id: &str) -> lash_core::store::SessionStore {
    lash_core::runtime::admit_session_view(
        &memory_session_store_factory().await,
        &lash_core::testing::store_fixtures::session_store_request(
            &SessionId::fixture(session_id),
            "restate-test-model",
            lash_core::SessionRelation::Root,
        ),
    )
    .await
    .expect("create a SQLite memory session store")
}

/// The trigger store of a fresh SQLite memory store set.
pub(super) async fn memory_trigger_store() -> Arc<dyn lash_core::TriggerStore> {
    lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory store set")
        .trigger_store()
}

mod compat_on_the_double;
mod declared_start_run_drain_on_the_double;
mod determinism;
mod effect_host_laws_on_the_double;
mod endpoint_protocol;
mod folded_generation_sentinel_on_the_double;
mod generation_sentinel_on_the_double;
mod guarded_surface_tests;
mod journal_cut_runner;
mod live_fault_park_on_the_double;
mod live_turn_probe;
mod otel_laws;
mod parent_end_on_the_double;
mod process_effect_summary;
mod process_tool_replay;
mod remote_turn_cancel;
mod replay_corpus;
mod run_control_witnesses;
mod run_coordinator_on_the_double;
mod run_owner_park;
mod segment_generation_handoff;
mod segment_redrive_on_the_double;
mod session_shift_roll_on_the_double;
mod shift_laws_on_the_double;
mod singleton_tool_run_on_the_double;
mod tool_context_conformance;
mod tool_run_sdk_contract;
mod trigger_authority;
mod turn_cancel_modes;
mod turn_crash_on_the_double;
mod turn_laws_on_the_double;
mod wait_handoff_generations;
use endpoint_protocol::{
    admission_journal, admitted_invocation_body, durable_wait_index_call_response,
    encode_call_replay, encode_completed_gate_sleep_replay, encode_journal_retry,
    encode_process_segment_send_replay, encode_process_terminal_delivery_replay,
    encode_recorded_commands_replay, encode_recorded_commands_with_invocations_replay,
    encode_run_replay, invoke_endpoint, invoke_endpoint_body, invoke_endpoint_body_open,
    invoke_endpoint_body_with_json_call_responses,
    invoke_endpoint_body_with_json_call_responses_then_suspend,
    invoke_endpoint_with_named_call_responses, invoke_endpoint_with_scripted_responses,
    invoke_process_workflow_body, invoke_process_workflow_endpoint, restate_call_frames,
    restate_completed_promise, restate_error_code, restate_error_message, restate_message_types,
    restate_output_failure_message, restate_output_json, restate_recorded_commands, with_admission,
};

fn registry_local_executor(
    registry: Arc<dyn ProcessRegistry>,
) -> RuntimeEffectLocalExecutor<'static> {
    let process_work = Arc::new(lash_core::NoProcessWork::for_registry(Arc::clone(
        &registry,
    )));
    RuntimeEffectLocalExecutor::processes(
        registry,
        process_work,
        lash_core::testing::process_engine_fixture()
            .with_artifact_ports(lash_core::ArtifactReferrerPorts::of_backend(
                &RECOVERY_ARTIFACT_BACKEND,
            ))
            .with_registration(lash_lashlang_runtime::lashlang_process_engine_registration(
                lash_lashlang_runtime::LashlangProcessEngine::new(
                    recovery_artifact_store(),
                    lash_lashlang_runtime::LashlangSurface::default(),
                    RECOVERY_ARTIFACT_BACKEND.worker_recovery(),
                ),
            )),
        lash_core::runtime::HostStartAdmission::default(),
    )
}

#[tokio::test]
async fn restate_scope_controller_refuses_wrong_scope_before_index_or_local_execution() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let scoped = controller
        .process_scope_for_test(durable_admission(&ExecutionScope::process(
            lash_core::ProcessId::fixture("admitted-restate-process"),
        )))
        .expect("scoped Restate controller");
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                ExecutionScope::process(lash_core::ProcessId::fixture("wrong-restate-process")),
                "shared-replay-key",
            )
            .expect("wrong-scope effect address"),
            lash_core::RuntimeAttribution::none(),
            "restate-scope-admission-sleep",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 1 },
        },
    );

    let error = scoped
        .controller()
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(|_envelope| async {
                panic!("wrong-scope Restate effect must not execute locally")
            }),
        )
        .await
        .expect_err("wrong Restate scope must be refused");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectScopeMismatch
    );
    assert_eq!(context.scope_effect_begins.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn deployment_host_raw_scoped_controller_refuses_wrong_scope_before_ingress() {
    let transport = Arc::new(effect_execution::ScriptedHttpTransport::new([]));
    let host = RestateEffectHost::new(
        RestateConnection::with_transport("https://restate.example", transport.clone()),
        test_restate_authority_id(),
    );
    let scoped = host
        .scoped(durable_admission(&ExecutionScope::process(
            lash_core::ProcessId::fixture("admitted-deployment-process"),
        )))
        .expect("scoped deployment host");
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                ExecutionScope::process(lash_core::ProcessId::fixture("wrong-deployment-process")),
                "wrong-deployment-effect",
            )
            .expect("wrong-scope deployment address"),
            lash_core::RuntimeAttribution::none(),
            "wrong-deployment-effect",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 1 },
        },
    );
    let local_executions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&local_executions);

    let error = scoped
        .controller()
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(RuntimeEffectOutcome::Sleep)
            }),
        )
        .await
        .expect_err("raw bound controller must reject a foreign effect address");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectScopeMismatch
    );
    assert!(
        transport.requests().is_empty(),
        "scope refusal must precede the retired-scope probe and effect ingress"
    );
    assert_eq!(local_executions.load(Ordering::SeqCst), 0);
}

fn registry_process_wiring(registry: Arc<dyn ProcessRegistry>) -> lash_core::ProcessWorkWiring {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let registry = Arc::clone(watched.registry());
    lash_core::ProcessWorkWiring::new(
        watched,
        Arc::new(lash_core::NoProcessWork::for_registry(registry)),
    )
}

fn test_turn_effect_invocation(
    session_id: &str,
    turn_id: &str,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: impl Into<String>,
    replay_key: impl Into<String>,
) -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            ExecutionScope::turn(
                lash_core::SessionId::fixture(session_id),
                TurnId::fixture(turn_id.to_string()),
            ),
            replay_key,
        )
        .expect("valid Restate test effect address"),
        lash_core::RuntimeAttribution::for_turn(
            lash_core::SessionId::fixture(session_id),
            lash_core::TurnId::fixture(turn_id),
            turn_index,
            protocol_iteration,
        ),
        effect_id,
    )
}

fn process_success(value: serde_json::Value) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(value))
}

fn legacy_process_success(value: serde_json::Value) -> ProcessAwaitOutput {
    let value =
        serde_json::from_value(value).expect("legacy process success is a valid tool value");
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success_tool_value(value))
}

fn process_cancellation(
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
) -> ProcessAwaitOutput {
    let mut cancellation = lash_core::ToolCancellation::runtime(message);
    cancellation.raw = raw.map(lash_core::ToolValue::untrusted_json);
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(cancellation))
}

fn process_failure(
    class: lash_core::ToolFailureClass,
    code: impl Into<String>,
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
) -> ProcessAwaitOutput {
    let mut failure = lash_core::ToolFailure::runtime(class, code, message);
    failure.raw = raw.map(lash_core::ToolValue::untrusted_json);
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(failure))
}

fn is_process_success(output: &ProcessAwaitOutput) -> bool {
    matches!(output, ProcessAwaitOutput::Settled { output } if output.is_success())
}

fn is_process_cancellation(output: &ProcessAwaitOutput) -> bool {
    matches!(
        output,
        ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Cancelled(_))
    )
}

fn durable_turn_scope(
    session_id: impl Into<SessionId>,
    turn_id: impl Into<TurnId>,
) -> ExecutionScope {
    let session_id = session_id.into();
    ExecutionScope::turn(&session_id, turn_id)
}

/// Restate service-protocol message types used by the FIG-779/FIG-790 gates.
/// `restate_sdk_shared_core::service_protocol::header` keeps these private, so
/// they are restated here (`SleepCommand = 0x040C`, `Suspension = 0x0001`,
/// `CallCommand = 0x040D`,
/// `CompletePromiseCommand = 0x040B`,
/// `OutputCommand = 0x0401`, `End = 0x0003`).
const RESTATE_SLEEP_COMMAND_MESSAGE_TYPE: u16 = 0x040C;
const RESTATE_CALL_COMMAND_MESSAGE_TYPE: u16 = 0x040D;
const RESTATE_SUSPENSION_MESSAGE_TYPE: u16 = 0x0001;
const RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE: u16 = 0x040B;
const RESTATE_GET_PROMISE_COMMAND_MESSAGE_TYPE: u16 = 0x0409;
const RESTATE_PEEK_PROMISE_COMMAND_MESSAGE_TYPE: u16 = 0x040A;
const RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE: u16 = 0x0401;
const RESTATE_END_MESSAGE_TYPE: u16 = 0x0003;
const RESTATE_RUN_COMMAND_MESSAGE_TYPE: u16 = 0x0411;
const RESTATE_PROPOSE_RUN_COMPLETION_MESSAGE_TYPE: u16 = 0x0005;

#[derive(Debug, Serialize, serde::Deserialize)]
struct Fig779TimerGuardReproInput {
    duration_ms: u64,
}

/// FIG-779 repro fixture: a workflow that sleeps on a durable timer through the
/// two paths that matter — the guarded Lash driver path and the bare SDK path.
#[restate_sdk::workflow]
trait Fig779TimerGuardRepro {
    async fn run(input: Json<Fig779TimerGuardReproInput>) -> HandlerResult<Json<()>>;

    async fn raw_sleep(input: Json<Fig779TimerGuardReproInput>) -> HandlerResult<Json<()>>;

    async fn repoll_fused_timer(input: Json<Fig779TimerGuardReproInput>)
    -> HandlerResult<Json<()>>;
}

struct Fig779TimerGuardReproImpl;

impl Fig779TimerGuardRepro for Fig779TimerGuardReproImpl {
    /// The production geometry of every sleep inside a process body:
    /// `turn_cancel == None`, raced against the process segment's durable
    /// cancel promise (FIG-3673). The timer's command, then the promise's.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let outcome = RestateControllerContext::sleep_or_turn_cancel(
            &ctx,
            &crate::services::DEFAULT_NAMESPACE,
            Duration::from_millis(input.duration_ms),
            None,
            crate::controller::context::ProcessCancelRace::Raced,
        )
        .await?;
        assert!(matches!(
            outcome,
            RestateTurnCancelRaceOutcome::Completed(())
        ));
        Ok(Json(()))
    }

    /// The same durable timer without the Lash guard, as an SDK control.
    async fn raw_sleep(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        restate_sdk::context::ContextTimers::sleep(&ctx, Duration::from_millis(input.duration_ms))
            .await?;
        Ok(Json(()))
    }

    async fn repoll_fused_timer(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let timer = restate_sdk::context::ContextTimers::sleep(
            &ctx,
            Duration::from_millis(input.duration_ms),
        );
        let state = timer.inner_context();
        let timer = guard_restate_context_future(timer, state);
        tokio::pin!(timer);
        std::future::poll_fn(|cx| {
            assert!(matches!(timer.as_mut().poll(cx), Poll::Pending));
            let _ = timer.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
        Ok(Json(()))
    }
}

/// FIG-1464 repro payload: an effect result the Restate journal can never
/// accept. Serializing it fails the same way a non-finite number or an
/// oversized/invalid journal payload does, which is the SDK-level `ctx.run`
/// failure shape observed in the workbench replay-panic loop.
#[derive(Debug)]
struct Fig1464UnjournalableEffectResult;

impl Serialize for Fig1464UnjournalableEffectResult {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom(
            "fig1464 effect result cannot be journaled",
        ))
    }
}

impl<'de> serde::Deserialize<'de> for Fig1464UnjournalableEffectResult {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "fig1464 effect result is never journaled",
        ))
    }
}

/// FIG-1464 replay payload: an effect result that journals cleanly and can never
/// be read back. That is the replay-path shape of the same SDK-level `ctx.run`
/// failure: the SDK skips the closure entirely on an already-journaled run entry,
/// so the failure comes out of deserializing the recorded value instead.
#[derive(Debug)]
struct Fig1464UnreadableJournaledResult;

impl Serialize for Fig1464UnreadableJournaledResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(41)
    }
}

impl<'de> serde::Deserialize<'de> for Fig1464UnreadableJournaledResult {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "fig1464 journaled effect result cannot be read back",
        ))
    }
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct Fig1464RunGuardReproInput {
    effect_name: String,
}

/// FIG-1464 repro fixture: the journaled-effect (`ctx.run`) leg of the durable
/// controller seam, executed through the one geometry that turns an SDK-level run
/// failure into a process abort — a second poll after the SDK recorded its
/// terminal attempt state.
#[restate_sdk::workflow]
trait Fig1464RunGuardRepro {
    async fn repoll_failed_run(input: Json<Fig1464RunGuardReproInput>) -> HandlerResult<Json<()>>;

    async fn repoll_replayed_run(input: Json<Fig1464RunGuardReproInput>)
    -> HandlerResult<Json<()>>;

    async fn journaled_run(input: Json<Fig1464RunGuardReproInput>) -> HandlerResult<Json<u32>>;

    async fn self_waking_run(input: Json<Fig1464RunGuardReproInput>) -> HandlerResult<Json<u32>>;
}

struct Fig1464RunGuardReproImpl;

impl Fig1464RunGuardRepro for Fig1464RunGuardReproImpl {
    /// The production geometry: a journaled effect whose `ctx.run` fails at the
    /// SDK level. `InterceptErrorFuture` records the handler-state failure,
    /// wakes synchronously and returns `Pending`; the SDK future has produced
    /// its terminal outcome for the attempt and must never be re-entered. Every
    /// poller above this seam (the turn event pump, the effect races) can poll
    /// the enclosing future again, so the seam - not its callers - has to fuse.
    async fn repoll_failed_run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig1464RunGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let mut run = RestateControllerContext::run_json_send(
            &ctx,
            UnjournalableFixtureStep(input.effect_name),
            None,
            async { Fig1464UnjournalableEffectResult },
        );
        std::future::poll_fn(|cx| {
            assert!(
                matches!(run.as_mut().poll(cx), Poll::Pending),
                "a failed journaled run must record its handler state and park"
            );
            let _ = run.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
        Ok(Json(()))
    }

    /// The replay geometry the ticket reports: the run entry is already
    /// journaled, so the SDK never invokes the closure. The recorded value
    /// cannot be read back, `InterceptErrorFuture` records the handler-state
    /// failure, wakes synchronously and returns `Pending` - with no closure to
    /// account for that wake, the guard must fuse a run future it never once saw
    /// the closure of.
    async fn repoll_replayed_run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig1464RunGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let mut run = RestateControllerContext::run_json_send(
            &ctx,
            UnreadableFixtureStep(input.effect_name),
            None,
            async { Fig1464UnreadableJournaledResult },
        );
        std::future::poll_fn(|cx| {
            assert!(
                matches!(run.as_mut().poll(cx), Poll::Pending),
                "a replayed run whose recorded value cannot be read must park"
            );
            let _ = run.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
        Ok(Json(()))
    }

    /// The same seam on the happy path: fusing a terminal attempt state must
    /// not swallow a journaled result.
    async fn journaled_run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig1464RunGuardReproInput>,
    ) -> HandlerResult<Json<u32>> {
        let Json(value) = RestateControllerContext::run_json_send(
            &ctx,
            ScalarFixtureStep(input.effect_name),
            None,
            async { 41_u32 },
        )
        .await?;
        Ok(Json(value + 1))
    }

    /// The run closure polls arbitrary lash code, and that code is allowed to
    /// wake its own task synchronously - `yield_now` is idiomatic one module
    /// over. Such a wake arrives before the closure's future has returned, so it
    /// must not fuse the run: fusing here would park a healthy effect forever
    /// while holding a paid completion.
    async fn self_waking_run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig1464RunGuardReproInput>,
    ) -> HandlerResult<Json<u32>> {
        let Json(value) = RestateControllerContext::run_json_send(
            &ctx,
            ScalarFixtureStep(input.effect_name),
            None,
            async {
                tokio::task::yield_now().await;
                41_u32
            },
        )
        .await?;
        Ok(Json(value + 1))
    }
}

#[derive(Debug)]
struct Fig779SuspendingProcessRunner;

fn scoped_runtime_invocation(
    scope: &ExecutionScope,
    kind: RuntimeEffectKind,
    effect_id: &str,
) -> RuntimeEffectInvocation {
    RuntimeEffectInvocation::new(
        EffectAddress::new(
            scope.clone(),
            format!("session:turn:1:0:{}:{effect_id}", kind.as_str()),
        )
        .expect("valid scoped runtime effect address"),
        RuntimeAttribution::for_turn("session", "turn", 1, 0),
        effect_id,
    )
}

#[async_trait::async_trait]
impl RestateProcessRunner for Fig779SuspendingProcessRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let outcome = scoped_effect_controller
            .controller()
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    scoped_runtime_invocation(
                        scoped_effect_controller.execution_scope(),
                        RuntimeEffectKind::Sleep,
                        "fig779-redrive-sleep",
                    ),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For {
                            duration_ms: 60_000,
                        },
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation.clone())
                    .with_turn_cancel_observation(false),
            )
            .await;
        // The sleep's recorded outcome is the only cancel signal this body
        // reads: its lent stop is never a shift input (FIG-3673).
        match outcome {
            Ok(RuntimeEffectOutcome::Sleep) => Ok(process_success(serde_json::Value::Null).into()),
            Err(error)
                if error.code == lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled =>
            {
                Ok(process_cancellation(
                    format!("process `{process_id}` observed durable cancellation"),
                    None,
                )
                .into())
            }
            Err(error) => Err(PluginError::Session(error.to_string())),
            Ok(other) => Err(PluginError::Session(format!(
                "unexpected sleep outcome: {other:?}"
            ))),
        }
    }
}

#[derive(Debug)]
struct Fig788TerminalRedriveRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig788TerminalRedriveRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        scoped_effect_controller
            .controller()
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    scoped_runtime_invocation(
                        scoped_effect_controller.execution_scope(),
                        RuntimeEffectKind::Sleep,
                        "fig788-terminal-redrive-sleep",
                    ),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For {
                            duration_ms: 60_000,
                        },
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation).with_turn_cancel_observation(false),
            )
            .await
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(process_success(serde_json::json!({"runner": "replayed"})).into())
    }
}

#[derive(Debug)]
struct Fig788SegmentBoundaryRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig788SegmentBoundaryRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        Ok(lash_core::ProcessRunOutcome::SegmentBoundary(
            lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "fig788-segment-program".to_string(),
                engine_state: vec![7, 8, 8],
            },
        ))
    }
}

#[derive(Debug)]
struct Fig788OrdinalOneTerminalRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig788OrdinalOneTerminalRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        assert_eq!(
            handover.expect("ordinal-one runner must receive its handover"),
            lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "fig788-terminal-program".to_string(),
                engine_state: vec![1],
            }
        );
        Ok(process_success(serde_json::json!({"segment": 1, "terminal": true})).into())
    }
}

#[derive(Debug)]
struct Fig811EffectfulOrdinalOneTerminalRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig811EffectfulOrdinalOneTerminalRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: lash_core::ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        assert_eq!(
            handover.expect("effectful ordinal-one runner must receive its handover"),
            lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "fig811-effectful-terminal-program".to_string(),
                engine_state: vec![8, 1, 1],
            }
        );
        scoped_effect_controller
            .controller()
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    scoped_runtime_invocation(
                        scoped_effect_controller.execution_scope(),
                        RuntimeEffectKind::Sleep,
                        "fig811-effectful-terminal-sleep",
                    ),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For { duration_ms: 1 },
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation).with_turn_cancel_observation(false),
            )
            .await
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(process_success(serde_json::json!({"segment": 1, "effectful_terminal": true})).into())
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig806TriggerRedriveInput {
    occurrence: lash_core::TriggerOccurrenceRequest,
}

#[restate_sdk::workflow]
trait Fig806TriggerRedrive {
    async fn run(
        input: Json<Fig806TriggerRedriveInput>,
    ) -> HandlerResult<Json<lash_core::facade_support::TriggerEmitReport>>;
}

struct Fig806TriggerRedriveImpl {
    router: lash_core::facade_support::TriggerRouter,
}

impl Fig806TriggerRedrive for Fig806TriggerRedriveImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig806TriggerRedriveInput>,
    ) -> HandlerResult<Json<lash_core::facade_support::TriggerEmitReport>> {
        let controller = RestateRuntimeEffectController::new_for_test(ctx);
        let scoped = controller
            .scoped_effect_controller(durable_admission(&ExecutionScope::runtime_operation(
                format!("fig806-trigger:{}", input.occurrence.idempotency_key),
            )))
            .map_err(HandlerError::from)?;
        let report = self
            .router
            .emit(input.occurrence, &scoped)
            .await
            .map_err(HandlerError::from)?;
        let request: restate_sdk::context::Request<'_, Json<()>, Json<()>> = ContextClient::request(
            controller.context(),
            RequestTarget::workflow("Fig806TriggerSink", "fig806-sink", "complete"),
            Json(()),
        );
        let Json(()) = request.call().await?;
        Ok(Json(report))
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig793LlmGateRedriveInput;

#[restate_sdk::workflow]
trait Fig793LlmGateRedrive {
    async fn run(input: Json<Fig793LlmGateRedriveInput>) -> HandlerResult<Json<bool>>;
}

/// A recorded effect as its `ctx.run` journal entry holds it: stamped with
/// this build's effect-journal generation.
fn journal_entry_value(recorded: RecordedRuntimeEffect) -> serde_json::Value {
    serde_json::to_value(JournaledEffectRecord::Recorded(recorded))
        .expect("encode a recorded effect's journal entry")
}

fn fig793_llm_envelope() -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::LlmCall, "fig793-llm"),
        RuntimeEffectCommand::LlmCall {
            request: {
                let mut request = Box::new(llm_spec());
                request.model.model = lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    lash_core::LlmProfileKey::new("test"),
                    request.model.metadata().clone(),
                );
                request
            },
        },
    )
}

fn fig793_llm_outcome() -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::LlmCall {
        result: Box::new(Ok(lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "journaled response".to_string(),
                response_meta: None,
            }],
            ..lash_core::LlmResponse::default()
        })),
        text_streamed: false,
        call_record: None,
        stream: Box::new(lash_core::LlmStreamRecord::unstreamed(
            lash_core::AssistantResponsePlan::default(),
        )),
    }
}

struct Fig793LlmGateRedriveImpl;

impl Fig793LlmGateRedrive for Fig793LlmGateRedriveImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig793LlmGateRedriveInput>,
    ) -> HandlerResult<Json<bool>> {
        let controller = RestateRuntimeEffectController::new_for_test(ctx);
        controller
            .execute_effect(
                fig793_llm_envelope(),
                RuntimeEffectLocalExecutor::testing(|_envelope| async { Ok(fig793_llm_outcome()) }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let key = test_restate_await_event_key(
            &durable_turn_scope("fig793-session", "fig793-turn"),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .map_err(TerminalError::from_error)?;
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    test_turn_effect_invocation(
                        "fig793-session",
                        "fig793-turn",
                        1,
                        0,
                        "turn_cancel.after_llm.0",
                        "turn_cancel.after_llm.0",
                    ),
                    RuntimeEffectCommand::PeekAwaitEvent { key },
                ),
                RuntimeEffectLocalExecutor::unavailable(),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::PeekAwaitEvent { resolution } = outcome else {
            return Err(TerminalError::new("FIG-793 fixture expected a peek outcome").into());
        };
        Ok(Json(resolution.is_some()))
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig1126PendingToolRedriveInput;

#[restate_sdk::workflow]
trait Fig1126RevokedAwaitBoundary {
    async fn run(input: Json<Fig1126PendingToolRedriveInput>) -> HandlerResult<Json<Resolution>>;
}

struct Fig1126RevokedAwaitBoundaryImpl;

impl Fig1126RevokedAwaitBoundary for Fig1126RevokedAwaitBoundaryImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig1126PendingToolRedriveInput>,
    ) -> HandlerResult<Json<Resolution>> {
        let scope = durable_turn_scope("fig1126-revoked-session", "fig1126-revoked-turn");
        let key = test_restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
                "fig1126-revoked-call",
            )),
        )
        .map_err(TerminalError::from_error)?;
        let outcome = RestateRuntimeEffectController::new_for_test(ctx)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "fig1126-revoked-await"),
                    RuntimeEffectCommand::AwaitEvent { key },
                ),
                RuntimeEffectLocalExecutor::await_event(tokio_util::sync::CancellationToken::new())
                    .with_turn_cancel_scope(scope),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::AwaitEvent { resolution } = outcome else {
            return Err(TerminalError::new("FIG-1126 fixture expected an await outcome").into());
        };
        Ok(Json(resolution))
    }
}

mod cancellation_and_effects;
mod cancelled_turn_withheld_input_on_the_double;
mod commit_retry_store;
mod conformance_and_poison;
mod direct_turn_acceptance_on_the_double;
mod durable_wait_source_seal;
mod durable_wait_turn_gate_peek;
mod effect_execution;
mod failure_settlement;
mod indexed_waits;
mod ingress_recovery;
mod postgres_ingress;
mod process_await_redrive;
mod process_cancel_race;
mod process_cancel_steps;
mod process_child_residency;
mod process_command_replay;
mod process_drive_inputs;
mod process_exhaustion_park;
mod process_park;
mod process_recovery;
mod process_registry_core;
mod process_registry_replay;
mod process_session_turn_cancel;
mod process_session_turn_laws;
mod process_signal_admission;
mod process_source_wait_end;
mod process_start_engine_cancel;
mod process_start_replay_on_the_double;
mod process_start_store_refusals;
mod process_terminal_await;
mod process_terminal_obligation_on_the_double;
mod process_workflow;
mod recording_context;
mod restate_redrive;
mod session_failure_evidence_on_the_double;
mod source_wait_transfer;
mod store_fault_journaling;
mod substrate_lost;
mod sync_hooks_retryable_faults;
mod tool_batch_parallelism_on_the_double;
mod tool_call_identity_on_the_double;
mod trigger_emit_outside_a_handler_on_the_double;
mod trigger_emit_reattach_on_the_double;
mod vm_broker_on_the_double;

use cancellation_and_effects::*;
use commit_retry_store::*;
use conformance_and_poison::*;
use effect_execution::*;
use process_await_redrive::*;
use process_recovery::*;
use process_registry_core::*;
use process_registry_replay::*;
use process_workflow::*;
use recording_context::*;

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig1142ReplayDivergenceInput;

#[restate_sdk::workflow]
trait Fig1142ReplayDivergence {
    async fn run(input: Json<Fig1142ReplayDivergenceInput>) -> HandlerResult<Json<bool>>;
}

struct Fig1142ReplayDivergenceImpl {
    model_version: Arc<AtomicUsize>,
    /// How many times the model call actually ran.
    executions: Arc<AtomicUsize>,
}

/// The first-incarnation model call's recorded entry: it dispatched nothing.
fn fig1142_recorded_llm_call() -> crate::controller::RecordedRuntimeEffect {
    crate::controller::RecordedRuntimeEffect {
        envelope: Arc::new(
            fig1142_llm_envelope(1)
                .canonical_form()
                .expect("canonical model-call envelope"),
        ),
        outcome: Ok(fig793_llm_outcome()),
    }
}

fn fig1142_llm_envelope(model_version: usize) -> RuntimeEffectEnvelope {
    let mut request = llm_spec();
    request.model.metadata_mut().wire_model = format!("model-v{model_version}");
    RuntimeEffectEnvelope::new(
        test_turn_effect_invocation(
            "fig1142-session",
            "fig1142-turn",
            0,
            0,
            "fig1142-replay-divergence",
            "fig1142-replay-divergence",
        ),
        RuntimeEffectCommand::LlmCall {
            request: {
                let mut request = Box::new(request);
                request.model.model = lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    lash_core::LlmProfileKey::new("test"),
                    request.model.metadata().clone(),
                );
                request
            },
        },
    )
}

impl Fig1142ReplayDivergence for Fig1142ReplayDivergenceImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig1142ReplayDivergenceInput>,
    ) -> HandlerResult<Json<bool>> {
        let model_version = self.model_version.load(Ordering::SeqCst);
        RestateRuntimeEffectController::new_for_test(ctx)
            .execute_effect(
                fig1142_llm_envelope(model_version),
                RuntimeEffectLocalExecutor::testing(|_| async {
                    self.executions.fetch_add(1, Ordering::SeqCst);
                    Ok(fig793_llm_outcome())
                }),
            )
            .await
            .map_err(|error| -> HandlerError {
                // A turn handler's contract (lash_restate::turn_service): a
                // divergence parks, so the attempt fails retryably and the
                // invocation keeps its journal.
                if error.turn_failure_cause() == lash_core::TurnFailureCause::Parked {
                    crate::parked_turn_failure(error)
                } else {
                    TerminalError::from_error(error).into()
                }
            })?;
        Ok(Json(true))
    }
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` to run
/// `model`, unbounded, unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
pub(crate) async fn created_session(
    core: &lash::LashCore,
    model: &str,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}

mod admin_namespace_filters;
mod lost_run_recovery;

/// L02: a sibling's registered wake requests progress without terminating X.
#[restate_sdk::workflow]
trait TerminalStateGuardProbe {
    async fn sibling_wake() -> HandlerResult<Json<u32>>;
    async fn hidden_terminal() -> HandlerResult<Json<()>>;
    async fn terminal_notification() -> HandlerResult<Json<()>>;
    async fn parked_run_selection() -> HandlerResult<Json<()>>;
    async fn parked_realization_attach() -> HandlerResult<Json<()>>;
}

struct TerminalStateGuardProbeImpl;

impl TerminalStateGuardProbe for TerminalStateGuardProbeImpl {
    async fn sibling_wake(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<u32>> {
        // Retain the real SDK attempt state while reproducing the registered
        // sibling wake from the rejected guard experiment.
        let sdk_result =
            restate_sdk::context::ContextSideEffects::run(&ctx, || async { Ok(Json(42_u32)) })
                .start();
        let state = sdk_result.inner_context();
        let mut polls = 0;
        let mut registered = None;
        let result = std::future::poll_fn(|cx| {
            polls += 1;
            match polls {
                1 => {
                    registered = Some(cx.waker().clone());
                    Poll::Pending
                }
                2 => {
                    registered.take().expect("registered sibling").wake();
                    Poll::Pending
                }
                3 => Poll::Ready(42),
                _ => panic!("completed result was re-polled"),
            }
        });
        let mut result = Box::pin(guard_restate_context_future(result, state.clone()));
        std::future::poll_fn(|cx| {
            assert!(result.as_mut().poll(cx).is_pending());
            assert!(result.as_mut().poll(cx).is_pending());
            assert!(!state.is_failed_or_suspended());
            assert_eq!(result.as_mut().poll(cx), Poll::Ready(42));
            assert!(result.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        Ok(Json(42))
    }

    async fn hidden_terminal(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<()>> {
        let mut timer = Box::pin(restate_sdk::context::ContextTimers::sleep(
            &ctx,
            Duration::from_secs(2),
        ));
        let state = timer.inner_context();
        let mut polls = 0;
        // Poll the SDK beneath a silent waker: the recorded terminal state is
        // authoritative even when its synchronous wake never reaches the guard.
        let result = std::future::poll_fn(|_| {
            polls += 1;
            timer.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        });
        let mut result = Box::pin(guard_restate_context_future(result, state.clone()));
        std::future::poll_fn(|cx| {
            assert!(result.as_mut().poll(cx).is_pending());
            assert!(
                state.is_failed_or_suspended(),
                "SDK hid its terminal error behind Pending"
            );
            assert!(result.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(result);
        assert_eq!(
            polls, 1,
            "recorded terminal Pending must never re-enter the SDK"
        );
        Ok(Json(()))
    }

    async fn terminal_notification(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<()>> {
        let mut result = Box::pin(
            restate_sdk::context::ContextSideEffects::run(&ctx, || async {
                Ok(Json(Fig1464UnjournalableEffectResult))
            })
            .start(),
        );
        let state = result.inner_context();
        let mut result = Box::pin(guard_restate_context_future(
            std::future::poll_fn(|_| {
                result
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
            }),
            state.clone(),
        ));
        std::future::poll_fn(|cx| {
            assert!(result.as_mut().poll(cx).is_pending());
            assert!(state.is_failed_or_suspended());
            assert!(
                state.is_failed_or_suspended(),
                "observation must remain repeatable"
            );
            Poll::Ready(())
        })
        .await;
        Ok(Json(()))
    }

    async fn parked_run_selection(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<()>> {
        // A Run owner parks on the VM selector over a source the journal
        // cannot answer yet; the closed input suspends the attempt there.
        let timer = restate_sdk::context::ContextTimers::sleep(&ctx, Duration::from_secs(2));
        let key = SealedDurableFuture::handle(&timer)
            .map(u32::from)
            .expect("the sleep registers a notification");
        let state = timer.inner_context();
        let mut selection = RestateControllerContext::select_run_sources(&ctx, vec![key]);
        assert_owner_repoll_stays_pending(&mut selection, &state).await;
        Ok(Json(()))
    }

    async fn parked_realization_attach(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<()>> {
        // The registered sleep lends its SDK attempt state; the Run owner
        // parks on the realization's receipt the journal cannot answer yet.
        let timer = restate_sdk::context::ContextTimers::sleep(&ctx, Duration::from_secs(2));
        let state = timer.inner_context();
        let selectable = RestateControllerContext::attach_run_realization(
            &ctx,
            "inv_parked_realization".to_owned(),
        )
        .await
        .expect("the attach registers");
        let mut receipt = selectable.value;
        assert_owner_repoll_stays_pending(&mut receipt, &state).await;
        Ok(Json(()))
    }
}

/// Poll `parked` the way an effect owner does: once per pass, under its own
/// waker, with nothing above it consulting the attempt's terminal state
/// between passes.
async fn assert_owner_repoll_stays_pending<T>(
    parked: &mut Pin<Box<dyn Future<Output = T> + Send + '_>>,
    state: &restate_sdk::endpoint::ContextInternal,
) {
    std::future::poll_fn(|_| {
        let mut owner = Context::from_waker(Waker::noop());
        assert!(parked.as_mut().poll(&mut owner).is_pending());
        assert!(
            state.is_failed_or_suspended(),
            "SDK hid its terminal error behind Pending"
        );
        assert!(
            parked.as_mut().poll(&mut owner).is_pending(),
            "the owner's next pass must not re-enter the SDK"
        );
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn l02_a_registered_sibling_wake_keeps_a_pending_result_live() {
    let endpoint = Endpoint::builder()
        .bind(TerminalStateGuardProbeImpl.serve())
        .build();
    invoke_endpoint(
        &endpoint,
        "TerminalStateGuardProbe",
        "sibling_wake",
        "sibling-wake",
        &(),
    )
    .await
    .expect("sibling wake preserves the pending result");
}

/// L02: the SDK's hidden terminal Pending ends polling even without a wake.
#[tokio::test]
async fn l02_a_hidden_terminal_pending_fuses_without_a_synchronous_wake() {
    let endpoint = Endpoint::builder()
        .bind(TerminalStateGuardProbeImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "TerminalStateGuardProbe",
        "hidden_terminal",
        "hidden-terminal",
        &(),
    )
    .await
    .expect("hidden terminal must not re-enter the SDK");
    assert_eq!(
        restate_message_types(&output).unwrap(),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
}

/// L02: an effect owner re-polling its parked VM source selection after the
/// SDK recorded a suspension stays Pending instead of re-entering the SDK
/// (FIG-5068).
#[tokio::test]
async fn l02_a_parked_run_source_selection_fuses_on_a_hidden_terminal() {
    let endpoint = Endpoint::builder()
        .bind(TerminalStateGuardProbeImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "TerminalStateGuardProbe",
        "parked_run_selection",
        "parked-run-selection",
        &(),
    )
    .await
    .expect("a parked selection must not re-enter the SDK");
    assert_eq!(
        restate_message_types(&output).unwrap(),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
}

/// L02: the same for a parked realization receipt (FIG-5068).
#[tokio::test]
async fn l02_a_parked_realization_receipt_fuses_on_a_hidden_terminal() {
    let endpoint = Endpoint::builder()
        .bind(TerminalStateGuardProbeImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "TerminalStateGuardProbe",
        "parked_realization_attach",
        "parked-realization-attach",
        &(),
    )
    .await
    .expect("a parked realization receipt must not re-enter the SDK");
    assert_eq!(
        restate_message_types(&output).unwrap().last(),
        Some(&RESTATE_SUSPENSION_MESSAGE_TYPE)
    );
}

/// L02: observing terminal state leaves the outer handler's error notification intact.
#[tokio::test]
async fn l02_terminal_observation_preserves_the_outer_handler_notification() {
    let endpoint = Endpoint::builder()
        .bind(TerminalStateGuardProbeImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "TerminalStateGuardProbe",
        "terminal_notification",
        "terminal-notification",
        &(),
    )
    .await
    .expect("outer handler must receive its recorded failure");
    assert!(
        restate_error_message(&output)
            .is_some_and(|message| message.contains("cannot be journaled"))
    );
    let types = restate_message_types(&output).unwrap();
    assert!(
        !types.contains(&RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE)
            && !types.contains(&RESTATE_END_MESSAGE_TYPE),
        "terminal observation must not fabricate a successful handler output"
    );
}
mod conformance_harness;
mod harness_store_tiers;

mod wait_generation_endpoint;

pub(crate) struct NoIntentsRealizer;
#[async_trait::async_trait]
impl lash_core::tool_dispatch::ToolRealizer for NoIntentsRealizer {
    async fn realize(
        &self,
        _: lash_core::tool_dispatch::RealizationRequest,
        _: lash_core::ScopedEffectController<'_>,
    ) -> Result<lash_core::tool_dispatch::RealizationReceipt, lash_core::RuntimeEffectControllerError>
    {
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeToolRunShape,
            "fixture admits no intent realization",
        ))
    }
}

struct UnjournalableFixtureStep(String);
impl crate::JournalStep for UnjournalableFixtureStep {
    type Output = Fig1464UnjournalableEffectResult;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "fixture.UnjournalableFixtureStep";
    fn instance(&self) -> String {
        self.0.clone()
    }
}

struct UnreadableFixtureStep(String);
impl crate::JournalStep for UnreadableFixtureStep {
    type Output = Fig1464UnreadableJournaledResult;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "fixture.UnreadableFixtureStep";
    fn instance(&self) -> String {
        self.0.clone()
    }
}

struct ScalarFixtureStep(String);
impl crate::JournalStep for ScalarFixtureStep {
    type Output = u32;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "fixture.ScalarFixtureStep";
    fn instance(&self) -> String {
        self.0.clone()
    }
}
