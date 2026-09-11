#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by protocol fixtures"
)]

//! Tests for the Restate adapter (extracted from lib.rs).
//!
//! The obsolete FIG-1127 nested-command refusal fixture was removed with the
//! journal-capable leaf surface. Current intent and process-replay laws cover
//! Restate at its sanctioned seams.

use super::*;
use crate::controller::context::guard_restate_context_future;
use crate::controller::{
    RecordedRuntimeEffect, RestateEffectExecution, restate_await_event_turn_cancel_wait_request,
    restate_effect_execution, restate_effect_name, restate_timer_turn_cancel_wait_request,
    validate_recorded_effect_envelope,
};
use crate::durable_wait::{
    DURABLE_WAIT_INDEX_IDENTITY_EPOCH, DURABLE_WAIT_INDEX_METADATA_KEY,
    RestateDurableWaitIndexMetadata, RestateTurnCancelWake, durable_wait_address_from_state_key,
    durable_wait_index_state_key, restate_await_event_key, split_cancellable_waits,
    validate_durable_wait_index_epoch,
};
use crate::process::{
    boundary_must_be_declined, handler_error_from_plugin, missing_segment_is_superseded,
    process_segment_workflow_key, restate_process_terminal_await_key,
    restate_process_terminal_output, restate_process_terminal_resolution,
    segment_execution_authority, terminal_completion_workflow_key, workflow_key_authority,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use lash_core::ProcessClockRebind as _;
use lash_core::ProcessWorkSubstrate as _;
use lash_core::TestProcessRegistryWriteExt;
use lash_core::facade_support::{ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation};
use lash_core::{
    AbandonWriter, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost,
    ExecutionScope, PluginError, ProcessAwaitOutput, ProcessCommand, ProcessEffectOutcome,
    ProcessExecutionContext, ProcessExternalRef, ProcessRegistry, QueuedLaneAcquisition,
    QueuedLaneAttempt, QueuedLaneProbe, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeInvocation, ScopedEffectController,
    facade_support::DurableProcessWorker, facade_support::TurnAddress, facade_support::TurnAttach,
};
use lash_core::{ProcessInput, ProcessRegistration, TriggerStore};
use lash_http_transport::HttpRequest;
use lash_http_transport::{HttpResponse, HttpResponseBody, HttpTransport, HttpTransportError};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::{MutexExt, RwLockExt};
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

mod effect_group_conformance;
mod effect_group_sdk_preconditions;
mod effect_group_shape;
mod endpoint_protocol;
mod process_tool_replay;
mod replay_corpus;
mod tool_context_conformance;
mod turn_cancel_modes;
use endpoint_protocol::{
    durable_wait_index_call_response, encode_call_replay, encode_captured_run_and_call_replay,
    encode_captured_run_and_interrupted_call_replay, encode_captured_run_command_replay,
    encode_completed_gate_sleep_replay, encode_completed_intent_drain_replay,
    encode_completed_sleep_replay, encode_one_way_call_replay, encode_process_segment_send_replay,
    encode_process_terminal_delivery_replay, encode_recorded_commands_replay, encode_run_replay,
    encode_two_one_way_calls_and_call_replay, invoke_endpoint, invoke_endpoint_body,
    invoke_endpoint_body_open, invoke_endpoint_body_with_json_call_responses, invoke_endpoint_open,
    invoke_endpoint_with_named_call_responses, invoke_endpoint_with_scripted_responses,
    invoke_process_workflow_endpoint, restate_call_frames, restate_command_frame_types,
    restate_completed_promise, restate_error_message, restate_message_types,
    restate_output_failure_message, restate_output_json,
};

fn registry_local_executor(
    registry: Arc<dyn ProcessRegistry>,
) -> RuntimeEffectLocalExecutor<'static> {
    let process_work = Arc::new(lash_core::NativeProcessWork::for_registry(Arc::clone(
        &registry,
    )));
    RuntimeEffectLocalExecutor::processes(registry, process_work)
}

struct QueuedLaneProbeDouble {
    attempts: Mutex<VecDeque<QueuedLaneAttempt>>,
    try_calls: AtomicUsize,
    pause_calls: AtomicUsize,
}

impl QueuedLaneProbeDouble {
    fn new(attempts: impl IntoIterator<Item = QueuedLaneAttempt>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into_iter().collect()),
            try_calls: AtomicUsize::new(0),
            pause_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl QueuedLaneProbe for QueuedLaneProbeDouble {
    async fn try_acquire(&self) -> Result<QueuedLaneAttempt, lash_core::RuntimeError> {
        self.try_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .attempts
            .lock_recover()
            .pop_front()
            .expect("queued-lane probe attempt"))
    }

    async fn pause(&self, _slice: Duration) {
        self.pause_calls.fetch_add(1, Ordering::SeqCst);
    }
}

async fn assert_restate_queued_lane_conformance() {
    let controller_probe = Arc::new(QueuedLaneProbeDouble::new([
        QueuedLaneAttempt::Busy(lash_core::testing::queued_lane_holder_for_testing(7_400)),
        QueuedLaneAttempt::Busy(lash_core::testing::queued_lane_holder_for_testing(7_401)),
    ]));
    let controller = RestateRuntimeEffectController::new(Arc::new(RecordingContext::default()));
    let result = lash_conformance::durable_queued_drain_wait_contract(
        &controller,
        Arc::clone(&controller_probe) as Arc<dyn QueuedLaneProbe>,
    )
    .await;
    let Err(error) = result else {
        panic!("the Restate handler controller must use the engine-paced lane wait")
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
    );
    assert!(error.is_retryable());
    assert_eq!(controller_probe.try_calls.load(Ordering::SeqCst), 2);
    assert_eq!(controller_probe.pause_calls.load(Ordering::SeqCst), 1);

    let host_probe = Arc::new(QueuedLaneProbeDouble::new([QueuedLaneAttempt::Busy(
        lash_core::testing::queued_lane_holder_for_testing(7_400),
    )]));
    let host = RestateEffectHost::new("http://127.0.0.1:8080");
    let result = lash_conformance::durable_queued_drain_wait_contract(
        &host,
        Arc::clone(&host_probe) as Arc<dyn QueuedLaneProbe>,
    )
    .await
    .expect("deployment-host queued-lane default");
    assert!(matches!(result, QueuedLaneAcquisition::NotAcquired));
    assert_eq!(host_probe.try_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host_probe.pause_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn restate_scope_controller_refuses_wrong_scope_before_index_or_local_execution() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scoped = controller
        .scoped_effect_controller(ExecutionScope::process("admitted-restate-process"))
        .expect("scoped Restate controller");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            lash_core::EffectAddress::new(
                ExecutionScope::process("wrong-restate-process"),
                "shared-replay-key",
            )
            .expect("wrong-scope effect address"),
            lash_core::RuntimeAttribution::none(),
            "restate-scope-admission-sleep",
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 1 },
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

fn registry_process_wiring(registry: Arc<dyn ProcessRegistry>) -> lash_core::ProcessWorkWiring {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let registry = Arc::clone(watched.registry());
    lash_core::ProcessWorkWiring::new(
        watched,
        Arc::new(lash_core::NativeProcessWork::for_registry(registry)),
    )
}

fn test_turn_effect_invocation(
    session_id: &str,
    turn_id: &str,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: impl Into<String>,
    replay_key: impl Into<String>,
) -> RuntimeInvocation {
    RuntimeInvocation::effect(
        lash_core::EffectAddress::new(ExecutionScope::turn(session_id, turn_id), replay_key)
            .expect("valid Restate test effect address"),
        lash_core::RuntimeAttribution::for_turn(
            session_id,
            turn_id,
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

struct PanicsWhenPolledAfterReady {
    completed: bool,
}

impl Future for PanicsWhenPolledAfterReady {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.completed, "non-fused future was polled after ready");
        self.completed = true;
        Poll::Ready(())
    }
}

struct CancelOnWake {
    parent: Waker,
    cancellation: tokio_util::sync::CancellationToken,
}

impl std::task::Wake for CancelOnWake {
    fn wake(self: Arc<Self>) {
        self.cancellation.cancel();
        self.parent.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.cancellation.cancel();
        self.parent.wake_by_ref();
    }
}

struct CancelOnWakeFuture<F> {
    future: Pin<Box<F>>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl<F: Future> Future for CancelOnWakeFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let waker = Waker::from(Arc::new(CancelOnWake {
            parent: cx.waker().clone(),
            cancellation: self.cancellation.clone(),
        }));
        self.future.as_mut().poll(&mut Context::from_waker(&waker))
    }
}

#[test]
fn restate_context_future_repoll_after_ready_stays_pending() {
    let mut future = Box::pin(guard_restate_context_future(PanicsWhenPolledAfterReady {
        completed: false,
    }));
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(()));
    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
}

/// A future that wakes its own task once before completing - the shape
/// `yield_now` and a re-armed `FuturesUnordered` both produce.
fn self_waking_then_ready() -> impl Future<Output = u32> {
    let mut woke = false;
    std::future::poll_fn(move |cx: &mut Context<'_>| {
        if woke {
            return Poll::Ready(7);
        }
        woke = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
}

/// FIG-1464: `ctx.run` polls arbitrary lash code, and a `RuntimeEffectCommand::LlmCall`
/// reaches this seam with no task boundary in between. A self-wake from that
/// code arrives before the closure has produced a value, so it is not the SDK's
/// terminal park and must not fuse the run - fusing it would hang the turn while
/// holding a paid completion.
#[test]
fn restate_run_future_closure_self_wake_does_not_fuse() {
    let relay = Arc::new(crate::controller::context::ClosureWakeRelay::default());
    let mut future = Box::pin(crate::controller::context::guard_restate_run_future(
        crate::controller::context::relay_closure_wakes(
            self_waking_then_ready(),
            Arc::clone(&relay),
        ),
        relay,
    ));
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(
        future.as_mut().poll(&mut context),
        Poll::Ready(7),
        "a wake attributed to the run closure must leave the run future pollable"
    );
}

/// The same wake shape from anywhere other than the closure's own future is the
/// SDK recording a terminal handler state - the intercept-error `wake_by_ref`
/// included, which is also the only wake the replay path can produce because the
/// closure is never invoked there. It must fuse.
#[test]
fn restate_run_future_unattributed_wake_fuses() {
    let relay = Arc::new(crate::controller::context::ClosureWakeRelay::default());
    let mut future = Box::pin(crate::controller::context::guard_restate_run_future(
        self_waking_then_ready(),
        relay,
    ));
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(
        future.as_mut().poll(&mut context),
        Poll::Pending,
        "an unattributed synchronous wake must never re-enter the SDK future"
    );
}

/// A closure-side future that is woken from another thread while the guard is
/// mid-poll - the shape a provider stream woken by the tokio I/O driver
/// produces. The wake is joined before returning, so it is guaranteed to land
/// inside this very poll.
fn cross_thread_woken_closure_future() -> impl Future<Output = u32> {
    let mut woke = false;
    std::future::poll_fn(move |cx: &mut Context<'_>| {
        if woke {
            return Poll::Ready(11);
        }
        woke = true;
        let waker = cx.waker().clone();
        std::thread::spawn(move || waker.wake())
            .join()
            .expect("cross-thread closure wake");
        Poll::Pending
    })
}

/// An SDK-shaped future that parks terminally: it polls its inner future, wakes
/// the task synchronously on the polling thread and returns `Pending`, exactly
/// as `InterceptErrorFuture` does after `ctx.fail`. Being already resolved, a
/// second poll is the bug this guard exists to prevent.
struct SdkTerminalPark<F> {
    inner: Pin<Box<F>>,
    parked: bool,
}

impl<F> Future for SdkTerminalPark<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(
            !this.parked,
            "the resolved SDK future must never be polled again"
        );
        let _ = this.inner.as_mut().poll(cx);
        this.parked = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// FIG-1464 round 3, residual A: a cross-thread wake from inside the run closure
/// landing during the same poll as the SDK's terminal park must not mask that
/// park. Attributing by arithmetic did exactly that - the closure wake was
/// invisible to the tracker's same-thread gate yet still counted against it, so
/// the two cancelled out, the guard stayed unfused and the next poll re-entered
/// the resolved SDK future.
#[test]
fn restate_run_future_cross_thread_closure_wake_does_not_mask_the_terminal_park() {
    let relay = Arc::new(crate::controller::context::ClosureWakeRelay::default());
    let mut future = Box::pin(crate::controller::context::guard_restate_run_future(
        SdkTerminalPark {
            inner: Box::pin(crate::controller::context::relay_closure_wakes(
                cross_thread_woken_closure_future(),
                Arc::clone(&relay),
            )),
            parked: false,
        },
        relay,
    ));
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(
        future.as_mut().poll(&mut context),
        Poll::Pending,
        "a cross-thread closure wake must not cancel out the SDK's terminal park"
    );
}

/// Restate service-protocol message types used by the FIG-779/FIG-790 gates.
/// `restate_sdk_shared_core::service_protocol::header` keeps these private, so
/// they are restated here (`SleepCommand = 0x040C`, `Suspension = 0x0001`,
/// `CallCommand = 0x040D`, `CompletePromiseCommand = 0x040B`,
/// `OutputCommand = 0x0401`, `End = 0x0003`).
const RESTATE_SLEEP_COMMAND_MESSAGE_TYPE: u16 = 0x040C;
const RESTATE_CALL_COMMAND_MESSAGE_TYPE: u16 = 0x040D;
const RESTATE_SUSPENSION_MESSAGE_TYPE: u16 = 0x0001;
const RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE: u16 = 0x040B;
const RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE: u16 = 0x0401;
const RESTATE_END_MESSAGE_TYPE: u16 = 0x0003;
const RESTATE_RUN_COMMAND_MESSAGE_TYPE: u16 = 0x0411;

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

    async fn cancel_on_suspend_wake(
        input: Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>>;

    async fn cancel_before_sleep(
        input: Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>>;

    async fn repoll_fused_timer(input: Json<Fig779TimerGuardReproInput>)
    -> HandlerResult<Json<()>>;
}

struct Fig779TimerGuardReproImpl;

impl Fig779TimerGuardRepro for Fig779TimerGuardReproImpl {
    /// The production geometry repaired by FIG-779: `turn_cancel == None`,
    /// which is what every sleep inside a process body uses (process runners
    /// call `without_turn_cancel_observation`). That branch guards `ctx.sleep()`
    /// with `RestateContextFuture` inside the timer/cancellation race.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let outcome = RestateControllerContext::sleep_or_turn_cancel(
            &ctx,
            Duration::from_millis(input.duration_ms),
            None,
            tokio_util::sync::CancellationToken::new(),
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

    async fn cancel_on_suspend_wake(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let race = RestateControllerContext::sleep_or_turn_cancel(
            &ctx,
            Duration::from_millis(input.duration_ms),
            None,
            cancellation.clone(),
        );
        CancelOnWakeFuture {
            future: Box::pin(race),
            cancellation,
        }
        .await?;
        Ok(Json(()))
    }

    async fn cancel_before_sleep(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let outcome = RestateControllerContext::sleep_or_turn_cancel(
            &ctx,
            Duration::from_millis(input.duration_ms),
            None,
            cancellation,
        )
        .await?;
        assert!(matches!(
            outcome,
            RestateTurnCancelRaceOutcome::TurnCancelled
        ));
        Ok(Json(()))
    }

    async fn repoll_fused_timer(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig779TimerGuardReproInput>,
    ) -> HandlerResult<Json<()>> {
        let timer = guard_restate_context_future(restate_sdk::context::ContextTimers::sleep(
            &ctx,
            Duration::from_millis(input.duration_ms),
        ));
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
/// controller seam, driven through the one geometry that turns an SDK-level run
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
        let mut run =
            RestateControllerContext::run_json_send(&ctx, input.effect_name, None, async {
                Fig1464UnjournalableEffectResult
            });
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
        let mut run =
            RestateControllerContext::run_json_send(&ctx, input.effect_name, None, async {
                Fig1464UnreadableJournaledResult
            });
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
        let Json(value) =
            RestateControllerContext::run_json_send(&ctx, input.effect_name, None, async {
                41_u32
            })
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
        let Json(value) =
            RestateControllerContext::run_json_send(&ctx, input.effect_name, None, async {
                tokio::task::yield_now().await;
                41_u32
            })
            .await?;
        Ok(Json(value + 1))
    }
}

struct Fig779DurableCancelTransport {
    registry: Arc<dyn ProcessRegistry>,
    process_id: ProcessId,
}

impl std::fmt::Debug for Fig779DurableCancelTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Fig779DurableCancelTransport")
            .field("process_id", &self.process_id)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl HttpTransport for Fig779DurableCancelTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        let cancellation_is_durable = self
            .registry
            .events_after(&self.process_id, 0)
            .await
            .map_err(|error| HttpTransportError::new(error.to_string()))?
            .iter()
            .any(|event| event.event_type == "process.cancel_requested");
        if !cancellation_is_durable {
            return std::future::pending().await;
        }
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(r#""cancel_requested""#),
        })
    }
}

#[derive(Debug)]
struct Fig779SuspendingProcessRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig779SuspendingProcessRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let outcome = scoped_effect_controller
            .controller()
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, "fig779-redrive-sleep"),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: 60_000,
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation.clone())
                    .with_turn_cancel_observation(false),
            )
            .await;
        match outcome {
            Ok(RuntimeEffectOutcome::Sleep) => Ok(process_success(serde_json::Value::Null).into()),
            Err(_) if cancellation.is_cancelled() => Ok(process_cancellation(
                format!(
                    "process `{}` observed durable cancellation",
                    registration.id
                ),
                None,
            )
            .into()),
            Err(error) => Err(PluginError::Session(error.to_string())),
            Ok(other) => Err(PluginError::Session(format!(
                "unexpected sleep outcome: {other:?}"
            ))),
        }
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug)]
struct Fig788TerminalRedriveRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig788TerminalRedriveRunner {
    async fn run_process_segment(
        &self,
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
                    runtime_invocation(RuntimeEffectKind::Sleep, "fig788-terminal-redrive-sleep"),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: 60_000,
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation).with_turn_cancel_observation(false),
            )
            .await
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(process_success(serde_json::json!({"runner": "replayed"})).into())
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug)]
struct Fig788SegmentBoundaryRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig788SegmentBoundaryRunner {
    async fn run_process_segment(
        &self,
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

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug)]
struct Fig788OrdinalOneTerminalRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig788OrdinalOneTerminalRunner {
    async fn run_process_segment(
        &self,
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

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug)]
struct Fig811EffectfulOrdinalOneTerminalRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for Fig811EffectfulOrdinalOneTerminalRunner {
    async fn run_process_segment(
        &self,
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
                    runtime_invocation(RuntimeEffectKind::Sleep, "fig811-effectful-terminal-sleep"),
                    RuntimeEffectCommand::Sleep { duration_ms: 1 },
                ),
                RuntimeEffectLocalExecutor::sleep(cancellation).with_turn_cancel_observation(false),
            )
            .await
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(process_success(serde_json::json!({"segment": 1, "effectful_terminal": true})).into())
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
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
        let controller = RestateRuntimeEffectController::new(ctx);
        let report = self
            .router
            .emit(input.occurrence, &controller)
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

fn fig793_llm_envelope() -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::LlmCall, "fig793-llm"),
        RuntimeEffectCommand::LlmCall {
            request: Box::new(llm_spec()),
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
    }
}

struct Fig793LlmGateRedriveImpl;

impl Fig793LlmGateRedrive for Fig793LlmGateRedriveImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig793LlmGateRedriveInput>,
    ) -> HandlerResult<Json<bool>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        controller
            .execute_effect(
                fig793_llm_envelope(),
                RuntimeEffectLocalExecutor::testing(|_envelope| async { Ok(fig793_llm_outcome()) }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let key = restate_await_event_key(
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
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("fig1126-revoked-call"),
        )
        .map_err(TerminalError::from_error)?;
        let outcome = RestateRuntimeEffectController::new(ctx)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "fig1126-revoked-await"),
                    RuntimeEffectCommand::AwaitEvent { key },
                ),
                RuntimeEffectLocalExecutor::await_event(
                    tokio_util::sync::CancellationToken::new(),
                    None,
                )
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
mod conformance_and_poison;
mod effect_execution;
mod process_await_redrive;
mod process_recovery;
mod process_registry_core;
mod process_registry_replay;
mod process_workflow;
mod recording_context;
mod restate_redrive;

use cancellation_and_effects::*;
use conformance_and_poison::*;
use effect_execution::*;
use process_await_redrive::*;
use process_recovery::*;
use process_registry_core::*;
use process_registry_replay::*;
use process_workflow::*;
use recording_context::*;

#[restate_sdk::workflow]
trait Fig1126PendingToolRedrive {
    async fn run(input: Json<Fig1126PendingToolRedriveInput>) -> HandlerResult<Json<Resolution>>;
}

struct Fig1126PendingToolRedriveImpl {
    tool_launches: Arc<AtomicUsize>,
    terminal_resumes: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig1142ReplayDivergenceInput;

#[restate_sdk::workflow]
trait Fig1142ReplayDivergence {
    async fn run(input: Json<Fig1142ReplayDivergenceInput>) -> HandlerResult<Json<bool>>;
}

struct Fig1142ReplayDivergenceImpl {
    model_version: Arc<AtomicUsize>,
}

fn fig1142_llm_envelope(model_version: usize) -> RuntimeEffectEnvelope {
    let mut request = llm_spec();
    request.model = format!("model-v{model_version}");
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
            request: Box::new(request),
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
        RestateRuntimeEffectController::new(ctx)
            .execute_effect(
                fig1142_llm_envelope(model_version),
                RuntimeEffectLocalExecutor::testing(|_| async { Ok(fig793_llm_outcome()) }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        Ok(Json(true))
    }
}

impl Fig1126PendingToolRedrive for Fig1126PendingToolRedriveImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig1126PendingToolRedriveInput>,
    ) -> HandlerResult<Json<Resolution>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let scope = durable_turn_scope("fig1126-session", "fig1126-turn");
        let pending_scope = scope.clone();
        let pending = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    test_turn_effect_invocation(
                        "fig1126-session",
                        "fig1126-turn",
                        0,
                        0,
                        "fig1126-pending-tool",
                        "fig1126-pending-tool",
                    ),
                    RuntimeEffectCommand::ToolAttempt {
                        call: prepared_tool_call_with("fig1126-call", "fig1126_pending_tool"),
                        execution_grant: None,
                        attempt: 1,
                        max_attempts: 1,
                    },
                ),
                RuntimeEffectLocalExecutor::testing(|_envelope| async {
                    self.tool_launches.fetch_add(1, Ordering::SeqCst);
                    let key = controller
                        .await_event_key(
                            &pending_scope,
                            AwaitEventWaitIdentity::tool_completion("fig1126-call"),
                        )
                        .await
                        .map_err(lash_core::RuntimeEffectControllerError::from)?;
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Pending {
                            key: Box::new(key),
                            pending: lash_core::PendingCompletion::new(),
                            duration_ms: 0,
                        }),
                        triggers: Vec::new(),
                    })
                }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = pending else {
            return Err(TerminalError::new("FIG-1126 fixture expected a tool outcome").into());
        };
        let lash_core::ToolAttemptLaunch::Pending { key, .. } = *launch else {
            return Err(TerminalError::new("FIG-1126 fixture expected a pending tool").into());
        };
        let waited = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    test_turn_effect_invocation(
                        "fig1126-session",
                        "fig1126-turn",
                        0,
                        0,
                        "fig1126-await-pending-tool",
                        "fig1126-await-pending-tool",
                    ),
                    RuntimeEffectCommand::AwaitEvent { key: *key },
                ),
                RuntimeEffectLocalExecutor::await_event(
                    tokio_util::sync::CancellationToken::new(),
                    None,
                )
                .with_turn_cancel_scope(scope),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::AwaitEvent { resolution } = waited else {
            return Err(TerminalError::new("FIG-1126 fixture expected a wait outcome").into());
        };
        self.terminal_resumes.fetch_add(1, Ordering::SeqCst);
        Ok(Json(resolution))
    }
}
