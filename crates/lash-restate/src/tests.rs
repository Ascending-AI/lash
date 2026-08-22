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
    RestateDurableWaitIndexMetadata, RestateDurableWaitIndexState, RestateTurnCancelWake,
    restate_await_event_key, split_cancellable_waits, validate_durable_wait_index_epoch,
};
use crate::process::{
    boundary_must_be_declined, missing_segment_is_superseded, process_segment_workflow_key,
    restate_process_terminal_await_key, restate_process_terminal_output,
    restate_process_terminal_resolution, retryable_registry_error, segment_execution_authority,
    terminal_completion_workflow_key, validate_segment_program_hash, workflow_key_authority,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use lash_core::TestProcessRegistryWriteExt;
use lash_core::facade_support::{ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation};
use lash_core::{
    AbandonWriter, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost,
    ExecutionScope, PluginError, ProcessAwaitOutput, ProcessCommand, ProcessEffectOutcome,
    ProcessExecutionContext, ProcessExternalRef, ProcessRegistry, Resolution, ResolveOutcome,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation, ScopedEffectController,
    facade_support::DurableProcessWorker, facade_support::ProcessAttach,
    facade_support::ProcessRunHandle, facade_support::TurnAddress, facade_support::TurnAttach,
};
use lash_core::{ProcessInput, ProcessRegistration, RuntimeScope, TriggerStore};
use lash_http_transport::HttpRequest;
use lash_http_transport::{HttpResponse, HttpResponseBody, HttpTransport, HttpTransportError};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use lash_sansio::sync::{MutexExt, RwLockExt};
use restate_sdk::context::{ContextClient, RequestTarget, RunRetryPolicy, WorkflowContext};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::prelude::Endpoint;
use restate_sdk::serde::Json;
use restate_sdk::service::Discoverable;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

mod endpoint_protocol;
mod process_tool_replay;
mod replay_corpus;
mod tool_context_conformance;
use endpoint_protocol::{
    encode_call_replay, encode_captured_run_and_call_replay,
    encode_captured_run_and_interrupted_call_replay, encode_captured_run_command_replay,
    encode_completed_captured_sleep_replay, encode_completed_gate_sleep_replay,
    encode_completed_intent_drain_replay, encode_completed_sleep_replay,
    encode_effectful_process_terminal_replay, encode_one_way_call_replay,
    encode_pending_sleep_replay, encode_process_segment_send_replay,
    encode_process_terminal_delivery_replay, encode_run_replay,
    encode_two_one_way_calls_and_call_replay, invoke_endpoint, invoke_endpoint_body,
    invoke_endpoint_body_open, invoke_endpoint_body_with_json_call_responses, invoke_endpoint_open,
    invoke_endpoint_with_named_call_responses, invoke_endpoint_with_scripted_responses,
    invoke_process_workflow_endpoint, restate_call_frames, restate_command_frame_types,
    restate_error_message, restate_message_types, restate_output_failure_message,
    restate_output_json,
};

fn durable_turn_scope(session_id: impl Into<String>, turn_id: impl Into<String>) -> ExecutionScope {
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
    process_id: String,
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
            Ok(RuntimeEffectOutcome::Sleep) => Ok(ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            }
            .into()),
            Err(_) if cancellation.is_cancelled() => Ok(ProcessAwaitOutput::Cancelled {
                message: format!(
                    "process `{}` observed durable cancellation",
                    registration.id
                ),
                raw: None,
                control: None,
            }
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
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"runner": "replayed"}),
            control: None,
        }
        .into())
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
                program_hash: Some("fig788-segment-program".to_string()),
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
                program_hash: Some("fig788-terminal-program".to_string()),
                engine_state: vec![1],
            }
        );
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"segment": 1, "terminal": true}),
            control: None,
        }
        .into())
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
                program_hash: Some("fig811-effectful-terminal-program".to_string()),
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
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"segment": 1, "effectful_terminal": true}),
            control: None,
        }
        .into())
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
            full_text: "journaled response".to_string(),
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
                    RuntimeInvocation::effect(
                        RuntimeScope::for_turn("fig793-session", "fig793-turn", 1, 0),
                        "turn_cancel.after_llm.0",
                        RuntimeEffectKind::PeekAwaitEvent,
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
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("fig1142-session", "fig1142-turn", 0, 0),
            "fig1142-replay-divergence",
            RuntimeEffectKind::LlmCall,
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
                    RuntimeInvocation::effect(
                        RuntimeScope::for_turn("fig1126-session", "fig1126-turn", 0, 0),
                        "fig1126-pending-tool",
                        RuntimeEffectKind::ToolAttempt,
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
                    RuntimeInvocation::effect(
                        RuntimeScope::for_turn("fig1126-session", "fig1126-turn", 0, 0),
                        "fig1126-await-pending-tool",
                        RuntimeEffectKind::AwaitEvent,
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

#[tokio::test]
async fn fig1126_pending_tool_redrives_after_worker_loss_and_resumes_once() {
    let tool_launches = Arc::new(AtomicUsize::new(0));
    let terminal_resumes = Arc::new(AtomicUsize::new(0));
    let endpoint = Endpoint::builder()
        .bind(
            Fig1126PendingToolRedriveImpl {
                tool_launches: Arc::clone(&tool_launches),
                terminal_resumes: Arc::clone(&terminal_resumes),
            }
            .serve(),
        )
        .build();
    let workflow_key = "fig1126-process-loss-redrive";
    let input = Fig1126PendingToolRedriveInput;

    let parked = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1126PendingToolRedrive",
        "run",
        workflow_key,
        &input,
        vec![("is_revoked".to_string(), serde_json::json!(false))],
    )
    .await
    .expect("the initial worker incarnation must park on the completion key");
    assert_eq!(tool_launches.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_resumes.load(Ordering::SeqCst), 0);
    assert!(
        restate_message_types(&parked)
            .expect("decode parked attempt")
            .contains(&RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the first worker incarnation must suspend: error={:?}",
        restate_error_message(&parked)
    );
    let parked_calls = restate_call_frames(&parked).expect("decode parked call commands");
    assert_eq!(
        parked_calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "await_resolution", "register_awakeable"],
        "the fixture must park through the production await-event/cancellation controller path"
    );

    let terminal = Resolution::Ok(serde_json::json!({ "answer": "resumed" }));
    let completions = parked_calls
        .iter()
        .map(|call| {
            let completion = match call.handler.as_str() {
                "is_revoked" => serde_json::json!(false),
                "await_resolution" => serde_json::to_value(terminal.clone())
                    .expect("serialize FIG-1126 wait resolution"),
                "register_awakeable" => {
                    serde_json::to_value(RestateDurableWaitRegistration::Registered)
                        .expect("serialize FIG-1126 gate registration")
                }
                other => panic!("unexpected FIG-1126 command `{other}`"),
            };
            (call.handler.clone(), completion)
        })
        .collect::<Vec<_>>();
    let replay = encode_captured_run_and_call_replay(workflow_key, &input, &parked, &completions)
        .expect("splice the exact parked journal and resolved completion key");

    // A second endpoint invocation is a fresh handler incarnation: it has no
    // first worker stack or in-memory future, only the captured journal and the
    // durable completion. This is the in-crate process-loss/redrive seam.
    // The winning event retires its gate entry, so the redrive needs that one
    // further index response before it can produce output.
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1126PendingToolRedrive",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("redrive the parked turn after worker loss");
    assert!(
        restate_error_message(&redriven).is_none(),
        "redrive must accept the exact first-incarnation command journal: {:?}",
        restate_error_message(&redriven)
    );
    assert_eq!(restate_output_json::<Resolution>(&redriven), Some(terminal));
    assert_eq!(
        tool_launches.load(Ordering::SeqCst),
        1,
        "journal replay must not launch the pending tool a second time"
    );
    assert_eq!(
        terminal_resumes.load(Ordering::SeqCst),
        1,
        "the resolved completion must execute the terminal continuation exactly once"
    );
}

#[tokio::test]
async fn fig1126_revoked_await_refuses_before_command_on_first_execution_and_redrive() {
    let endpoint = Endpoint::builder()
        .bind(Fig1126RevokedAwaitBoundaryImpl.serve())
        .build();
    let workflow_key = "fig1126-revoked-await-boundary";
    let input = Fig1126PendingToolRedriveInput;

    let first = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1126RevokedAwaitBoundary",
        "run",
        workflow_key,
        &input,
        vec![("is_revoked".to_string(), serde_json::json!(true))],
    )
    .await
    .expect("revoked await must return a typed refusal");
    let calls = restate_call_frames(&first).expect("decode revoked await calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked"],
        "first execution must refuse before the await-event gate"
    );
    assert!(
        restate_output_failure_message(&first)
            .is_some_and(|failure| failure.contains("await_event_unknown_or_revoked")),
        "first execution must preserve the typed revoked-session refusal"
    );

    let replay = encode_call_replay(
        workflow_key,
        &input,
        &[(calls[0].clone(), Some(serde_json::json!(true)))],
        None,
    )
    .expect("splice the refusing revocation journal");
    let redriven = invoke_endpoint_body(&endpoint, "Fig1126RevokedAwaitBoundary", "run", replay)
        .await
        .expect("redrive the refusing revocation journal");
    assert!(
        restate_call_frames(&redriven)
            .expect("decode revoked await redrive calls")
            .is_empty(),
        "redrive must append no await-event gate command after refusal"
    );
    assert!(
        restate_output_failure_message(&redriven)
            .is_some_and(|failure| failure.contains("await_event_unknown_or_revoked")),
        "redrive must preserve the typed revoked-session refusal"
    );
}

/// Runbook-level replay proof: splice the captured first-incarnation run into
/// a handler whose reconstructed envelope has changed, then inspect the error
/// exactly as the Restate host renders it.
#[tokio::test]
async fn fig1142_replay_divergence_runbook_renders_path_summary() {
    let model_version = Arc::new(AtomicUsize::new(1));
    let endpoint = Endpoint::builder()
        .bind(
            Fig1142ReplayDivergenceImpl {
                model_version: Arc::clone(&model_version),
            }
            .serve(),
        )
        .build();
    let workflow_key = "fig1142-rendered-divergence";
    let input = Fig1142ReplayDivergenceInput;
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1142ReplayDivergence",
        "run",
        workflow_key,
        &input,
    )
    .await
    .expect("capture the first-incarnation runtime-effect run");
    assert!(
        restate_message_types(&suspended)
            .expect("decode first-incarnation frames")
            .contains(&RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the fixture must suspend with its runtime-effect run unresolved"
    );

    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(
            fig1142_llm_envelope(1)
                .canonical_form()
                .expect("canonical first-incarnation envelope"),
        ),
        outcome: Ok(fig793_llm_outcome()),
    };
    let replay = encode_run_replay(
        workflow_key,
        &input,
        &suspended,
        serde_json::to_value(recorded).expect("serialize first-incarnation effect"),
    )
    .expect("splice the first-incarnation runtime-effect run");

    model_version.store(2, Ordering::SeqCst);
    let redriven = invoke_endpoint_body(&endpoint, "Fig1142ReplayDivergence", "run", replay)
        .await
        .expect("the divergent redrive must render a terminal output failure");
    let rendered = restate_output_failure_message(&redriven)
        .expect("the Restate host must render the replay-divergence failure");
    assert!(
        rendered.contains("restate_effect_hash_mismatch"),
        "rendered failure omitted the typed mismatch code: {rendered}"
    );
    assert!(
        rendered.contains("divergent_paths=[command.request.model]"),
        "rendered failure omitted the per-path divergence summary: {rendered}"
    );
}

/// FIG-779 repro. A not-yet-completed durable timer is an SDK-legitimate
/// synchronous-wake-then-Pending state — it is exactly how the SDK signals a
/// durable suspension. `RestateContextFuture` must fuse that resolved inner
/// future and yield so the SDK's handler-state wrapper writes the suspension.
///
/// This drives the real Restate endpoint, context, VM, and `ctx.sleep()`
/// future. The invocation body is complete (no further frames), so the VM's
/// input is closed; `DoProgress` then hits its suspension condition, the sleep
/// resolves as `Err(Suspended)`, `DurableFutureImpl` records the state, wakes
/// synchronously and returns `Pending`. Restate closes the request stream the
/// same way whenever it parks an invocation on a pending timer.
#[tokio::test]
async fn fig779_pending_durable_timer_suspends_through_guard() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();

    let output = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "run",
        "fig779-timer",
        &Fig779TimerGuardReproInput { duration_ms: 2_000 },
    )
    .await
    .expect("pending durable timer invocation should suspend without panicking");
    let message_types = restate_message_types(&output).expect("decode Restate response frames");
    assert_eq!(
        message_types,
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "the attempt must end as a suspension, not a failed-attempt conversion"
    );
}

/// Once the SDK records suspension, its one-shot handler state is terminal for
/// the attempt. A cancellation made ready by that same synchronous wake must
/// not let the sibling race return `Cancelled` before the SDK consumes the
/// suspension. Conversely, cancellation observed while the timer is genuinely
/// pending and unfused wins after the timer command has been journaled.
#[tokio::test]
async fn fig779_sleep_suspension_and_cancellation_preserve_recorded_precedence() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();
    let input = Fig779TimerGuardReproInput { duration_ms: 2_000 };

    let suspended = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "cancel_on_suspend_wake",
        "fig779-cancel-on-suspend",
        &input,
    )
    .await
    .expect("same-poll cancellation must preserve the recorded suspension");
    assert_eq!(
        restate_message_types(&suspended).expect("decode suspended race frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );

    let cancelled = invoke_endpoint_open(
        &endpoint,
        "Fig779TimerGuardRepro",
        "cancel_before_sleep",
        "fig779-cancel-before-sleep",
        &input,
    )
    .await
    .expect("pre-existing cancellation should complete after journaling the timer");
    assert_eq!(
        restate_message_types(&cancelled).expect("decode cancelled race frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE
        ]
    );
}

#[tokio::test]
async fn fig779_suspended_process_redrive_observes_durable_cancellation() {
    let process_id = "fig779-durable-cancel-redrive";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register redrive process");
    let cancel_ingress = RestateIngressClient::new(RestateConnection::with_transport(
        "https://restate.invalid",
        Arc::new(Fig779DurableCancelTransport {
            registry: Arc::clone(&registry),
            process_id: process_id.to_string(),
        }),
    ));
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new(
                Arc::new(Fig779SuspendingProcessRunner),
                Arc::clone(&registry),
                continuation_store(),
                cancel_ingress,
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        execution_id: None,
    };

    let suspended = invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
        .await
        .expect("first process attempt should suspend on its durable timer");
    assert_eq!(
        restate_message_types(&suspended).expect("decode suspended process frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );

    registry
        .append_event(
            process_id,
            lash_core::ProcessEventAppendRequest::cancel_requested(
                process_id,
                Some("cancel while suspended".to_string()),
            ),
        )
        .await
        .expect("record durable process cancellation");
    let replay = encode_pending_sleep_replay(process_id, &input, &suspended)
        .expect("encode suspended process redrive");
    let cancelled = invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", replay)
        .await
        .expect("redrive should replay the timer command before observing cancellation");
    assert_eq!(
        restate_message_types(&cancelled).expect("decode cancelled redrive frames"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE
        ]
    );
    assert!(matches!(
        registry
            .get_process(process_id)
            .await
            .expect("read process")
            .expect("read redriven process")
            .outcome,
        Some(ProcessAwaitOutput::Cancelled { .. })
    ));
}

#[tokio::test]
async fn fig788_terminal_outcome_landing_preserves_the_suspended_command_prefix() {
    let process_id = "fig788-terminal-outcome-redrive";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register FIG-788 process");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788TerminalRedriveRunner),
                Arc::clone(&registry),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        execution_id: None,
    };

    let suspended = invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
        .await
        .expect("first process attempt should suspend");
    assert_eq!(
        restate_message_types(&suspended).expect("decode suspended process frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );

    let stored = ProcessAwaitOutput::Cancelled {
        message: "terminal outcome landed between attempts".to_string(),
        raw: None,
        control: None,
    };
    registry
        .complete_process(
            process_id,
            stored.clone(),
            workflow_key_authority(process_id),
        )
        .await
        .expect("store terminal outcome between attempts");
    let replay = encode_completed_captured_sleep_replay(process_id, &input, &suspended)
        .expect("splice the deployed suspended journal");
    let output = invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", replay)
        .await
        .expect("terminal redrive must preserve the deployed command prefix");

    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        })
    );
}

#[tokio::test]
async fn fig788_ordinal_one_terminal_delivery_redrive_retains_its_handover() {
    let process_id = "fig788-ordinal-one-terminal-redrive";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register ordinal-one process");
    let (execution_authority, started) =
        invocation_started(process_id, "fig788-ordinal-one-execution", 1);
    registry
        .record_first_started_with_authority(process_id, started, &execution_authority)
        .await
        .expect("record retained Restate execution start");
    let persisted = lash_core::PersistedSegmentHandover {
        segment_ordinal: 1,
        program_hash: "fig788-terminal-program".to_string(),
        handover: lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: Some("fig788-terminal-program".to_string()),
            engine_state: vec![1],
        },
    };
    continuations
        .put_segment_handover(process_id, persisted.clone())
        .await
        .expect("persist ordinal-one handover");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788OrdinalOneTerminalRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        execution_id: Some("fig788-ordinal-one-execution".to_string()),
    };

    let terminal_delivery_suspension =
        invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
            .await
            .expect("ordinal-one terminal delivery should suspend on its call");
    assert_eq!(
        restate_message_types(&terminal_delivery_suspension)
            .expect("decode ordinal-one terminal suspension"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&terminal_delivery_suspension)
    );
    assert_eq!(
        continuations
            .get_segment_handover(process_id, 1)
            .await
            .expect("read handover during terminal delivery"),
        Some(persisted.clone()),
        "redrive input must survive until the journaled terminal delivery resolves"
    );

    let replay =
        encode_process_terminal_delivery_replay(process_id, &input, &terminal_delivery_suspension)
            .expect("splice deployed ordinal-one terminal journal");
    let output = invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", replay)
        .await
        .expect("ordinal-one redrive must reconstruct and resolve the terminal prefix");
    let stored = registry
        .get_process(process_id)
        .await
        .expect("read terminal process")
        .expect("terminal process record")
        .outcome
        .expect("stored terminal outcome");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        })
    );
    assert_eq!(
        continuations
            .get_segment_handover(process_id, 1)
            .await
            .expect("read handover after terminal delivery"),
        Some(persisted),
        "handover must remain replay authority until terminal retention pruning"
    );
}

#[tokio::test]
async fn fig811_post_terminal_redrive_replays_delivery_after_handover_cleanup() {
    let process_id = "fig811-post-terminal-absorber";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register FIG-811 segmented process");
    let (execution_authority, started) =
        invocation_started(process_id, "fig811-post-terminal-execution", 1);
    registry
        .record_first_started_with_authority(process_id, started, &execution_authority)
        .await
        .expect("record retained Restate execution start");
    continuations
        .put_segment_handover(
            process_id,
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                program_hash: "fig788-terminal-program".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("fig788-terminal-program".to_string()),
                    engine_state: vec![1],
                },
            },
        )
        .await
        .expect("persist FIG-811 ordinal-one handover");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788OrdinalOneTerminalRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        execution_id: Some("fig811-post-terminal-execution".to_string()),
    };

    let suspended = invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
        .await
        .expect("terminal attempt should suspend during root delivery");
    assert_eq!(
        restate_message_types(&suspended).expect("decode terminal suspension"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let stored = registry
        .get_process(process_id)
        .await
        .expect("read terminal process")
        .expect("terminal process record")
        .outcome
        .expect("stored terminal outcome");
    continuations
        .delete_segment_handovers(process_id)
        .await
        .expect("model crash after delivery and handover cleanup");

    let replay = encode_process_terminal_delivery_replay(process_id, &input, &suspended)
        .expect("splice the deployed terminal delivery");
    let output = invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", replay)
        .await
        .expect("post-terminal redrive must absorb after reconstructing delivery");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        })
    );
}

#[tokio::test]
async fn fig811_effectful_post_terminal_redrive_replays_the_complete_prefix() {
    let process_id = "fig811-effectful-post-terminal-redrive";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register effectful FIG-811 process");
    let (execution_authority, started) =
        invocation_started(process_id, "fig811-effectful-terminal-execution", 1);
    registry
        .record_first_started_with_authority(process_id, started, &execution_authority)
        .await
        .expect("record retained effectful Restate execution start");
    continuations
        .put_segment_handover(
            process_id,
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                program_hash: "fig811-effectful-terminal-program".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("fig811-effectful-terminal-program".to_string()),
                    engine_state: vec![8, 1, 1],
                },
            },
        )
        .await
        .expect("persist effectful ordinal-one handover");
    let trace_sink = Arc::new(RecordingTraceSink::default());
    let trace_sink_dyn: Arc<dyn lash_trace::TraceSink> = trace_sink.clone();
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig811EffectfulOrdinalOneTerminalRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .with_trace_sink(
                trace_sink_dyn,
                lash_trace::TraceContext {
                    run_id: Some("fig811-workflow-trace".to_string()),
                    ..lash_trace::TraceContext::default()
                },
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        execution_id: Some("fig811-effectful-terminal-execution".to_string()),
    };

    let effect_suspension =
        invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
            .await
            .expect("effectful attempt should suspend on its journaled effect");
    assert_eq!(
        restate_message_types(&effect_suspension).expect("decode effect suspension"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    assert!(trace_sink.records.lock_recover().iter().any(|record| {
        record.event.kind() == "durable_timer_started"
            && record.context.run_id.as_deref() == Some("fig811-workflow-trace")
            && record.context.session_id.as_deref() == Some("session")
    }));

    let completed_effect =
        encode_completed_captured_sleep_replay(process_id, &input, &effect_suspension)
            .expect("splice completed effect prefix");
    let terminal_delivery_suspension =
        invoke_endpoint_body(&endpoint, "LashProcessWorkflow", "run", completed_effect)
            .await
            .expect("effect completion should reach terminal delivery");
    assert_eq!(
        restate_message_types(&terminal_delivery_suspension)
            .expect("decode effectful terminal suspension"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );

    let complete_replay = encode_effectful_process_terminal_replay(
        process_id,
        &input,
        &effect_suspension,
        &terminal_delivery_suspension,
    )
    .expect("splice the complete effectful terminal prefix");
    let completed = invoke_endpoint_body_open(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        complete_replay.clone(),
    )
    .await
    .expect("terminal delivery should complete before the modeled crash");
    let stored = registry
        .get_process(process_id)
        .await
        .expect("read effectful terminal process")
        .expect("effectful terminal process record")
        .outcome
        .expect("stored effectful terminal outcome");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&completed),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored.clone()),
        })
    );

    let redriven =
        invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", complete_replay)
            .await
            .expect("post-delivery redrive should preserve the complete deployed prefix");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&redriven),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        }),
        "endpoint error: {:?}",
        restate_error_message(&redriven)
    );
}

#[tokio::test]
async fn fig788_cancel_landing_after_segment_send_preserves_the_deployed_prefix() {
    let process_id = "fig788-segment-cancel-redrive";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register FIG-788 segmented process");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788SegmentBoundaryRunner),
                Arc::clone(&registry),
                continuations,
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        execution_id: None,
    };

    let segment_finish_suspension =
        invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
            .await
            .expect("first segment attempt should suspend after scheduling its successor");
    assert_eq!(
        restate_message_types(&segment_finish_suspension)
            .expect("decode segment-finish suspension frames"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            0x040E,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&segment_finish_suspension)
    );

    registry
        .append_event(
            process_id,
            lash_core::ProcessEventAppendRequest::cancel_requested(
                process_id,
                Some("cancel landed after successor send".to_string()),
            ),
        )
        .await
        .expect("record between-attempt cancellation");
    let replay = encode_process_segment_send_replay(process_id, &input, &segment_finish_suspension)
        .expect("splice deployed segment-send journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("cancelled segment redrive must preserve the deployed send prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode appended cancellation forwarding")
            .iter()
            .map(|call| (call.key.as_str(), call.handler.as_str()))
            .collect::<Vec<_>>(),
        vec![("fig788-segment-cancel-redrive#1", "deliver_cancel")]
    );
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::SegmentChained {
            next_segment_ordinal: 1,
        })
    );
}

#[tokio::test]
async fn fig806_reserved_trigger_redrive_replays_the_process_start_prefix() {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    let registration = store
        .execute_command(
            "fig806-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fig806").expect("owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fig806"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "fig806/subscription",
                    lash_core::ProcessExecutionEnvRef::new("process-env:fig806"),
                    "ui.button.pressed",
                    source_key.clone(),
                    ProcessInput::Engine {
                        kind: "fig806-engine".to_string(),
                        payload: serde_json::json!({}),
                    },
                    lash_core::ProcessIdentity::new("fig806-engine"),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("register trigger subscription")
        .expect("trigger registration outcome");
    assert!(matches!(
        registration,
        lash_core::TriggerCommandOutcome::Mutation { .. }
    ));
    let registry = process_registry();
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        Some(Arc::clone(&registry)),
        None,
    );
    let endpoint = Endpoint::builder()
        .bind(Fig806TriggerRedriveImpl { router }.serve())
        .build();
    let input = Fig806TriggerRedriveInput {
        occurrence: lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            "fig806-occurrence",
        ),
    };
    let workflow_key = "fig806-trigger-redrive";

    let suspended = invoke_endpoint(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        workflow_key,
        &input,
    )
    .await
    .expect("trigger start should suspend on its invocation id");
    assert_eq!(
        restate_message_types(&suspended).expect("decode trigger suspension"),
        vec![0x040E, RESTATE_SUSPENSION_MESSAGE_TYPE],
        "endpoint error: {:?}",
        restate_error_message(&suspended)
    );
    let replay = encode_one_way_call_replay(workflow_key, &input, &suspended)
        .expect("splice deployed trigger process start");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("reserved trigger redrive must preserve the process-start prefix");

    let report = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&output)
        .expect("decode trigger emit report");
    assert_eq!(report.deliveries.len(), 1);
    assert_eq!(
        restate_call_frames(&output)
            .expect("decode post-start call")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["complete"]
    );
    assert_eq!(
        registry
            .list_processes(&lash_core::ProcessListFilter::default())
            .await
            .expect("list trigger processes")
            .len(),
        1,
        "one occurrence must still create exactly one process"
    );
}

async fn register_fig811_subscription(
    store: &dyn lash_core::TriggerStore,
    operation_id: &str,
    subscription_key: &str,
    source_key: &str,
) -> String {
    let outcome = store
        .execute_command(
            operation_id,
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fig811")
                    .expect("FIG-811 owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fig811"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    subscription_key,
                    lash_core::ProcessExecutionEnvRef::new(format!(
                        "process-env:{subscription_key}"
                    )),
                    "ui.button.pressed",
                    source_key,
                    ProcessInput::Engine {
                        kind: "fig811-engine".to_string(),
                        payload: serde_json::json!({}),
                    },
                    lash_core::ProcessIdentity::new("fig811-engine"),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("register FIG-811 trigger subscription")
        .expect("FIG-811 trigger registration outcome");
    let lash_core::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("register must return a mutation receipt");
    };
    receipt.subscription_id
}

#[tokio::test]
async fn fig811_two_subscription_sqlite_redrive_preserves_canonical_start_order() {
    let store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::memory()
            .await
            .expect("open SQLite trigger store"),
    );
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    let _alpha_id = register_fig811_subscription(
        store.as_ref(),
        "fig811-register-alpha",
        "alpha",
        &source_key,
    )
    .await;
    let _beta_id =
        register_fig811_subscription(store.as_ref(), "fig811-register-beta", "beta", &source_key)
            .await;
    let mut expected_subscriptions = store
        .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
        .await
        .expect("list FIG-811 subscriptions for canonical order");
    expected_subscriptions.sort_by(|left, right| {
        left.owner_scope
            .namespace()
            .cmp(&right.owner_scope.namespace())
            .then_with(|| left.subscription_key.cmp(&right.subscription_key))
            .then_with(|| left.subscription_id.cmp(&right.subscription_id))
    });
    let expected_subscription_ids = expected_subscriptions
        .iter()
        .map(|subscription| subscription.subscription_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        expected_subscription_ids.len(),
        2,
        "the FIG-811 fixture must contain exactly its alpha and beta subscriptions"
    );

    let registry = process_registry();
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        Some(Arc::clone(&registry)),
        None,
    );
    let endpoint = Endpoint::builder()
        .bind(Fig806TriggerRedriveImpl { router }.serve())
        .build();
    let input = Fig806TriggerRedriveInput {
        occurrence: lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            "fig811-two-subscription-occurrence",
        ),
    };
    let workflow_key = "fig811-two-subscription-redrive";
    let invocation_ids = ["inv_fig811_alpha_process", "inv_fig811_beta_process"];

    let suspended = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        workflow_key,
        &input,
        invocation_ids.iter().map(ToString::to_string).collect(),
        Vec::new(),
    )
    .await
    .expect("initial multi-subscription attempt should suspend after both starts");
    assert_eq!(
        restate_message_types(&suspended).expect("decode multi-subscription suspension"),
        vec![
            0x040E,
            0x040E,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&suspended)
    );

    let replay = encode_two_one_way_calls_and_call_replay(
        workflow_key,
        &input,
        &suspended,
        invocation_ids,
        serde_json::Value::Null,
    )
    .expect("splice both deployed process starts");
    let output = invoke_endpoint_body(&endpoint, "Fig806TriggerRedrive", "run", replay)
        .await
        .expect("multi-subscription redrive must preserve process-start ordering");
    let report = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&output)
        .unwrap_or_else(|| {
            panic!(
                "decode multi-subscription replay report; endpoint error={:?}, output failure={:?}",
                restate_error_message(&output),
                (
                    restate_output_failure_message(&output),
                    restate_message_types(&output)
                )
            )
        });
    assert_eq!(
        report
            .deliveries
            .iter()
            .map(|delivery| (delivery.subscription_id.as_str(), &delivery.outcome,))
            .collect::<Vec<_>>(),
        expected_subscription_ids
            .into_iter()
            .map(|subscription_id| {
                (
                    subscription_id,
                    &lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved,
                )
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        registry
            .list_processes(&lash_core::ProcessListFilter::default())
            .await
            .expect("list trigger processes")
            .len(),
        2,
        "two subscriptions create exactly two deterministic processes"
    );
}

#[tokio::test]
async fn fig811_independent_client_retry_reports_duplicate_without_a_second_process() {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    register_fig811_subscription(
        store.as_ref(),
        "fig811-register-client-retry",
        "client-retry",
        &source_key,
    )
    .await;
    let registry = process_registry();
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        Some(Arc::clone(&registry)),
        None,
    );
    let endpoint = Endpoint::builder()
        .bind(Fig806TriggerRedriveImpl { router }.serve())
        .build();
    let input = Fig806TriggerRedriveInput {
        occurrence: lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            "fig811-client-retry-occurrence",
        ),
    };
    let workflow_invocation_id = "inv_restate_workflow_LashProcessWorkflow_fig811_client_retry";

    let first = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        "fig811-client-attempt-one",
        &input,
        vec![workflow_invocation_id.to_string()],
        vec![serde_json::Value::Null],
    )
    .await
    .expect("first independent client invocation");
    let first = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&first)
        .expect("decode first client report");
    assert!(matches!(
        first.deliveries.as_slice(),
        [lash_core::facade_support::TriggerDeliveryEmitReceipt {
            outcome: lash_core::facade_support::TriggerDeliveryEmitOutcome::Started,
            ..
        }]
    ));

    let second = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        "fig811-client-attempt-two",
        &input,
        vec![workflow_invocation_id.to_string()],
        vec![serde_json::Value::Null],
    )
    .await
    .expect("second independent client invocation");
    let second = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&second)
        .expect("decode second client report");
    assert!(matches!(
        second.deliveries.as_slice(),
        [lash_core::facade_support::TriggerDeliveryEmitReceipt {
            outcome: lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved,
            ..
        }]
    ));
    assert_eq!(
        registry
            .list_processes(&lash_core::ProcessListFilter::default())
            .await
            .expect("list trigger processes")
            .len(),
        1,
        "independent retry must retain exactly one process"
    );
}

async fn fig793_pre_fix_suspended_llm_run(
    invocation_id: &str,
) -> (Endpoint, Bytes, serde_json::Value) {
    let endpoint = Endpoint::builder()
        .bind(Fig793LlmGateRedriveImpl.serve())
        .build();
    let input = Fig793LlmGateRedriveInput;
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig793LlmGateRedrive",
        "run",
        invocation_id,
        &input,
    )
    .await
    .expect("capture deployed pre-FIG-793 LLM journal");
    assert_eq!(
        restate_message_types(&suspended).expect("decode suspended LLM run frames"),
        vec![
            RESTATE_RUN_COMMAND_MESSAGE_TYPE,
            0x0005,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(
            fig793_llm_envelope()
                .canonical_form()
                .expect("canonical FIG-793 LLM envelope"),
        ),
        outcome: Ok(fig793_llm_outcome()),
    };
    (
        endpoint,
        suspended,
        serde_json::to_value(recorded).expect("serialize recorded LLM outcome"),
    )
}

#[tokio::test]
async fn fig793_pre_fix_suspended_llm_run_redrives_without_cancellation() {
    let invocation_id = "fig793-pre-fix-no-cancel";
    let (endpoint, suspended, completion) = fig793_pre_fix_suspended_llm_run(invocation_id).await;
    let replay = encode_run_replay(
        invocation_id,
        &Fig793LlmGateRedriveInput,
        &suspended,
        completion,
    )
    .expect("splice pre-FIG-793 LLM journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig793LlmGateRedrive",
        "run",
        replay,
        vec![serde_json::json!(false), serde_json::Value::Null],
    )
    .await
    .expect("new cancellation observation must extend the deployed LLM prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode post-LLM observation calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "peek"]
    );
    assert_eq!(restate_output_json::<bool>(&output), Some(false));
}

#[tokio::test]
async fn fig793_pre_fix_suspended_llm_run_redrives_to_cancelled_boundary() {
    let invocation_id = "fig793-pre-fix-cancelled";
    let (endpoint, suspended, completion) = fig793_pre_fix_suspended_llm_run(invocation_id).await;
    let replay = encode_run_replay(
        invocation_id,
        &Fig793LlmGateRedriveInput,
        &suspended,
        completion,
    )
    .expect("splice pre-FIG-793 cancelled LLM journal");
    let cancellation = Resolution::Ok(serde_json::json!({
        "state": "cancel_requested",
        "cancellation": {
            "request_id": "fig793-cancel",
            "reason": "cancel landed while the LLM run was suspended"
        }
    }));
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig793LlmGateRedrive",
        "run",
        replay,
        vec![
            serde_json::json!(false),
            serde_json::to_value(Some(cancellation)).expect("serialize durable cancellation"),
        ],
    )
    .await
    .expect("cancelled redrive must extend the deployed LLM prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode cancelled post-LLM observation calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "peek"]
    );
    assert_eq!(restate_output_json::<bool>(&output), Some(true));
}

/// PR #78's synthetic re-poll defense is re-scoped to the fused-state boundary.
/// A manual second poll stays pending without reaching the fused SDK future or
/// introducing a panic-capable branch in the production handler boundary.
#[tokio::test]
async fn fig779_real_restate_timer_repoll_stays_pending_without_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "repoll_fused_timer",
        "fig779-repoll-fused-timer",
        &Fig779TimerGuardReproInput { duration_ms: 2_000 },
    )
    .await
    .expect("re-polling the fused wrapper must not panic");

    assert_eq!(
        restate_message_types(&output).expect("decode re-poll response frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "the fused timer must preserve the SDK suspension"
    );
}

/// FIG-1464: the workbench replay-panic loop. A journaled effect whose
/// `ctx.run` fails at the SDK level leaves an already-`Ready` run future behind
/// `InterceptErrorFuture`'s recorded-failure park. Re-entering it aborts the
/// handler task, so the attempt ends with no output and no `End`, Restate
/// redrives it, and the deterministic replay panics again - the turn can never
/// terminate. The seam must fuse the run future instead.
#[tokio::test]
async fn fig1464_failed_journaled_run_repoll_stays_pending_without_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "repoll_failed_run",
        "fig1464-repoll-failed-run",
        &Fig1464RunGuardReproInput {
            effect_name: "lash:fig1464-unjournalable-effect".to_string(),
        },
    )
    .await
    .expect("re-polling a failed journaled run must not panic");

    let message_types = restate_message_types(&output).expect("decode failed-run response frames");
    assert!(
        !message_types.contains(&RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE)
            && !message_types.contains(&RESTATE_END_MESSAGE_TYPE),
        "a failed journaled run must not land a fabricated output: {message_types:?}"
    );
    assert!(
        restate_error_message(&output)
            .is_some_and(|message| message.contains("cannot be journaled")),
        "the attempt must end on the recorded run failure, not a panic"
    );
}

/// FIG-1464: the same panic loop on the replay path, which is the interleaving
/// the ticket actually reports. The run entry is already journaled, so the SDK
/// skips the closure entirely; reading the recorded value back fails, and
/// `InterceptErrorFuture` parks on the recorded failure after waking
/// synchronously. A fuse that waited for the closure to complete would stay
/// inert for this whole attempt, so the second poll would re-enter an
/// already-resolved SDK future and abort the handler.
#[tokio::test]
async fn fig1464_replayed_unreadable_run_repoll_stays_pending_without_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let key = "fig1464-repoll-replayed-run";
    let input = Fig1464RunGuardReproInput {
        effect_name: "lash:fig1464-unreadable-journaled-effect".to_string(),
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "repoll_replayed_run",
        key,
        &input,
    )
    .await
    .expect("the first attempt must journal the run and park");
    assert!(
        restate_message_types(&suspended)
            .expect("decode first-attempt frames")
            .contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "the effect must be journaled as a RunCommand before the replay leg"
    );

    let body = encode_run_replay(key, &input, &suspended, serde_json::json!(41))
        .expect("encode completed journaled run replay");
    let output = invoke_endpoint_body(
        &endpoint,
        "Fig1464RunGuardRepro",
        "repoll_replayed_run",
        body,
    )
    .await
    .expect("re-polling a replayed run failure must not panic");

    let message_types = restate_message_types(&output).expect("decode replayed-run frames");
    assert!(
        !message_types.contains(&RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE)
            && !message_types.contains(&RESTATE_END_MESSAGE_TYPE),
        "a replayed run failure must not land a fabricated output: {message_types:?}"
    );
    assert!(
        restate_error_message(&output)
            .is_some_and(|message| message.contains("cannot be read back")),
        "the attempt must end on the recorded replay failure, not a panic: {:?}",
        restate_error_message(&output)
    );
}

/// FIG-1464 contrast: fusing the terminal attempt state must not swallow a
/// journaled run that really did produce a result.
#[tokio::test]
async fn fig1464_journaled_run_still_returns_its_recorded_result() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let key = "fig1464-journaled-run";
    let input = Fig1464RunGuardReproInput {
        effect_name: "lash:fig1464-journaled-effect".to_string(),
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "journaled_run",
        key,
        &input,
    )
    .await
    .expect("the journaled run must park on its proposed completion");
    assert!(
        restate_message_types(&suspended)
            .expect("decode journaled run frames")
            .contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "the effect must be journaled as a RunCommand"
    );

    let body = encode_run_replay(key, &input, &suspended, serde_json::json!(41))
        .expect("encode completed journaled run replay");
    let output = invoke_endpoint_body(&endpoint, "Fig1464RunGuardRepro", "journaled_run", body)
        .await
        .expect("the completed journaled run must return its recorded result");

    assert_eq!(restate_output_json::<u32>(&output), Some(42));
}

/// FIG-1464: a wake the run closure's own future issued must not fuse the run.
/// `LlmCall` routes to a journaled run with no task boundary between the
/// streaming code and this seam, so a same-task self-wake from that code reaches
/// the guard. Treating it as the SDK's terminal park would fuse a healthy run:
/// the effect would never even be proposed as a `RunCommand`, and the turn would
/// hang holding a paid completion.
#[tokio::test]
async fn fig1464_self_waking_run_closure_does_not_fuse_the_run() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let key = "fig1464-self-waking-run";
    let input = Fig1464RunGuardReproInput {
        effect_name: "lash:fig1464-self-waking-effect".to_string(),
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "self_waking_run",
        key,
        &input,
    )
    .await
    .expect("a self-waking run closure must still reach its proposed completion");
    assert!(
        restate_message_types(&suspended)
            .expect("decode self-waking run frames")
            .contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "a self-waking run closure must still journal its effect"
    );

    let body = encode_run_replay(key, &input, &suspended, serde_json::json!(41))
        .expect("encode completed self-waking run replay");
    let output = invoke_endpoint_body(&endpoint, "Fig1464RunGuardRepro", "self_waking_run", body)
        .await
        .expect("the completed self-waking run must return its recorded result");

    assert_eq!(restate_output_json::<u32>(&output), Some(42));
}

/// FIG-779 contrast: an already-completed timer replays straight to `Ready`, so
/// the guard never sees a synchronous wake. This is why the panic is only
/// reachable on the attempt that first parks on the timer, not on the resume.
#[tokio::test]
async fn fig779_completed_durable_timer_replay_does_not_enter_guard_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();
    let input = Fig779TimerGuardReproInput { duration_ms: 2_000 };
    let body = encode_completed_sleep_replay("fig779-timer", &input)
        .expect("encode completed durable timer replay");

    invoke_endpoint_body(&endpoint, "Fig779TimerGuardRepro", "run", body)
        .await
        .expect("completed durable timer replay should finish without panicking");
}

/// FIG-779 control: the identical input against the bare SDK timer is handled
/// correctly — the endpoint writes a `SleepCommand` followed by a `Suspension`
/// frame. The synchronous-wake-then-Pending shape is therefore the SDK's normal
/// suspension protocol, not a driver-invariant violation.
#[tokio::test]
async fn fig779_sdk_pending_durable_timer_suspends_cleanly_without_guard() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();

    let output = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "raw_sleep",
        "fig779-raw-timer",
        &Fig779TimerGuardReproInput { duration_ms: 2_000 },
    )
    .await
    .expect("the SDK must encode a pending timer suspension without panicking");
    let message_types = restate_message_types(&output).expect("decode Restate response frames");
    assert_eq!(
        message_types,
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct Fig790TurnEventPumpInput {
    process_id: String,
    prequeue_event: bool,
}

#[restate_sdk::workflow]
trait Fig790TurnEventPump {
    async fn run(input: Json<Fig790TurnEventPumpInput>) -> HandlerResult<Json<()>>;
}

struct Fig790TurnEventPumpImpl;

impl Fig790TurnEventPump for Fig790TurnEventPumpImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig790TurnEventPumpInput>,
    ) -> HandlerResult<Json<()>> {
        let request: restate_sdk::context::Request<
            '_,
            Json<RestateProcessAwaitRequest>,
            Json<ProcessAwaitOutput>,
        > = ContextClient::request(
            &ctx,
            RequestTarget::workflow(
                "LashProcessWorkflow",
                input.process_id.clone(),
                "await_terminal",
            ),
            Json(RestateProcessAwaitRequest {
                process_id: input.process_id,
            }),
        );
        let mut run_future = Box::pin(async move {
            let Json(_output) = request.call().await?;
            Ok::<(), TerminalError>(())
        });
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        if input.prequeue_event {
            event_tx
                .send(())
                .await
                .expect("queue the event that makes the pump branch ready");
        }
        drop(event_tx);

        let mut handler_state = ();
        lash_core::drive_with_event_pump(
            run_future.as_mut(),
            &mut event_rx,
            &mut handler_state,
            |(), _| Box::pin(async {}),
        )
        .await?;
        Ok(Json(()))
    }
}

async fn assert_fig790_turn_event_pump_suspends_cleanly(invocation_id: &str, prequeue_event: bool) {
    let endpoint = Endpoint::builder()
        .bind(Fig790TurnEventPumpImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "Fig790TurnEventPump",
        "run",
        invocation_id,
        &Fig790TurnEventPumpInput {
            process_id: format!("{invocation_id}-process"),
            prequeue_event,
        },
    )
    .await
    .expect("the event pump must let the substrate consume its suspension");
    assert_eq!(
        restate_message_types(&output).expect("decode process-await suspension frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
}

#[tokio::test]
async fn fig790_turn_event_pump_does_not_repoll_a_suspending_durable_future() {
    assert_fig790_turn_event_pump_suspends_cleanly("fig790-turn-event-pump", true).await;
}

#[tokio::test]
async fn fig790_turn_event_pump_with_empty_channel_suspends_cleanly() {
    assert_fig790_turn_event_pump_suspends_cleanly("fig790-turn-event-pump-empty", false).await;
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig790ProcessAwaitRedriveInput {
    process_id: String,
    cancel_on_suspend_wake: bool,
}

#[restate_sdk::workflow]
trait Fig790ProcessAwaitRedrive {
    async fn run(
        input: Json<Fig790ProcessAwaitRedriveInput>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>>;
}

struct Fig790ProcessAwaitRedriveImpl {
    registry: Arc<dyn ProcessRegistry>,
}

impl Fig790ProcessAwaitRedrive for Fig790ProcessAwaitRedriveImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig790ProcessAwaitRedriveInput>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let effect = controller.execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "fig790-process-await"),
                RuntimeEffectCommand::process(ProcessCommand::Await {
                    process_id: input.process_id,
                }),
            ),
            RuntimeEffectLocalExecutor::processes(Arc::clone(&self.registry), None)
                .with_process_turn_cancellation(
                    lash_core::facade_support::ProcessTurnCancellation::new(
                        cancellation.clone(),
                        durable_turn_scope("session", "turn"),
                    ),
                ),
        );
        let outcome = if input.cancel_on_suspend_wake {
            CancelOnWakeFuture {
                future: Box::pin(effect),
                cancellation,
            }
            .await
        } else {
            effect.await
        }
        .map_err(TerminalError::from_error)?;
        let ProcessEffectOutcome::Await { output } =
            outcome.into_process().map_err(TerminalError::from_error)?
        else {
            return Err(TerminalError::new(
                "process-await fixture returned the wrong process outcome",
            )
            .into());
        };
        Ok(Json(*output))
    }
}

fn fig790_cancelled_process_output(process_id: &str) -> ProcessAwaitOutput {
    ProcessAwaitOutput::Cancelled {
        message: format!("process `{process_id}` observed durable turn cancellation"),
        raw: None,
        control: None,
    }
}

async fn fig790_process_await_endpoint(process_id: &str) -> (Endpoint, Arc<dyn ProcessRegistry>) {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration(process_id))
        .await
        .expect("register FIG-790 process");
    let endpoint = Endpoint::builder()
        .bind(
            Fig790ProcessAwaitRedriveImpl {
                registry: Arc::clone(&registry),
            }
            .serve(),
        )
        .build();
    (endpoint, registry)
}

async fn fig790_pre_pr_suspended_process_call(
    process_id: &str,
) -> endpoint_protocol::RestateCallFrame {
    let endpoint = Endpoint::builder()
        .bind(Fig790TurnEventPumpImpl.serve())
        .build();
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790TurnEventPump",
        "run",
        &format!("{process_id}-pre-pr-fixture"),
        &Fig790TurnEventPumpInput {
            process_id: process_id.to_string(),
            prequeue_event: false,
        },
    )
    .await
    .expect("capture the deployed pre-PR process-await journal shape");
    let calls = restate_call_frames(&suspended).expect("decode pre-PR process-await call");
    let [call] = calls.as_slice() else {
        panic!("pre-PR process-await fixture must contain exactly one call");
    };
    assert_eq!(call.handler, "await_terminal");
    call.clone()
}

// Deployment compatibility gate: a process-await invocation suspended before
// FIG-790 has only `await_terminal` in its journal. Its terminal redrive must
// accept that command as an exact prefix and append cancellation observation.
#[tokio::test]
async fn fig790_pre_pr_suspended_process_await_redrives_to_terminal() {
    let process_id = "fig790-pre-pr-terminal";
    let pre_pr_call = fig790_pre_pr_suspended_process_call(process_id).await;
    let (endpoint, _registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "deployment_compat": "terminal" }),
        control: None,
    };
    let replay = encode_call_replay(
        "fig790-pre-pr-terminal",
        &input,
        &[(
            pre_pr_call,
            Some(serde_json::to_value(&terminal).expect("serialize process terminal")),
        )],
        None,
    )
    .expect("splice pre-PR terminal journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![
            serde_json::to_value(RestateDurableWaitRegistration::Registered)
                .expect("serialize registered observation"),
            serde_json::Value::Null,
        ],
    )
    .await
    .expect("new code must redrive the deployed pre-PR journal prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode appended terminal-redrive calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable", "unregister_awakeable"]
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(terminal)
    );
}

// Deployment compatibility gate: the same pre-PR suspended prefix must also
// redrive when turn cancellation was already resolved before registration.
// This is deliberately distinct from a revoked session.
#[tokio::test]
async fn fig790_pre_pr_suspended_process_await_redrives_to_cancelled() {
    let process_id = "fig790-pre-pr-cancelled";
    let pre_pr_call = fig790_pre_pr_suspended_process_call(process_id).await;
    let (endpoint, _registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let cancelled = fig790_cancelled_process_output(process_id);
    let replay = encode_call_replay(
        "fig790-pre-pr-cancelled",
        &input,
        &[(pre_pr_call, None)],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize already-resolved turn cancellation"),
        )),
    )
    .expect("splice pre-PR cancelled journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![
            serde_json::to_value(RestateDurableWaitRegistration::Registered)
                .expect("serialize registered observation"),
            serde_json::Value::Null,
            serde_json::to_value(&cancelled).expect("serialize cancelled process terminal"),
        ],
    )
    .await
    .expect("already-resolved cancellation must redrive the pre-PR journal");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode appended cancellation-redrive calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable", "cancel", "await_terminal"]
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(cancelled)
    );
}

#[tokio::test]
async fn fig790_revoked_session_unwinds_turn_without_cancelling_process() {
    let process_id = "fig790-revoked-session";
    let (endpoint, registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let registration = serde_json::to_value(RestateDurableWaitRegistration::Revoked)
        .expect("serialize revoked registration");

    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-revoked-session",
        &input,
    )
    .await
    .expect("capture the process-await command prefix");
    let calls = restate_call_frames(&suspended).expect("decode process-await call frames");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"],
        "session revocation must preserve the old journal prefix and emit no process cancel"
    );

    let replay = encode_call_replay(
        "fig790-revoked-session",
        &input,
        &[
            (calls[0].clone(), None),
            (calls[1].clone(), Some(registration)),
        ],
        None,
    )
    .expect("splice revoked-session call journal");
    let first = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay.clone(),
    )
    .await
    .expect("revoked session should terminalize the dead turn");
    assert_eq!(
        restate_call_frames(&first)
            .expect("decode revoked-session calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "session revocation must emit no process cancel"
    );
    assert!(
        restate_output_failure_message(&first)
            .is_some_and(|failure| failure.contains("used and deleted")),
        "revoked process await must terminalize with the typed deleted-session refusal"
    );
    assert!(
        !registry
            .get_process(process_id)
            .await
            .expect("revoked await keeps its process record")
            .expect("revoked await keeps the process present")
            .is_terminal(),
        "revoking session observation edges must not terminalize the process"
    );

    let redriven = invoke_endpoint_body(&endpoint, "Fig790ProcessAwaitRedrive", "run", replay)
        .await
        .expect("revoked-session redrive must accept the identical command sequence");
    assert_eq!(
        restate_call_frames(&redriven)
            .expect("decode revoked-session redrive calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "revoked-session redrive must not append a process cancel"
    );
    assert!(
        !registry
            .get_process(process_id)
            .await
            .expect("redriven revoked await keeps its process record")
            .expect("redriven revoked await keeps the process present")
            .is_terminal()
    );
}

#[tokio::test]
async fn fig790_registered_session_revocation_unwinds_without_cancelling_process() {
    let process_id = "fig790-registered-then-revoked";
    let (endpoint, registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-registered-then-revoked",
        &input,
    )
    .await
    .expect("capture registered process-await commands");
    let calls = restate_call_frames(&suspended).expect("decode registered process-await calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"]
    );
    let replay = encode_call_replay(
        "fig790-registered-then-revoked",
        &input,
        &[
            (calls[0].clone(), None),
            (
                calls[1].clone(),
                Some(
                    serde_json::to_value(RestateDurableWaitRegistration::Registered)
                        .expect("serialize registered observation"),
                ),
            ),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::SessionRevoked)
                .expect("serialize registered-session revocation"),
        )),
    )
    .expect("splice registered-then-revoked process await");
    let output = invoke_endpoint_body(&endpoint, "Fig790ProcessAwaitRedrive", "run", replay)
        .await
        .expect("registered revocation must terminalize the dead turn");

    assert!(
        restate_call_frames(&output)
            .expect("decode registered-revocation calls")
            .is_empty(),
        "registered session revocation must emit no process cancel"
    );
    assert!(
        restate_output_failure_message(&output)
            .is_some_and(|failure| failure.contains("used and deleted")),
        "registered revocation must preserve typed SessionDeleted settlement"
    );
    assert!(
        !registry
            .get_process(process_id)
            .await
            .expect("registered revocation keeps its process record")
            .expect("registered revocation keeps the process present")
            .is_terminal(),
        "registered session revocation must not terminalize the process"
    );
}

#[tokio::test]
async fn fig790_process_terminal_wins_when_terminal_and_cancellation_are_both_ready() {
    let process_id = "fig790-terminal-and-cancel-ready";
    let (endpoint, _registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-terminal-and-cancel-ready",
        &input,
    )
    .await
    .expect("capture process-await race commands");
    let calls = restate_call_frames(&suspended).expect("decode process-await race calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"]
    );
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "winner": "process_terminal" }),
        control: None,
    };
    let replay = encode_call_replay(
        "fig790-terminal-and-cancel-ready",
        &input,
        &[
            (
                calls[0].clone(),
                Some(serde_json::to_value(&terminal).expect("serialize process terminal")),
            ),
            (
                calls[1].clone(),
                Some(
                    serde_json::to_value(RestateDurableWaitRegistration::Registered)
                        .expect("serialize registered observation"),
                ),
            ),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize simultaneously-ready cancellation"),
        )),
    )
    .expect("splice both-ready process-await race");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("process terminal must win the biased Restate handle order");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode both-ready appended calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["unregister_awakeable"],
        "the both-ready race must not append process cancellation"
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(terminal)
    );
}

#[tokio::test]
async fn fig790_cancel_during_suspension_of_a_process_turn_composes_with_fig779() {
    let process_id = "fig790-cancel-during-suspension";
    let (endpoint, _registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let registered = serde_json::to_value(RestateDurableWaitRegistration::Registered)
        .expect("serialize registered turn-cancel wait");

    let registering = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-cancel-during-suspension",
        &input,
    )
    .await
    .expect("turn-cancel registration must suspend cleanly");
    assert_eq!(
        restate_message_types(&registering).expect("decode turn-cancel registration frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let registration_calls =
        restate_call_frames(&registering).expect("decode turn-cancel registration call");
    assert_eq!(
        registration_calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"]
    );

    let registered_replay = encode_call_replay(
        "fig790-cancel-during-suspension",
        &input,
        &[
            (registration_calls[0].clone(), None),
            (registration_calls[1].clone(), Some(registered.clone())),
        ],
        None,
    )
    .expect("splice registered turn-cancel observation");
    let process_suspended = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        registered_replay,
    )
    .await
    .expect("process await must preserve suspension precedence");
    assert_eq!(
        restate_message_types(&process_suspended).expect("decode suspended process-await frames"),
        vec![RESTATE_SUSPENSION_MESSAGE_TYPE],
        "the durable process await must suspend before later cancellation is observed"
    );
    assert!(
        restate_call_frames(&process_suspended)
            .expect("decode suspended process-await calls")
            .is_empty()
    );

    let cancelled = fig790_cancelled_process_output(process_id);
    let replay = encode_call_replay(
        "fig790-cancel-during-suspension",
        &input,
        &[
            (registration_calls[0].clone(), None),
            (registration_calls[1].clone(), Some(registered)),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize durable turn cancellation"),
        )),
    )
    .expect("splice suspended process-await journal and cancellation signal");
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![
            serde_json::Value::Null,
            serde_json::to_value(&cancelled).expect("serialize cancelled process terminal"),
        ],
    )
    .await
    .expect("cancel-during-suspension redrive should finish deterministically");
    assert_eq!(
        restate_call_frames(&redriven)
            .expect("decode post-cancellation calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["cancel", "await_terminal"]
    );
    assert_eq!(
        restate_message_types(&redriven).expect("decode cancellation redrive frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE
        ]
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&redriven),
        Some(cancelled)
    );
}

#[tokio::test]
async fn fig790_second_await_terminal_suspension_redrives_after_journaled_cancel() {
    let process_id = "fig790-second-await-redrive";
    let (endpoint, _registry) = fig790_process_await_endpoint(process_id).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_id: process_id.to_string(),
        cancel_on_suspend_wake: false,
    };
    let initial = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-second-await-redrive",
        &input,
    )
    .await
    .expect("capture initial process-await calls");
    let initial_calls = restate_call_frames(&initial).expect("decode initial process-await calls");
    let registered = serde_json::to_value(RestateDurableWaitRegistration::Registered)
        .expect("serialize registered turn cancellation");
    let cancellation_signal = Some((
        17,
        serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
            .expect("serialize turn cancellation"),
    ));
    let cancellation_replay = encode_call_replay(
        "fig790-second-await-redrive",
        &input,
        &[
            (initial_calls[0].clone(), None),
            (initial_calls[1].clone(), Some(registered.clone())),
        ],
        cancellation_signal.clone(),
    )
    .expect("splice cancellation-winning process-await journal");
    let cancel_suspended = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        cancellation_replay,
    )
    .await
    .expect("suspend on the process-cancel command");
    assert_eq!(
        restate_message_types(&cancel_suspended).expect("decode process-cancel suspension frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ]
    );
    let cancel_calls = restate_call_frames(&cancel_suspended).expect("decode journaled cancel");
    let [cancel_call] = cancel_calls.as_slice() else {
        panic!("cancellation winner must append exactly one process cancel");
    };
    assert_eq!(cancel_call.handler, "cancel");

    let post_cancel_replay = encode_call_replay(
        "fig790-second-await-redrive",
        &input,
        &[
            (initial_calls[0].clone(), None),
            (initial_calls[1].clone(), Some(registered.clone())),
            (cancel_call.clone(), Some(serde_json::Value::Null)),
        ],
        cancellation_signal.clone(),
    )
    .expect("splice completed process cancel");
    let second_await_suspended = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        post_cancel_replay,
    )
    .await
    .expect("suspend on the second process terminal await");
    assert_eq!(
        restate_message_types(&second_await_suspended)
            .expect("decode second-await suspension frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ]
    );
    let second_await_calls =
        restate_call_frames(&second_await_suspended).expect("decode second terminal await");
    let [second_await_call] = second_await_calls.as_slice() else {
        panic!("post-cancel suspension must append exactly one terminal await");
    };
    assert_eq!(second_await_call.handler, "await_terminal");

    let cancelled = fig790_cancelled_process_output(process_id);
    let redrive = encode_call_replay(
        "fig790-second-await-redrive",
        &input,
        &[
            (initial_calls[0].clone(), None),
            (initial_calls[1].clone(), Some(registered)),
            (cancel_call.clone(), Some(serde_json::Value::Null)),
            (
                second_await_call.clone(),
                Some(
                    serde_json::to_value(&cancelled)
                        .expect("serialize second-await process terminal"),
                ),
            ),
        ],
        cancellation_signal,
    )
    .expect("splice suspended second-await journal");
    let output = invoke_endpoint_body(&endpoint, "Fig790ProcessAwaitRedrive", "run", redrive)
        .await
        .expect("redrive must resume the journaled post-cancel terminal await");
    assert!(
        restate_call_frames(&output)
            .expect("decode second-await redrive calls")
            .is_empty(),
        "redrive must consume the exact journal without appending another cancel"
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(cancelled)
    );
}

#[test]
fn restate_session_cancel_sweep_excludes_turn_control_addresses() {
    let durable_wait = RestateDurableWaitAddress {
        workflow_key: "tool-wait".to_string(),
        session_id: Some("session".to_string()),
        classification: RestateDurableWaitClassification::DurableWait,
    };
    let cancel_gate = RestateDurableWaitAddress {
        workflow_key: "turn-cancel".to_string(),
        session_id: Some("session".to_string()),
        classification: RestateDurableWaitClassification::TurnControl,
    };
    let terminal = RestateDurableWaitAddress {
        workflow_key: "turn-terminal".to_string(),
        session_id: Some("session".to_string()),
        classification: RestateDurableWaitClassification::TurnControl,
    };

    let (cancelled, retained) = split_cancellable_waits(vec![
        durable_wait.clone(),
        cancel_gate.clone(),
        terminal.clone(),
    ]);
    assert_eq!(cancelled, vec![durable_wait]);
    assert_eq!(retained, vec![cancel_gate, terminal]);
}

// FIG-1631 fixtures: a turn-scoped sleep, which is the second caller of the
// shared turn-cancel gate. These pin the gate's journal geometry and its
// outcomes from the sleep side, so a change that only happened to keep the
// process-await tests green still has to answer for the sleep call site.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct Fig1631SleepGateInput {
    duration_ms: u64,
}

#[restate_sdk::workflow]
trait Fig1631SleepGate {
    async fn run(input: Json<Fig1631SleepGateInput>) -> HandlerResult<Json<String>>;
}

struct Fig1631SleepGateImpl;

impl Fig1631SleepGate for Fig1631SleepGateImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig1631SleepGateInput>,
    ) -> HandlerResult<Json<String>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, "fig1631-sleep-gate"),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: input.duration_ms,
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(tokio_util::sync::CancellationToken::new())
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await;
        Ok(Json(match outcome {
            Ok(RuntimeEffectOutcome::Sleep) => "slept".to_string(),
            Ok(other) => format!("unexpected outcome: {other:?}"),
            Err(error) => match &error.cause {
                Some(lash_core::RuntimeErrorCause::SessionDeleted { session_id }) => {
                    format!("session_deleted:{session_id}")
                }
                _ => error.code.to_string(),
            },
        }))
    }
}

fn fig1631_sleep_gate_endpoint() -> Endpoint {
    Endpoint::builder()
        .bind(Fig1631SleepGateImpl.serve())
        .build()
}

fn fig1631_sleep_gate_input() -> Fig1631SleepGateInput {
    Fig1631SleepGateInput {
        duration_ms: 60_000,
    }
}

// FIG-1631 fixtures: a turn-scoped await-event, the third caller of the shared
// gate. Before this change it raced through a nested workflow handler instead,
// so these pin the migrated journal geometry and both retirement paths.
// The scope must match `runtime_invocation`, or the effect is refused for a
// turn-cancel scope mismatch before it journals anything.
const FIG1631_AWAIT_SESSION: &str = "session";

#[restate_sdk::workflow]
trait Fig1631AwaitEventGate {
    async fn run(input: Json<Fig1126PendingToolRedriveInput>) -> HandlerResult<Json<String>>;
}

struct Fig1631AwaitEventGateImpl;

impl Fig1631AwaitEventGate for Fig1631AwaitEventGateImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig1126PendingToolRedriveInput>,
    ) -> HandlerResult<Json<String>> {
        let scope = durable_turn_scope(FIG1631_AWAIT_SESSION, "turn");
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("fig1631-await-call"),
        )
        .map_err(TerminalError::from_error)?;
        let outcome = RestateRuntimeEffectController::new(ctx)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "fig1631-await-gate"),
                    RuntimeEffectCommand::AwaitEvent { key },
                ),
                RuntimeEffectLocalExecutor::await_event(
                    tokio_util::sync::CancellationToken::new(),
                    None,
                )
                .with_turn_cancel_scope(scope),
            )
            .await;
        Ok(Json(match outcome {
            Ok(RuntimeEffectOutcome::AwaitEvent { resolution }) => {
                serde_json::to_string(&resolution).map_err(TerminalError::from_error)?
            }
            Ok(other) => format!("unexpected outcome: {other:?}"),
            Err(error) => match &error.cause {
                Some(lash_core::RuntimeErrorCause::SessionDeleted { session_id }) => {
                    format!("session_deleted:{session_id}")
                }
                _ => error.code.to_string(),
            },
        }))
    }
}

fn fig1631_await_event_endpoint() -> Endpoint {
    Endpoint::builder()
        .bind(Fig1631AwaitEventGateImpl.serve())
        .build()
}

fn fig1631_resolution_label(resolution: &Resolution) -> String {
    serde_json::to_string(resolution).expect("serialize resolution label")
}

/// Park a turn-scoped await-event on its gate and pin the journal positions.
///
/// The migrated gate journals the event call, then its gate awakeable, then the
/// registration — the same geometry as process await, and the positions every
/// redrive below lands on.
async fn fig1631_parked_await_event_gate(
    endpoint: &Endpoint,
    workflow_key: &str,
) -> Vec<endpoint_protocol::RestateCallFrame> {
    let parked = invoke_endpoint_with_named_call_responses(
        endpoint,
        "Fig1631AwaitEventGate",
        "run",
        workflow_key,
        &Fig1126PendingToolRedriveInput,
        vec![("is_revoked".to_string(), serde_json::json!(false))],
    )
    .await
    .expect("park the turn-scoped await-event on its gate");
    let calls = restate_call_frames(&parked).expect("decode await-event gate calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "await_resolution", "register_awakeable"],
        "the await-event gate replaces the nested workflow hop with one gate registration"
    );
    assert!(
        restate_message_types(&parked)
            .expect("decode parked await-event frames")
            .contains(&RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the gate must park once both the event and its gate are journaled"
    );
    calls
}

#[tokio::test]
async fn fig1631_await_event_gate_journals_the_event_before_its_gate() {
    let endpoint = fig1631_await_event_endpoint();
    fig1631_parked_await_event_gate(&endpoint, "fig1631-await-gate-positions").await;
}

/// Completion path: the winning event retires the gate entry it registered.
#[tokio::test]
async fn fig1631_await_event_completion_retires_its_gate_entry() {
    let endpoint = fig1631_await_event_endpoint();
    let terminal = Resolution::Ok(serde_json::json!({ "answer": "gated" }));
    let completed = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1631AwaitEventGate",
        "run",
        "fig1631-await-gate-completion",
        &Fig1126PendingToolRedriveInput,
        vec![
            ("is_revoked".to_string(), serde_json::json!(false)),
            ("register_awakeable".to_string(), fig1631_registered_gate()),
            (
                "await_resolution".to_string(),
                serde_json::to_value(&terminal).expect("serialize awaited resolution"),
            ),
            ("unregister_awakeable".to_string(), serde_json::Value::Null),
        ],
    )
    .await
    .expect("the event must win and retire its gate");
    assert_eq!(
        restate_call_frames(&completed)
            .expect("decode completion-path calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec![
            "is_revoked",
            "await_resolution",
            "register_awakeable",
            "unregister_awakeable"
        ],
        "a completed await-event must retire exactly the gate entry it registered"
    );
    assert_eq!(
        restate_output_json::<String>(&completed).as_deref(),
        Some(fig1631_resolution_label(&terminal).as_str())
    );
}

/// Cancel path: the index already dropped the entry, so the waiter must not
/// unregister it again — but it must release the losing event wait, which the
/// retired nested workflow used to do from its own journal.
#[tokio::test]
async fn fig1631_turn_cancelled_await_event_releases_the_losing_event_wait() {
    let endpoint = fig1631_await_event_endpoint();
    let workflow_key = "fig1631-await-gate-cancel";
    let calls = fig1631_parked_await_event_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &Fig1126PendingToolRedriveInput,
        &[
            (calls[0].clone(), Some(serde_json::json!(false))),
            (calls[1].clone(), None),
            (calls[2].clone(), Some(fig1631_registered_gate())),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize turn cancellation"),
        )),
    )
    .expect("splice a turn cancellation over the parked await-event gate");
    let cancelled = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1631AwaitEventGate",
        "run",
        replay,
        vec![serde_json::to_value(ResolveOutcome::Accepted).expect("serialize resolve outcome")],
    )
    .await
    .expect("turn cancellation must resolve the parked await-event");
    assert_eq!(
        restate_call_frames(&cancelled)
            .expect("decode cancel-path calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["resolve"],
        "the cancel path releases the losing event wait and leaves gate retirement to the index"
    );
    assert_eq!(
        restate_output_json::<String>(&cancelled).as_deref(),
        Some(fig1631_resolution_label(&Resolution::Cancelled).as_str())
    );
}

/// A session revoked out from under a parked await-event unwinds the turn as a
/// deleted session, the same new outcome the sleep gate now reports.
#[tokio::test]
async fn fig1631_session_revoked_await_event_unwinds_as_a_deleted_session() {
    let endpoint = fig1631_await_event_endpoint();
    let workflow_key = "fig1631-await-gate-revoked";
    let calls = fig1631_parked_await_event_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &Fig1126PendingToolRedriveInput,
        &[
            (calls[0].clone(), Some(serde_json::json!(false))),
            (calls[1].clone(), None),
            (
                calls[2].clone(),
                Some(
                    serde_json::to_value(RestateDurableWaitRegistration::Revoked)
                        .expect("serialize revoked registration"),
                ),
            ),
        ],
        None,
    )
    .expect("splice a revoked gate registration");
    let revoked = invoke_endpoint_body(&endpoint, "Fig1631AwaitEventGate", "run", replay)
        .await
        .expect("a revoked registration must unwind the await-event");
    assert_eq!(
        restate_output_json::<String>(&revoked).as_deref(),
        Some(format!("session_deleted:{FIG1631_AWAIT_SESSION}").as_str())
    );
}

fn fig1631_registered_gate() -> serde_json::Value {
    serde_json::to_value(RestateDurableWaitRegistration::Registered)
        .expect("serialize registered gate")
}

/// Walk a turn-scoped sleep to the point where its gate is live and its timer
/// is journaled.
///
/// The two stages are the deployed journal shape and the reason the gate is
/// ordered the way it is: the first attempt parks on `register_awakeable`, so
/// no timer is ever journaled until the session is known to be live. Only once
/// that registration completes does the timer become a command.
async fn fig1631_parked_sleep_gate(
    endpoint: &Endpoint,
    workflow_key: &str,
) -> (Vec<u8>, Vec<endpoint_protocol::RestateCallFrame>) {
    let registering = invoke_endpoint(
        endpoint,
        "Fig1631SleepGate",
        "run",
        workflow_key,
        &fig1631_sleep_gate_input(),
    )
    .await
    .expect("capture the gate registration");
    let calls = restate_call_frames(&registering).expect("decode sleep-gate calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable"],
        "the sleep gate registers before it can park on anything"
    );
    assert_eq!(
        restate_message_types(&registering).expect("decode registration frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ],
        "no timer may be journaled before the gate knows the session is live"
    );

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        None,
    )
    .expect("splice the completed gate registration");
    let parked = invoke_endpoint_body(endpoint, "Fig1631SleepGate", "run", replay)
        .await
        .expect("park on the gate's timer");
    assert_eq!(
        restate_message_types(&parked).expect("decode parked timer frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ],
        "a registered gate journals its timer next and parks on it"
    );
    (parked.to_vec(), calls)
}

/// The gate's journal positions are the contract: a fresh handler incarnation
/// must accept the recorded registration and timer and carry the sleep to its
/// ordinary completion.
#[tokio::test]
async fn fig1631_sleep_gate_redrives_from_its_journal_positions() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-redrive";
    let (parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_completed_gate_sleep_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &parked,
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
    )
    .expect("splice the parked gate journal with a fired timer");
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("redrive the parked sleep gate");
    assert!(
        restate_error_message(&redriven).is_none(),
        "redrive must accept the recorded gate prefix: {:?}",
        restate_error_message(&redriven)
    );
    assert_eq!(
        restate_output_json::<String>(&redriven).as_deref(),
        Some("slept")
    );
}

/// The new observable outcome: a session revoked while the sleep is parked
/// unwinds the turn as a deleted session instead of reporting a plain
/// cancellation.
#[tokio::test]
async fn fig1631_session_revoked_mid_sleep_unwinds_as_a_deleted_session() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-revoked-mid-sleep";
    let (_parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::SessionRevoked)
                .expect("serialize mid-sleep revocation"),
        )),
    )
    .expect("splice a revocation that fires after the gate registered");
    let revoked = invoke_endpoint_body(&endpoint, "Fig1631SleepGate", "run", replay)
        .await
        .expect("revocation must resolve the parked sleep");
    assert_eq!(
        restate_output_json::<String>(&revoked).as_deref(),
        Some("session_deleted:session"),
        "a revoked session must not be reported as an ordinary sleep cancellation"
    );
}

/// A session already revoked when the gate registers takes the same exit
/// without ever journaling a timer.
#[tokio::test]
async fn fig1631_session_revoked_before_sleep_registers_never_journals_a_timer() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let revoked = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        "fig1631-sleep-gate-revoked-at-registration",
        &fig1631_sleep_gate_input(),
        vec![(
            "register_awakeable".to_string(),
            serde_json::to_value(RestateDurableWaitRegistration::Revoked)
                .expect("serialize revoked registration"),
        )],
    )
    .await
    .expect("a revoked registration must unwind without parking");
    assert_eq!(
        restate_message_types(&revoked).expect("decode revoked-registration frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE,
        ],
        "a revoked session must not journal a timer it will never wait on"
    );
    assert_eq!(
        restate_output_json::<String>(&revoked).as_deref(),
        Some("session_deleted:session")
    );
}

/// Completion path: the winning timer must hand the gate entry back, or the
/// index keeps owing a wake to an awakeable nobody is holding.
#[tokio::test]
async fn fig1631_sleep_completion_retires_its_gate_entry() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-completion-retires";
    let (parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_completed_gate_sleep_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &parked,
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
    )
    .expect("splice a fired timer over the parked gate");
    let completed = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("the timer must win and retire the gate");
    assert_eq!(
        restate_call_frames(&completed)
            .expect("decode completion-path calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["unregister_awakeable"],
        "a completed sleep must retire exactly the gate entry it registered"
    );
}

/// Cancel path: the index resolved the awakeable, so it has already dropped the
/// entry. A second `unregister_awakeable` here would be the waiter clearing
/// state it no longer owns.
#[tokio::test]
async fn fig1631_turn_cancelled_sleep_leaves_gate_retirement_to_the_index() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-cancel-retires";
    let (_parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize turn cancellation"),
        )),
    )
    .expect("splice a turn cancellation over the parked gate");
    let cancelled = invoke_endpoint_body(&endpoint, "Fig1631SleepGate", "run", replay)
        .await
        .expect("turn cancellation must resolve the parked sleep");
    assert!(
        restate_call_frames(&cancelled)
            .expect("decode cancel-path calls")
            .is_empty(),
        "the index owns the entry it just resolved; the waiter must not unregister it again"
    );
    assert_eq!(
        restate_output_json::<String>(&cancelled).as_deref(),
        Some("runtime_effect_sleep_cancelled")
    );
}

#[test]
fn durable_wait_index_epoch_rejects_legacy_state_and_accepts_fresh_state() {
    let session_id = "upgrade-session";
    let durable_wait = RestateDurableWaitAddress {
        workflow_key: "durable-workflow".to_string(),
        session_id: Some(session_id.to_string()),
        classification: RestateDurableWaitClassification::DurableWait,
    };
    let turn_control = RestateDurableWaitAddress {
        workflow_key: "control-workflow".to_string(),
        session_id: Some(session_id.to_string()),
        classification: RestateDurableWaitClassification::TurnControl,
    };
    let awakeable = RestateDurableWaitAwakeableRequest {
        address: durable_wait.clone(),
        awakeable_id: "awakeable-1".to_string(),
    };

    // This byte payload is the value persisted under the old `waits` key.
    let old_layout_bytes = serde_json::to_vec(&RestateDurableWaitIndexState {
        revoked: true,
        waits: vec![durable_wait.clone(), turn_control.clone()],
        awakeables: vec![awakeable.clone()],
    })
    .expect("serialize old wait-index layout");
    let old_layout: RestateDurableWaitIndexState =
        serde_json::from_slice(&old_layout_bytes).expect("read pre-cutover wait-index layout");
    assert!(old_layout.revoked);
    assert_eq!(old_layout.waits.len(), 2);
    assert_eq!(
        old_layout.awakeables[0].awakeable_id,
        awakeable.awakeable_id
    );

    let error = validate_durable_wait_index_epoch(None, &["waits".to_string()])
        .expect_err("pre-cutover aggregate state must be rejected");
    assert!(error.contains("drain and recreate"));
    assert!(
        validate_durable_wait_index_epoch(None, &["wait-index/v1/metadata".to_string()])
            .expect_err("v1 wait-index state must be rejected")
            .contains("pre-cutover")
    );
    validate_durable_wait_index_epoch(None, &[]).expect("fresh state opens");
    validate_durable_wait_index_epoch(
        Some(DURABLE_WAIT_INDEX_IDENTITY_EPOCH),
        &[DURABLE_WAIT_INDEX_METADATA_KEY.to_string()],
    )
    .expect("matching epoch reopens current state");
    let wrong_epoch = validate_durable_wait_index_epoch(
        Some(DURABLE_WAIT_INDEX_IDENTITY_EPOCH - 1),
        &[DURABLE_WAIT_INDEX_METADATA_KEY.to_string()],
    )
    .expect_err("wrong identity epoch must be rejected");
    assert!(wrong_epoch.contains("incompatible with epoch 3"));
    assert!(wrong_epoch.contains("drain and recreate"));
    assert!(DURABLE_WAIT_INDEX_METADATA_KEY.starts_with("wait-index/v2/"));
}

fn wait_index_measurement_address(ordinal: usize) -> RestateDurableWaitAddress {
    RestateDurableWaitAddress {
        workflow_key: format!("{ordinal:064x}"),
        session_id: Some("restate-postgres-workers-e2e".to_string()),
        classification: RestateDurableWaitClassification::DurableWait,
    }
}

fn aggregate_wait_index_serialized_bytes(k: usize) -> usize {
    let waits = (0..k)
        .map(wait_index_measurement_address)
        .collect::<Vec<_>>();
    let mut bytes = 0;
    for registered in 1..=k {
        bytes += serde_json::to_vec(&RestateDurableWaitIndexState {
            revoked: false,
            waits: waits[..registered].to_vec(),
            awakeables: Vec::new(),
        })
        .expect("serialize aggregate wait-index register state")
        .len();
    }
    for remaining in (0..k).rev() {
        bytes += serde_json::to_vec(&RestateDurableWaitIndexState {
            revoked: false,
            waits: waits[..remaining].to_vec(),
            awakeables: Vec::new(),
        })
        .expect("serialize aggregate wait-index settle state")
        .len();
    }
    bytes
}

fn keyed_wait_index_serialized_bytes(k: usize) -> usize {
    let metadata_bytes = serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
        .expect("serialize keyed wait-index metadata")
        .len();
    metadata_bytes
        + (0..k)
            .map(wait_index_measurement_address)
            .map(|address| {
                serde_json::to_vec(&address)
                    .expect("serialize keyed wait-index entry")
                    .len()
            })
            .sum::<usize>()
}

#[test]
fn durable_wait_index_k_effect_measurements_are_linear() {
    for k in [4, 16] {
        // Each concurrent effect produces one register and one settle index
        // invocation in the workers-harness turn shape.
        let index_object_calls = 2 * k;
        let before_bytes = aggregate_wait_index_serialized_bytes(k);
        let after_bytes = keyed_wait_index_serialized_bytes(k);
        println!(
            "wait-index measurement K={k}: index_object_calls={index_object_calls}, before_serialized_state_bytes={before_bytes}, after_serialized_state_bytes={after_bytes}"
        );
        assert_eq!(index_object_calls, 2 * k);
        assert!(after_bytes < before_bytes);
    }
    assert_eq!(
        keyed_wait_index_serialized_bytes(16)
            - serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
                .expect("serialize metadata")
                .len(),
        4 * (keyed_wait_index_serialized_bytes(4)
            - serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
                .expect("serialize metadata")
                .len())
    );
}

#[test]
fn restate_effect_name_uses_lash_replay_key() {
    let invocation = RuntimeInvocation::effect(
        lash_core::runtime::RuntimeScope::for_turn("session", "turn", 1, 2),
        "effect",
        RuntimeEffectKind::ToolAttempt,
        "session:turn:1:2:tool_attempt:effect",
    );

    assert_eq!(
        restate_effect_name(&invocation),
        "lash:session:turn:1:2:tool_attempt:effect"
    );
}

#[tokio::test]
async fn restate_effect_host_satisfies_scope_factory_conformance() {
    lash_core::testing::conformance::effect_host(|| {
        Arc::new(RestateEffectHost::new("http://127.0.0.1:8080"))
    })
    .await;
}

#[tokio::test]
async fn restate_turn_work_driver_satisfies_shared_conformance() {
    let context = Arc::new(RecordingContext::default());
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new(context));
    lash_core::testing::conformance::turn_work_driver(host).await;
}

#[tokio::test]
async fn restate_handler_controller_satisfies_concurrent_replay_conformance() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let replay_context = Arc::clone(&context);

    lash_core::testing::conformance::effect_controller_concurrent_replay_deterministic(
        &controller,
        move || replay_context.start_replay(),
    )
    .await;

    let durable_context = Arc::new(ReplayableRecordingContext::default());
    let durable_controller = RestateRuntimeEffectController::new(Arc::clone(&durable_context));
    let durable_replay_context = Arc::clone(&durable_context);
    lash_core::testing::conformance::effect_controller_journaled_effect_replay(
        &durable_controller,
        move || durable_replay_context.start_replay(),
    )
    .await;

    let tool_context = Arc::new(ReplayableRecordingContext::default());
    let tool_controller = RestateRuntimeEffectController::new(Arc::clone(&tool_context));
    let tool_replay_context = Arc::clone(&tool_context);
    lash_core::testing::conformance::effect_controller_tool_attempt_fanout_replay_deterministic(
        &tool_controller,
        move || tool_replay_context.start_replay(),
    )
    .await;

    let runs = context.runs();
    assert_eq!(runs.len(), 4);
    assert!(runs.iter().any(|name| name.ends_with(":effect-slow")));
    assert!(runs.iter().any(|name| name.ends_with(":effect-fast")));

    let tool_runs = tool_context.runs();
    assert_eq!(tool_runs.len(), 4);
    assert!(
        tool_runs
            .iter()
            .any(|name| name.ends_with(":tool-attempt-slow"))
    );
    assert!(
        tool_runs
            .iter()
            .any(|name| name.ends_with(":tool-attempt-fast"))
    );
}

#[tokio::test]
async fn durable_trace_reemits_on_redrive_without_adding_a_journal_command() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().segment_effect_budget(1),
    )
    .with_trace_sink_and_context(
        sink_dyn,
        lash_trace::TraceContext {
            run_id: Some("restate-host-run".to_string()),
            ..lash_trace::TraceContext::default()
        },
    );
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new("trace-replay-session"),
            "trace-replay-tool",
            RuntimeEffectKind::ToolAttempt,
            "trace-replay-tool",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call_with("trace-replay-call", "trace_replay_tool"),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let local_calls = Arc::new(AtomicUsize::new(0));

    let first_calls = Arc::clone(&local_calls);
    controller
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                first_calls.fetch_add(1, Ordering::SeqCst);
                Ok(restate_segment_tool_attempt_outcome(0))
            }),
        )
        .await
        .expect("live journaled effect");
    context.start_replay();
    let replay_calls = Arc::clone(&local_calls);
    controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                replay_calls.fetch_add(1, Ordering::SeqCst);
                Ok(restate_segment_tool_attempt_outcome(0))
            }),
        )
        .await
        .expect("replayed journaled effect");
    assert_eq!(
        RuntimeEffectController::wants_segment_boundary(
            &controller,
            &lash_core::SegmentProgress {
                effects_executed: 1,
                journaled_bytes_estimate: Some(128),
            },
        ),
        Some(lash_core::BoundaryReason::JournalBudget)
    );

    assert_eq!(
        local_calls.load(Ordering::SeqCst),
        1,
        "redrive reuses the journaled outcome"
    );
    assert_eq!(
        context.runs().len(),
        2,
        "each handler pass issues only the effect's ctx.run; trace append adds no command"
    );
    let events = sink
        .records
        .lock_recover()
        .iter()
        .map(|record| record.event.kind())
        .collect::<Vec<_>>();
    assert_eq!(
        events,
        vec![
            "journaled_effect_started",
            "journaled_effect_settled",
            "journaled_effect_started",
            "journaled_effect_settled",
            "durable_segment_boundary",
        ],
        "redrive repetition is the benign live-observation class"
    );
    let records = sink.records.lock_recover();
    assert!(records.iter().all(|record| {
        record.context.run_id.as_deref() == Some("restate-host-run")
            && record.context.session_id.as_deref() == Some("trace-replay-session")
    }));
    assert_eq!(
        records
            .last()
            .and_then(|record| record.context.effect_id.as_deref()),
        Some("trace-replay-tool"),
        "segment boundaries retain the scope of the effect that crossed the budget"
    );
}

#[tokio::test]
async fn restate_handler_controller_journals_typed_trigger_execution() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new("restate-trigger-session"),
            "restate-trigger-list",
            RuntimeEffectKind::Trigger,
            "restate-trigger-list",
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session("restate-trigger-session"),
                filter: lash_core::TriggerSubscriptionFilter::default(),
            }),
        },
    );
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::new())
        as Arc<dyn lash_core::TriggerStore>;

    let outcome = controller
        .execute_effect(envelope, RuntimeEffectLocalExecutor::triggers(store))
        .await
        .expect("handler-scoped Restate controller must execute typed trigger effects")
        .into_trigger()
        .expect("typed trigger outcome");

    assert!(matches!(
        outcome,
        Ok(lash_core::TriggerCommandOutcome::List { records }) if records.is_empty()
    ));
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-trigger-list"]
    );
}

fn fig1464_poison_list_envelope(session: &str, effect: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new(session),
            effect,
            RuntimeEffectKind::Trigger,
            effect,
        ),
        RuntimeEffectCommand::Trigger {
            command: Box::new(lash_core::TriggerCommand::List {
                owner_scope: lash_core::TriggerOwnerScope::session(session),
                filter: lash_core::TriggerSubscriptionFilter::default(),
            }),
        },
    )
}

/// FIG-1464 poison path: an effect outcome the durable journal will refuse
/// would fail every redrive of the enclosing turn identically, leaving the turn
/// uncommitted forever. The seam decides that verdict itself and gives up with a
/// typed terminal failure the host can observe, while still journaling the
/// effect exactly once so replay reproduces the same give-up.
#[tokio::test]
async fn fig1464_unjournalable_effect_outcome_gives_up_with_a_typed_terminal_failure() {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    store
        .execute_command(
            "fig1464-poison-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::session("restate-poison-session"),
                actor: lash_core::ProcessOriginator::host_scoped("fig1464"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "fig1464/poison-subscription",
                    lash_core::ProcessExecutionEnvRef::new("process-env:fig1464"),
                    "ui.button.pressed",
                    source_key,
                    ProcessInput::Engine {
                        kind: "fig1464-engine".to_string(),
                        payload: serde_json::json!({}),
                    },
                    lash_core::ProcessIdentity::new("fig1464-engine"),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("seed a subscription so the listed outcome outgrows the budget")
        .expect("trigger registration outcome");

    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        // Sits above the poison substitute (envelope plus a fixed-length typed
        // message) and below the listed subscription record.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(700),
    );

    let error = controller
        .execute_effect(
            fig1464_poison_list_envelope("restate-poison-session", "restate-poison-list"),
            RuntimeEffectLocalExecutor::triggers(store as Arc<dyn lash_core::TriggerStore>),
        )
        .await
        .expect_err("an unjournalable effect outcome must not be recorded as a result");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        error.code.is_terminal(),
        "the give-up must not be re-attempted"
    );
    assert!(
        error.message.contains("durable journal budget"),
        "the typed failure must name why the effect gave up: {}",
        error.message
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-poison-list"],
        "the give-up is journaled once, so replay reproduces it"
    );
}

/// FIG-1464: the poison substitute still carries the envelope replay validation
/// matches on, so an envelope that is itself over budget leaves no journalable
/// record at all. Journaling the substitute anyway would propose an entry the
/// engine rejects, reviving the redrive loop with the give-up now silent. The
/// seam decides before it pays for the effect, and occupies the journal slot with
/// the fixed-size poison entry so the journal shape does not depend on the
/// configured budget.
#[tokio::test]
async fn fig1464_over_budget_envelope_gives_up_with_a_fixed_size_poison_entry() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    );
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::new())
        as Arc<dyn lash_core::TriggerStore>;

    let error = controller
        .execute_effect(
            fig1464_poison_list_envelope("restate-wide-envelope-session", "restate-wide-envelope"),
            RuntimeEffectLocalExecutor::triggers(store),
        )
        .await
        .expect_err("an unjournalable envelope must not be recorded as a result");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        error.code.is_terminal(),
        "the give-up must not be re-attempted"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-wide-envelope"],
        "the give-up must occupy its journal slot exactly once"
    );
}

/// FIG-1464: the tool-batch and durable-process-command sites run their effect
/// outside the run closure, so the budget give-up has to be their pre-flight
/// gate. A give-up decided after the batch ran would discard a settled batch
/// with no journal entry, and the next redrive would execute every child again.
#[tokio::test]
async fn fig1464_over_budget_tool_batch_gives_up_before_running_the_batch() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    );
    let executed = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&executed);

    let error = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::ToolBatch, "fig1464-over-budget-batch"),
                RuntimeEffectCommand::ToolBatch {
                    batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
                },
            ),
            RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                ran.store(true, Ordering::SeqCst);
                Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RestateEffectController,
                    "an over-budget tool batch must never run",
                ))
            }),
        )
        .await
        .expect_err("an unjournalable envelope must not be recorded as a result");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "the give-up must be decided before the batch runs"
    );
    assert_eq!(
        context.runs.lock_recover().len(),
        1,
        "the give-up must occupy its journal slot exactly once"
    );
}

/// FIG-1464 deciding risk: the give-up verdict reads a process-configured
/// budget, so a budget change between attempts must not flip the *shape* of the
/// journal. The give-up occupies its slot with a fixed-size poison entry, so a
/// larger budget on redrive consumes the same slot and observes the same typed
/// failure instead of diverging by proposing a record where the first attempt
/// proposed nothing.
#[tokio::test]
async fn fig1464_over_budget_give_up_replays_identically_under_a_larger_budget() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::new())
        as Arc<dyn lash_core::TriggerStore>;
    let envelope =
        || fig1464_poison_list_envelope("restate-budget-flip-session", "restate-budget-flip");

    let recorded = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        envelope(),
        RuntimeEffectLocalExecutor::triggers(Arc::clone(&store)),
    )
    .await
    .expect_err("the over-budget envelope must give up");
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-budget-flip"],
        "the give-up must occupy its journal slot"
    );

    context.replaying.store(true, Ordering::SeqCst);
    let replayed = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        // The budget the redrive was configured with now clears the envelope, so
        // an un-journaled give-up would have journaled a record here.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(envelope(), RuntimeEffectLocalExecutor::triggers(store))
    .await
    .expect_err("replaying the poison entry must reproduce the give-up");

    assert_eq!(replayed.code, recorded.code);
    assert_eq!(
        replayed.message, recorded.message,
        "the replayed give-up must render the journaled verdict, not the new budget"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        ["lash:restate-budget-flip", "lash:restate-budget-flip"],
        "the redrive must consume the same journal slot, not add one"
    );
}

/// FIG-1464 round 3, residual B: at the tool-batch and process-command sites the
/// effect runs outside the run closure, so nothing but this seam's own journal
/// slot can stop a replay from running it again. Re-deciding the give-up from
/// live config was not enough: a redrive configured with a larger budget cleared
/// the envelope, ran the batch, and only then replayed the poison entry and threw
/// the settled batch away - an at-least-once execution of every child. The
/// verdict is journaled ahead of the batch, so the journaled verdict is what
/// decides on replay and the batch never runs.
#[tokio::test]
async fn fig1464_over_budget_tool_batch_replay_under_a_larger_budget_never_runs_the_batch() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let batch_envelope = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::ToolBatch, "fig1464-budget-flip-batch"),
            RuntimeEffectCommand::ToolBatch {
                batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
            },
        )
    };
    let never_runs = || {
        RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            panic!("a batch whose give-up is already journaled must never run");
        })
    };

    let recorded = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(batch_envelope(), never_runs())
    .await
    .expect_err("the over-budget batch must give up before running");

    context.replaying.store(true, Ordering::SeqCst);
    let replayed = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        // Big enough that a give-up re-decided from live config would proceed,
        // run the batch, and only then meet the journaled give-up.
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(batch_envelope(), never_runs())
    .await
    .expect_err("the journaled verdict must still give up");

    assert_eq!(
        replayed.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert_eq!(
        replayed.message, recorded.message,
        "the replayed give-up must render the journaled verdict, not the new budget"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:session:turn:1:0:tool_batch:fig1464-budget-flip-batch.journal-budget",
            "lash:session:turn:1:0:tool_batch:fig1464-budget-flip-batch.journal-budget"
        ],
        "the redrive must consume the same verdict slot and add none"
    );
}

/// FIG-1767: both eager effect arms (durable process command and durable tool batch)
/// emit byte-identical journal records before and after collapsing into the shared helper.
#[tokio::test]
async fn fig1767_journal_entry_byte_sequence_equality() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    );

    // Arm 1: Durable Process Command
    let process_invocation = RuntimeInvocation::effect(
        RuntimeScope::for_turn("fig1767-session", "fig1767-turn", 1, 0),
        "fig1767-process-cmd",
        RuntimeEffectKind::Process,
        "fig1767-process-cmd",
    );
    let process_envelope = RuntimeEffectEnvelope::new(
        process_invocation,
        RuntimeEffectCommand::Process {
            command: Box::new(ProcessCommand::ParentEnd {
                identity: lash_core::ToolIntentIdentity {
                    session_id: "fig1767".to_string(),
                    execution_scope_id: "scope".to_string(),
                    tool_call_id: "call".to_string(),
                    intent_index: 0,
                    replay_key: "key".to_string(),
                },
                process_id: "fig1767-proc".to_string(),
                policy: lash_core::ProcessParentEndPolicy::Cancel,
                reason: "fig1767-test".to_string(),
            }),
        },
    );
    controller
        .execute_effect(
            process_envelope.clone(),
            RuntimeEffectLocalExecutor::processes(process_registry(), None),
        )
        .await
        .expect("process command effect execution");

    // Retrieve records produced for DurableProcessCommand. These bytes are the
    // golden serialization captured from main before the helper extraction.

    {
        let process_verdict_key = "lash:fig1767-process-cmd.journal-budget";
        let process_record_key = "lash:fig1767-process-cmd";

        let records = context.records.lock_recover();
        let process_verdict_bytes = records
            .get(process_verdict_key)
            .expect("process budget verdict journal entry");
        let process_record_bytes = records
            .get(process_record_key)
            .expect("process effect record journal entry");

        // Pin verdict entry byte sequence: JournaledBudgetVerdict::Proceed serializes as "Proceed"
        assert_eq!(
            process_verdict_bytes.as_slice(),
            b"\"Proceed\"",
            "process command budget verdict byte sequence mismatch"
        );
        assert_eq!(
            process_record_bytes,
            br##"{"envelope":{"json":"{\"invocation\":{\"scope\":{\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\",\"turn_index\":1,\"protocol_iteration\":0},\"subject\":{\"type\":\"effect\",\"effect_id\":\"fig1767-process-cmd\",\"kind\":\"process\"},\"replay\":{\"key\":\"fig1767-process-cmd\"}},\"command\":{\"type\":\"process\",\"command\":{\"op\":\"parent_end\",\"identity\":{\"session_id\":\"fig1767\",\"execution_scope_id\":\"scope\",\"tool_call_id\":\"call\",\"intent_index\":0,\"replay_key\":\"key\"},\"process_id\":\"fig1767-proc\",\"policy\":\"cancel\",\"reason\":\"fig1767-test\"}}}","hash":"c282f7946c2d914f62cc9d3494999c640754549700179d2df1dd31a78450da5e"},"outcome":{"Ok":{"type":"process","result":{"op":"parent_end","outcome":{"status":"refused","identity":{"session_id":"fig1767","execution_scope_id":"scope","tool_call_id":"call","intent_index":0,"replay_key":"key"},"process_id":"fig1767-proc","code":"plugin","message":"plugin session error: unknown process `fig1767-proc`"}}}}}"##,
            "process command recorded effect golden bytes changed"
        );
    }

    // Arm 2: Durable Tool Batch
    let batch_invocation = RuntimeInvocation::effect(
        RuntimeScope::for_turn("fig1767-session", "fig1767-turn", 1, 0),
        "fig1767-tool-batch",
        RuntimeEffectKind::ToolBatch,
        "fig1767-tool-batch",
    );
    let batch_envelope = RuntimeEffectEnvelope::new(
        batch_invocation,
        RuntimeEffectCommand::ToolBatch {
            batch: lash_core::PreparedToolBatch::new("fig1767-batch", vec![prepared_tool_call()]),
        },
    );
    controller
        .execute_effect(
            batch_envelope.clone(),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Ok(RuntimeEffectOutcome::ToolBatch {
                    launches: vec![],
                    triggers: vec![],
                    settlement_order: vec![],
                })
            }),
        )
        .await
        .expect("tool batch effect execution");

    // Retrieve records produced for DurableToolBatch
    let batch_verdict_key = "lash:fig1767-tool-batch.journal-budget";
    let batch_record_key = "lash:fig1767-tool-batch";

    let records = context.records.lock_recover();
    let batch_verdict_bytes = records
        .get(batch_verdict_key)
        .expect("batch budget verdict journal entry");
    let batch_record_bytes = records
        .get(batch_record_key)
        .expect("batch effect record journal entry");

    assert_eq!(
        batch_verdict_bytes.as_slice(),
        b"\"Proceed\"",
        "tool batch budget verdict byte sequence mismatch"
    );
    assert_eq!(
        batch_record_bytes,
        br##"{"envelope":{"json":"{\"invocation\":{\"scope\":{\"session_id\":\"fig1767-session\",\"turn_id\":\"fig1767-turn\",\"turn_index\":1,\"protocol_iteration\":0},\"subject\":{\"type\":\"effect\",\"effect_id\":\"fig1767-tool-batch\",\"kind\":\"tool_batch\"},\"replay\":{\"key\":\"fig1767-tool-batch\"}},\"command\":{\"type\":\"tool_batch\",\"batch\":{\"batch_id\":\"fig1767-batch\",\"calls\":[{\"call\":{\"call_id\":\"call-1\",\"tool_id\":\"tool:tool\",\"tool_name\":\"tool\",\"args\":{}},\"replay_suffix\":\"child:0:call-1\"}]}}}","hash":"053953e6fdac9aa935e9ca8c4b79bb6d7457792445c864505ca308bfffe7a3c6"},"outcome":{"Ok":{"type":"tool_batch","launches":[],"settlement_order":[]}}}"##,
        "tool batch recorded effect golden bytes changed"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:fig1767-process-cmd.journal-budget",
            "lash:fig1767-process-cmd",
            "lash:fig1767-tool-batch.journal-budget",
            "lash:fig1767-tool-batch"
        ],
        "each eager effect must journal its budget verdict before its recorded effect"
    );
}

/// FIG-1767: redriving an eager effect (both durable process command and durable tool batch)
/// whose journaled budget verdict is a give-up executes nothing — the run future reaches
/// the helper unpolled and is never executed.
#[tokio::test]
async fn fig1767_give_up_verdict_redrive_executes_nothing() {
    let context = Arc::new(ReplayableRecordingContext::default());

    // 1. Durable Process Command over budget
    let process_invocation = RuntimeInvocation::effect(
        RuntimeScope::for_turn("fig1767-session", "fig1767-turn", 1, 0),
        "fig1767-over-budget-proc",
        RuntimeEffectKind::Process,
        "fig1767-over-budget-proc",
    );
    let process_envelope = RuntimeEffectEnvelope::new(
        process_invocation,
        RuntimeEffectCommand::Process {
            command: Box::new(ProcessCommand::ParentEnd {
                identity: lash_core::ToolIntentIdentity {
                    session_id: "fig1767".to_string(),
                    execution_scope_id: "scope".to_string(),
                    tool_call_id: "call".to_string(),
                    intent_index: 0,
                    replay_key: "key".to_string(),
                },
                process_id: "fig1767-proc".to_string(),
                policy: lash_core::ProcessParentEndPolicy::Cancel,
                reason: "fig1767-test".to_string(),
            }),
        },
    );

    let recorded_proc_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        process_envelope.clone(),
        RuntimeEffectLocalExecutor::processes(process_registry(), None),
    )
    .await
    .expect_err("process command over budget must give up");

    assert_eq!(
        recorded_proc_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );

    // Redrive process command under a larger budget — must read journaled verdict and execute nothing.
    // Use a real process executor: if the gate moved after the work, its outcome observer would
    // witness the ParentEnd operation before the missing effect record fails the redrive.
    context.replaying.store(true, Ordering::SeqCst);
    let replay_registry = process_registry();
    replay_registry
        .register_process(external_registration("fig1767-proc"))
        .await
        .expect("register process for redrive witness");
    let process_executed = Arc::new(AtomicBool::new(false));
    let ran_proc = Arc::clone(&process_executed);
    let process_observer: lash_core::ProcessOutcomeObserver = Arc::new(move |_| {
        ran_proc.store(true, Ordering::SeqCst);
    });

    let replayed_proc_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(
        process_envelope,
        RuntimeEffectLocalExecutor::processes(replay_registry, None)
            .with_process_outcome_observer(process_observer),
    )
    .await
    .expect_err("replayed give-up verdict must return poisoned error");

    assert_eq!(
        replayed_proc_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        !process_executed.load(Ordering::SeqCst),
        "redriving a process command give-up verdict must execute nothing"
    );

    // 2. Durable Tool Batch over budget
    context.replaying.store(false, Ordering::SeqCst);
    let batch_invocation = RuntimeInvocation::effect(
        RuntimeScope::for_turn("fig1767-session", "fig1767-turn", 1, 0),
        "fig1767-over-budget-batch",
        RuntimeEffectKind::ToolBatch,
        "fig1767-over-budget-batch",
    );
    let batch_envelope = RuntimeEffectEnvelope::new(
        batch_invocation,
        RuntimeEffectCommand::ToolBatch {
            batch: lash_core::PreparedToolBatch::new("fig1767-batch", vec![prepared_tool_call()]),
        },
    );

    let recorded_batch_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
    )
    .execute_effect(
        batch_envelope.clone(),
        RuntimeEffectLocalExecutor::testing(|_| async {
            panic!("tool batch initial attempt must give up before running work");
        }),
    )
    .await
    .expect_err("tool batch over budget must give up");

    assert_eq!(
        recorded_batch_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );

    // Redrive tool batch under a larger budget — must read journaled verdict and execute nothing
    context.replaying.store(true, Ordering::SeqCst);
    let batch_executed = Arc::new(AtomicBool::new(false));
    let ran_batch = Arc::clone(&batch_executed);

    let replayed_batch_err = RestateRuntimeEffectController::with_options(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().journaled_effect_byte_budget(4_096),
    )
    .execute_effect(
        batch_envelope,
        RuntimeEffectLocalExecutor::testing(move |_| async move {
            ran_batch.store(true, Ordering::SeqCst);
            panic!("tool batch work closure must never be executed on give-up redrive");
        }),
    )
    .await
    .expect_err("replayed give-up verdict must return poisoned error");

    assert_eq!(
        replayed_batch_err.code,
        lash_core::RuntimeErrorCode::RestateJournaledEffectPoisoned
    );
    assert!(
        !batch_executed.load(Ordering::SeqCst),
        "redriving a tool batch give-up verdict must execute nothing"
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [
            "lash:fig1767-over-budget-proc.journal-budget",
            "lash:fig1767-over-budget-proc.journal-budget",
            "lash:fig1767-over-budget-batch.journal-budget",
            "lash:fig1767-over-budget-batch.journal-budget"
        ],
        "a give-up redrive must consume only the verdict slot before the next effect"
    );
}

#[tokio::test]
async fn journaled_cancel_peeks_replay_while_live_watcher_observes_later_cancel() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scope = durable_turn_scope("journaled-peek-session", "journaled-peek-turn");
    let key = controller
        .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .await
        .expect("cancel gate key");
    let envelope = |identity: &str| {
        RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope {
                    session_id: "journaled-peek-session".to_string(),
                    turn_id: Some("journaled-peek-turn".to_string()),
                    turn_index: None,
                    protocol_iteration: None,
                },
                identity,
                RuntimeEffectKind::PeekAwaitEvent,
                identity,
            ),
            RuntimeEffectCommand::PeekAwaitEvent { key: key.clone() },
        )
    };
    let start = envelope("turn_cancel.start_gate");
    let post_abort = envelope("turn_cancel.post_abort_gate");
    assert_ne!(
        start.stable_hash().expect("start hash"),
        post_abort.stable_hash().expect("post-abort hash"),
        "later owner reads require a distinct causal identity"
    );

    let first = controller
        .execute_effect(start.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("fresh start-gate peek")
        .into_peek_await_event()
        .expect("start-gate outcome");
    assert_eq!(first, None);

    let cancellation = Resolution::Ok(serde_json::json!({
        "status": "cancel_requested",
        "request_id": "after-start"
    }));
    assert_eq!(
        controller
            .resolve_await_event(&key, cancellation.clone())
            .await
            .expect("resolve cancel gate"),
        ResolveOutcome::Accepted
    );
    let later = controller
        .execute_effect(
            post_abort.clone(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("fresh post-abort peek")
        .into_peek_await_event()
        .expect("post-abort outcome");
    assert_eq!(later, Some(cancellation.clone()));

    context.start_replay();
    let replayed_start = controller
        .execute_effect(start, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("replayed start-gate peek")
        .into_peek_await_event()
        .expect("replayed start-gate outcome");
    let replayed_later = controller
        .execute_effect(post_abort, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("replayed post-abort peek")
        .into_peek_await_event()
        .expect("replayed post-abort outcome");
    assert_eq!(replayed_start, None);
    assert_eq!(replayed_later, Some(cancellation.clone()));

    let live = controller
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("live watcher observes durable cancellation");
    assert_eq!(live, cancellation);
}

#[test]
fn restate_handler_controller_disallows_concurrent_effect_calls() {
    let controller = RestateRuntimeEffectController::new(Arc::new(RecordingContext::default()));

    assert!(
        !controller.supports_concurrent_effects(),
        "Restate handler context calls such as ctx.run must be awaited before the next effect call"
    );
}

#[test]
fn recorded_runtime_effect_hash_mismatch_fails_explicitly() {
    let recorded_envelope = test_sleep_envelope(1)
        .canonical_form()
        .expect("recorded envelope");
    let reconstructed = test_sleep_envelope(2)
        .canonical_form()
        .expect("reconstructed envelope");
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(recorded_envelope),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    };

    let err = validate_recorded_effect_envelope(recorded, &reconstructed, None)
        .expect_err("hash mismatch");

    assert_eq!(
        err.code,
        lash_core::RuntimeErrorCode::RestateEffectHashMismatch
    );
    assert!(
        err.code.is_replay_mismatch(),
        "Restate replay divergence must retain the shared typed classification"
    );
    assert_eq!(
        err.summary.expect("mismatch summary"),
        lash_core::RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.duration_ms".to_string()],
        }
    );
}

#[test]
fn recorded_runtime_effect_hash_match_returns_replayed_outcome() {
    let envelope = test_sleep_envelope(1)
        .canonical_form()
        .expect("canonical envelope");
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(envelope.clone()),
        outcome: Ok(RuntimeEffectOutcome::Sleep),
    };

    let outcome = validate_recorded_effect_envelope(recorded, &envelope, None)
        .expect("hash match")
        .expect("replayed outcome");

    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
}

fn test_sleep_envelope(duration_ms: u64) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            lash_core::runtime::RuntimeScope::for_turn("session", "turn", 0, 0),
            "sleep:test",
            RuntimeEffectKind::Sleep,
            "sleep:test",
        ),
        RuntimeEffectCommand::Sleep { duration_ms },
    )
}

fn llm_spec() -> lash_core::LlmRequestSpec {
    lash_core::LlmRequestSpec {
        model: "model".to_string(),
        messages: Vec::new(),
        attachments: Vec::new(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session".to_string(),
            "session:frame:test".to_string(),
            "session:request:test".to_string(),
        ),
        output_spec: None,
    }
}

fn prepared_tool_call() -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall::from_parts(
        "call-1",
        "tool:tool",
        "tool",
        serde_json::json!({}),
        None,
        serde_json::Value::Null,
    )
}

fn prepared_tool_call_with(call_id: &str, tool_name: &str) -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall::from_parts(
        call_id,
        format!("tool:{tool_name}"),
        tool_name,
        serde_json::json!({ "call": call_id }),
        None,
        serde_json::Value::Null,
    )
}

fn completed_tool_record(call_id: &str, tool_name: &str) -> lash_core::ToolCallRecord {
    lash_core::ToolCallRecord {
        call_id: Some(call_id.to_string()),
        tool: tool_name.to_string(),
        args: serde_json::json!({ "call": call_id }),
        output: lash_core::ToolCallOutput::success(serde_json::json!({ "call": call_id })),
        duration_ms: 1,
    }
}

fn external_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::ExternallyOwned,
        lash_core::ProcessProvenance::host(),
    )
}

fn rerunnable_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
}

fn owner_bound_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::OwnerBound,
        lash_core::ProcessProvenance::host(),
    )
}

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

fn process_registry() -> Arc<dyn ProcessRegistry> {
    Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite registry")
    }))
}

fn continuation_store() -> Arc<dyn lash_core::ProcessContinuationStore> {
    Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite continuation store")
    }))
}

fn process_stores() -> (
    Arc<dyn ProcessRegistry>,
    Arc<dyn lash_core::ProcessContinuationStore>,
) {
    let storage = Arc::new(sync_await(async {
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("sqlite process stores")
    }));
    (
        Arc::clone(&storage) as Arc<dyn ProcessRegistry>,
        storage as Arc<dyn lash_core::ProcessContinuationStore>,
    )
}

fn lashlang_process_input(input: lash_lashlang_runtime::LashlangProcessInput) -> ProcessInput {
    input
        .into_process_input()
        .expect("serialize lashlang process input")
}

#[derive(Default)]
struct DurableMemoryAttachmentStore {
    inner: lash_core::facade_support::InMemoryAttachmentStore,
}

#[async_trait::async_trait]
impl lash_core::AttachmentStore for DurableMemoryAttachmentStore {
    fn persistence(&self) -> lash_core::AttachmentStorePersistence {
        lash_core::AttachmentStorePersistence::Durable
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: lash_core::AttachmentCreateMeta,
    ) -> Result<lash_core::AttachmentRef, lash_core::AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<lash_core::StoredAttachment, lash_core::AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<Option<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.head(id).await
    }
}

#[derive(Default)]
struct DurableMemoryProcessEnvStore {
    inner: lash_core::facade_support::InMemoryProcessExecutionEnvStore,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for DurableMemoryProcessEnvStore {
    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        self.inner.put_process_execution_env(env_ref, bytes).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

static RECOVERY_PROCESS_ENV_STORE: LazyLock<Arc<DurableMemoryProcessEnvStore>> =
    LazyLock::new(|| Arc::new(DurableMemoryProcessEnvStore::default()));

struct CommitRetryStore {
    inner: Arc<dyn lash_core::RuntimePersistence>,
    lease_claim_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    fn new(inner: Arc<dyn lash_core::RuntimePersistence>) -> Self {
        Self {
            inner,
            lease_claim_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

lash_core::impl_noop_attachment_manifest!(CommitRetryStore);

// Pass-through wrapper over the shared in-memory recovery store; every
// segment delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::SessionCommitStore for CommitRetryStore {
    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, lash_core::StoreError> {
        self.inner.admit_and_bind_session(binding).await
    }

    async fn load_session(
        &self,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, lash_core::StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        self.inner.commit_runtime_state(commit).await
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<lash_core::SessionMeta>, lash_core::StoreError> {
        self.inner.load_session_meta().await
    }
}

#[async_trait::async_trait]
impl lash_core::SessionExecutionLeaseStore for CommitRetryStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, lash_core::StoreError> {
        self.lease_claim_count.fetch_add(1, Ordering::SeqCst);
        self.inner
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                executor_id,
                claim_nonce,
                lease_ttl_ms,
            )
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLease, lash_core::StoreError> {
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.release_session_execution_lease(completion).await
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<lash_core::SessionExecutionLease>, lash_core::StoreError> {
        self.inner.get_session_execution_lease(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::QueuedWorkStore for CommitRetryStore {
    async fn enqueue_queued_work(
        &self,
        batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> Result<lash_core::runtime::QueuedWorkBatch, lash_core::StoreError> {
        self.inner.enqueue_queued_work(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
    ) -> Result<Option<lash_core::runtime::QueuedWorkClaim>, lash_core::StoreError> {
        self.inner
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<lash_core::QueuedWorkClaimOutcome, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work(session_id, session_execution_lease, owner, boundary, policy)
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<lash_core::runtime::TurnInputClaim>,
            Option<lash_core::runtime::QueuedWorkClaim>,
        ),
        lash_core::StoreError,
    > {
        self.inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<lash_core::SelectedQueuedWorkClaimOutcome, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                batch_ids,
                policy,
            )
            .await
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &lash_core::runtime::QueuedWorkClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &str,
    ) -> Result<lash_core::store::PendingSessionWorkOrdering, lash_core::StoreError> {
        self.inner.pending_session_work_ordering(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_pending_queued_work(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::TurnInputStore for CommitRetryStore {
    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, lash_core::StoreError> {
        self.inner.enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::PendingTurnInput>, lash_core::StoreError> {
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelReceipt>, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_active_turn_inputs(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
            )
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::runtime::TurnInputClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_turn_input_claim(claim).await
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> Result<usize, lash_core::StoreError> {
        self.inner
            .defer_orphaned_active_turn_inputs(session_id, session_execution_lease, scope)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::StoreMaintenance for CommitRetryStore {
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, lash_core::StoreError> {
        self.inner
            .seed_session_trigger_manifest_ref_for_testing(session_id)
            .await
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, lash_core::StoreError> {
        self.inner
            .raw_session_owned_artifact_refs_for_testing(session_id)
            .await
    }

    async fn vacuum(&self) -> lash_core::MaintenanceResult<lash_core::VacuumReport> {
        self.inner.vacuum().await
    }

    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<lash_core::GcReport> {
        self.inner.gc_unreachable().await
    }
}

#[test]
fn restate_command_execution_plan_is_explicit_for_every_command() {
    let cases = vec![
        (RuntimeEffectCommand::Sleep { duration_ms: 1 }, "timer"),
        (
            RuntimeEffectCommand::process(ProcessCommand::List {
                session_scope: lash_core::SessionScope::new("session"),
                mode: lash_core::ProcessListMode::Live,
            }),
            "direct_process",
        ),
        (
            RuntimeEffectCommand::AwaitEvent {
                key: restate_await_event_key(
                    &durable_turn_scope("session", "turn"),
                    AwaitEventWaitIdentity::Custom {
                        key: "event".to_string(),
                    },
                )
                .expect("await-event key"),
            },
            "await_event",
        ),
        (
            RuntimeEffectCommand::PeekAwaitEvent {
                key: restate_await_event_key(
                    &durable_turn_scope("session", "turn"),
                    AwaitEventWaitIdentity::Custom {
                        key: "peek-event".to_string(),
                    },
                )
                .expect("peek-await-event key"),
            },
            "peek_await_event",
        ),
        (
            RuntimeEffectCommand::LlmCall {
                request: Box::new(llm_spec()),
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::Direct {
                request: Box::new(llm_spec()),
                usage_source: "test".to_string(),
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::ToolBatch {
                batch: lash_core::PreparedToolBatch::new("batch", vec![prepared_tool_call()]),
            },
            "durable_tool_batch",
        ),
        (
            RuntimeEffectCommand::ExecCode {
                language: "code".to_string(),
                code: "1 + 1".to_string(),
            },
            // The interpreter is composite: it can issue nested timers,
            // waits, tools, and model calls. Rebuild it on handler replay and
            // let those child effects use their own stable journal keys.
            "direct_local",
        ),
        (
            RuntimeEffectCommand::Checkpoint {
                checkpoint: lash_core::CheckpointKind::AfterWork,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::SyncExecutionEnvironment {
                update_machine_config: true,
            },
            "journaled_run",
        ),
        (
            RuntimeEffectCommand::Trigger {
                command: Box::new(lash_core::TriggerCommand::List {
                    owner_scope: lash_core::TriggerOwnerScope::session("session"),
                    filter: lash_core::TriggerSubscriptionFilter::default(),
                }),
            },
            "journaled_run",
        ),
    ];

    for (command, expected) in cases {
        let kind = command.kind();
        // A grouped child on an arm that rebuilds the envelope into a target with
        // no membership slot must be refused, not silently stripped: those arms
        // record no canonical envelope, so the wake rule has no hash to fold
        // into and the group's identity fence would simply vanish.
        let grouped =
            RuntimeEffectEnvelope::new(runtime_invocation(kind, "classification"), command.clone())
                .in_effect_group(
                    "scope:group:batch:0",
                    0,
                    lash_core::GroupWakePolicy::First,
                    lash_core::LoserPolicy::RunToCompletion,
                );
        let grouped_result = restate_effect_execution(grouped);
        let carries_membership = matches!(
            expected,
            "direct_local" | "durable_tool_batch" | "journaled_run"
        );
        match grouped_result {
            Ok(_) => assert!(
                carries_membership,
                "the {expected} arm drops group membership silently; it must refuse instead"
            ),
            Err(error) => {
                assert!(
                    !carries_membership,
                    "the {expected} arm can carry membership and must not refuse it: {error}"
                );
                assert_eq!(
                    error.code,
                    lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
                    "an unhonored membership must be a typed group-shape refusal"
                );
            }
        }

        let execution = restate_effect_execution(RuntimeEffectEnvelope {
            invocation: runtime_invocation(kind, "classification"),
            command,
            group: None,
        })
        .expect("an ungrouped effect classifies");
        let actual = match execution {
            RestateEffectExecution::DirectProcess { .. } => "direct_process",
            RestateEffectExecution::DurableProcessCommand { .. } => "durable_process_command",
            RestateEffectExecution::DirectLocal { .. } => "direct_local",
            RestateEffectExecution::DurableToolBatch { .. } => "durable_tool_batch",
            RestateEffectExecution::Timer { .. } => "timer",
            RestateEffectExecution::AwaitEvent { .. } => "await_event",
            RestateEffectExecution::PeekAwaitEvent { .. } => "peek_await_event",
            RestateEffectExecution::JournaledRun { .. } => "journaled_run",
        };
        assert_eq!(actual, expected);
    }
}

#[derive(Default)]
struct RecordingContext {
    endpoint: Option<Endpoint>,
    block_sleeps: AtomicBool,
    sleeps: Mutex<Vec<u64>>,
    runs: Mutex<Vec<String>>,
    started: Mutex<Vec<ProcessRegistration>>,
    started_execution_contexts: Mutex<Vec<ProcessExecutionContext>>,
    process_command_log: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<(String, Option<String>)>>,
    resolved_events: Mutex<Vec<RestateDurableWaitResolveRequest>>,
    awaited_events: Mutex<HashMap<String, Resolution>>,
    durable_events: Mutex<HashMap<String, Resolution>>,
    durable_event_notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    session_waits: Mutex<HashMap<String, Vec<RestateDurableWaitAddress>>>,
    revoked_sessions: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct RecordingTraceSink {
    records: Mutex<Vec<lash_trace::TraceRecord>>,
}

impl lash_trace::TraceSink for RecordingTraceSink {
    fn append(&self, record: &lash_trace::TraceRecord) -> Result<(), lash_trace::TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        Ok(())
    }
}

impl RecordingContext {
    fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint: Some(endpoint),
            ..Default::default()
        }
    }

    fn resolve_process_terminal(&self, process_id: &str, output: &ProcessAwaitOutput) {
        let key = restate_process_terminal_await_key(process_id).expect("terminal await key");
        let resolution =
            restate_process_terminal_resolution(output).expect("terminal await resolution");
        self.awaited_events
            .lock_recover()
            .insert(key.promise_key(), resolution);
    }

    fn durable_event_notify(&self, workflow_key: &str) -> Arc<tokio::sync::Notify> {
        self.durable_event_notifies
            .lock_recover()
            .entry(workflow_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    fn reset_invocation_state_for_replay_preserving_durable_event(&self, workflow_key: &str) {
        // Restate replays invocation-local awakeables in journal order. This
        // in-memory context must discard the prior pass's turn-control and
        // process-terminal resolutions while retaining external input.
        let preserved = self
            .durable_events
            .lock_recover()
            .get(workflow_key)
            .cloned()
            .expect("durable event to preserve during replay");
        let mut events = self.durable_events.lock_recover();
        events.clear();
        events.insert(workflow_key.to_string(), preserved);
        drop(events);
        self.awaited_events.lock_recover().clear();
    }

    fn resolve_durable_event(&self, request: RestateDurableWaitResolveRequest) -> ResolveOutcome {
        if request
            .address
            .session_id
            .as_deref()
            .is_some_and(|session_id| self.revoked_sessions.lock_recover().contains(session_id))
        {
            return ResolveOutcome::UnknownOrRevoked;
        }
        self.terminalize_durable_event(request)
    }

    fn terminalize_durable_event(
        &self,
        request: RestateDurableWaitResolveRequest,
    ) -> ResolveOutcome {
        self.resolved_events.lock_recover().push(request.clone());
        let mut events = self.durable_events.lock_recover();
        if let Some(terminal) = events.get(&request.address.workflow_key) {
            return ResolveOutcome::AlreadyResolved {
                terminal: terminal.clone(),
            };
        }
        events.insert(request.address.workflow_key.clone(), request.resolution);
        drop(events);
        self.durable_event_notify(&request.address.workflow_key)
            .notify_waiters();
        ResolveOutcome::Accepted
    }

    fn settle_session_wait(&self, address: &RestateDurableWaitAddress) {
        if address.classification == RestateDurableWaitClassification::TurnControl {
            return;
        }
        let Some(session_id) = address.session_id.as_deref() else {
            return;
        };
        if let Some(waits) = self.session_waits.lock_recover().get_mut(session_id) {
            waits.retain(|wait| wait != address);
        }
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<RecordingContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps.lock_recover().push(duration.as_millis() as u64);
        let block = self.block_sleeps.load(Ordering::SeqCst);
        Box::pin(async move {
            if block {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        _effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock_recover().push(_effect_name);
        Box::pin(async move { Ok(Json(future.await)) })
    }

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let process_id = registration.id.clone();
        let endpoint = self.endpoint.clone();
        self.process_command_log
            .lock_recover()
            .push(format!("send:{process_id}"));
        self.started.lock_recover().push(registration.clone());
        self.started_execution_contexts
            .lock_recover()
            .push(execution_context.clone());
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                let complete_runs =
                    matches!(registration.input.as_ref(), ProcessInput::ToolCall { .. });
                invoke_process_workflow_endpoint(
                    &endpoint,
                    "run",
                    &process_id,
                    &RestateProcessWorkflowInput {
                        registration,
                        execution_context,
                        segment_ordinal: 0,
                        execution_id: None,
                    },
                    complete_runs,
                )
                .await?;
            }
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let endpoint = self.endpoint.clone();
        let process_id = request.process_id.clone();
        self.cancelled
            .lock_recover()
            .push((request.process_id.clone(), request.reason.clone()));
        Box::pin(async move {
            if let Some(endpoint) = endpoint {
                invoke_process_workflow_endpoint(&endpoint, "cancel", &process_id, &request, false)
                    .await?;
            }
            Ok(())
        })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let context = Arc::clone(self);
        Box::pin(async move {
            if let Some(session_id) = request.address.session_id.as_deref() {
                if context.revoked_sessions.lock_recover().contains(session_id) {
                    context.terminalize_durable_event(RestateDurableWaitResolveRequest {
                        address: request.address,
                        resolution: Resolution::Cancelled,
                    });
                    return Ok(Resolution::Cancelled);
                }
                context
                    .session_waits
                    .lock_recover()
                    .entry(session_id.to_string())
                    .or_default()
                    .push(request.address.clone());
            }
            let notify = context.durable_event_notify(&request.address.workflow_key);
            loop {
                if let Some(resolution) = context
                    .durable_events
                    .lock_recover()
                    .get(&request.address.workflow_key)
                    .cloned()
                {
                    context.settle_session_wait(&request.address);
                    return Ok(resolution);
                }
                if let Some(timeout_ms) = request.timeout_ms {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                address: request.address.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                address: request.address.clone(),
                                resolution: Resolution::Timeout,
                            });
                        }
                    }
                } else {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = cancellation.cancelled() => {
                            context.resolve_durable_event(RestateDurableWaitResolveRequest {
                                address: request.address.clone(),
                                resolution: Resolution::Cancelled,
                            });
                        }
                    }
                }
            }
        })
    }

    fn peek_event<'run>(
        &'run self,
        _address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(None) })
    }

    fn await_process_terminal<'run>(
        &'run self,
        process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.process_command_log
            .lock_recover()
            .push(format!("call:{process_id}"));
        let result = restate_process_terminal_await_key(&process_id)
            .map_err(TerminalError::from_error)
            .and_then(|key| {
                self.awaited_events
                    .lock_recover()
                    .get(&key.promise_key())
                    .cloned()
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "process terminal await is unresolved: {process_id}"
                        ))
                    })
            })
            .and_then(|resolution| {
                restate_process_terminal_output(&process_id, resolution)
                    .map_err(TerminalError::from_error)
            });
        Box::pin(async move { result })
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let outcome = self.resolve_durable_event(request);
        Box::pin(async move { Ok(outcome) })
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: String,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        if revoke {
            self.revoked_sessions
                .lock_recover()
                .insert(session_id.clone());
        }
        let waits = self
            .session_waits
            .lock_recover()
            .remove(&session_id)
            .unwrap_or_default();
        let (resolve, retain): (Vec<_>, Vec<_>) = if revoke {
            (waits, Vec::new())
        } else {
            split_cancellable_waits(waits)
        };
        if !retain.is_empty() {
            self.session_waits.lock_recover().insert(session_id, retain);
        }
        for address in resolve {
            self.terminalize_durable_event(RestateDurableWaitResolveRequest {
                address,
                resolution: Resolution::Cancelled,
            });
        }
        Box::pin(async { Ok(()) })
    }

    fn session_is_revoked<'run>(
        &'run self,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let revoked = self.revoked_sessions.lock_recover().contains(&session_id);
        Box::pin(async move { Ok(revoked) })
    }
}

#[derive(Default)]
struct ReplayableRecordingContext {
    sleeps: Mutex<Vec<u64>>,
    runs: Mutex<Vec<String>>,
    records: Mutex<HashMap<String, Vec<u8>>>,
    replaying: AtomicBool,
    append_missing_on_replay: AtomicBool,
    peek_records: Mutex<Vec<Option<Resolution>>>,
    peek_cursor: AtomicUsize,
    events: Arc<RecordingContext>,
    process_worker: Mutex<Option<DurableProcessWorker>>,
    defer_process_workflows: AtomicBool,
    replay_process_workflow_starts_from_journal: AtomicBool,
    live_process_workflow_starts: AtomicUsize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
struct ToolIntentJournalCorpusFixture {
    crash_point: String,
    captured_from_endpoint_interruption: bool,
    invocation_body_bytes: Vec<u8>,
    expected_response_command_frame_types: Vec<u16>,
    expected_output: Option<serde_json::Value>,
    expected_signal_events: usize,
}

const TOOL_INTENT_CORPUS_KEY: &str = "tool-intent-corpus-v1";
const TOOL_INTENT_CORPUS_SESSION: &str = "tool-intent-corpus-session";
const TOOL_INTENT_CORPUS_TURN: &str = "tool-intent-corpus-turn";
const TOOL_INTENT_CORPUS_TARGET: &str = "tool-intent-corpus-target";

#[restate_sdk::workflow]
trait ToolIntentCorpusReplay {
    async fn run(input: Json<()>) -> HandlerResult<Json<serde_json::Value>>;
}

struct ToolIntentCorpusReplayImpl {
    registry: Arc<dyn ProcessRegistry>,
}

impl ToolIntentCorpusReplay for ToolIntentCorpusReplayImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(()): Json<()>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let scope = ExecutionScope::turn(TOOL_INTENT_CORPUS_SESSION, TOOL_INTENT_CORPUS_TURN);
        let attempt = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    RuntimeInvocation::effect(
                        RuntimeScope::for_turn(
                            TOOL_INTENT_CORPUS_SESSION,
                            TOOL_INTENT_CORPUS_TURN,
                            0,
                            0,
                        ),
                        "tool-intent-corpus-attempt",
                        RuntimeEffectKind::ToolAttempt,
                        "tool-intent-corpus-attempt",
                    ),
                    RuntimeEffectCommand::ToolAttempt {
                        call: prepared_tool_call_with(
                            "tool-intent-corpus-call",
                            "tool_intent_corpus",
                        ),
                        execution_grant: None,
                        attempt: 1,
                        max_attempts: 1,
                    },
                ),
                RuntimeEffectLocalExecutor::testing(|_| async {
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record(
                                "tool-intent-corpus-call",
                                "tool_intent_corpus",
                            )),
                            intents: lash_core::ToolIntents::v1(vec![
                                lash_core::ToolIntent::SignalProcess(
                                    lash_core::SignalProcessIntent {
                                        session_id: TOOL_INTENT_CORPUS_SESSION.to_string(),
                                        process_id: TOOL_INTENT_CORPUS_TARGET.to_string(),
                                        signal_name: "resume".to_string(),
                                        payload: serde_json::json!({
                                            "source": "checked-in-endpoint-corpus"
                                        }),
                                    },
                                ),
                            ]),
                        }),
                        triggers: Vec::new(),
                    })
                }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = attempt else {
            return Err(TerminalError::new("corpus attempt returned the wrong effect").into());
        };
        let lash_core::ToolAttemptLaunch::Done { intents, .. } = *launch else {
            return Err(TerminalError::new("corpus attempt did not finish").into());
        };
        let outcomes = lash_core::testing::execute_tool_intents_with_services(
            controller
                .scoped_effect_controller(scope)
                .map_err(TerminalError::from_error)?,
            lash_core::testing::effect_backed_process_service(Arc::clone(&self.registry)),
            TOOL_INTENT_CORPUS_SESSION,
            "tool-intent-corpus-call",
            &intents,
        )
        .await;
        Ok(Json(
            serde_json::to_value(outcomes).map_err(TerminalError::from_error)?,
        ))
    }
}

async fn tool_intent_corpus_endpoint() -> (Endpoint, Arc<dyn ProcessRegistry>) {
    let clock: Arc<dyn lash_core::Clock> = Arc::new(ToolIntentCorpusClock);
    let registry = lash_sqlite_store::SqliteProcessRegistry::memory()
        .await
        .expect("open corpus process registry")
        .with_runtime_clock(clock)
        .expect("install fixed corpus clock");
    registry
        .register_process(
            ProcessRegistration::new(
                TOOL_INTENT_CORPUS_TARGET,
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "endpoint-corpus"}),
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.resume".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("seed corpus signal target");
    let endpoint = Endpoint::builder()
        .bind(
            ToolIntentCorpusReplayImpl {
                registry: Arc::clone(&registry),
            }
            .serve(),
        )
        .build();
    (endpoint, registry)
}

#[derive(Debug)]
struct ToolIntentCorpusClock;

#[async_trait::async_trait]
impl lash_core::Clock for ToolIntentCorpusClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        1_700_000_000_123
    }

    fn timestamp_rfc3339(&self) -> String {
        "2023-11-14T22:13:20.123Z".to_string()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.timestamp_ms() as i64)
            .expect("fixed corpus timestamp")
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

async fn replay_tool_intent_corpus_fixture(
    fixture: &ToolIntentJournalCorpusFixture,
) -> (Vec<u16>, Option<serde_json::Value>, usize) {
    let (endpoint, registry) = tool_intent_corpus_endpoint().await;
    let response = invoke_endpoint_body(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        bytes::Bytes::from(fixture.invocation_body_bytes.clone()),
    )
    .await
    .expect("feed checked-in corpus bytes through the Restate endpoint");
    let signal_events = registry
        .events_after(TOOL_INTENT_CORPUS_TARGET, 0)
        .await
        .expect("read corpus signal outcomes")
        .into_iter()
        .filter(|event| event.event_type == "signal.resume")
        .count();
    (
        restate_command_frame_types(&response),
        restate_output_json::<serde_json::Value>(&response),
        signal_events,
    )
}

#[tokio::test]
async fn checked_in_tool_intent_journals_replay_through_endpoint_with_literal_outcomes() {
    for checked_in in [
        include_bytes!("../tests/fixtures/tool_intent_journals/v1-mid-drain.json").as_slice(),
        include_bytes!("../tests/fixtures/tool_intent_journals/v1-mid-intent.json").as_slice(),
        include_bytes!("../tests/fixtures/tool_intent_journals/v1-full-drain.json").as_slice(),
    ] {
        let fixture: ToolIntentJournalCorpusFixture =
            serde_json::from_slice(checked_in).expect("decode checked-in endpoint corpus fixture");
        assert!(
            fixture.captured_from_endpoint_interruption,
            "{} must name its real endpoint-interruption provenance",
            fixture.crash_point
        );
        let (command_frames, output, signal_events) =
            replay_tool_intent_corpus_fixture(&fixture).await;
        assert_eq!(
            command_frames, fixture.expected_response_command_frame_types,
            "{} response command frames",
            fixture.crash_point
        );
        assert_eq!(
            output, fixture.expected_output,
            "{} output",
            fixture.crash_point
        );
        assert_eq!(
            signal_events, fixture.expected_signal_events,
            "{} literal process outcome count",
            fixture.crash_point
        );
    }
}

/// Regeneration is deliberately separate from the replay law above: the law
/// only consumes checked-in bytes. This ignored capture utility obtains each
/// prefix by closing a real endpoint invocation at the named `RunCommand`.
#[tokio::test]
#[ignore = "explicit corpus capture utility"]
async fn capture_tool_intent_journal_corpus_from_real_endpoint_interruptions() {
    let (endpoint, _) = tool_intent_corpus_endpoint().await;
    let first_interruption = invoke_endpoint(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        TOOL_INTENT_CORPUS_KEY,
        &(),
    )
    .await
    .expect("interrupt at the ToolAttempt run");
    let mid_drain = encode_captured_run_command_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &[],
        &[],
    )
    .expect("capture the completed-attempt journal prefix");
    let second_interruption = invoke_endpoint_body(
        &endpoint,
        "ToolIntentCorpusReplay",
        "run",
        mid_drain.clone(),
    )
    .await
    .expect("interrupt at the intent command run");
    let call_completion = serde_json::to_value(ResolveOutcome::Accepted)
        .expect("serialize durable-wait resolution outcome");
    let mid_intent = encode_captured_run_and_interrupted_call_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &second_interruption,
        None,
    )
    .expect("capture pending intent-command journal");
    let progressed = encode_captured_run_and_interrupted_call_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &second_interruption,
        Some(call_completion.clone()),
    )
    .expect("complete the intent's nested durable-wait call");
    let third_interruption =
        invoke_endpoint_body(&endpoint, "ToolIntentCorpusReplay", "run", progressed)
            .await
            .expect("interrupt while recording the settled intent outcome");
    let full = encode_completed_intent_drain_replay(
        TOOL_INTENT_CORPUS_KEY,
        &(),
        &first_interruption,
        &second_interruption,
        &third_interruption,
        call_completion,
    )
    .expect("capture completed intent-command journal");

    let captures = [
        (
            "v1-mid-drain",
            "after_tool_attempt_before_signal_command",
            mid_drain,
        ),
        (
            "v1-mid-intent",
            "after_signal_command_commit_before_reply",
            mid_intent,
        ),
        ("v1-full-drain", "full_drain", full),
    ];
    for (name, crash_point, invocation_body) in captures {
        let mut fixture = ToolIntentJournalCorpusFixture {
            crash_point: crash_point.to_string(),
            captured_from_endpoint_interruption: true,
            invocation_body_bytes: invocation_body.to_vec(),
            expected_response_command_frame_types: Vec::new(),
            expected_output: None,
            expected_signal_events: 0,
        };
        let (frames, output, signal_events) = replay_tool_intent_corpus_fixture(&fixture).await;
        fixture.expected_response_command_frame_types = frames;
        fixture.expected_output = output;
        fixture.expected_signal_events = signal_events;
        let mut bytes = serde_json::to_vec_pretty(&fixture).expect("serialize corpus fixture");
        bytes.push(b'\n');
        std::fs::write(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/tool_intent_journals")
                .join(format!("{name}.json")),
            bytes,
        )
        .expect("write captured endpoint corpus fixture");
    }
}

impl ReplayableRecordingContext {
    fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.append_missing_on_replay.store(false, Ordering::SeqCst);
        self.peek_cursor.store(0, Ordering::SeqCst);
    }

    fn start_replay_allowing_journal_extension(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.append_missing_on_replay.store(true, Ordering::SeqCst);
        self.peek_cursor.store(0, Ordering::SeqCst);
    }

    fn runs(&self) -> Vec<String> {
        self.runs.lock_recover().clone()
    }

    fn recorded_runtime_effect_envelopes(&self) -> Vec<(String, RuntimeEffectEnvelope)> {
        let mut envelopes = self
            .records
            .lock_recover()
            .iter()
            .map(|(effect_name, bytes)| {
                let recorded: RecordedRuntimeEffect =
                    serde_json::from_slice(bytes).expect("recorded runtime effect");
                let canonical =
                    serde_json::to_value(recorded.envelope).expect("canonical envelope value");
                let json = canonical
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .expect("canonical envelope json");
                let envelope =
                    serde_json::from_str(json).expect("canonical runtime effect envelope");
                (effect_name.clone(), envelope)
            })
            .collect::<Vec<_>>();
        envelopes.sort_by(|left, right| left.0.cmp(&right.0));
        envelopes
    }

    fn recorded_runtime_effects(
        &self,
    ) -> std::collections::BTreeMap<String, RecordedRuntimeEffect> {
        self.records
            .lock_recover()
            .iter()
            .map(|(effect_name, bytes)| {
                let recorded =
                    serde_json::from_slice(bytes).expect("decode recorded runtime effect");
                (effect_name.clone(), recorded)
            })
            .collect()
    }

    fn install_recorded_runtime_effects(
        &self,
        records: std::collections::BTreeMap<String, RecordedRuntimeEffect>,
    ) {
        *self.records.lock_recover() = records
            .into_iter()
            .map(|(effect_name, recorded)| {
                let bytes = serde_json::to_vec(&recorded)
                    .expect("encode installed recorded runtime effect");
                (effect_name, bytes)
            })
            .collect();
    }

    fn recorded_runtime_effect(&self, effect_name: &str) -> Option<RecordedRuntimeEffect> {
        self.records
            .lock_recover()
            .get(effect_name)
            .map(|bytes| serde_json::from_slice(bytes).expect("decode recorded runtime effect"))
    }

    fn install_process_worker(&self, worker: DurableProcessWorker) {
        *self.process_worker.lock_recover() = Some(worker);
    }

    fn defer_process_workflows(&self) {
        self.defer_process_workflows.store(true, Ordering::SeqCst);
    }

    fn replay_process_workflow_starts_from_journal(&self) {
        self.replay_process_workflow_starts_from_journal
            .store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct PositionalReplayContext {
    sleeps: Mutex<Vec<u64>>,
    runs: Mutex<Vec<String>>,
    records: Mutex<Vec<(String, Vec<u8>)>>,
    replaying: AtomicBool,
    replay_cursor: AtomicUsize,
}

impl PositionalReplayContext {
    fn start_replay(&self) {
        self.replaying.store(true, Ordering::SeqCst);
        self.replay_cursor.store(0, Ordering::SeqCst);
    }

    fn runs(&self) -> Vec<String> {
        self.runs.lock_recover().clone()
    }

    fn record_count(&self) -> usize {
        self.records.lock_recover().len()
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<PositionalReplayContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps.lock_recover().push(duration.as_millis() as u64);
        Box::pin(async { Ok(()) })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock_recover().push(effect_name.clone());
        if self.replaying.load(Ordering::SeqCst) {
            let position = self.replay_cursor.fetch_add(1, Ordering::SeqCst);
            let recorded = self.records.lock_recover().get(position).cloned();
            return Box::pin(async move {
                let (recorded_effect_name, bytes) = recorded.ok_or_else(|| {
                    TerminalError::new(format!("missing recorded effect at position {position}"))
                })?;
                if recorded_effect_name != effect_name {
                    return Err(TerminalError::new(format!(
                        "recorded effect at position {position} was `{recorded_effect_name}`, got `{effect_name}`"
                    )));
                }
                serde_json::from_slice(&bytes)
                    .map(Json)
                    .map_err(TerminalError::from_error)
            });
        }

        let context = Arc::clone(self);
        Box::pin(async move {
            let value = future.await;
            let bytes = serde_json::to_vec(&value).map_err(TerminalError::from_error)?;
            context.records.lock_recover().push((effect_name, bytes));
            Ok(Json(value))
        })
    }

    fn start_process_workflow<'run>(
        &'run self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow start is unsupported")) })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process workflow cancel is unsupported")) })
    }

    fn await_event<'run>(
        &'run self,
        _request: RestateDurableWaitAwaitRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("event await is unsupported")) })
    }

    fn peek_event<'run>(
        &'run self,
        _address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(None) })
    }

    fn await_process_terminal<'run>(
        &'run self,
        _process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("process terminal await is unsupported")) })
    }

    fn resolve_event<'run>(
        &'run self,
        _request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("event resolve is unsupported")) })
    }

    fn update_session_waits<'run>(
        &'run self,
        _session_id: String,
        _revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Err(TerminalError::new("session wait update is unsupported")) })
    }
}

impl<'ctx> RestateControllerContext<'ctx> for Arc<ReplayableRecordingContext> {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.sleeps.lock_recover().push(duration.as_millis() as u64);
        Box::pin(async { Ok(()) })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        _retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run,
    {
        self.runs.lock_recover().push(effect_name.clone());
        let replaying = self.replaying.load(Ordering::SeqCst);
        if replaying {
            let recorded = self.records.lock_recover().get(&effect_name).cloned();
            if let Some(bytes) = recorded {
                return Box::pin(async move {
                    serde_json::from_slice(&bytes)
                        .map(Json)
                        .map_err(TerminalError::from_error)
                });
            }
            if !self.append_missing_on_replay.load(Ordering::SeqCst) {
                return Box::pin(async move {
                    Err(TerminalError::new(format!(
                        "missing recorded effect `{effect_name}`"
                    )))
                });
            }
        }

        let context = Arc::clone(self);
        Box::pin(async move {
            let value = future.await;
            let bytes = serde_json::to_vec(&value).map_err(TerminalError::from_error)?;
            context.records.lock_recover().insert(effect_name, bytes);
            Ok(Json(value))
        })
    }

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let worker = self.process_worker.lock_recover().clone();
        let context = Arc::clone(self);
        Box::pin(async move {
            if context.replaying.load(Ordering::SeqCst)
                && context
                    .replay_process_workflow_starts_from_journal
                    .load(Ordering::SeqCst)
            {
                return Ok(format!("invocation-{}", registration.id));
            }
            if context.defer_process_workflows.load(Ordering::SeqCst) {
                return Ok(format!("invocation-{}", registration.id));
            }
            context
                .live_process_workflow_starts
                .fetch_add(1, Ordering::SeqCst);
            let Some(worker) = worker else {
                return Err(TerminalError::new("process workflow start is unsupported"));
            };
            let process_id = registration.id.clone();
            let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
            let scoped_effect_controller = controller
                .scoped_effect_controller(ExecutionScope::process(&process_id))
                .map_err(TerminalError::from_error)?;
            let cancellation = tokio_util::sync::CancellationToken::new();
            let mut handover = None;
            let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
                &process_id,
                format!("test-workflow:{process_id}"),
            );
            let output = loop {
                match worker
                    .run_process_segment_with_scoped_effect_controller(
                        registration.clone(),
                        execution_context.clone(),
                        execution_write_authority.clone(),
                        scoped_effect_controller.clone(),
                        cancellation.clone(),
                        handover,
                    )
                    .await
                    .map_err(TerminalError::from_error)?
                {
                    lash_core::ProcessRunOutcome::Terminal(output)
                    | lash_core::ProcessRunOutcome::TerminalWithParentEnd { output, .. } => {
                        break *output;
                    }
                    lash_core::ProcessRunOutcome::SegmentBoundary(next) => handover = Some(next),
                }
            };
            context
                .events
                .resolve_process_terminal(&process_id, &output);
            Ok(format!("invocation-{process_id}"))
        })
    }

    fn request_process_workflow_cancel<'run>(
        &'run self,
        _request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(()) })
    }

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_event(request, cancellation)
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        let resolution = if self.replaying.load(Ordering::SeqCst) {
            let position = self.peek_cursor.fetch_add(1, Ordering::SeqCst);
            self.peek_records
                .lock_recover()
                .get(position)
                .cloned()
                .ok_or_else(|| {
                    TerminalError::new(format!(
                        "missing recorded await-event peek at position {position}"
                    ))
                })
        } else {
            let resolution = self
                .events
                .durable_events
                .lock_recover()
                .get(&address.workflow_key)
                .cloned();
            self.peek_records.lock_recover().push(resolution.clone());
            Ok(resolution)
        };
        Box::pin(async move { resolution })
    }

    fn await_process_terminal<'run>(
        &'run self,
        process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.await_process_terminal(process_id)
    }

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.resolve_event(request)
    }

    fn update_session_waits<'run>(
        &'run self,
        session_id: String,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        self.events.update_session_waits(session_id, revoke)
    }
}

fn runtime_invocation(kind: RuntimeEffectKind, effect_id: &str) -> RuntimeInvocation {
    RuntimeInvocation::effect(
        lash_core::runtime::RuntimeScope::for_turn("session", "turn", 1, 0),
        effect_id,
        kind,
        format!("session:turn:1:0:{}:{effect_id}", kind.as_str()),
    )
}

#[test]
fn restate_turn_cancel_race_excludes_process_owned_waits() {
    let turn_scope = durable_turn_scope("session", "turn");
    let process_scoped_sleep = RuntimeInvocation::effect(
        lash_core::runtime::RuntimeScope::for_turn("session", "turn", 1, 0),
        "parent:process:worker:sleep:1",
        RuntimeEffectKind::Sleep,
        "session:turn:1:0:process:worker:sleep:1",
    );
    assert!(
        restate_timer_turn_cancel_wait_request(&process_scoped_sleep, false, None)
            .expect("process sleep classification")
            .is_none(),
        "background process sleep must outlive its originating turn"
    );
    assert!(
        restate_timer_turn_cancel_wait_request(
            &process_scoped_sleep,
            true,
            Some(&ExecutionScope::process("worker")),
        )
        .expect("explicit process scope")
        .is_none(),
        "an explicitly process-owned wait must not observe its causal turn's cancel gate"
    );

    assert!(
        restate_await_event_turn_cancel_wait_request(
            &runtime_invocation(RuntimeEffectKind::AwaitEvent, "process-wait"),
            false,
            None,
        )
        .expect("process await-event classification")
        .is_none(),
        "background process await-event must outlive its originating turn"
    );

    assert!(
        restate_timer_turn_cancel_wait_request(
            &runtime_invocation(RuntimeEffectKind::Sleep, "turn-sleep"),
            true,
            Some(&turn_scope),
        )
        .expect("turn sleep classification")
        .is_some(),
        "foreground turn sleep must observe the durable cancellation gate"
    );

    assert!(
        restate_await_event_turn_cancel_wait_request(
            &runtime_invocation(RuntimeEffectKind::AwaitEvent, "turn-process-wait"),
            true,
            Some(&turn_scope),
        )
        .expect("foreground process await-event classification")
        .is_some(),
        "a Lashlang wait inside a foreground turn must observe the turn gate"
    );
}

#[tokio::test]
async fn restate_controller_executes_atomic_effect_inside_run() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let err = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::ToolAttempt, "step"),
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared_tool_call(),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            ),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect_err("unavailable local executor should be returned from ctx.run");

    assert_eq!(
        err.code,
        lash_core::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable
    );
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        &["lash:session:turn:1:0:tool_attempt:step".to_string()]
    );
    assert!(context.sleeps.lock_recover().is_empty());
}

#[tokio::test]
async fn restate_positional_replay_records_tool_attempt_as_one_command() {
    let context = Arc::new(PositionalReplayContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let call = prepared_tool_call_with("call-fast", "fast_tool");
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-attempt"),
        RuntimeEffectCommand::ToolAttempt {
            call,
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let local_runs = Arc::new(AtomicUsize::new(0));

    let first = host
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing({
                let local_runs = Arc::clone(&local_runs);
                |_envelope| async move {
                    local_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record("call-fast", "fast_tool")),
                            intents: lash_core::ToolIntents::v1(vec![
                                lash_core::ToolIntent::StartProcess(Box::new(
                                    lash_core::StartProcessIntent {
                                        session_id: "session".to_string(),
                                        request: lash_core::ProcessStartRequest::external(
                                            "positional-replay-child",
                                            lash_core::ProcessOriginator::host_scoped(
                                                "restate-positional-law",
                                            ),
                                            serde_json::json!({"captured": true}),
                                        ),
                                        on_parent_end: lash_core::ProcessParentEndPolicy::Abandon,
                                    },
                                )),
                            ]),
                        }),
                        triggers: Vec::new(),
                    })
                }
            }),
        )
        .await
        .expect("first attempt run");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = first else {
        panic!("expected tool attempt outcome");
    };
    assert!(matches!(
        &*launch,
        lash_core::ToolAttemptLaunch::Done { record, .. } if record.call_id.as_deref() == Some("call-fast")
    ));
    assert_eq!(context.record_count(), 1);
    assert_eq!(context.runs().len(), 1);
    assert_eq!(local_runs.load(Ordering::SeqCst), 1);

    context.start_replay();
    let replayed = host
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(|_| async {
                panic!("positional replay should not rerun the ToolAttempt executor")
            }),
        )
        .await
        .expect("replayed attempt run");

    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = replayed else {
        panic!("expected replayed tool attempt outcome");
    };
    assert!(matches!(
        &*launch,
        lash_core::ToolAttemptLaunch::Done { record, .. } if record.call_id.as_deref() == Some("call-fast")
    ));
    assert_eq!(context.record_count(), 1);
    assert_eq!(context.runs().len(), 2);
    assert_eq!(local_runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn restate_controller_routes_sleep_only_through_timer() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "sleep"),
                RuntimeEffectCommand::Sleep { duration_ms: 42 },
            ),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("sleep");

    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    assert_eq!(context.sleeps.lock_recover().as_slice(), &[42]);
    assert!(context.runs.lock_recover().is_empty());
}

#[tokio::test]
async fn restate_turn_wait_rejects_missing_cancel_scope() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new(context);
    let error = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "missing-cancel-scope"),
                RuntimeEffectCommand::Sleep { duration_ms: 1 },
            ),
            RuntimeEffectLocalExecutor::sleep(tokio_util::sync::CancellationToken::new()),
        )
        .await
        .expect_err("turn sleep must not silently disable durable cancellation");
    assert_eq!(error.code.as_str(), "restate_turn_cancel_scope_missing");
}

#[tokio::test]
async fn restate_timer_stops_when_its_fresh_attempt_is_cancelled() {
    let context = Arc::new(RecordingContext::default());
    context.block_sleeps.store(true, Ordering::SeqCst);
    let host = RestateRuntimeEffectController::new(context.clone());
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let error = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Sleep, "cancelled-sleep"),
                RuntimeEffectCommand::Sleep {
                    duration_ms: 60_000,
                },
            ),
            RuntimeEffectLocalExecutor::sleep(cancellation)
                .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
        )
        .await
        .expect_err("cancelled Restate timer must stop the interpreter attempt");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled
    );
    assert_eq!(context.sleeps.lock_recover().as_slice(), &[60_000]);
}

#[tokio::test]
async fn restate_suspended_timer_is_woken_by_the_durable_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    context.block_sleeps.store(true, Ordering::SeqCst);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = Arc::clone(&context);
    let task_cancellation = cancellation.clone();
    let sleep = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, "suspended-sleep"),
                    RuntimeEffectCommand::Sleep {
                        duration_ms: 300_000,
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(task_cancellation)
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!sleep.is_finished(), "timer must genuinely remain pending");

    let cancel_key = restate_await_event_key(
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            address: RestateDurableWaitAddress::for_key(&cancel_key),
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": {
                    "request_id": "cancel-suspended-sleep",
                    "origin": "test",
                },
            })),
        }),
        ResolveOutcome::Accepted
    );

    let error = tokio::time::timeout(Duration::from_secs(1), sleep)
        .await
        .expect("durable cancel gate must wake a suspended timer promptly")
        .expect("join suspended timer")
        .expect_err("durable turn cancellation must abort the timer effect");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RuntimeEffectSleepCancelled
    );
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn restate_suspended_await_event_is_woken_by_the_durable_turn_cancel_gate() {
    let context = Arc::new(RecordingContext::default());
    let awaited_key = restate_await_event_key(
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::Custom {
            key: "wait-for-signal".to_string(),
        },
    )
    .expect("await-event key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = Arc::clone(&context);
    let task_cancellation = cancellation.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "suspended-await-event"),
                    RuntimeEffectCommand::AwaitEvent { key: awaited_key },
                ),
                RuntimeEffectLocalExecutor::await_event(task_cancellation, None)
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !wait.is_finished(),
        "await-event must genuinely remain pending"
    );

    let cancel_key = restate_await_event_key(
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("cancel gate key");
    assert_eq!(
        context.resolve_durable_event(RestateDurableWaitResolveRequest {
            address: RestateDurableWaitAddress::for_key(&cancel_key),
            resolution: Resolution::Ok(serde_json::json!({
                "state": "cancel_requested",
                "cancellation": {
                    "request_id": "cancel-suspended-await-event",
                    "origin": "test",
                },
            })),
        }),
        ResolveOutcome::Accepted
    );

    let outcome = tokio::time::timeout(Duration::from_secs(1), wait)
        .await
        .expect("durable cancel gate must wake a suspended await-event promptly")
        .expect("join suspended await-event")
        .expect("turn cancellation should terminalize the await-event");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));
    assert!(cancellation.is_cancelled());
}

#[tokio::test]
async fn restate_routes_every_execution_scope_to_an_exact_durable_wait_address() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let scopes = [
        durable_turn_scope("session", "turn"),
        ExecutionScope::process("process"),
        ExecutionScope::queue_drain("session", "drain"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::runtime_operation("operation"),
    ];
    let mut addresses = HashSet::new();

    for (index, scope) in scopes.into_iter().enumerate() {
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::Custom {
                key: format!("scope-{index}"),
            },
        )
        .expect("scope wait key");
        let address = RestateDurableWaitAddress::for_key(&key);
        assert!(addresses.insert(address.workflow_key.clone()));
        assert!(
            !address.index_key().contains('/'),
            "wait-index object keys must remain one ingress path segment"
        );
        let resolution = Resolution::Ok(serde_json::json!({ "scope": index }));
        assert_eq!(
            host.resolve_await_event(&key, resolution.clone())
                .await
                .expect("resolve scope wait"),
            ResolveOutcome::Accepted
        );
        assert_eq!(
            host.await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .expect("await scope wait"),
            resolution
        );
    }
}

#[tokio::test]
async fn restate_execute_effect_honors_cancellation_and_terminalizes_late_resolution() {
    let context = Arc::new(RecordingContext::default());
    let key = restate_await_event_key(
        &durable_turn_scope("session", "turn"),
        AwaitEventWaitIdentity::tool_completion("cancel-tool"),
    )
    .expect("cancel wait key");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_context = context.clone();
    let task_key = key.clone();
    let task_cancellation = cancellation.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "cancel-wait"),
                    RuntimeEffectCommand::AwaitEvent { key: task_key },
                ),
                RuntimeEffectLocalExecutor::await_event(task_cancellation, None)
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !wait.is_finished(),
        "mock wait must genuinely remain pending"
    );
    cancellation.cancel();
    let outcome = wait
        .await
        .expect("join cancellation wait")
        .expect("cancel wait");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::AwaitEvent {
            resolution: Resolution::Cancelled,
        }
    ));

    let host = RestateRuntimeEffectController::new(context);
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Cancelled,
        }
    );
}

#[tokio::test]
async fn restate_deadline_durably_terminalizes_timeout() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let key = restate_await_event_key(
        &ExecutionScope::runtime_operation("deadline-operation"),
        AwaitEventWaitIdentity::Custom {
            key: "deadline".to_string(),
        },
    )
    .expect("deadline key");
    let resolution = host
        .await_await_event(
            &key,
            tokio_util::sync::CancellationToken::new(),
            Some(std::time::Instant::now() + Duration::from_millis(10)),
        )
        .await
        .expect("deadline wait");
    assert_eq!(resolution, Resolution::Timeout);
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late deadline resolve"),
        ResolveOutcome::AlreadyResolved {
            terminal: Resolution::Timeout,
        }
    );
}

#[tokio::test]
async fn restate_session_cancel_cancels_current_waits_but_allows_new_waits() {
    let context = Arc::new(RecordingContext::default());
    let first_key = restate_await_event_key(
        &ExecutionScope::queue_drain("cancel-session", "drain-one"),
        AwaitEventWaitIdentity::Custom {
            key: "first".to_string(),
        },
    )
    .expect("first session wait");
    let task_context = context.clone();
    let task_key = first_key.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .await_await_event(&task_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    let host = RestateRuntimeEffectController::new(context.clone());
    host.cancel_await_events_for_session("cancel-session")
        .await
        .expect("cancel session waits");
    assert_eq!(
        wait.await
            .expect("join cancelled session wait")
            .expect("cancelled session wait"),
        Resolution::Cancelled
    );

    let next_key = restate_await_event_key(
        &durable_turn_scope("cancel-session", "turn-two"),
        AwaitEventWaitIdentity::Custom {
            key: "next".to_string(),
        },
    )
    .expect("next session wait");
    let expected = Resolution::Ok(serde_json::json!("resumed"));
    host.resolve_await_event(&next_key, expected.clone())
        .await
        .expect("resolve new session wait");
    assert_eq!(
        host.await_await_event(&next_key, tokio_util::sync::CancellationToken::new(), None,)
            .await
            .expect("new wait after cancel"),
        expected
    );
}

#[tokio::test]
async fn restate_session_delete_revokes_current_and_future_waits() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let key = restate_await_event_key(
        &ExecutionScope::session_delete("deleted-session"),
        AwaitEventWaitIdentity::Custom {
            key: "delete".to_string(),
        },
    )
    .expect("delete wait");
    let task_context = context.clone();
    let task_key = key.clone();
    let wait = tokio::spawn(async move {
        RestateRuntimeEffectController::new(task_context)
            .await_await_event(&task_key, tokio_util::sync::CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    host.revoke_await_events_for_session("deleted-session")
        .await
        .expect("revoke deleted session waits");
    assert_eq!(
        wait.await
            .expect("join deleted wait")
            .expect("deleted wait"),
        Resolution::Cancelled
    );

    let future_key = restate_await_event_key(
        &durable_turn_scope("deleted-session", "future-turn"),
        AwaitEventWaitIdentity::Custom {
            key: "future".to_string(),
        },
    )
    .expect("future revoked wait");
    let future_error = host
        .await_await_event(
            &future_key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("future revoked wait is not observable");
    assert_eq!(future_error.code.as_str(), "await_event_unknown_or_revoked");
    assert_eq!(
        host.resolve_await_event(&future_key, Resolution::Ok(serde_json::json!("late")))
            .await
            .expect("late resolve after deletion"),
        ResolveOutcome::UnknownOrRevoked
    );
}

#[tokio::test]
async fn restate_effect_host_checks_revocation_then_awaits_resolution() {
    let expected = Resolution::Ok(serde_json::json!({ "answer": "approved" }));
    let scripted = Arc::new(ScriptedHttpTransport::new([
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered("false"),
        },
        HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(
                serde_json::to_string(&expected).expect("encode resolution"),
            ),
        },
    ]));
    let host = RestateEffectHost::new(RestateConnection::with_transport(
        "https://restate.example",
        scripted.clone(),
    ));
    let key = restate_await_event_key(
        &durable_turn_scope("single-call-session", "single-call-turn"),
        AwaitEventWaitIdentity::Custom {
            key: "single-call-wait".to_string(),
        },
    )
    .expect("single-call wait key");

    let resolution = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("await resolution through ingress");

    assert_eq!(resolution, expected);
    let requests = scripted.requests();
    assert_eq!(requests.len(), 2, "durable wait must check its tombstone");
    assert!(
        requests[0]
            .url
            .contains("/LashDurableWaitIndex/single-call-session/")
            && requests[0].url.ends_with("/is_revoked"),
        "durable wait must check the session tombstone first: {}",
        requests[0].url
    );
    assert!(
        requests[1].url.contains("/LashDurableWaitWorkflow/")
            && requests[1].url.ends_with("/await_resolution"),
        "durable wait must call await_resolution directly: {}",
        requests[1].url
    );
}

struct PostCommitFailingQueuedWorkRunHandle {
    attempts: AtomicUsize,
    recovered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lash_core::facade_support::QueuedWorkRunHandle for PostCommitFailingQueuedWorkRunHandle {
    async fn run_queued_work(
        &self,
        _request: lash_core::facade_support::QueuedWorkRunRequest,
    ) -> Result<(), lash_core::facade_support::QueuedWorkRunError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(lash_core::facade_support::QueuedWorkRunError::transient(
                PluginError::Session(
                    "FIG-430 deterministic post-commit dispatch failure".to_string(),
                ),
            ));
        }
        self.recovered.notify_one();
        Ok(())
    }
}

/// FIG-430: durable acceptance is final once the pending-input row commits.
/// Dispatch failure is operational telemetry, and the wake retries itself
/// without waiting for another enqueue or unrelated host event.
#[tokio::test]
async fn restate_enqueue_never_errors_after_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-enqueue-post-commit-error";
    let provider = lash_core::testing::TestProvider::builder()
        .kind("fig-430-stub")
        .complete(|_| async { Ok(lash_core::LlmResponse::default()) })
        .build()
        .into_handle();
    let queued_work = Arc::new(PostCommitFailingQueuedWorkRunHandle {
        attempts: AtomicUsize::new(0),
        recovered: tokio::sync::Notify::new(),
    });
    let recovered = queued_work.recovered.notified();
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(lash_core::ModelSpec::new(
            "fig-430-model",
            std::num::NonZeroUsize::new(1024).expect("non-zero context window"),
        ))
        .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            dir.path().join("sessions"),
        )))
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(DurableMemoryAttachmentStore::default()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(DurableMemoryProcessEnvStore::default()))
        .queued_work_driver(lash_core::facade_support::QueuedWorkDriver::new(
            queued_work.clone(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lash-restate-fig430-test",
            "lash-restate-fig430-test-boot",
        ))
        .expect("build FIG-430 core");
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open FIG-430 session");

    let outcome = session
        .enqueue(lash_core::TurnInput::text("commit before dispatch"))
        .id("fig-430-retry")
        .send()
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(1), recovered)
        .await
        .expect("the failed post-commit wake must retry on its own");
    let persisted = session
        .pending_turn_inputs()
        .await
        .expect("inspect committed pending input");

    match (&outcome, persisted.as_slice()) {
        (Err(_), []) => {}
        (Ok(receipt), [stored]) => {
            assert_eq!(stored.input_id, receipt.input_id);
            assert_eq!(stored.session_id, receipt.session_id);
            assert_eq!(stored.source_key, receipt.source_key);
            assert_eq!(stored.ingress, receipt.ingress);
            assert_eq!(receipt.source_key.as_deref(), Some("host:fig-430-retry"));
        }
        (Err(error), stored) => panic!(
            "enqueue returned an undifferentiated error after durable commit: \
             caller_outcome={error:?}, persisted_row_count={}",
            stored.len()
        ),
        (Ok(receipt), stored) => panic!(
            "successful enqueue must identify exactly one durable row: \
             caller_outcome={receipt:?}, persisted_rows={stored:?}"
        ),
    }
    assert_eq!(
        queued_work.attempts.load(Ordering::SeqCst),
        2,
        "the wake path retries exactly once after the injected failure"
    );

    let retry_receipt = session
        .enqueue(lash_core::TurnInput::text("commit before dispatch"))
        .id("fig-430-retry")
        .send()
        .await
        .expect("retry the same durable source identity");
    assert_eq!(
        Some(&retry_receipt.input_id),
        outcome.as_ref().ok().map(|receipt| &receipt.input_id),
        "the source key is the idempotent retry identity"
    );
    assert_eq!(
        session
            .pending_turn_inputs()
            .await
            .expect("inspect idempotent retry")
            .len(),
        1,
        "an exact source retry must not create another durable input"
    );
}

fn replay_test_policy(session_id: &str) -> lash_core::SessionPolicy {
    let mut policy = lash_core::testing::mock_session_policy();
    policy.session_id = Some(session_id.to_string());
    policy
}

fn replay_test_state(
    session_id: &str,
    policy: &lash_core::SessionPolicy,
) -> lash_core::RuntimeSessionState {
    lash_core::RuntimeSessionState {
        session_id: session_id.to_string(),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn replay_test_input(turn_id: &str) -> lash_core::TurnInput {
    let mut input = lash_core::TurnInput::text("finish once");
    input.trace_turn_id = Some(turn_id.to_string());
    input
}

struct Fig1293EchoTools;

fn fig1293_echo_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:fig1293_echo",
        "fig1293_echo",
        "Return the supplied literal value.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": {} },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": { "echo": {} },
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
        lash_core::ToolOutcome::ok(serde_json::json!({
            "echo": call.args.get("value").cloned().unwrap_or_default(),
        }))
    }
}

fn fig1293_migrated_tool_factories() -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
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

async fn fig1293_seed_control_target(registry: &Arc<dyn ProcessRegistry>, session_id: &str) {
    registry
        .register_process_with_observers(
            ProcessRegistration::new(
                "fig1293-control-target",
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "fig1293"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.stdin".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[session_id.to_string()],
        )
        .await
        .expect("register FIG-1293 control target");
}

struct RestateParentEndIntentProvider {
    calls: Arc<AtomicUsize>,
}

fn restate_parent_end_intent_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:restate_parent_end_intent",
        "restate_parent_end_intent",
        "Start a child with recorded Cancel parent-end policy.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RestateParentEndIntentProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![restate_parent_end_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "restate_parent_end_intent")
            .then(|| Arc::new(restate_parent_end_intent_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        panic!("the Restate parent-end law must use AttemptContext")
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"started": true})),
            lash_core::ToolIntents::v1(
                ["first", "second"]
                    .into_iter()
                    .map(|child| {
                        lash_core::ToolIntent::StartProcess(Box::new(
                            lash_core::StartProcessIntent {
                                session_id: call.context.session_id().to_string(),
                                request: lash_core::ProcessStartRequest::new(
                                    format!("restate-parent-end-child-{child}"),
                                    ProcessInput::Engine {
                                        kind: "restate-parent-end-law".to_string(),
                                        payload: serde_json::json!({
                                            "source": "restate-parent-end-law",
                                            "child": child,
                                        }),
                                    },
                                    lash_core::RecoveryContract::Rerunnable,
                                    lash_core::ProcessOriginator::host_scoped(
                                        "restate-parent-end-law",
                                    ),
                                )
                                .with_env_spec(
                                    lash_core::ProcessExecutionEnvSpec::new(
                                        lash_core::PluginOptions::default(),
                                        lash_core::testing::mock_session_policy(),
                                    ),
                                ),
                                on_parent_end: lash_core::ProcessParentEndPolicy::Cancel,
                            },
                        ))
                    })
                    .collect(),
            ),
        )
    }
}

/// Engine backing the parent-end law's recorded child starts. Recorded-intent
/// starts cross the same engine admission gate direct starts do, so the kind the
/// intent declares must be registered on the host.
struct RestateParentEndLawEngine;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for RestateParentEndLawEngine {
    fn kind(&self) -> &'static str {
        "restate-parent-end-law"
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        Ok(lash_core::ProcessAwaitOutput::Success {
            value: serde_json::json!({"parent_end_law": "child ran"}),
            control: None,
        }
        .into())
    }
}

#[derive(Default)]
struct RestateParentEndFaultState {
    crash_before_record_remaining: AtomicUsize,
    crash_after_recorded_parent_end: AtomicUsize,
    recorded_parent_end_count: AtomicUsize,
    completed_local_side_effects: AtomicUsize,
    frames: Mutex<Vec<RuntimeEffectEnvelope>>,
    outcomes: Mutex<Vec<lash_core::ToolIntentParentEndOutcome>>,
}

struct RestateParentEndFaultController {
    inner: RestateRuntimeEffectController<'static, Arc<ReplayableRecordingContext>>,
    state: Arc<RestateParentEndFaultState>,
}

impl AwaitEventResolver for RestateParentEndFaultController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        self.inner.replay_ownership()
    }

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        self.inner.journal_addressing()
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.inner.allows_process_lifetime_completion_keys()
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RestateParentEndFaultController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let is_parent_end = matches!(
            &envelope.command,
            RuntimeEffectCommand::Process { command }
                if matches!(command.as_ref(), ProcessCommand::ParentEnd { .. })
        );
        if is_parent_end {
            self.state.frames.lock_recover().push(envelope.clone());
        }
        let crash_before_record = is_parent_end
            && self
                .state
                .crash_before_record_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        let outcome = if crash_before_record {
            let state = Arc::clone(&self.state);
            self.inner
                .execute_effect(
                    envelope,
                    local_executor.with_process_outcome_observer(Arc::new(move |outcome| {
                        assert!(matches!(outcome, ProcessEffectOutcome::ParentEnd { .. }));
                        state
                            .completed_local_side_effects
                            .fetch_add(1, Ordering::SeqCst);
                        panic!(
                            "injected crash after Restate ParentEnd side effect and before outcome recording"
                        );
                    })),
                )
                .await
        } else {
            self.inner.execute_effect(envelope, local_executor).await
        };
        if let Ok(RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::ParentEnd { outcome },
        }) = &outcome
        {
            self.state.outcomes.lock_recover().push((**outcome).clone());
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
                    "injected crash after a Restate ParentEnd outcome and before the next command"
                );
            }
        }
        outcome
    }
}

struct PanicAtToolIntentParentEnd;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAtToolIntentParentEnd {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "tool_intent.parent_end" {
            panic!("injected crash after ToolBatch commit and before parent-end teardown");
        }
    }
}

async fn replay_test_runtime(
    session_id: &str,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
) -> lash_core::facade_support::LashRuntime {
    Box::pin(replay_test_runtime_with_plugins(
        session_id,
        policy,
        initial_state,
        host,
        store,
        lash_core::testing::test_standard_protocol_factories(),
    ))
    .await
}

async fn replay_test_runtime_with_plugins(
    session_id: &str,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> lash_core::facade_support::LashRuntime {
    Box::pin(replay_test_runtime_with_plugins_and_registry(
        session_id,
        policy,
        initial_state,
        host,
        store,
        plugin_factories,
        None,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
async fn replay_test_runtime_with_plugins_and_registry(
    session_id: &str,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    process_registry: Option<Arc<dyn ProcessRegistry>>,
) -> lash_core::facade_support::LashRuntime {
    let mut builder = lash_core::facade_support::LashRuntime::builder(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
        lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-replay-test",
            "lash-restate-replay-test-boot",
        ),
    )
    .with_session_id(session_id)
    .with_policy(policy)
    .with_initial_state(initial_state)
    .with_runtime_host(host)
    .with_plugin_factories(plugin_factories)
    .with_store(store);
    if let Some(process_registry) = process_registry {
        builder = builder.with_process_registry(process_registry);
    }
    Box::pin(builder.build())
        .await
        .expect("build replay test runtime")
}

async fn run_restate_replay_turn(
    runtime: &mut lash_core::facade_support::LashRuntime,
    context: Arc<ReplayableRecordingContext>,
    session_id: &str,
    turn_id: &str,
) -> lash_core::facade_support::AssembledTurn {
    let controller = RestateRuntimeEffectController::new(context);
    let scoped_effect_controller = controller
        .scoped_effect_controller(durable_turn_scope(session_id, turn_id))
        .expect("scoped restate controller");
    runtime
        .stream_turn(
            replay_test_input(turn_id),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await
        .expect("run replay test turn")
}

async fn run_restate_replay_turn_with_parent_end_fault(
    runtime: &mut lash_core::facade_support::LashRuntime,
    context: Arc<ReplayableRecordingContext>,
    state: Arc<RestateParentEndFaultState>,
    session_id: &str,
    turn_id: &str,
) -> lash_core::facade_support::AssembledTurn {
    let scope = durable_turn_scope(session_id, turn_id);
    let inner: RestateRuntimeEffectController<'static, Arc<ReplayableRecordingContext>> =
        RestateRuntimeEffectController::new(context);
    let scoped_effect_controller = ScopedEffectController::shared(
        Arc::new(RestateParentEndFaultController { inner, state }),
        scope,
    )
    .expect("shared Restate parent-end fault controller");
    runtime
        .stream_turn(
            replay_test_input(turn_id),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await
        .expect("run replay test turn with ParentEnd fault")
}

#[tokio::test]
async fn fig1293_public_migrated_tools_redrive_with_literal_restate_outcomes() {
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
                            full_text: "child literal".to_string(),
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text: "child literal".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        2 => lash_core::LlmResponse {
                            full_text: "migrated tools complete".to_string(),
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
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    let process_registry = process_registry();
    fig1293_seed_control_target(&process_registry, session_id).await;
    let plugin_factories = fig1293_migrated_tool_factories();
    context.install_process_worker(DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                plugin_factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            Arc::clone(&process_registry),
            lash_core::testing::runtime_lease_owner(),
        ),
    ));

    let mut first = replay_test_runtime_with_plugins_and_registry(
        session_id,
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
        run_restate_replay_turn(&mut first, first_context, session_id, turn_id).await
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
            "start_command".to_string(),
            "start_command".to_string(),
            "write_stdin".to_string(),
            "cancel_process".to_string(),
            "fig1293_echo".to_string(),
            "fig1293_echo".to_string(),
        ],
        "leaf tools and batch children are attempts; batch, spawn_agent, and the shell process body are not"
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
    for kind in ["start_process", "signal_process", "cancel_process"] {
        assert!(
            outer_outcome_json.contains(&format!(r#""kind":"{kind}""#)),
            "missing executed Restate {kind} outcome: {outer_outcome_json}",
        );
    }
    let direct_orchestration_children = before
        .iter()
        .map(|(_, envelope)| envelope)
        .filter(|envelope| {
            let is_spawn_command = match &envelope.command {
                RuntimeEffectCommand::Process { command } => match command.as_ref() {
                    ProcessCommand::Start { registration, .. } => {
                        registration.id == "process:subagent:fig1293-spawn-agent"
                    }
                    ProcessCommand::Await { process_id } => {
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
        session_id,
        policy,
        initial_state,
        host,
        runtime_store,
        plugin_factories,
        Some(process_registry),
    )
    .await;
    let turn =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), session_id, turn_id).await;
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
                "start_command".to_string(),
                serde_json::json!({
                    "__handle__": "process",
                    "done": false,
                    "id": "tool-intent:v1:sha256:d12cb7271490769b5b5c5ab863c95b415580cebaba0d2e7dadb13f23ebc4b9ae",
                    "process_id": "tool-intent:v1:sha256:d12cb7271490769b5b5c5ab863c95b415580cebaba0d2e7dadb13f23ebc4b9ae",
                    "running": true,
                    "status": "running",
                }),
            ),
            (
                "start_command".to_string(),
                serde_json::json!({
                    "__handle__": "process",
                    "done": true,
                    "id": "tool-intent:v1:sha256:18bd210d837d743200aea291e68d5c8769976320090c8ab5680b4683ded5a3ac:detached",
                    "process_id": "tool-intent:v1:sha256:18bd210d837d743200aea291e68d5c8769976320090c8ab5680b4683ded5a3ac:detached",
                    "running": false,
                    "status": "detached",
                }),
            ),
            (
                "write_stdin".to_string(),
                serde_json::json!({
                    "process_id": "fig1293-control-target",
                    "sequence": 2,
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
                            "tool": "fig1293_echo",
                        },
                        {
                            "duration_ms": 0,
                            "index": 1,
                            "result": {"echo": "beta"},
                            "success": true,
                            "tool": "fig1293_echo",
                        },
                    ]
                }),
            ),
        ]
    );
}

#[tokio::test]
async fn restate_handler_replay_retries_final_lash_commit_idempotently() {
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
                        full_text: "committed once".to_string(),
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
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());

    let mut first = replay_test_runtime(
        session_id,
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
    )
    .await;
    let first_turn =
        run_restate_replay_turn(&mut first, Arc::clone(&context), session_id, turn_id).await;
    assert!(matches!(
        first_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    let first_runs = context.runs();
    assert!(!first_runs.is_empty());

    let blocking_owner =
        lash_core::LeaseOwnerIdentity::opaque("replay-blocker", "replay-blocker:001");
    let blocking_lease = lash_core::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        &*store,
        session_id,
        &blocking_owner,
        "restate-handler-replay-retries-final-lash-commit-idempotently-executor",
        60_000,
    )
    .await
    .expect("claim replay-blocking advisory lease")
    .acquired()
    .expect("replay-blocking advisory lease");
    context.start_replay();
    let retry_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(CommitRetryStore::new(Arc::clone(&runtime_store)));
    let mut replay =
        replay_test_runtime(session_id, policy, initial_state, host, retry_store).await;
    let replay_turn =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), session_id, turn_id).await;
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(first_turn.llm_calls.len(), 1);
    assert_eq!(replay_turn.llm_calls, first_turn.llm_calls);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    lash_core::SessionExecutionLeaseStore::release_session_execution_lease(
        &*store,
        &blocking_lease.completion(),
    )
    .await
    .expect("release replay-blocking advisory lease");

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
async fn restate_public_parent_end_cancel_survives_crash_after_tool_batch_commit() {
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
                            full_text: "parent end complete".to_string(),
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
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    context.defer_process_workflows();
    let process_registry = process_registry();
    context.install_process_worker(DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                plugin_factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            Arc::clone(&process_registry),
            lash_core::testing::runtime_lease_owner(),
        ),
    ));

    let mut first = replay_test_runtime_with_plugins_and_registry(
        session_id,
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
        run_restate_replay_turn(&mut first, first_context, session_id, turn_id).await
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
        session_id,
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
            session_id,
            turn_id,
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
            "replay_key": "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d:parent-end:process:parent-end:tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d",
            "command": {
                "type": "process",
                "command": {
                    "op": "parent_end",
                    "identity": {
                        "session_id": "restate-parent-end-replay",
                        "execution_scope_id": "restate-parent-end-turn-1",
                        "tool_call_id": "restate-parent-end-call",
                        "intent_index": 0,
                        "replay_key": "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d"
                    },
                    "process_id": "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d",
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
        session_id,
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
            session_id,
            turn_id,
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
                session_id: "restate-parent-end-replay".to_string(),
                execution_scope_id: "restate-parent-end-turn-1".to_string(),
                tool_call_id: "restate-parent-end-call".to_string(),
                intent_index: 0,
                replay_key: "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d".to_string(),
            },
            process_id: "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d".to_string(),
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
        session_id,
        policy,
        initial_state,
        host,
        Arc::clone(&runtime_store),
        plugin_factories,
        Some(Arc::clone(&process_registry)),
    )
    .await;
    let redriven =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), session_id, turn_id).await;
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
                "replay_key": "tool-intent:v1:sha256:86bea9a96b9f63fecf1a6a7c5bde6c1222dee41b6cf511413f3309a01df9af7c:parent-end:process:parent-end:tool-intent:v1:sha256:86bea9a96b9f63fecf1a6a7c5bde6c1222dee41b6cf511413f3309a01df9af7c",
                "command": {
                    "type": "process",
                    "command": {
                        "op": "parent_end",
                        "identity": {
                            "session_id": "restate-parent-end-replay",
                            "execution_scope_id": "restate-parent-end-turn-1",
                            "tool_call_id": "restate-parent-end-call",
                            "intent_index": 1,
                            "replay_key": "tool-intent:v1:sha256:86bea9a96b9f63fecf1a6a7c5bde6c1222dee41b6cf511413f3309a01df9af7c"
                        },
                        "process_id": "tool-intent:v1:sha256:86bea9a96b9f63fecf1a6a7c5bde6c1222dee41b6cf511413f3309a01df9af7c",
                        "policy": "cancel",
                        "reason": "recorded start intent parent ended with cancel policy"
                    }
                }
            }),
            serde_json::json!({
                "replay_key": "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d:parent-end:process:parent-end:tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d",
                "command": {
                    "type": "process",
                    "command": {
                        "op": "parent_end",
                        "identity": {
                            "session_id": "restate-parent-end-replay",
                            "execution_scope_id": "restate-parent-end-turn-1",
                            "tool_call_id": "restate-parent-end-call",
                            "intent_index": 0,
                            "replay_key": "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d"
                        },
                        "process_id": "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d",
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
                    session_id: "restate-parent-end-replay".to_string(),
                    execution_scope_id: "restate-parent-end-turn-1".to_string(),
                    tool_call_id: "restate-parent-end-call".to_string(),
                    intent_index: 1,
                    replay_key: "tool-intent:v1:sha256:86bea9a96b9f63fecf1a6a7c5bde6c1222dee41b6cf511413f3309a01df9af7c".to_string(),
                },
                process_id: "tool-intent:v1:sha256:86bea9a96b9f63fecf1a6a7c5bde6c1222dee41b6cf511413f3309a01df9af7c".to_string(),
            },
            lash_core::ToolIntentParentEndOutcome::Cancelled {
                identity: lash_core::ToolIntentIdentity {
                    session_id: "restate-parent-end-replay".to_string(),
                    execution_scope_id: "restate-parent-end-turn-1".to_string(),
                    tool_call_id: "restate-parent-end-call".to_string(),
                    intent_index: 0,
                    replay_key: "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d".to_string(),
                },
                process_id: "tool-intent:v1:sha256:aed2b39895a632b1e3f05b495f23810921cfa9b446cea68dc924c868562d4a7d".to_string(),
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
async fn restate_replay_lease_acquisition_takes_recorded_branch() {
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
                        full_text: "fresh worker progressed".to_string(),
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
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
    let context = Arc::new(ReplayableRecordingContext::default());

    let mut suspended = replay_test_runtime(
        session_id,
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        Arc::clone(&runtime_store),
    )
    .await;
    let suspended_context = Arc::clone(&context);
    let suspended_turn = tokio::spawn(async move {
        run_restate_replay_turn(&mut suspended, suspended_context, session_id, turn_id).await
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

    let mut fresh_worker =
        replay_test_runtime(session_id, policy, initial_state, host, runtime_store).await;
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let scoped_effect_controller = controller
        .scoped_effect_controller(durable_turn_scope(session_id, turn_id))
        .expect("scoped replay controller");
    let replay_turn = fresh_worker
        .stream_turn(
            replay_test_input(turn_id),
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

struct ReplayScalarPendingTools {
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
                    session_id: call.context.session_id().to_string(),
                    process_id: "restate-recorded-intent-target".to_string(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"source": "recorded-scalar-attempt"}),
                },
            )]),
        )
    }
}

#[tokio::test]
async fn restate_replay_does_not_reexecute_scalar_lashlang_tool_before_pending_wait() {
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
            lash_protocol_rlm::RlmProtocolPluginConfig::new(
                lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
                lash_protocol_rlm::ExecutionBound::secs(30),
                lash_protocol_rlm::ExecutionBound::instructions(64 * 1024 * 1024),
            ),
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
                        full_text: source.to_string(),
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
    let policy = replay_test_policy(session_id);
    let initial_state = replay_test_state(session_id, &policy);
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
            &[session_id.to_string()],
        )
        .await
        .expect("register the recorded-intent signal target");
    let process_worker =
        DurableProcessWorker::new(lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                plugin_factories.clone(),
            )),
            host.clone(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            Arc::clone(&process_registry),
            lash_core::testing::runtime_lease_owner(),
        ));
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
        session_id,
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
        run_restate_replay_turn(&mut first, first_context, session_id, turn_id).await
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
        .events_after("restate-recorded-intent-target", 0)
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
            "restate-recorded-intent-target",
            lash_core::ProcessAwaitOutput::Success {
                value: serde_json::json!("live state mutated after drain"),
                control: None,
            },
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
        session_id,
        policy,
        initial_state,
        host,
        retry_store,
        plugin_factories,
        Some(Arc::clone(&process_registry)),
    ))
    .await;
    let replay_turn =
        run_restate_replay_turn(&mut replay, Arc::clone(&context), session_id, turn_id).await;
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
        .events_after("restate-recorded-intent-target", 0)
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
async fn restate_controller_schedules_process_workflow_without_running_executor() {
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
                    observers: vec!["session".to_string()],
                    env_spec: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
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
            .get_process("task-1")
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
            .list_observed_by("session")
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
async fn restate_controller_replays_process_start_await_command_sequence() {
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
    let await_terminal = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-await-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: process_id.to_string(),
            }),
        )
    };
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "done": true }),
        control: None,
    };

    host.execute_effect(
        start(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("first start");
    registry
        .complete_process(
            process_id,
            terminal.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete child process");
    context.resolve_process_terminal(process_id, &terminal);
    host.execute_effect(
        await_terminal(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("first await");

    // Simulates Restate replay of the same parent handler after a later
    // suspension resumes. The already persisted registry record has an
    // external_ref at this point, but the handler must still issue the same
    // Restate send before the await call so the journal command sequence stays
    // send -> call -> ... on every replay.
    host.execute_effect(
        start(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("replay start");
    host.execute_effect(
        await_terminal(),
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
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
async fn restate_controller_start_emits_send_when_external_ref_already_exists() {
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
            process_id,
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
        RuntimeEffectLocalExecutor::processes(registry, None),
    )
    .await
    .expect("start with existing external ref");

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[format!("send:{process_id}")],
        "pre-existing external_ref must not suppress the journaled Restate send"
    );
}

async fn run_parent_shaped_start_await_suspend_flow(
    host: &RestateRuntimeEffectController<'_, Arc<RecordingContext>>,
    registry: Arc<dyn ProcessRegistry>,
    process_id: &str,
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
        RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("parent flow start child");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "parent-flow-await-child"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: process_id.to_string(),
            }),
        ),
        RuntimeEffectLocalExecutor::processes(registry, None),
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

#[tokio::test]
async fn restate_controller_replays_parent_shaped_start_await_suspend_flow() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let process_id = "task-parent-flow-replay";
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "done": true }),
        control: None,
    };
    let suspend_key = restate_await_event_key(
        &ExecutionScope::process(process_id),
        AwaitEventWaitIdentity::Custom {
            key: "parent-resume-input".to_string(),
        },
    )
    .expect("parent suspend key");
    context.resolve_process_terminal(process_id, &terminal);
    context.resolve_durable_event(RestateDurableWaitResolveRequest {
        address: RestateDurableWaitAddress::for_key(&suspend_key),
        resolution: Resolution::Ok(serde_json::json!({ "answer": "resume" })),
    });

    run_parent_shaped_start_await_suspend_flow(
        &host,
        registry.clone(),
        process_id,
        suspend_key.clone(),
    )
    .await;
    run_parent_shaped_start_await_suspend_flow(&host, registry, process_id, suspend_key).await;

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[
            format!("send:{process_id}"),
            format!("call:{process_id}"),
            format!("send:{process_id}"),
            format!("call:{process_id}"),
        ],
        "a parent-shaped replay after suspension must preserve child start/await command order"
    );
}

#[tokio::test]
async fn restate_controller_schedules_lashlang_process_with_serializable_input() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let module = lashlang::parse("process scan(root: str) { finish root }")
        .expect("lashlang process module");
    let catalog = lashlang::LashlangHostCatalog::new();
    let linked_module = lashlang::LinkedModule::link(
        module.clone(),
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all()),
    )
    .expect("link lashlang module");
    let process_ref = linked_module
        .artifact
        .process_ref("scan")
        .expect("scan process ref")
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("root".to_string(), serde_json::json!("."));
    let registration = ProcessRegistration::new(
        "process-1",
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref.clone(),
            process_ref: process_ref.clone(),
            host_requirements_ref: linked_module.host_requirements_ref.clone(),
            process_name: "scan".to_string(),
            args: args.clone(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("session")),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
        "process-env:test:process-1",
    )))
    .with_wake_session_id(Some("session".to_string()));

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "lashlang-process-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: Vec::new(),
                    env_spec: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
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
            .map(|external| external.backend.as_str()),
        Some("restate")
    );
    assert_eq!(
        registry
            .get_process("process-1")
            .await
            .expect("read process")
            .expect("registered process")
            .external_ref
            .as_ref()
            .map(|external| external.backend.as_str()),
        Some("restate")
    );
    let started = context.started.lock_recover().clone();
    assert_eq!(started.len(), 1);
    let ProcessInput::Engine { kind, payload } = started[0].input.as_ref() else {
        panic!("expected engine process input");
    };
    assert_eq!(kind, lash_lashlang_runtime::LASHLANG_ENGINE_KIND);
    let sent = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("typed lashlang payload");
    assert_eq!(sent.module_ref, linked_module.module_ref);
    assert_eq!(sent.process_ref, process_ref);
    assert_eq!(
        sent.host_requirements_ref,
        linked_module.host_requirements_ref
    );
    assert_eq!(sent.process_name, "scan");
    assert_eq!(sent.args, args);
    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| { registration.wake_session_id.as_deref() })
            .collect::<Vec<_>>(),
        vec![Some("session")]
    );
}

#[tokio::test]
async fn restate_controller_lists_and_transfers_observers_through_process_effects() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let s1 = lash_core::SessionScope::new("s1");
    let s2 = lash_core::SessionScope::new("s2");
    registry
        .register_process(external_registration("task-list"))
        .await
        .expect("register");
    registry
        .add_observer(
            &s1.session_id,
            "task-list",
            lash_core::ProcessObserverBy::host("restate-list-test"),
        )
        .await
        .expect("observe");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-list-s1"),
                RuntimeEffectCommand::process(ProcessCommand::List {
                    session_scope: s1.clone(),
                    mode: lash_core::ProcessListMode::Live,
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("list");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::List { entries },
    } = outcome
    else {
        panic!("wrong list outcome");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "task-list");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-transfer"),
                RuntimeEffectCommand::process(ProcessCommand::Transfer {
                    from_scope: s1.clone(),
                    to_scope: s2.clone(),
                    process_ids: vec!["task-list".to_string()],
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("transfer");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Transfer
        }
    ));

    let entries = registry
        .list_observed_by(&s2.session_id)
        .await
        .expect("s2 observed");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "task-list");
    assert!(
        registry
            .list_observed_by(&s1.session_id)
            .await
            .expect("s1")
            .is_empty()
    );
    assert!(context.started.lock_recover().is_empty());
}

#[tokio::test]
async fn restate_controller_awaits_and_signals_through_process_effects() {
    let context = Arc::new(RecordingContext::default());
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let host = RestateRuntimeEffectController::new(context.clone()).with_trace_sink(sink_dyn);
    let registry = process_registry();
    registry
        .register_process(external_registration("task-await-signal"))
        .await
        .expect("register");
    registry
        .register_process(
            external_registration("task-signal")
                .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
                .with_extra_event_types([lash_core::ProcessEventType {
                    name: "signal.notify".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                }]),
        )
        .await
        .expect("register signal target");
    let awaited_output = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "done": true }),
        control: None,
    };
    registry
        .complete_process(
            "task-await-signal",
            awaited_output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    context.resolve_process_terminal("task-await-signal", &awaited_output);

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-await"),
                RuntimeEffectCommand::process(ProcessCommand::Await {
                    process_id: "task-await-signal".to_string(),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("await");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Await { output },
    } = outcome
    else {
        panic!("wrong await outcome");
    };
    assert_eq!(
        *output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "done": true }),
            control: None,
        }
    );
    assert_eq!(
        sink.records
            .lock_recover()
            .iter()
            .map(|record| record.event.kind())
            .collect::<Vec<_>>(),
        vec!["durable_wait_parked", "durable_wait_resolved"],
        "process awaits expose the same durable wait evidence as await-event"
    );

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-signal"),
                RuntimeEffectCommand::process(ProcessCommand::Signal {
                    process_id: "task-signal".to_string(),
                    signal_name: "notify".to_string(),
                    signal_id: "notify".to_string(),
                    request: lash_core::ProcessEventAppendRequest::new(
                        "signal.notify",
                        serde_json::json!({ "signal": "notify" }),
                    )
                    .with_replay_key("signal:notify"),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("signal");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Signal { event },
    } = outcome
    else {
        panic!("wrong signal outcome");
    };
    assert_eq!(event.event_type, "signal.notify");
    assert!(context.started.lock_recover().is_empty());

    // Append-before-resolve discipline: the durable event is the record, the
    // promise resolution is only the wake-up, keyed by the Nth occurrence of
    // this signal name so repeated signals map onto one-shot engine promises.
    {
        let resolved = context.resolved_events.lock_recover();
        assert_eq!(resolved.len(), 1);
        let expected_key = restate_await_event_key(
            &ExecutionScope::process("task-signal"),
            AwaitEventWaitIdentity::process_signal("task-signal", "notify", 1),
        )
        .expect("first signal wait key");
        assert_eq!(
            resolved[0].address,
            RestateDurableWaitAddress::for_key(&expected_key)
        );
        assert_eq!(
            resolved[0].resolution,
            Resolution::Ok(serde_json::json!({ "signal": "notify" }))
        );
    }

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-signal-2"),
                RuntimeEffectCommand::process(ProcessCommand::Signal {
                    process_id: "task-signal".to_string(),
                    signal_name: "notify".to_string(),
                    signal_id: "notify-2".to_string(),
                    request: lash_core::ProcessEventAppendRequest::new(
                        "signal.notify",
                        serde_json::json!({ "signal": "notify-2" }),
                    )
                    .with_replay_key("signal:notify-2"),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("second signal");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Signal { .. },
    } = outcome
    else {
        panic!("wrong second signal outcome");
    };
    let resolved = context.resolved_events.lock_recover();
    assert_eq!(resolved.len(), 2);
    let expected_key = restate_await_event_key(
        &ExecutionScope::process("task-signal"),
        AwaitEventWaitIdentity::process_signal("task-signal", "notify", 2),
    )
    .expect("second signal wait key");
    assert_eq!(
        resolved[1].address,
        RestateDurableWaitAddress::for_key(&expected_key),
        "second signal must resolve the ordinal-2 wait key"
    );
}

#[tokio::test]
async fn restate_controller_cancel_requests_call_workflow_cancel() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new(context.clone());
    let registry = process_registry();
    let registration = external_registration("task-cancel");
    registry
        .register_process(registration)
        .await
        .expect("register");

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: "task-cancel".to_string(),
                    reason: Some("user requested".to_string()),
                    replay: None,
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry, None),
        )
        .await
        .expect("cancel");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Cancel { record },
    } = outcome
    else {
        panic!("wrong outcome");
    };

    assert!(!record.is_terminal());
    assert_eq!(
        context.cancelled.lock_recover().as_slice(),
        &[(
            "task-cancel".to_string(),
            Some("user requested".to_string())
        )]
    );
}

#[derive(Debug, PartialEq, Eq)]
struct RecordedProcessRun {
    process_id: String,
    wake_target_session_id: Option<String>,
    tool_effect_id: Option<String>,
    execution_scope_id: String,
    controller_replay_ownership: lash_core::EffectReplayOwnership,
}

#[derive(Default)]
struct RecordingRunner {
    ran: Mutex<Vec<RecordedProcessRun>>,
    cancelled: Mutex<Vec<RestateProcessCancelRequest>>,
}

#[async_trait::async_trait]
impl RestateProcessRunner for RecordingRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.ran.lock_recover().push(RecordedProcessRun {
            process_id: registration.id.clone(),
            wake_target_session_id: registration.wake_session_id.clone(),
            tool_effect_id: execution_context
                .causal_invocation
                .and_then(|invocation| invocation.effect_id().map(str::to_string)),
            execution_scope_id: scoped_effect_controller.scope_id().to_string(),
            controller_replay_ownership: scoped_effect_controller.controller().replay_ownership(),
        });
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"ok": true}),
            control: None,
        }
        .into())
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.cancelled.lock_recover().push(request);
        Ok(())
    }
}

struct AlreadyStartedRunner {
    calls: Mutex<usize>,
    winner: lash_core::LeaseOwnerIdentity,
}

#[async_trait::async_trait]
impl RestateProcessRunner for AlreadyStartedRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        *self.calls.lock_recover() += 1;
        Err(PluginError::ProcessAlreadyStarted {
            process_id: registration.id,
            by: Box::new(self.winner.clone()),
        })
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

struct SegmentedRecordingRunner {
    outcomes: Mutex<VecDeque<lash_core::ProcessRunOutcome>>,
    handovers: Mutex<Vec<Option<lash_core::SegmentHandover>>>,
    runs: AtomicUsize,
}

#[derive(Default)]
struct CancellationAwareRunner {
    started: tokio::sync::Notify,
    finish_successfully: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct BlockingCancelSignalTransport {
    requests: Mutex<Vec<HttpRequest>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct CeilingCancelWatchTransport {
    requests: AtomicUsize,
}

#[async_trait::async_trait]
impl HttpTransport for CeilingCancelWatchTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(timeout.unwrap_or(Duration::ZERO)).await;
        Err(
            HttpTransportError::new("cancel watch attach ceiling elapsed")
                .with_kind(lash_core::ProviderFailureKind::Timeout)
                .with_code("timeout")
                .retryable(true),
        )
    }
}

/// Answers every cancel-watch attach with Restate's 404 for a service no
/// registered deployment binds.
#[derive(Debug, Default)]
struct UnregisteredCancelWatchTransport;

#[async_trait::async_trait]
impl HttpTransport for UnregisteredCancelWatchTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: 404,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(
                r#"{"message":"Service 'LashProcessWorkflow' not found"}"#,
            ),
        })
    }
}

#[derive(Debug, Default)]
struct BrokenCancelWatchTransport;

#[async_trait::async_trait]
impl HttpTransport for BrokenCancelWatchTransport {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        Err(HttpTransportError::new("cancel watch transport failed"))
    }
}

#[async_trait::async_trait]
impl HttpTransport for BlockingCancelSignalTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.requests.lock_recover().push(request);
        self.started.notify_one();
        self.release.notified().await;
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered(r#""cancel_requested""#),
        })
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for CancellationAwareRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.started.notify_one();
        tokio::select! {
            _ = cancellation.cancelled() => Ok(ProcessAwaitOutput::Cancelled {
                message: "cancel signal observed".to_string(),
                raw: None,
                control: None,
            }
            .into()),
            _ = self.finish_successfully.notified() => Ok(ProcessAwaitOutput::Success {
                value: serde_json::json!("runner completed"),
                control: None,
            }
            .into()),
        }
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for SegmentedRecordingRunner {
    async fn run_process_segment(
        &self,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.handovers.lock_recover().push(handover);
        self.outcomes
            .lock_recover()
            .pop_front()
            .ok_or_else(|| PluginError::Session("unexpected duplicate segment run".to_string()))
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

fn inline_process_scope(process_id: &str) -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
        lash_core::ExecutionScope::process(process_id.to_string()),
    )
    .expect("inline process scope")
}

async fn pending_process_cancel_signal() -> Result<(), HandlerError> {
    std::future::pending().await
}

#[tokio::test]
async fn running_process_cancel_uses_native_signal_without_poll_delay() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let signal_transport = Arc::new(BlockingCancelSignalTransport::default());
    let cancel_ingress = RestateIngressClient::new(RestateConnection::with_transport(
        "https://restate.invalid",
        signal_transport.clone(),
    ));
    let workflow = Arc::new(LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
        cancel_ingress,
    ));
    let registration = rerunnable_registration("prompt-cancel");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");

    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            let cancellation_signal = workflow.cancellation_signal("prompt-cancel", 0);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    inline_process_scope("prompt-cancel"),
                    0,
                    None,
                    cancellation_signal,
                )
                .await
        })
    };
    runner.started.notified().await;
    signal_transport.started.notified().await;
    registry
        .append_event(
            "prompt-cancel",
            lash_core::ProcessEventAppendRequest::cancel_requested(
                "prompt-cancel",
                Some("stop promptly".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    signal_transport.release.notify_one();

    let outcome = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("native cancellation signal must not hang")
        .expect("join running process")
        .expect("run process");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Cancelled { .. })
    ));
    let requests = signal_transport.requests.lock_recover();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://restate.invalid/LashProcessWorkflow/prompt-cancel/await_cancel"
    );
}

#[tokio::test]
async fn cancel_watch_reissues_after_attach_ceiling_until_segment_completes() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let transport = Arc::new(CeilingCancelWatchTransport::default());
    let connection = RestateConnection::with_transport_and_config(
        "https://restate.invalid",
        transport.clone(),
        short_restate_timeouts(100, 10),
    );
    let workflow = LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
        RestateIngressClient::new(connection),
    );
    let registration = rerunnable_registration("ceiling-reissues");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    let finish = Arc::clone(&runner);
    let finisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(55)).await;
        finish.finish_successfully.notify_one();
    });

    let outcome = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            inline_process_scope("ceiling-reissues"),
            0,
            None,
            workflow.cancellation_signal("ceiling-reissues", 0),
        )
        .await
        .expect("attach ceiling expiry must not fail the segment");
    finisher.await.expect("finish segment");

    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert!(
        transport.requests.load(Ordering::SeqCst) >= 3,
        "the cancel watch must re-attach across several ceilings"
    );
}

#[tokio::test]
async fn non_timeout_cancel_watch_error_fails_the_segment() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let workflow = LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        process_registry(),
        continuation_store(),
        RestateIngressClient::new(RestateConnection::with_transport(
            "https://restate.invalid",
            Arc::new(BrokenCancelWatchTransport),
        )),
    );

    let error = workflow
        .run_registration(
            rerunnable_registration("broken-cancel-watch"),
            ProcessExecutionContext::default(),
            inline_process_scope("broken-cancel-watch"),
            0,
            None,
            workflow.cancellation_signal("broken-cancel-watch", 0),
        )
        .await
        .expect_err("a non-timeout cancel watch failure must fail the segment");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();

    assert!(
        source.to_string().contains("cancel watch transport failed"),
        "unexpected handler error: {error:?}"
    );
    assert!(
        source.to_string().starts_with("Retryable error"),
        "a transport fault is worth retrying, which is the case the \
         missing-registration terminal below has to be distinguishable from: \
         {error:?}"
    );
}

/// A cancel watch addressed to a service no deployment binds ends the segment
/// with the engine's own 404-class **terminal**, not a retryable error the
/// engine backs off forever (FIG-1579).
///
/// The cancel watch is a `loop`, and every error in it that is not a timeout
/// leaves through one arm. Before this, a 404 left through the same arm as a
/// broken socket and became `HandlerErrorInner::Retryable`, so a deployment that
/// forgot to `bind(LashProcessWorkflowImpl…)` produced an invocation retrying
/// with infinite exponential backoff and no operator ever told what was wrong —
/// the engine-tier twin of the warn-and-strand this contract rules out. A
/// missing registration is deterministic: retrying cannot make it appear.
#[tokio::test]
async fn an_unregistered_cancel_watch_service_is_a_terminal_not_an_indefinite_retry() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let workflow = LashProcessWorkflowImpl::new(
        Arc::clone(&runner),
        process_registry(),
        continuation_store(),
        RestateIngressClient::new(RestateConnection::with_transport(
            "https://restate.invalid",
            Arc::new(UnregisteredCancelWatchTransport),
        )),
    );

    let error = workflow
        .run_registration(
            rerunnable_registration("unregistered-cancel-watch"),
            ProcessExecutionContext::default(),
            inline_process_scope("unregistered-cancel-watch"),
            0,
            None,
            workflow.cancellation_signal("unregistered-cancel-watch", 0),
        )
        .await
        .expect_err("a cancel watch against an unbound service must fail the segment");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
    let rendered = source.to_string();

    assert!(
        rendered.starts_with("Terminal error [404]"),
        "a missing registration must leave as the engine's own terminal, so the \
         invocation ends instead of backing off forever: {error:?}"
    );
    assert!(
        rendered.contains("LashProcessWorkflow/await_cancel"),
        "the terminal names the address nothing binds, because `404 from \
         Restate` is not something an operator can act on: {error:?}"
    );
}

/// The classifier the terminal above turns on: a `404` is a missing registration
/// only on a route that addresses a service by name, and only for that status.
///
/// The negative half is the one that matters. `404` is not self-describing — on
/// a **control** route it names a missing *resource*, and most often an
/// invocation that is already gone: completed, killed, or aged out of retention.
/// A predicate that read the status alone would let a caller wired onto
/// `PATCH /invocations/{id}/cancel` terminalize real work over a cancel that
/// arrived one moment late, which is the opposite of the mistake this exists to
/// prevent. Routes therefore opt in by name, and an unclassified one keeps the
/// retryable handling every 404 had before.
#[test]
fn only_a_404_on_a_service_call_route_reads_as_a_missing_registration() {
    let service_call = |operation| crate::RestateHttpError::Status {
        operation,
        url: "https://restate.invalid/LashProcessWorkflow/k/await_cancel".to_string(),
        status: 404,
        body: "not found".to_string(),
    };
    for operation in [
        "Restate workflow call",
        "Restate object call",
        "Restate /send",
    ] {
        assert!(
            service_call(operation).is_service_unregistered(),
            "`{operation}` addresses a service by name, so its 404 is a missing \
             registration"
        );
        assert!(!service_call(operation).is_timeout());
    }

    for operation in [
        "Restate invocation cancel",
        "Restate invocation kill",
        "Restate SQL query",
    ] {
        let control_route = crate::RestateHttpError::Status {
            operation,
            url: "https://restate.invalid/invocations/inv_1/cancel".to_string(),
            status: 404,
            body: "invocation not found".to_string(),
        };
        assert!(
            !control_route.is_service_unregistered(),
            "`{operation}` is a control route: its 404 names a resource that is \
             gone, and terminalizing on it would end real work over a cancel \
             that arrived late"
        );
    }

    let unavailable = crate::RestateHttpError::Status {
        operation: "Restate workflow call",
        url: "https://restate.invalid/LashProcessWorkflow/k/await_cancel".to_string(),
        status: 503,
        body: "overloaded".to_string(),
    };
    assert!(
        !unavailable.is_service_unregistered(),
        "a busy engine is worth retrying; only `no such service or handler` is not"
    );
}

#[tokio::test]
async fn transient_cancel_registry_read_error_cannot_fall_through_to_success() {
    let runner = Arc::new(CancellationAwareRunner::default());
    let registry = process_registry();
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::clone(&runner),
        Arc::clone(&registry),
        continuation_store(),
    ));
    let registration = rerunnable_registration("transient-cancel-read");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");

    let (signal, cancellation_signal) = tokio::sync::oneshot::channel();
    let run = {
        let workflow = Arc::clone(&workflow);
        tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default(),
                    inline_process_scope("transient-cancel-read"),
                    0,
                    None,
                    async move {
                        cancellation_signal.await.map_err(|_| {
                            HandlerError::from(TerminalError::new(
                                "test cancellation signal sender dropped",
                            ))
                        })
                    },
                )
                .await
        })
    };
    runner.started.notified().await;
    registry
        .append_event(
            "transient-cancel-read",
            lash_core::ProcessEventAppendRequest::cancel_requested(
                "transient-cancel-read",
                Some("retry the durable read".to_string()),
            ),
        )
        .await
        .expect("append cancel request");
    workflow.fail_next_cancel_reads(1);
    signal
        .send(())
        .expect("resolve process cancellation signal");
    runner.finish_successfully.notify_one();

    let outcome = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("transient registry error must be retried")
        .expect("join running process")
        .expect("run process");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Cancelled { .. })
    ));
}

#[tokio::test]
async fn exhausted_cancel_confirmation_is_a_retryable_handler_error() {
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::new(CancellationAwareRunner::default()),
        process_registry(),
        continuation_store(),
    );
    workflow.fail_next_cancel_reads(6);

    let error = workflow
        .confirm_process_cancel_requested_for_test("cancel-confirmation")
        .await
        .expect_err("exhausted confirmation must stay retryable");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
    let rendered = source.to_string();

    assert!(
        format!("{error:?}").contains("Retryable"),
        "unexpected handler error: {error:?}"
    );
    assert!(
        rendered.contains("cancellation confirmation failed after 6 attempts"),
        "unexpected handler error: {error:?}"
    );
}

#[tokio::test]
async fn absent_event_after_cancel_promise_is_a_terminal_handler_error() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("missing-cancel-event"))
        .await
        .expect("register process");
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::new(CancellationAwareRunner::default()),
        registry,
        continuation_store(),
    );

    let error = workflow
        .confirm_process_cancel_requested_for_test("missing-cancel-event")
        .await
        .expect_err("a resolved promise without its durable event must be terminal");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();

    assert!(
        source.to_string().starts_with("Terminal error [500]:"),
        "unexpected handler error: {error:?}"
    );
}

#[tokio::test]
async fn durable_segment_handover_resumes_once_and_terminalizes_once() {
    let continuation = lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash: Some("program-v1".to_string()),
        engine_state: vec![1, 2, 3],
    };
    let terminal = ProcessAwaitOutput::Success {
        value: serde_json::json!({"result": 42}),
        control: None,
    };
    let runner = Arc::new(SegmentedRecordingRunner {
        outcomes: Mutex::new(VecDeque::from([
            lash_core::ProcessRunOutcome::SegmentBoundary(continuation.clone()),
            terminal.clone().into(),
        ])),
        handovers: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
    });
    let (registry, continuations) = process_stores();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        Arc::clone(&continuations),
    );
    let registration = rerunnable_registration("segmented-durable");
    let _record = registry
        .register_process(registration.clone())
        .await
        .expect("register segmented process");
    let first_context = Arc::new(ReplayableRecordingContext::default());
    let first_controller = RestateRuntimeEffectController::new(first_context.clone());

    let first = workflow
        .run_registration(
            registration.clone(),
            ProcessExecutionContext::default(),
            first_controller
                .scoped_effect_controller(ExecutionScope::process("segmented-durable"))
                .expect("durable first-segment scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run first segment");
    let lash_core::ProcessRunOutcome::SegmentBoundary(first_handover) = first else {
        panic!("first incarnation must end at a segment boundary");
    };
    let persisted = lash_core::PersistedSegmentHandover {
        segment_ordinal: 1,
        program_hash: "program-v1".to_string(),
        handover: first_handover,
    };
    continuations
        .put_segment_handover("segmented-durable", persisted.clone())
        .await
        .expect("persist before successor schedule");
    continuations
        .put_segment_handover("segmented-durable", persisted.clone())
        .await
        .expect("crash recovery repeats the persist idempotently");
    assert_eq!(
        process_segment_workflow_key("segmented-durable", 1),
        "segmented-durable#1"
    );

    let loaded = continuations
        .get_segment_handover("segmented-durable", 1)
        .await
        .expect("load successor handover")
        .expect("persisted successor handover");
    let resumed = validate_segment_program_hash("segmented-durable", loaded)
        .expect("matching program identity");
    first_context.start_replay();
    let successor_context = Arc::new(ReplayableRecordingContext::default());
    let successor_controller = RestateRuntimeEffectController::new(successor_context);
    let second = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            successor_controller
                .scoped_effect_controller(ExecutionScope::process("segmented-durable"))
                .expect("durable successor scope"),
            1,
            Some(resumed),
            pending_process_cancel_signal(),
        )
        .await
        .expect("run successor segment");
    assert!(matches!(second, lash_core::ProcessRunOutcome::Terminal(_)));
    assert_eq!(runner.runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        runner.handovers.lock_recover().as_slice(),
        &[None, Some(continuation)]
    );
    let events = registry
        .events_after("segmented-durable", 0)
        .await
        .expect("process events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count(),
        1,
        "only the true terminal is process-visible"
    );
    let awaited = lash_core::facade_support::ProcessAwaiter::polling(registry)
        .await_terminal("segmented-durable")
        .await
        .expect("await true terminal");
    assert_eq!(awaited, terminal);
}

#[derive(Clone, Copy, Debug)]
enum RestateSegmentReplayPoint {
    GetHandover,
    PutHandover,
    CancelCheck,
}

fn restate_segment_tool_attempt_outcome(ordinal: u64) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(completed_tool_record(
                &format!("matrix-call-{ordinal}"),
                "matrix_tool",
            )),
            intents: lash_core::ToolIntents::default(),
        }),
        triggers: Vec::new(),
    }
}

#[tokio::test]
async fn restate_segment_transition_replay_matrix_preserves_lineage_invariants() {
    for replay_point in [
        RestateSegmentReplayPoint::GetHandover,
        RestateSegmentReplayPoint::PutHandover,
        RestateSegmentReplayPoint::CancelCheck,
    ] {
        let process_id = format!("matrix-{replay_point:?}").to_ascii_lowercase();
        let (registry, continuations) = process_stores();
        let registration = rerunnable_registration(&process_id);
        registry
            .register_process(registration.clone())
            .await
            .expect("register matrix process");
        let terminal = ProcessAwaitOutput::Success {
            value: serde_json::json!({"result": 99, "effects": [0, 1, 2]}),
            control: None,
        };
        let runner = Arc::new(SegmentedRecordingRunner {
            outcomes: Mutex::new(VecDeque::from([
                lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("matrix-program-v1".to_string()),
                    engine_state: vec![1],
                }),
                lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("matrix-program-v1".to_string()),
                    engine_state: vec![2],
                }),
                terminal.clone().into(),
            ])),
            handovers: Mutex::new(Vec::new()),
            runs: AtomicUsize::new(0),
        });
        let workflow = LashProcessWorkflowImpl::new_for_test(
            Arc::clone(&runner),
            Arc::clone(&registry),
            Arc::clone(&continuations),
        );
        let mut input_handover = None;
        let mut successor_keys = HashSet::new();

        for ordinal in 0_u64..3 {
            let context = Arc::new(ReplayableRecordingContext::default());
            let controller = RestateRuntimeEffectController::with_options(
                Arc::clone(&context),
                RestateEffectControllerOptions::default().segment_effect_budget(1),
            );
            let local_calls = Arc::new(AtomicUsize::new(0));
            let envelope = RuntimeEffectEnvelope::new(
                RuntimeInvocation::effect(
                    lash_core::runtime::RuntimeScope::new(process_id.clone()),
                    format!("matrix-effect-{ordinal}"),
                    RuntimeEffectKind::ToolAttempt,
                    format!("matrix:{process_id}:{ordinal}"),
                ),
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared_tool_call_with(&format!("matrix-call-{ordinal}"), "matrix_tool"),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            );
            let first_calls = Arc::clone(&local_calls);
            let first_effect = controller
                .execute_effect(
                    envelope.clone(),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        first_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(restate_segment_tool_attempt_outcome(ordinal))
                    }),
                )
                .await
                .expect("first matrix effect");
            let progress = lash_core::SegmentProgress {
                effects_executed: 1,
                journaled_bytes_estimate: None,
            };
            assert_eq!(
                RuntimeEffectController::wants_segment_boundary(&controller, &progress),
                Some(lash_core::BoundaryReason::JournalBudget)
            );
            context.start_replay();
            let replay_calls = Arc::clone(&local_calls);
            let replay_effect = controller
                .execute_effect(
                    envelope,
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        replay_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(restate_segment_tool_attempt_outcome(ordinal))
                    }),
                )
                .await
                .expect("replayed matrix effect");
            assert_eq!(
                serde_json::to_value(&replay_effect).expect("serialize replay effect"),
                serde_json::to_value(&first_effect).expect("serialize first effect"),
                "replay effect identity"
            );
            assert_eq!(
                local_calls.load(Ordering::SeqCst),
                1,
                "handler replay must not double-execute an effect"
            );
            assert_eq!(
                RuntimeEffectController::wants_segment_boundary(&controller, &progress),
                Some(lash_core::BoundaryReason::JournalBudget),
                "replay must cut at the identical completed-effect budget"
            );

            if ordinal > 0 {
                let loaded = continuations
                    .get_segment_handover(&process_id, ordinal)
                    .await
                    .expect("get input handover")
                    .expect("running segment input survives");
                if matches!(replay_point, RestateSegmentReplayPoint::GetHandover) {
                    assert_eq!(
                        continuations
                            .get_segment_handover(&process_id, ordinal)
                            .await
                            .expect("replayed get"),
                        Some(loaded.clone())
                    );
                }
                input_handover = Some(
                    validate_segment_program_hash(&process_id, loaded)
                        .expect("valid matrix handover"),
                );
            }

            let run_once = workflow
                .run_registration(
                    registration.clone(),
                    ProcessExecutionContext::default(),
                    inline_process_scope(&process_id),
                    ordinal,
                    input_handover.take(),
                    pending_process_cancel_signal(),
                )
                .await
                .expect("matrix segment run");
            if ordinal == 2 {
                assert_eq!(run_once, terminal.clone().into());
                break;
            }
            let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = run_once else {
                panic!("matrix segment {ordinal} must request a boundary");
            };
            let next = ordinal + 1;
            let persisted = lash_core::PersistedSegmentHandover {
                segment_ordinal: next,
                program_hash: "matrix-program-v1".to_string(),
                handover: boundary,
            };
            continuations
                .put_segment_handover(&process_id, persisted.clone())
                .await
                .expect("put matrix handover");
            if matches!(replay_point, RestateSegmentReplayPoint::PutHandover) {
                continuations
                    .put_segment_handover(&process_id, persisted)
                    .await
                    .expect("replayed put is idempotent");
            }
            let cancel_checks = if matches!(replay_point, RestateSegmentReplayPoint::CancelCheck) {
                2
            } else {
                1
            };
            for _ in 0..cancel_checks {
                assert!(
                    !workflow
                        .process_cancel_requested(&process_id)
                        .await
                        .expect("matrix cancel check")
                );
            }
            let key = process_segment_workflow_key(&process_id, next);
            assert!(
                successor_keys.insert(key.clone()),
                "one successor per ordinal"
            );
        }

        assert_eq!(
            successor_keys.len(),
            2,
            "exactly one successor for each boundary"
        );
        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            3,
            "no duplicate incarnation"
        );
        registry
            .complete_process(
                &process_id,
                terminal.clone(),
                workflow_key_authority(&process_id),
            )
            .await
            .expect("write root terminal");
        let attach_after_retention =
            lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry));
        assert_eq!(
            attach_after_retention
                .await_terminal(&process_id)
                .await
                .expect("post-retention durable attach"),
            terminal
        );
        assert_eq!(
            registry
                .events_after(&process_id, 0)
                .await
                .expect("matrix events")
                .iter()
                .filter(|event| event.semantics.terminal.is_some())
                .count(),
            1,
            "root terminal is durable and exactly once"
        );
    }
}

#[test]
fn missing_segment_handover_distinguishes_superseded_orphan_from_current_input() {
    let latest = lash_core::PersistedSegmentHandover {
        segment_ordinal: 4,
        program_hash: "program-v1".to_string(),
        handover: lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: Some("program-v1".to_string()),
            engine_state: vec![4],
        },
    };
    assert!(missing_segment_is_superseded(2, Some(&latest)));
    assert!(!missing_segment_is_superseded(4, Some(&latest)));
    assert!(!missing_segment_is_superseded(5, Some(&latest)));
    assert!(!missing_segment_is_superseded(1, None));
}

#[tokio::test]
async fn persisted_handover_is_change_feed_and_event_invariant() {
    let (registry, continuations) = process_stores();
    let _record = registry
        .register_process(rerunnable_registration("segment-invariant"))
        .await
        .expect("register process");
    let (_, cursor) = registry
        .processes_changed_since(lash_core::ProcessChangeCursor::default(), 10)
        .await
        .expect("initial change feed");
    continuations
        .put_segment_handover(
            "segment-invariant",
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                program_hash: "program-v1".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::DurationCap,
                    program_hash: Some("program-v1".to_string()),
                    engine_state: vec![9],
                },
            },
        )
        .await
        .expect("persist handover");
    let (changes, next_cursor) = registry
        .processes_changed_since(cursor, 10)
        .await
        .expect("change feed after handover");
    assert!(changes.is_empty());
    assert_eq!(next_cursor, cursor);
    assert!(
        registry
            .events_after("segment-invariant", 0)
            .await
            .expect("events")
            .is_empty()
    );
}

#[tokio::test]
async fn segment_program_hash_mismatch_is_typed_and_cancel_redrives_successor_engine() {
    let mismatch = validate_segment_program_hash(
        "hash-bound",
        lash_core::PersistedSegmentHandover {
            segment_ordinal: 1,
            program_hash: "old-program".to_string(),
            handover: lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("new-program".to_string()),
                engine_state: vec![],
            },
        },
    )
    .expect_err("changed program must fail closed");
    assert_eq!(
        mismatch.code,
        lash_core::RuntimeErrorCode::RestateSegmentProgramHashMismatch
    );

    let runner = Arc::new(SegmentedRecordingRunner {
        outcomes: Mutex::new(VecDeque::from([ProcessAwaitOutput::Success {
            value: serde_json::Value::Null,
            control: None,
        }
        .into()])),
        handovers: Mutex::new(Vec::new()),
        runs: AtomicUsize::new(0),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    let registration = rerunnable_registration("cancel-between-segments");
    registry
        .register_process(registration.clone())
        .await
        .expect("register process");
    registry
        .append_event(
            "cancel-between-segments",
            lash_core::ProcessEventAppendRequest::cancel_requested(
                "cancel-between-segments",
                Some("stop".to_string()),
            ),
        )
        .await
        .expect("cancel between segments");
    let outcome = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            inline_process_scope("cancel-between-segments"),
            1,
            Some(lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: Some("program-v1".to_string()),
                engine_state: vec![1],
            }),
            async { Ok(()) },
        )
        .await
        .expect("cancelled successor");
    assert!(matches!(
        outcome,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Cancelled { .. })
    ));
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        1,
        "the cancelled successor must still be driven for replay-consistent command emission"
    );
}

#[test]
fn cancel_terminal_from_successor_routes_to_root_await_workflow_key() {
    assert_eq!(terminal_completion_workflow_key("process-1", 0), None);
    assert_eq!(
        terminal_completion_workflow_key("process-1", 1),
        Some("process-1".to_string())
    );
    assert_eq!(
        process_segment_workflow_key("process-1", 1),
        "process-1#1",
        "the running successor key must differ from the terminal await key"
    );
}

#[test]
fn boundary_registry_io_errors_are_retryable_handler_failures() {
    let error = retryable_registry_error(lash_core::PluginError::Session(
        "transient registry outage".to_string(),
    ));
    let debug = format!("{error:?}");
    assert!(
        debug.contains("Retryable"),
        "registry I/O must ask Restate to retry: {debug}"
    );
}

#[test]
fn boundary_with_armed_wait_is_declined_instead_of_terminalized() {
    let mut record = lash_core::ProcessRecord::from_registration(rerunnable_registration("wait"));
    record.wait = Some(lash_core::WaitState {
        since_ms: 1,
        kind: lash_core::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:wait:signal.ready:1".to_string(),
            ordinal: 1,
        },
    });
    assert!(boundary_must_be_declined(Some(&record)));
    record.wait = None;
    assert!(!boundary_must_be_declined(Some(&record)));
}

#[tokio::test]
async fn process_workflow_endpoint_smoke_schedules_runs_and_cancels_process() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                runner.clone(),
                registry.clone(),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let context = Arc::new(RecordingContext::with_endpoint(endpoint));
    let host = RestateRuntimeEffectController::new(context.clone());
    let registration =
        external_registration("task-smoke").with_wake_session_id(Some("wake-smoke".to_string()));
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-smoke"),
    ));

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: vec!["session".to_string()],
                    env_spec: None,
                    execution_context: Box::new(execution_context),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry.clone(), None),
        )
        .await
        .expect("start through endpoint smoke");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("wrong start outcome");
    };

    let external_ref = record.external_ref.as_ref().expect("external ref");
    assert_eq!(external_ref.backend, "restate");
    assert_eq!(external_ref.id, "LashProcessWorkflow/task-smoke");
    assert_eq!(
        external_ref
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("invocation-task-smoke"))
    );

    let observed = registry
        .list_observed_by("session")
        .await
        .expect("session observed");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].id, "task-smoke");
    let observed_external_ref = observed[0].external_ref.as_ref().expect("external ref");
    assert_eq!(observed_external_ref.backend, "restate");
    assert_eq!(observed_external_ref.id, "LashProcessWorkflow/task-smoke");

    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| registration.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-smoke"]
    );
    assert_eq!(
        runner.ran.lock_recover().as_slice(),
        &[RecordedProcessRun {
            process_id: "task-smoke".to_string(),
            wake_target_session_id: Some("wake-smoke".to_string()),
            tool_effect_id: Some("tool-smoke".to_string()),
            execution_scope_id: "task-smoke".to_string(),
            controller_replay_ownership: lash_core::EffectReplayOwnership::Controller,
        }]
    );

    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-smoke-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: "task-smoke".to_string(),
                    reason: Some("stop-smoke".to_string()),
                    replay: None,
                }),
            ),
            RuntimeEffectLocalExecutor::processes(registry, None),
        )
        .await
        .expect("cancel through endpoint smoke");
    assert!(matches!(
        outcome,
        RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Cancel { .. }
        }
    ));
    assert_eq!(
        context.cancelled.lock_recover().as_slice(),
        &[("task-smoke".to_string(), Some("stop-smoke".to_string()))]
    );
    assert_eq!(
        runner.cancelled.lock_recover().as_slice(),
        &[RestateProcessCancelRequest {
            process_id: "task-smoke".to_string(),
            reason: Some("stop-smoke".to_string()),
        }]
    );
}

struct RecoveryProcessTool;

impl RecoveryProcessTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_echo",
            "recovery_echo",
            "Echo a line and emit a durable process wake.",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "recovery_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RecoveryProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let _ = call;
        lash_core::ToolOutcome::err_fmt(
            "recovery_echo owes a process.wake emission and runs only on the leaf attempt route",
        )
    }

    async fn execute_attempt(
        &self,
        call: lash_core::ToolCall<'_>,
    ) -> lash_core::ToolAttemptOutcome {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(process_id) = call.context.runtime_process_id() else {
            return lash_core::ToolAttemptOutcome::done_without_intents(
                lash_core::ToolOutcomeDone::from_output(lash_core::ToolCallOutput::failure(
                    lash_core::ToolFailure::runtime(
                        lash_core::ToolFailureClass::Internal,
                        "recovery_echo_outside_process",
                        "recovery_echo runs only inside a durable process",
                    ),
                )),
            );
        };
        // The wake append is journal-capable work: the attempt declares it and
        // the intent executor emits it once the attempt commits.
        let intent = lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
            session_id: call.context.session_id().to_string(),
            process_id: process_id.to_string(),
            event_type: "process.wake".to_string(),
            payload: serde_json::json!({ "message": line, "wake_input": line }),
        });
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "echo": line })),
            lash_core::ToolIntents::v1(vec![intent]),
        )
    }
}

struct SnapshotRecoveryTool;

impl SnapshotRecoveryTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:snapshot_echo",
            "snapshot_echo",
            "Echo a line from a snapshot-backed process tool.",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "snapshot_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SnapshotRecoveryTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "snapshot_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        lash_core::ToolOutcome::ok(serde_json::json!({ "echo": format!("snapshot:{line}") }))
    }
}

#[derive(serde::Deserialize)]
struct SnapshotRecoveryToolOptions {
    snapshot_ref: String,
}

fn snapshot_recovery_tool_options(snapshot_ref: &str) -> lash_core::PluginOptions {
    lash_core::PluginOptions::typed(
        "snapshot-recovery-tool",
        serde_json::json!({ "snapshot_ref": snapshot_ref }),
    )
    .expect("snapshot recovery plugin options")
}

fn snapshot_recovery_tool_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::PluginSpecFactory::new(
        "snapshot-recovery-tool",
        Arc::new(|ctx| {
            let snapshot_available = ctx
                .plugin_options
                .decode::<SnapshotRecoveryToolOptions>("snapshot-recovery-tool")
                .map_err(|err| {
                    lash_core::PluginError::Registration(format!(
                        "invalid snapshot recovery tool options: {err}"
                    ))
                })?
                .is_some_and(|options| options.snapshot_ref == "tool-authority:sha256:ok");
            let spec = if snapshot_available {
                lash_core::facade_support::PluginSpec::new()
                    .with_tool_provider(Arc::new(SnapshotRecoveryTool))
            } else {
                lash_core::facade_support::PluginSpec::new()
            };
            Ok(spec)
        }),
    ))
}

fn recovery_worker(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> DurableProcessWorker {
    recovery_worker_with_plugins(registry, store_factory, Vec::new())
}

fn recovery_worker_with_plugins(
    registry: Arc<dyn ProcessRegistry>,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    extra_plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> DurableProcessWorker {
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(RecoveryProcessTool);
    let mut plugins = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn lash_core::facade_support::PluginFactory>,
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "recovery-tool",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
        )),
    ];
    plugins.extend(extra_plugins);
    let plugin_host = lash_core::facade_support::PluginHost::new(plugins);
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        RECOVERY_PROCESS_ENV_STORE.clone();
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_env_store(process_env_store)
    .with_process_engine(Arc::new(
        lash_lashlang_runtime::LashlangProcessEngine::in_memory(
            lash_lashlang_runtime::LashlangSurface::default(),
        ),
    ));
    DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            store_factory,
            registry,
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(recovery_session_policy()),
    )
}

struct ProcessParentIntentTool {
    calls: Arc<AtomicUsize>,
}

impl ProcessParentIntentTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:process_parent_intent",
            "process_parent_intent",
            "Optionally start a child carrying a Cancel-at-parent-end policy.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "emit": { "type": "boolean" },
                    "child": { "type": "string" }
                },
                "required": ["emit", "child"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "process_parent_intent"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProcessParentIntentTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "process_parent_intent").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        panic!("the process-parent law must use AttemptContext")
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
            .unwrap_or("missing-child");
        let intents = if call
            .args
            .get("emit")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::StartProcess(Box::new(
                lash_core::StartProcessIntent {
                    session_id: call.context.session_id().to_string(),
                    request: lash_core::ProcessStartRequest::external(
                        format!("ignored-{child}"),
                        lash_core::ProcessOriginator::host_scoped("process-parent-law"),
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

fn process_parent_intent_plugin(
    calls: Arc<AtomicUsize>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "process-parent-intent",
        lash_core::facade_support::PluginSpec::new()
            .with_tool_provider(Arc::new(ProcessParentIntentTool { calls })),
    ))
}

struct PanicOnceAfterDurableProcessTerminal {
    crashes: AtomicUsize,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicOnceAfterDurableProcessTerminal {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "process.parent_end.after_terminal"
            && self.crashes.fetch_add(1, Ordering::SeqCst) == 0
        {
            panic!("injected crash after durable process terminal and before parent teardown");
        }
    }
}

fn process_parent_worker(
    registry: Arc<dyn ProcessRegistry>,
    plugin: Arc<dyn lash_core::facade_support::PluginFactory>,
    probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot,
) -> DurableProcessWorker {
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        RECOVERY_PROCESS_ENV_STORE.clone();
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_env_store(process_env_store)
    .with_process_engine(Arc::new(
        lash_lashlang_runtime::LashlangProcessEngine::in_memory(
            lash_lashlang_runtime::LashlangSurface::default(),
        ),
    ));
    let plugins = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn lash_core::facade_support::PluginFactory>,
        plugin,
    ];
    DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(plugins)),
            runtime_host,
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            registry,
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(recovery_session_policy())
        .with_turn_phase_probe_slot(probe_slot),
    )
}

async fn process_parent_lashlang_registration(
    process_id: &str,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process main() {
          early = await tools.process_parent_intent({ emit: true, child: "segmented" })?
          later = await tools.process_parent_intent({ emit: false, child: "none" })?
          finish later.child
        }
        "#,
    )
    .expect("parse segmented process-parent law");
    let contract = ProcessParentIntentTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "process_parent_intent",
            "tool:process_parent_intent",
            lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
        )
        .expect("link process-parent law tool");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default().with_processes(),
        ),
    )
    .expect("link segmented process-parent law");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store segmented process-parent artifact");
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("main")
                .expect("main process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("process-parent-law")),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
async fn process_parents_teardown_after_durable_end_across_segments_and_tool_call_route() {
    let (registry, continuations) = process_stores();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let plugin = process_parent_intent_plugin(Arc::clone(&provider_calls));
    let probe_slot = lash_core::runtime::RuntimeTurnPhaseProbeSlot::default();
    probe_slot.set_for_session(
        "process-parent-law",
        Arc::new(PanicOnceAfterDurableProcessTerminal {
            crashes: AtomicUsize::new(0),
        }),
    );
    let worker = process_parent_worker(Arc::clone(&registry), Arc::clone(&plugin), probe_slot);
    let workflow = Arc::new(
        LashProcessWorkflowImpl::new_for_test(
            Arc::new(RestateCoreProcessRunner::new(worker.clone())),
            Arc::clone(&registry),
            Arc::clone(&continuations),
        )
        .with_segment_effect_budget_selector(|_| 1),
    );
    let env_ref = persist_recovery_env_ref().await;
    let segmented =
        process_parent_lashlang_registration("segmented-process-parent", env_ref.clone()).await;
    registry
        .register_process(segmented.clone())
        .await
        .expect("register segmented process parent");

    let mut ordinal = 0_u64;
    let mut input_handover = None;
    let mut boundary_count = 0_usize;
    let mut execution_id = None::<String>;
    loop {
        let context = Arc::new(ReplayableRecordingContext::default());
        context.defer_process_workflows();
        let context_evidence = Arc::clone(&context);
        let controller = RestateRuntimeEffectController::with_options(
            context,
            RestateEffectControllerOptions::default().segment_effect_budget(1),
        );
        let workflow = Arc::clone(&workflow);
        let registration = segmented.clone();
        let handover = input_handover.take();
        let retained = registry
            .get_process("segmented-process-parent")
            .await
            .expect("read retained process execution")
            .expect("segmented process exists");
        let (current_execution_id, execution_authority) = segment_execution_authority(
            "segmented-process-parent",
            ordinal,
            execution_id.as_deref(),
            &format!("process-parent-law-invocation-{ordinal}"),
            retained.first_started.as_deref(),
        )
        .expect("derive process-parent invocation authority");
        execution_id = Some(current_execution_id);
        let run = tokio::spawn(async move {
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_authority),
                    controller
                        .scoped_effect_controller(ExecutionScope::process(
                            "segmented-process-parent",
                        ))
                        .expect("segmented process scope"),
                    ordinal,
                    handover,
                    pending_process_cancel_signal(),
                )
                .await
        })
        .await;
        match run {
            Ok(Ok(lash_core::ProcessRunOutcome::SegmentBoundary(boundary))) => {
                boundary_count += 1;
                let durable_state: serde_json::Value =
                    serde_json::from_slice(&boundary.engine_state)
                        .expect("decode versioned Lashlang handover state");
                let visible_processes = registry
                    .list_processes(&lash_core::ProcessListFilter {
                        status: lash_core::ProcessStatusFilter::Any,
                        ..lash_core::ProcessListFilter::default()
                    })
                    .await
                    .expect("inspect early intent children");
                assert_eq!(durable_state["version"], serde_json::json!(3));
                assert_eq!(
                    durable_state["parent_end_actions"]
                        .as_array()
                        .expect("handover carries parent-end action array")
                        .len(),
                    1,
                    "the early intent survives every segment boundary; provider_calls={}; records={:?}; processes={visible_processes:?}",
                    provider_calls.load(Ordering::SeqCst),
                    context_evidence
                        .records
                        .lock_recover()
                        .values()
                        .filter_map(
                            |bytes| serde_json::from_slice::<RecordedRuntimeEffect>(bytes).ok()
                        )
                        .map(|recorded| recorded.outcome)
                        .collect::<Vec<_>>(),
                );
                let next = ordinal + 1;
                continuations
                    .put_segment_handover(
                        "segmented-process-parent",
                        lash_core::PersistedSegmentHandover {
                            segment_ordinal: next,
                            program_hash: boundary
                                .program_hash
                                .clone()
                                .expect("versioned Lashlang program hash"),
                            handover: boundary,
                        },
                    )
                    .await
                    .expect("durably store process-parent handover");
                let loaded = continuations
                    .get_segment_handover("segmented-process-parent", next)
                    .await
                    .expect("reload process-parent handover")
                    .expect("stored process-parent handover");
                input_handover = Some(
                    validate_segment_program_hash("segmented-process-parent", loaded)
                        .expect("valid process-parent program identity"),
                );
                ordinal = next;
            }
            Err(join_error) if join_error.is_panic() => break,
            other => panic!("unexpected segmented process-parent result: {other:?}"),
        }
    }
    assert_eq!(
        boundary_count, 2,
        "the real Lashlang process spans three segments"
    );
    let terminal_parent = registry
        .get_process("segmented-process-parent")
        .await
        .expect("read terminal segmented parent")
        .expect("segmented parent exists");
    assert_eq!(
        terminal_parent.outcome,
        Some(ProcessAwaitOutput::Success {
            value: serde_json::json!("none"),
            control: None,
        }),
        "the terminal is durable before the injected teardown crash"
    );
    let pending = registry
        .get_pending_parent_end_plan("segmented-process-parent")
        .await
        .expect("load segmented parent-end plan")
        .expect("the early-segment action survives in durable state");
    let [segmented_action] = pending.actions.as_slice() else {
        panic!("expected one literal early-segment action: {pending:?}");
    };
    assert_eq!(
        segmented_action.parent_end.policy,
        lash_core::ProcessParentEndPolicy::Cancel
    );
    assert_eq!(
        lash_core::derive_tool_intent_identity(
            &segmented_action.identity.session_id,
            &segmented_action.identity.execution_scope_id,
            Some(&segmented_action.identity.tool_call_id),
            segmented_action.identity.intent_index as usize,
        )
        .expect("rederive retained segmented identity"),
        segmented_action.identity,
        "the retained action carries its full validated identity"
    );
    assert_eq!(
        registry
            .events_after(&segmented_action.parent_end.process_id, 0)
            .await
            .expect("events before segmented redrive")
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        0,
        "the crash is after terminal commit and before teardown"
    );

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("redrive durable segmented parent-end plan");
    assert!(
        registry
            .get_pending_parent_end_plan("segmented-process-parent")
            .await
            .expect("inspect completed segmented plan")
            .is_none()
    );
    assert_eq!(
        registry
            .events_after(&segmented_action.parent_end.process_id, 0)
            .await
            .expect("events after segmented redrive")
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        1,
        "redrive applies the early-segment Cancel exactly once"
    );

    let tool_parent = ProcessRegistration::new(
        "tool-call-process-parent",
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                "tool-call-process-parent-call",
                "tool:process_parent_intent",
                "process_parent_intent",
                serde_json::json!({"emit": true, "child": "tool-call"}),
                None,
                serde_json::Value::Null,
            ),
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("process-parent-law")),
    )
    .with_execution_env_ref(Some(env_ref));
    registry
        .register_process(tool_parent)
        .await
        .expect("register ToolCall process parent");
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive ToolCall process parent");
    let tool_terminal = lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry))
        .await_terminal("tool-call-process-parent")
        .await
        .expect("await ToolCall parent terminal");
    assert_eq!(
        tool_terminal,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({"child": "tool-call"}),
            control: None,
        }
    );
    let children = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("list process-parent children");
    for child_name in ["segmented", "tool-call"] {
        let child = children
            .iter()
            .find(|record| {
                matches!(
                    record.input.as_ref(),
                    ProcessInput::External { metadata }
                        if metadata["process_parent_child"] == child_name
                )
            })
            .unwrap_or_else(|| panic!("missing {child_name} child in {children:?}"));
        tokio::time::timeout(
            Duration::from_secs(5),
            lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry)).await_event(
                &child.id,
                "process.cancel_requested",
                0,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out awaiting {child_name} child Cancel delivery"))
        .unwrap_or_else(|error| panic!("failed awaiting {child_name} child Cancel: {error}"));
        assert_eq!(
            registry
                .events_after(&child.id, 0)
                .await
                .expect("read child cancellation")
                .iter()
                .filter(|event| event.event_type == "process.cancel_requested")
                .count(),
            1,
            "{child_name} child receives one literal Cancel"
        );
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 3);
}

fn recovery_session_policy() -> lash_core::SessionPolicy {
    lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("model spec"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    }
}

async fn persist_recovery_env_ref() -> lash_core::ProcessExecutionEnvRef {
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::empty(),
        recovery_session_policy(),
    );
    lash_core::runtime::persist_process_execution_env(RECOVERY_PROCESS_ENV_STORE.as_ref(), &spec)
        .await
        .expect("persist recovery process execution env")
}

async fn persist_snapshot_recovery_env_ref(
    snapshot_ref: &str,
) -> lash_core::ProcessExecutionEnvRef {
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        snapshot_recovery_tool_options(snapshot_ref),
        recovery_session_policy(),
    );
    lash_core::runtime::persist_process_execution_env(RECOVERY_PROCESS_ENV_STORE.as_ref(), &spec)
        .await
        .expect("persist snapshot recovery process execution env")
}

fn process_wake_event_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "process.wake".to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec {
            wake: Some(lash_core::ProcessWakeSpec {
                when: Some(lash_core::ProcessValueSelector::Present(
                    "/wake_input".to_string(),
                )),
                input: lash_core::ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
            ..lash_core::ProcessEventSemanticsSpec::default()
        },
    }
}

async fn snapshot_lashlang_registration(
    process_id: &str,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process main() {
          called = await tools.snapshot_echo({ line: "restored" })?
          finish called.echo
        }
        "#,
    )
    .expect("snapshot lashlang module");
    let contract = SnapshotRecoveryTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "snapshot_echo",
            "tool:snapshot_echo",
            lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
        )
        .expect("host catalog operation must not conflict");
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_sleep()
                .with_process_signals(),
        ),
    )
    .expect("link snapshot lashlang module");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked_module.artifact,
    )
    .await
    .expect("store snapshot lashlang module artifact");
    let process_ref = linked_module
        .artifact
        .process_ref("main")
        .expect("main process ref")
        .clone();
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref,
            process_ref,
            host_requirements_ref: linked_module.host_requirements_ref,
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
async fn sqlite_process_recovery_reopens_registry_worker_observers_wakes_and_cancel() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_a = recovery_worker(Arc::clone(&registry_a), Arc::clone(&store_factory));
    let _root_store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::default(),
            policy: recovery_session_policy(),
        })
        .await
        .expect("create root session store before wake delivery");
    let endpoint_a = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker_a)),
                Arc::clone(&registry_a),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let context_a = Arc::new(RecordingContext::with_endpoint(endpoint_a));
    let host_a = RestateRuntimeEffectController::new(context_a);
    let creator_scope = lash_core::SessionScope::new("root");
    let env_ref = persist_recovery_env_ref().await;
    let registration = ProcessRegistration::new(
        "recover-tool",
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                "recover-call",
                "tool:recovery_echo",
                "recovery_echo",
                serde_json::json!({ "line": "wake-after-rebuild" }),
                None,
                serde_json::Value::Null,
            ),
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(creator_scope.clone()),
    )
    .with_extra_event_types([process_wake_event_type()])
    .with_execution_env_ref(Some(env_ref))
    .with_wake_session_id(Some(creator_scope.session_id.clone()));

    host_a
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration,
                    observers: vec![creator_scope.session_id.clone()],
                    env_spec: None,
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            RuntimeEffectLocalExecutor::processes(Arc::clone(&registry_a), None),
        )
        .await
        .expect("schedule and run process through Restate endpoint");
    drop(host_a);
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let observed = registry_b
        .list_observed_by(&creator_scope.session_id)
        .await
        .expect("list reopened observations");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].id, "recover-tool");
    assert_eq!(
        lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("recover-tool")
            .await
            .expect("await recovered terminal process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "echo": "wake-after-rebuild" }),
            control: None,
        }
    );
    let queue_store = store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::default(),
            policy: lash_core::SessionPolicy {
                model: lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
                ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
            },
        })
        .await
        .expect("open root session store");
    let queued = queue_store
        .list_queued_work("root")
        .await
        .expect("list queued wakes");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].items.len(), 1);
    let lash_core::runtime::QueuedWorkPayload::ProcessWake { wake } = &queued[0].items[0].payload
    else {
        panic!("expected process wake queue payload");
    };
    assert_eq!(wake.input, "wake-after-rebuild");
    assert_eq!(wake.target_session_id, "root");

    let worker_b = recovery_worker(Arc::clone(&registry_b), store_factory);
    let endpoint_b = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker_b)),
                Arc::clone(&registry_b),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let context_b = Arc::new(RecordingContext::with_endpoint(endpoint_b));
    let host_b = RestateRuntimeEffectController::new(context_b);
    host_b
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "recovery-cancel"),
                RuntimeEffectCommand::process(ProcessCommand::Cancel {
                    process_id: "recover-tool".to_string(),
                    reason: Some("post-rebuild cancel probe".to_string()),
                    replay: None,
                }),
            ),
            RuntimeEffectLocalExecutor::processes(Arc::clone(&registry_b), None),
        )
        .await
        .expect("cancel through reopened process workflow");
    assert!(
        registry_b
            .events_after("recover-tool", 0)
            .await
            .expect("events after cancel")
            .iter()
            .any(|event| event.event_type == "process.cancel_requested")
    );
}

#[tokio::test]
async fn sqlite_process_recovery_rebuilds_snapshot_plugin_options_after_worker_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:ok").await;
    registry_a
        .register_process(snapshot_lashlang_registration("snapshot-ok", env_ref).await)
        .await
        .expect("register snapshot-backed process");
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_b = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        store_factory,
        vec![snapshot_recovery_tool_factory()],
    );
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("recover snapshot-backed process");

    assert_eq!(
        lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("snapshot-ok")
            .await
            .expect("await recovered snapshot-backed process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!("snapshot:restored"),
            control: None,
        }
    );
}

#[tokio::test]
async fn sqlite_process_recovery_terminalizes_revoked_snapshot_plugin_options() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:revoked").await;
    registry_a
        .register_process(snapshot_lashlang_registration("snapshot-revoked", env_ref).await)
        .await
        .expect("register revoked snapshot-backed process");
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_b = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        store_factory,
        vec![snapshot_recovery_tool_factory()],
    );
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("recover revoked snapshot-backed process");

    let await_output = lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry_b))
        .await_terminal("snapshot-revoked")
        .await
        .expect("await terminal revoked snapshot-backed process");
    let ProcessAwaitOutput::Failure { code, message, .. } = await_output else {
        panic!("expected revoked snapshot process failure, got {await_output:#?}");
    };
    assert_eq!(code, "process_host_environment_incompatible");
    assert!(
        message.contains("module `tools` does not expose operation `snapshot_echo`"),
        "{message}"
    );
}

/// Build a durable registration for a trigger-started Lashlang engine process.
///
/// A trigger-started process carries the trigger route's engine payload and
/// provenance whose `caused_by` is the
/// trigger occurrence that fired it — distinct from a turn-started process, whose
/// provenance traces to a live turn/tool call. The module artifact is stored
/// in the process-global in-memory artifact store, mirroring how a trigger
/// route's linked module is published before the process runs; that store
/// survives the registry/worker reopen within a single test process.
async fn trigger_lashlang_registration(process_id: &str, resource: &str) -> ProcessRegistration {
    let module =
        lashlang::parse("process notify(resource: str) { finish { triggered: resource } }")
            .expect("lashlang trigger module");
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link lashlang trigger module");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked_module.artifact,
    )
    .await
    .expect("store lashlang trigger module artifact");
    let process_ref = linked_module
        .artifact
        .process_ref("notify")
        .expect("notify process ref")
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("resource".to_string(), serde_json::json!(resource));
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.module_ref,
            process_ref,
            host_requirements_ref: linked_module.host_requirements_ref,
            process_name: "notify".to_string(),
            args,
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("root")).with_caused_by(
            Some(lash_core::CausalRef::SessionNode {
                session_id: "root".to_string(),
                node_id: "trigger:resource.updated".to_string(),
            }),
        ),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

async fn typescript_process_registration(process_id: &str) -> ProcessRegistration {
    let linked = lash_typescript::link(
        r#"
        const worker = defineProcess({
          name: "worker",
          signals: {},
          run: async () => { return { ok: true }; }
        });
        finish(null);
        "#,
        &lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link TypeScript process");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store TypeScript artifact");
    assert_eq!(
        linked.artifact.compilation_dialect,
        lashlang::CompilationDialect::Typescript
    );
    let process = linked
        .artifact
        .canonical_ir
        .process("worker")
        .expect("worker process declaration");
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("worker")
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_signal_event_types(
        process,
    ))
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
async fn typescript_artifact_runs_through_process_engine_to_terminal() {
    let registry = process_registry();
    registry
        .register_process(typescript_process_registration("typescript-worker").await)
        .await
        .expect("register TypeScript process");

    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("run stored TypeScript artifact");
    assert_eq!(
        lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry))
            .await_terminal("typescript-worker")
            .await
            .expect("await TypeScript process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "ok": true }),
            control: None,
        }
    );
}

fn assert_lashlang_engine_record(
    record: &lash_core::ProcessRecord,
    expected_process_name: &str,
    expected_args: serde_json::Map<String, serde_json::Value>,
) {
    let ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
        panic!(
            "persisted Lashlang process must use generic engine input, got {:?}",
            record.input
        );
    };
    assert_eq!(
        kind,
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        "persisted row must dispatch through the registered Lashlang process engine"
    );
    let decoded = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("persisted Lashlang engine payload must decode after registry reopen");
    assert_eq!(decoded.process_name, expected_process_name);
    assert_eq!(decoded.args, expected_args);
}

/// Phase-B recovery: a TRIGGER-started process whose worker died mid-flight is
/// left non-terminal in the durable registry; a subsequent worker reopening
/// that registry must drive it to completion via the recovery sweep — the same
/// durable re-execution guarantee a turn-started process has (invariant 3).
///
/// Mirrors `sqlite_process_recovery_reopens_registry_worker_observers_wakes_and_cancel`
/// but the process is started by a trigger occurrence (a `lashlang` engine row
/// with trigger provenance), not by a live turn's tool call.
#[tokio::test]
async fn sqlite_trigger_started_process_recovered_after_worker_registry_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;

    // A worker started the trigger process and crashed before it could run:
    // the durable row exists and is non-terminal. We register it directly to
    // model exactly that mid-flight crash state.
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    registry_a
        .register_process(trigger_lashlang_registration("trigger-notify", "issue-42").await)
        .await
        .expect("register trigger-started process");
    let persisted_before_rebuild = registry_a
        .get_process("trigger-notify")
        .await
        .expect("read process")
        .expect("persisted trigger-started process before recovery");
    assert!(
        !persisted_before_rebuild.is_terminal(),
        "freshly trigger-started process must be non-terminal before recovery"
    );
    drop(registry_a);

    // Reopen the registry and stand up a fresh worker over it: the crash
    // recovery counterpart. The recovery sweep submits the non-terminal process
    // by workflow key; Restate coalesces duplicates and the workflow writes the
    // terminal outcome.
    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let reopened_record = registry_b
        .get_process("trigger-notify")
        .await
        .expect("read process")
        .expect("trigger-started process survives registry reopen");
    assert_lashlang_engine_record(
        &reopened_record,
        "notify",
        serde_json::Map::from_iter([("resource".to_string(), serde_json::json!("issue-42"))]),
    );
    assert_eq!(
        registry_b
            .list_non_terminal_page(
                std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                None,
            )
            .await
            .expect("list non-terminal after reopen")
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["trigger-notify"],
        "the trigger-started process must be on the recovery worklist after reopen"
    );

    let worker_b = recovery_worker(Arc::clone(&registry_b), Arc::clone(&store_factory));
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("recover non-terminal trigger-started process");

    assert_eq!(
        lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("trigger-notify")
            .await
            .expect("await recovered trigger-started process"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "triggered": "issue-42" }),
            control: None,
        },
        "the trigger-started process must run to its terminal value on recovery"
    );
    assert!(
        registry_b
            .list_non_terminal_page(
                std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                None,
            )
            .await
            .expect("list non-terminal after recovery")
            .records
            .is_empty(),
        "recovery must drive the trigger-started process to terminal"
    );

    // Idempotent by process_id: re-running the sweep over an already-terminal
    // process is a no-op and never double-executes it.
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("second recovery sweep is idempotent");
    assert_eq!(
        lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(&registry_b))
            .await_terminal("trigger-notify")
            .await
            .expect("await after idempotent re-sweep"),
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "triggered": "issue-42" }),
            control: None,
        }
    );
}

/// A process tool that counts executions in a shared atomic.
struct CountingProcessTool {
    executions: Arc<AtomicUsize>,
}

impl CountingProcessTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_count",
            "recovery_count",
            "Increment a shared execution counter (a stand-in non-idempotent side effect).",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "recovery_count"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_count").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let executed = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        lash_core::ToolOutcome::ok(serde_json::json!({ "executed": executed, "line": line }))
    }
}

fn counting_tool_plugin(
    executions: Arc<AtomicUsize>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "counting-process-tool",
        lash_core::facade_support::PluginSpec::new()
            .with_tool_provider(Arc::new(CountingProcessTool { executions })),
    ))
}

fn counting_tool_registration(
    id: &str,
    disposition: lash_core::RecoveryContract,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                format!("{id}-call"),
                "tool:recovery_count",
                "recovery_count",
                serde_json::json!({ "line": id }),
                None,
                serde_json::Value::Null,
            ),
        },
        disposition,
        lash_core::ProcessProvenance::host(),
    )
    .with_execution_env_ref(Some(env_ref))
}

fn discover_service<S: Discoverable>(_: &S) -> restate_sdk::discovery::Service {
    S::discover()
}

#[tokio::test]
async fn restate_workflows_and_wait_index_bind_with_required_handlers() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let service =
        LashProcessWorkflowImpl::new_for_test(runner, registry, continuation_store()).serve();
    let discovery = discover_service(&service);
    let wait_workflow = LashDurableWaitWorkflowImpl.serve();
    let wait_workflow_discovery = discover_service(&wait_workflow);
    let wait_index = LashDurableWaitIndexImpl.serve();
    let wait_index_discovery = discover_service(&wait_index);
    let endpoint = Endpoint::builder()
        .bind(service)
        .bind(wait_workflow)
        .bind(wait_index)
        .build();

    assert_eq!(discovery.name.to_string(), "LashProcessWorkflow");
    assert_eq!(
        discovery.ty.to_string(),
        restate_sdk::discovery::ServiceType::Workflow.to_string()
    );
    assert_eq!(discovery.handlers.len(), 6);

    let run = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "run")
        .expect("run handler discovery");
    let cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "cancel")
        .expect("cancel handler discovery");
    let await_terminal = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "await_terminal")
        .expect("await_terminal handler discovery");
    let complete_terminal = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "complete_terminal")
        .expect("complete_terminal handler discovery");
    let deliver_cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "deliver_cancel")
        .expect("deliver_cancel handler discovery");
    let await_cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "await_cancel")
        .expect("await_cancel handler discovery");

    assert_eq!(
        run.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("WORKFLOW")
    );
    assert_eq!(
        cancel.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        await_terminal
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        complete_terminal
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        deliver_cancel
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        await_cancel.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("SHARED")
    );

    let response = endpoint.handle(
        http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v3+json")
            .body(Empty::<bytes::Bytes>::new())
            .expect("discover request"),
    );
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.restate.endpointmanifest.v3+json")
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("discover response body")
        .to_bytes();
    let manifest: serde_json::Value =
        serde_json::from_slice(&body).expect("discover response json");
    let workflow = manifest["services"]
        .as_array()
        .expect("services array")
        .iter()
        .find(|service| service["name"] == "LashProcessWorkflow")
        .expect("workflow service");
    let handlers = workflow["handlers"].as_array().expect("handlers array");
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "run" && handler["ty"] == "WORKFLOW")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "cancel" && handler["ty"] == "SHARED")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "await_terminal" && handler["ty"] == "SHARED")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "complete_terminal" && handler["ty"] == "SHARED" })
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "deliver_cancel" && handler["ty"] == "SHARED" })
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "await_cancel" && handler["ty"] == "SHARED" })
    );
    assert_eq!(
        wait_workflow_discovery.name.to_string(),
        "LashDurableWaitWorkflow"
    );
    assert!(wait_workflow_discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "await_resolution"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert_eq!(wait_workflow_discovery.handlers.len(), 3);
    assert!(
        wait_workflow_discovery
            .handlers
            .iter()
            .all(|handler| handler.name.to_string() != "observe"),
        "the superseded observe handoff must not remain registered"
    );
    assert!(wait_workflow_discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "resolve"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert_eq!(
        wait_index_discovery.name.to_string(),
        "LashDurableWaitIndex"
    );
    for required in ["register", "settle", "resolve", "cancel_all", "revoke_all"] {
        assert!(
            wait_index_discovery
                .handlers
                .iter()
                .any(|handler| handler.name.to_string() == required),
            "missing wait-index handler {required}"
        );
    }
}

#[tokio::test]
async fn process_deployment_driver_and_workflow_share_registry() {
    let registry = process_registry();
    let deployment = RestateProcessDeployment::new(
        "http://127.0.0.1:8080",
        Arc::clone(&registry),
        continuation_store(),
    );
    let driver = deployment.process_work_driver();
    let driver_registry = driver.process_registry();

    assert!(Arc::ptr_eq(&driver.process_registry(), &driver_registry));

    let worker = DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::empty()),
            lash_core::facade_support::RuntimeHostConfig::in_memory(
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
            ),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            Arc::clone(&driver_registry),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_change_hub(driver.change_hub()),
    );
    let service = deployment.workflow(worker).serve();
    let discovery = discover_service(&service);
    let endpoint = Endpoint::builder().bind(service).build();

    assert_eq!(discovery.name.to_string(), "LashProcessWorkflow");
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "run"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("WORKFLOW")
    }));
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "cancel"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "await_terminal"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));

    let response = endpoint.handle(
        http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v3+json")
            .body(Empty::<bytes::Bytes>::new())
            .expect("discover request"),
    );
    assert_eq!(response.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn process_workflow_impl_runs_and_cancels_through_runner() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    // The workflow only ever runs lash-executed rows: `submit_record` refuses to
    // POST an ExternallyOwned row, and the registry rejects a workflow-key
    // completion of one (ADR 0027) — so the fixture is Rerunnable.
    let registration = rerunnable_registration("task-workflow")
        .with_wake_session_id(Some("wake-session".to_string()));
    registry
        .register_process(registration.clone())
        .await
        .expect("register workflow process");
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-effect"),
    ));

    let output = workflow
        .run_registration(
            registration,
            execution_context,
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("task-workflow"),
            )
            .expect("inline process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("workflow run");
    workflow
        .cancel_registration(RestateProcessCancelRequest {
            process_id: "task-workflow".to_string(),
            reason: Some("stop".to_string()),
        })
        .await
        .expect("workflow cancel");

    assert!(matches!(
        output,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert_eq!(
        runner.ran.lock_recover().as_slice(),
        &[RecordedProcessRun {
            process_id: "task-workflow".to_string(),
            wake_target_session_id: Some("wake-session".to_string()),
            tool_effect_id: Some("tool-effect".to_string()),
            execution_scope_id: "task-workflow".to_string(),
            controller_replay_ownership: lash_core::EffectReplayOwnership::Runtime,
        }]
    );
    assert_eq!(
        runner.cancelled.lock_recover().as_slice(),
        &[RestateProcessCancelRequest {
            process_id: "task-workflow".to_string(),
            reason: Some("stop".to_string()),
        }]
    );
}

#[tokio::test]
async fn terminal_retry_returns_the_stored_outcome() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow =
        LashProcessWorkflowImpl::new_for_test(runner, registry.clone(), continuation_store());
    registry
        .register_process(rerunnable_registration("terminal-retry"))
        .await
        .expect("register process");
    let stored = ProcessAwaitOutput::Success {
        value: serde_json::json!({"winner": "stored"}),
        control: None,
    };
    registry
        .complete_process(
            "terminal-retry",
            stored.clone(),
            lash_core::ProcessCompletionAuthority::workflow_key("terminal-retry"),
        )
        .await
        .expect("commit terminal");

    let replayed = workflow
        .complete_with_stored_outcome(
            "terminal-retry",
            ProcessAwaitOutput::Failure {
                class: lash_core::ToolFailureClass::Execution,
                code: "divergent".to_string(),
                message: "must not replace the stored outcome".to_string(),
                raw: None,
                control: None,
            },
        )
        .await
        .expect("terminal retry");

    assert_eq!(replayed, stored);
    assert_eq!(
        registry
            .events_after("terminal-retry", 0)
            .await
            .expect("terminal events")
            .into_iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count(),
        1
    );
}

fn invocation_started(
    process_id: &str,
    execution_id: &str,
    attempt: u32,
) -> (
    lash_core::ProcessExecutionWriteAuthority,
    lash_core::ProcessStarted,
) {
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id)
        .bind_attempt(attempt);
    let mut started = authority
        .invocation_started()
        .expect("bound invocation authority");
    started.started_at_ms = u64::from(attempt);
    (authority, started)
}

#[tokio::test]
async fn restate_invocation_identity_distinguishes_replay_from_fresh_execution() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("invocation-rerun").with_max_attempts(Some(2)))
        .await
        .expect("register rerunnable");

    let (first_authority, first_started) =
        invocation_started("invocation-rerun", "invocation-1", 1);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                first_started.clone(),
                &first_authority,
            )
            .await
            .expect("first invocation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                first_started,
                &first_authority,
            )
            .await
            .expect("cross-replica journal replay"),
        lash_core::ProcessStartOutcome::AlreadyApplied(_)
    ));

    let (second_authority, second_started) =
        invocation_started("invocation-rerun", "invocation-2", 2);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                second_started,
                &second_authority,
            )
            .await
            .expect("fresh invocation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
    let (third_authority, third_started) =
        invocation_started("invocation-rerun", "invocation-3", 3);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "invocation-rerun",
                third_started,
                &third_authority,
            )
            .await
            .expect("attempt budget verdict"),
        lash_core::ProcessStartOutcome::AttemptsExhausted {
            attempts: 2,
            max_attempts: 2,
            ..
        }
    ));
}

#[tokio::test]
async fn segment_zero_ignores_stale_carried_execution_identity() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("root-stale-id").with_max_attempts(Some(2)))
        .await
        .expect("register rerunnable");
    let (first_authority, first_started) =
        invocation_started("root-stale-id", "stale-invocation", 1);
    registry
        .record_first_started_with_authority(
            "root-stale-id",
            first_started.clone(),
            &first_authority,
        )
        .await
        .expect("record old attempt");

    let (execution_id, authority) = segment_execution_authority(
        "root-stale-id",
        0,
        Some("stale-invocation"),
        "fresh-invocation",
        Some(&first_started),
    )
    .expect("segment-zero identity");
    assert_eq!(execution_id, "fresh-invocation");
    let authority = authority.bind_attempt(2);
    let mut started = authority
        .invocation_started()
        .expect("bound fresh invocation");
    started.started_at_ms = 2;
    assert!(matches!(
        registry
            .record_first_started_with_authority("root-stale-id", started, &authority)
            .await
            .expect("fresh root attempt"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
}

#[tokio::test]
async fn redriven_mid_chain_segment_consumes_attempt_and_respects_budget() {
    let registry = process_registry();
    registry
        .register_process(
            owner_bound_registration("owner-bound-redrive").with_max_attempts(Some(2)),
        )
        .await
        .expect("register owner-bound");
    let (root_authority, root_started) =
        invocation_started("owner-bound-redrive", "root-invocation", 1);
    registry
        .record_first_started_with_authority(
            "owner-bound-redrive",
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("record root");

    let (_, redrive_authority) = segment_execution_authority(
        "owner-bound-redrive",
        1,
        None,
        "redrive-invocation-1",
        Some(&root_started),
    )
    .expect("validated handover redrive identity");
    let redrive_authority = redrive_authority.bind_attempt(2);
    let mut redrive_started = redrive_authority
        .invocation_started()
        .expect("bound redrive");
    redrive_started.started_at_ms = 2;
    let redrive_record = match registry
        .record_first_started_with_authority(
            "owner-bound-redrive",
            redrive_started,
            &redrive_authority,
        )
        .await
        .expect("owner-bound continuation may rebind at a handover")
    {
        lash_core::ProcessStartOutcome::Started(record) => record,
        other => panic!("expected a new continuation attempt, got {other:?}"),
    };
    assert_eq!(
        redrive_record
            .first_started
            .as_deref()
            .map(|started| started.attempt),
        Some(2)
    );

    let retained = redrive_record
        .first_started
        .as_deref()
        .expect("retained redrive start");
    let (_, exhausted_authority) = segment_execution_authority(
        "owner-bound-redrive",
        1,
        None,
        "redrive-invocation-2",
        Some(retained),
    )
    .expect("second handover redrive identity");
    let exhausted_authority = exhausted_authority.bind_attempt(3);
    let exhausted_started = exhausted_authority
        .invocation_started()
        .expect("bound exhausted redrive");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "owner-bound-redrive",
                exhausted_started,
                &exhausted_authority,
            )
            .await
            .expect("attempt budget verdict"),
        lash_core::ProcessStartOutcome::AttemptsExhausted {
            attempts: 2,
            max_attempts: 2,
            ..
        }
    ));
}

#[tokio::test]
async fn rerunnable_mid_chain_redrive_continues_from_validated_handover() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("rerunnable-redrive"))
        .await
        .expect("register rerunnable");
    let (root_authority, root_started) =
        invocation_started("rerunnable-redrive", "root-invocation", 1);
    registry
        .record_first_started_with_authority(
            "rerunnable-redrive",
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("record root");

    let (_, redrive_authority) = segment_execution_authority(
        "rerunnable-redrive",
        1,
        None,
        "redrive-invocation",
        Some(&root_started),
    )
    .expect("validated handover redrive identity");
    let redrive_authority = redrive_authority.bind_attempt(2);
    let redrive_started = redrive_authority
        .invocation_started()
        .expect("bound redrive");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "rerunnable-redrive",
                redrive_started,
                &redrive_authority,
            )
            .await
            .expect("rerunnable continuation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
}

#[tokio::test]
async fn owner_bound_segment_continuation_reuses_root_invocation_identity() {
    let registry = process_registry();
    registry
        .register_process(owner_bound_registration("owner-bound-segment"))
        .await
        .expect("register owner-bound");
    let (root_authority, root_started) =
        invocation_started("owner-bound-segment", "root-invocation", 1);
    registry
        .record_first_started_with_authority(
            "owner-bound-segment",
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("start root segment");

    let (execution_id, successor_authority) = segment_execution_authority(
        "owner-bound-segment",
        1,
        Some("root-invocation"),
        "successor-handler-invocation",
        Some(&root_started),
    )
    .expect("validated live successor");
    assert_eq!(execution_id, "root-invocation");
    let successor_authority = successor_authority.bind_attempt(1);
    let mut successor_started = successor_authority
        .invocation_started()
        .expect("bound successor");
    successor_started.started_at_ms = root_started.started_at_ms;
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "owner-bound-segment",
                successor_started,
                &successor_authority,
            )
            .await
            .expect("mid-chain continuation"),
        lash_core::ProcessStartOutcome::AlreadyApplied(_)
    ));
    let (fresh_authority, fresh_started) =
        invocation_started("owner-bound-segment", "fresh-invocation", 2);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                "owner-bound-segment",
                fresh_started,
                &fresh_authority,
            )
            .await
            .expect("fresh owner-bound invocation verdict"),
        lash_core::ProcessStartOutcome::AlreadyStarted { .. }
    ));
}

#[tokio::test]
async fn run_registration_abandons_restarted_owner_bound_without_running() {
    // When the engine re-invokes the workflow for an OwnerBound row whose prior
    // incarnation already recorded `first_started` but left no outcome, the run
    // handler must not re-execute it. The workflow-key recovery path records an
    // Abandoned{Sweep} terminal so durable awaiters resolve.
    let started_owner = lash_core::LeaseOwnerIdentity::opaque("owner-a", "incarnation-1");
    let runner = Arc::new(AlreadyStartedRunner {
        calls: Mutex::new(0),
        winner: started_owner.clone(),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    let registration = owner_bound_registration("ob-restart");
    registry
        .register_process(registration.clone())
        .await
        .expect("register owner-bound process");
    // Simulate the prior incarnation that began executing but never completed.
    registry
        .record_first_started(
            "ob-restart",
            lash_core::ProcessStarted {
                owner: started_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 42,
            },
        )
        .await
        .expect("record prior incarnation start");

    let output = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("ob-restart"),
            )
            .expect("inline process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run_registration");

    // The real runner rejects this before user-code execution when its atomic
    // start write observes the prior OwnerBound attempt.
    assert_eq!(*runner.calls.lock_recover(), 1);
    let lash_core::ProcessRunOutcome::Terminal(output) = &output else {
        panic!("expected terminal output, got {output:?}");
    };
    let ProcessAwaitOutput::Abandoned { evidence, .. } = output.as_ref() else {
        panic!("expected Abandoned output, got {output:?}");
    };
    assert_eq!(evidence.writer, AbandonWriter::Sweep);
    assert_eq!(evidence.owner.as_ref(), Some(&started_owner));
    let record = registry
        .get_process("ob-restart")
        .await
        .expect("read process")
        .expect("get abandoned row");
    assert!(record.is_terminal(), "the row is completed as terminal");
    assert!(matches!(
        record.outcome,
        Some(ProcessAwaitOutput::Abandoned { .. })
    ));
}

#[tokio::test]
async fn run_registration_runs_fresh_owner_bound() {
    // A fresh OwnerBound row has no `first_started` (the runner records it inside
    // run_process, during execution), so the re-invocation guard must NOT fire:
    // the runner executes normally on the first invocation.
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    let registration = owner_bound_registration("ob-fresh");
    registry
        .register_process(registration.clone())
        .await
        .expect("register fresh owner-bound process");

    let output = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("ob-fresh"),
            )
            .expect("inline process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run_registration");

    assert!(matches!(
        output,
        lash_core::ProcessRunOutcome::Terminal(output)
            if matches!(*output, ProcessAwaitOutput::Success { .. })
    ));
    assert_eq!(
        runner
            .ran
            .lock_recover()
            .iter()
            .map(|run| run.process_id.clone())
            .collect::<Vec<_>>(),
        vec!["ob-fresh".to_string()],
        "a fresh OwnerBound row runs through the runner on first invocation"
    );
}

#[tokio::test]
async fn ingress_runner_submits_non_terminal_process_by_workflow_key() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // A non-terminal, Lash-executed (Rerunnable) process is the durable
    // worklist row the ingress runner must submit. ExternallyOwned rows are
    // never submitted (ADR 0019), so the submittable case uses a Rerunnable row.
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("task-1"))
        .await
        .expect("register");

    // Minimal mock ingress: capture two submissions, then reply 202 Accepted
    // so the reqwest submit succeeds. The second submit exercises the
    // registry's exact-repeat external_ref path for a still-running process.
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = captured.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.expect("read request");
            captured_server
                .lock_recover()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            socket
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 49\r\n\r\n{\"invocationId\":\"inv_task_1\",\"status\":\"Accepted\"}",
                )
                .await
                .expect("write response");
            socket.flush().await.expect("flush");
        }
    });

    let runner = RestateProcessIngressRunner::new(
        format!("http://{addr}"),
        registry.clone(),
        continuation_store(),
    );
    let _ = runner.claim_and_run_pending().await.expect("drive pending");
    let _ = runner
        .claim_and_run_pending()
        .await
        .expect("drive pending again");
    server.await.expect("mock ingress server task");

    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        2,
        "the non-terminal process must be submitted on both scans"
    );
    let request = &requests[0];
    assert!(
        request.starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "submits the keyed workflow run: {request}"
    );
    assert!(
        !request.contains("idempotency-key:"),
        "workflow sends must not carry an idempotency header; Restate coalesces by workflow key: {request}"
    );
    assert!(
        requests[1].starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "repeat scan submits the same keyed workflow run: {}",
        requests[1]
    );

    // The durable backend reference is recorded so the process is observably
    // owned by Restate.
    let record = registry
        .get_process("task-1")
        .await
        .expect("read process")
        .expect("get process");
    assert_eq!(
        record.external_ref.as_ref().map(|e| e.backend.as_str()),
        Some("restate"),
        "the durable external_ref must be recorded after a successful submit"
    );
    assert_eq!(
        record
            .external_ref
            .as_ref()
            .and_then(|external| external.metadata.as_ref())
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("inv_task_1"))
    );
}

#[tokio::test]
async fn ingress_sweep_resumes_latest_segment_without_duplicate_segment_zero() {
    let (registry, continuations) = process_stores();
    registry
        .register_process(rerunnable_registration("mid-chain"))
        .await
        .expect("register");
    continuations
        .put_segment_handover(
            "mid-chain",
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 3,
                program_hash: "program-v1".to_string(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: Some("program-v1".to_string()),
                    engine_state: vec![3],
                },
            },
        )
        .await
        .expect("persist live segment");
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_mid_chain_3","status":"Accepted"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuations);
    let _ = runner.claim_and_run_pending().await.expect("drive pending");
    server.await.expect("capture server");

    let requests = captured.lock_recover();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/mid-chain%233/run/send "),
        "recovery must address the latest segment workflow key: {}",
        requests[0]
    );
    assert!(
        requests[0].contains("\"segment_ordinal\":3"),
        "recovery input must preserve the latest ordinal: {}",
        requests[0]
    );
    assert!(
        !requests[0].contains("\"execution_id\""),
        "an ingress redrive must mint identity from its new invocation: {}",
        requests[0]
    );
    assert!(!requests[0].starts_with("POST /LashProcessWorkflow/mid-chain/run/send "));
}

#[tokio::test]
async fn ingress_sweep_skips_externally_owned_and_reconciles_abandon_request() {
    // ADR 0019 at the Restate tier: the ingress sweep never POSTs a run for an
    // ExternallyOwned row (Lash does not execute it), but it does reconcile such
    // a row's pending Abandon Request into an `Abandoned{ReconciledRequest}`
    // terminal — mirroring the core sweep's `reconcile_externally_owned_abandon`.
    // A Rerunnable row alongside them still submits, so exactly one ingress call
    // fires and it is for the Lash-executed row.
    let registry = process_registry();
    registry
        .register_process(external_registration("ext-abandon"))
        .await
        .expect("register externally-owned row with pending abandon");
    registry
        .request_process_abandon(
            "ext-abandon",
            lash_core::AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 111,
                reason: Some("host retired".to_string()),
            },
        )
        .await
        .expect("record abandon request");
    registry
        .register_process(external_registration("ext-idle"))
        .await
        .expect("register externally-owned row without abandon");
    registry
        .register_process(rerunnable_registration("rerun-1"))
        .await
        .expect("register rerunnable row");

    // The capture server accepts exactly one connection: if any ExternallyOwned
    // row were submitted, a second connect would be attempted and the extra
    // submit would fail, so the single-response server also proves they are not.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_rerun_1","status":"Accepted"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuation_store());
    let report = runner
        .claim_and_run_pending()
        .await
        .expect("sweep skips externally-owned rows and submits the rerunnable one");
    server.await.expect("mock ingress server task");

    // Skipped is not silent: an externally-owned row is a typed deferral on this
    // tier too, so one registry reads the same whichever tier drove it.
    assert_eq!(report.admitted, vec!["rerun-1".to_string()]);
    let externally_owned = report
        .deferred
        .iter()
        .filter(|entry| entry.disposition == ProcessRecoveryAttemptOutcome::ExternallyOwned)
        .map(|entry| entry.process_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        externally_owned,
        vec!["ext-abandon".to_string(), "ext-idle".to_string()]
    );

    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        1,
        "only the Rerunnable row is submitted; ExternallyOwned rows are never POSTed"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/rerun-1/run/send "),
        "the single submit is the Lash-executed row: {}",
        requests[0]
    );

    // The abandon-request externally-owned row is now terminal Abandoned, written
    // by the reconciled-request path with no Lash execution owner to name.
    let abandoned = registry
        .get_process("ext-abandon")
        .await
        .expect("read process")
        .expect("get reconciled row");
    assert!(
        abandoned.is_terminal(),
        "an externally-owned row with a pending abandon request is reconciled to terminal"
    );
    let Some(ProcessAwaitOutput::Abandoned { evidence, .. }) = abandoned.outcome.as_ref() else {
        panic!("expected Abandoned terminal, got {:?}", abandoned.status);
    };
    assert_eq!(evidence.writer, AbandonWriter::ReconciledRequest);
    assert!(
        evidence.owner.is_none(),
        "externally-owned work has no Lash execution owner to name"
    );

    // The externally-owned row without an abandon request is left untouched for
    // its external owner to complete.
    let idle = registry
        .get_process("ext-idle")
        .await
        .expect("read process")
        .expect("get idle externally-owned row");
    assert!(
        !idle.is_terminal(),
        "an externally-owned row with no abandon request is left non-terminal"
    );
}

struct MockHttpResponse {
    status: &'static str,
    body: &'static str,
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let mut scratch = [0u8; 1024];
    loop {
        let n = socket.read(&mut scratch).await.expect("read request");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
        let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if buf.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn spawn_restate_http_capture(
    responses: Vec<MockHttpResponse>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            captured_server.lock_recover().push(request);
            let body = response.body.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status,
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(body).await.expect("write response body");
            socket.flush().await.expect("flush");
        }
    });
    (format!("http://{addr}"), captured, server)
}

async fn spawn_restate_http_black_hole() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _request = read_http_request(&mut socket).await;
        std::future::pending::<()>().await;
    });
    (format!("http://{addr}"), server)
}

async fn spawn_restate_http_stalled_body(
    status: &'static str,
    body: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _request = read_http_request(&mut socket).await;
        let header = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write response headers");
        socket.flush().await.expect("flush response headers");
        std::future::pending::<()>().await;
    });
    (format!("http://{addr}"), server)
}

fn short_restate_timeouts(
    control_timeout_ms: u64,
    attach_ceiling_ms: u64,
) -> RestateConnectionConfig {
    RestateConnectionConfig {
        control_timeout_ms,
        attach_ceiling_ms,
    }
}

fn assert_retryable_timeout(error: RestateHttpError, expected_message: &str) {
    let RestateHttpError::Request { source, .. } = error else {
        panic!("expected typed Restate request timeout, got {error}");
    };
    assert_eq!(source.kind, lash_core::ProviderFailureKind::Timeout);
    assert_eq!(source.code.as_deref(), Some("timeout"));
    assert!(source.retryable);
    assert!(source.message.contains(expected_message), "{source}");
}

#[test]
fn restate_connection_timeout_config_has_serde_defaults() {
    let defaults: RestateConnectionConfig = serde_json::from_str("{}").expect("default config");
    assert_eq!(defaults.control_timeout_ms, 30_000);
    assert_eq!(defaults.attach_ceiling_ms, 6 * 60 * 60 * 1_000);

    let configured: RestateConnectionConfig = serde_json::from_value(serde_json::json!({
        "control_timeout_ms": 125,
        "attach_ceiling_ms": 9_000,
    }))
    .expect("configured timeouts");
    assert_eq!(configured.control_timeout_ms, 125);
    assert_eq!(configured.attach_ceiling_ms, 9_000);

    let unknown = serde_json::from_value::<RestateConnectionConfig>(serde_json::json!({
        "control_timeout_ms": 125,
        "attach_ceiling_ms": 9_000,
        "control_timout_ms": 10,
    }))
    .expect_err("unknown timeout fields must be rejected");
    assert!(
        unknown
            .to_string()
            .contains("unknown field `control_timout_ms`")
    );

    for (field, value) in [
        (
            "control_timeout_ms",
            serde_json::json!({"control_timeout_ms": 0}),
        ),
        (
            "attach_ceiling_ms",
            serde_json::json!({"attach_ceiling_ms": 0}),
        ),
    ] {
        let error = serde_json::from_value::<RestateConnectionConfig>(value)
            .expect_err("zero timeout must be rejected");
        assert!(
            error
                .to_string()
                .contains(&format!("{field} must be greater than zero")),
            "unexpected config error: {error}"
        );
    }
}

#[tokio::test]
async fn restate_control_operation_times_out_against_black_hole() {
    let (base_url, server) = spawn_restate_http_black_hole().await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(500, 2_000),
    ));
    let started = std::time::Instant::now();

    let error = client
        .send_service_json("LashService", "run", &serde_json::json!({}))
        .await
        .expect_err("control submit must time out");
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;

    assert_retryable_timeout(error, "control deadline");
    assert!(elapsed >= Duration::from_millis(400), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
}

#[tokio::test]
async fn restate_attach_survives_control_timeout_and_honors_ceiling() {
    let control_timeout = Duration::from_millis(20);
    let response_delay = Duration::from_millis(80);
    let attach_ceiling = Duration::from_secs(2);
    let (base_url, _captured, server) = spawn_restate_http_capture_delayed(
        vec![MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"attached"}"#,
        }],
        response_delay,
    )
    .await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(
            control_timeout.as_millis() as u64,
            attach_ceiling.as_millis() as u64,
        ),
    ));
    let started = std::time::Instant::now();

    let output: ProcessAwaitOutput = client
        .call_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "await_terminal",
            &RestateProcessAwaitRequest {
                process_id: "process-1".to_string(),
            },
        )
        .await
        .expect("attach must use its ceiling rather than the control timeout");
    let elapsed = started.elapsed();
    server.await.expect("delayed response server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("attached"),
            control: None,
        }
    );
    assert!(elapsed > control_timeout, "elapsed {elapsed:?}");
    assert!(elapsed < attach_ceiling, "elapsed {elapsed:?}");

    let (base_url, black_hole) = spawn_restate_http_black_hole().await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(100, 500),
    ));
    let started = std::time::Instant::now();
    let error = client
        .call_workflow_json::<_, serde_json::Value>(
            "LashWorkflow",
            "key",
            "await",
            &serde_json::json!({}),
        )
        .await
        .expect_err("attach ceiling must bound a black hole");
    let elapsed = started.elapsed();
    black_hole.abort();
    let _ = black_hole.await;

    assert_retryable_timeout(error, "attach ceiling");
    assert!(elapsed >= Duration::from_millis(400), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
}

#[tokio::test]
async fn restate_attach_body_read_is_clamped_to_control_timeout() {
    let (base_url, server) = spawn_restate_http_stalled_body("200 OK", "{}").await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(100, 2_000),
    ));
    let started = std::time::Instant::now();

    let error = client
        .call_workflow_json::<_, serde_json::Value>(
            "LashWorkflow",
            "key",
            "await",
            &serde_json::json!({}),
        )
        .await
        .expect_err("an attach body is no longer a durable park");
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;

    assert_retryable_timeout(error, "attach ceiling response body");
    assert!(elapsed >= Duration::from_millis(75), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_millis(500), "elapsed {elapsed:?}");
}

#[tokio::test]
async fn restate_success_and_error_body_reads_share_the_request_deadline() {
    for status in ["202 Accepted", "500 Internal Server Error"] {
        let (base_url, server) = spawn_restate_http_stalled_body(status, "{}").await;
        let client = RestateIngressClient::new(RestateConnection::with_config(
            base_url,
            short_restate_timeouts(30, 500),
        ));
        let error = client
            .send_service_json("LashService", "run", &serde_json::json!({}))
            .await
            .expect_err("stalled response body must time out");
        server.abort();
        let _ = server.await;
        assert_retryable_timeout(error, "control deadline response body");
    }
}

#[tokio::test]
async fn restate_ingress_client_parses_send_invocation_id() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_123","status":"Accepted"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let invocation_id = client
        .send_workflow_json(
            "WorkbenchTurnWorkflow",
            "turn-1",
            "run",
            &serde_json::json!({ "turn_id": "turn-1" }),
        )
        .await
        .expect("send workflow");
    server.await.expect("capture server");

    assert_eq!(invocation_id.as_str(), "inv_123");
    let requests = captured.lock_recover();
    assert!(
        requests[0].starts_with("POST /WorkbenchTurnWorkflow/turn-1/run/send "),
        "unexpected request: {}",
        requests[0]
    );
    assert!(requests[0].contains(r#""turn_id":"turn-1""#));
}

#[derive(Debug)]
struct ScriptedHttpTransport {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<VecDeque<HttpResponse>>,
}

impl ScriptedHttpTransport {
    fn new(responses: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl HttpTransport for ScriptedHttpTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.requests.lock_recover().push(request);
        self.responses
            .lock_recover()
            .pop_front()
            .ok_or_else(|| HttpTransportError::new("scripted transport exhausted"))
    }
}

#[derive(Debug)]
struct AuthorizationTransport {
    inner: Arc<dyn HttpTransport>,
    token: Arc<RwLock<String>>,
}

#[async_trait::async_trait]
impl HttpTransport for AuthorizationTransport {
    async fn send(
        &self,
        mut request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        let token = self.token.read_recover().clone();
        request
            .headers
            .push(("authorization".to_string(), format!("Bearer {token}")));
        self.inner.send(request, timeout).await
    }
}

fn accepted_response(invocation_id: &str) -> HttpResponse {
    HttpResponse {
        status: 202,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: HttpResponseBody::buffered(format!(
            r#"{{"invocationId":"{invocation_id}","status":"Accepted"}}"#
        )),
    }
}

#[tokio::test]
async fn host_transport_injects_authorization_on_ingress_submit() {
    let scripted = Arc::new(ScriptedHttpTransport::new([accepted_response("inv_auth")]));
    let token = Arc::new(RwLock::new("cloud-token".to_string()));
    let decorated: Arc<dyn HttpTransport> = Arc::new(AuthorizationTransport {
        inner: scripted.clone(),
        token,
    });
    let connection = RestateConnection::with_transport("https://cloud.example", decorated);
    let client = RestateIngressClient::new(connection);

    client
        .send_service_json("LashService", "run", &serde_json::json!({"input": "hello"}))
        .await
        .expect("authenticated ingress submit");

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value == "Bearer cloud-token"
    }));
}

#[tokio::test]
async fn ingress_unauthorized_error_mentions_status_401() {
    let scripted: Arc<dyn HttpTransport> = Arc::new(ScriptedHttpTransport::new([HttpResponse {
        status: 401,
        headers: Vec::new(),
        body: HttpResponseBody::buffered(r#"{"message":"missing bearer token"}"#),
    }]));
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        scripted,
    ));

    let error = client
        .send_service_json("LashService", "run", &serde_json::json!({}))
        .await
        .expect_err("unauthorized submit must fail");

    assert!(error.to_string().contains("status 401"), "{error}");
    assert!(
        error.to_string().contains("missing bearer token"),
        "{error}"
    );
}

#[tokio::test]
async fn authorization_decorator_reads_rotated_credentials_per_request() {
    let scripted = Arc::new(ScriptedHttpTransport::new([
        accepted_response("inv_first"),
        accepted_response("inv_second"),
    ]));
    let token = Arc::new(RwLock::new("first-token".to_string()));
    let decorated: Arc<dyn HttpTransport> = Arc::new(AuthorizationTransport {
        inner: scripted.clone(),
        token: Arc::clone(&token),
    });
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        decorated,
    ));

    client
        .send_service_json("LashService", "run", &serde_json::json!({"attempt": 1}))
        .await
        .expect("first submit");
    *token.write_recover() = "second-token".to_string();
    client
        .send_service_json("LashService", "run", &serde_json::json!({"attempt": 2}))
        .await
        .expect("second submit");

    let authorization = scripted
        .requests()
        .into_iter()
        .map(|request| {
            request
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .expect("authorization header")
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(authorization, ["Bearer first-token", "Bearer second-token"]);
}

#[tokio::test]
async fn restate_ingress_client_accepts_previously_accepted_send() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_duplicate","status":"PreviouslyAccepted"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let invocation_id = client
        .send_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "run",
            &serde_json::json!({ "process_id": "process-1" }),
        )
        .await
        .expect("idempotent duplicate send");
    server.await.expect("capture server");

    assert_eq!(invocation_id.as_str(), "inv_duplicate");
}

#[tokio::test]
async fn restate_ingress_client_calls_workflow_and_decodes_output() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":{"ok":true}}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let output: ProcessAwaitOutput = client
        .call_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "await_terminal",
            &RestateProcessAwaitRequest {
                process_id: "process-1".to_string(),
            },
        )
        .await
        .expect("call workflow");
    server.await.expect("capture server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "ok": true }),
            control: None,
        }
    );
    let requests = captured.lock_recover();
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/process-1/await_terminal "),
        "unexpected request: {}",
        requests[0]
    );
    assert!(!requests[0].contains("/send "));
}

#[tokio::test]
async fn restate_ingress_client_pins_effect_replay_with_idempotency_key() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"status":"cancelled"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let output: Resolution = client
        .call_workflow_json_idempotent(
            "LashDurableWaitWorkflow",
            "promise-key",
            "await_resolution",
            &serde_json::json!({}),
            "stable-envelope-hash",
        )
        .await
        .expect("call idempotent workflow");
    server.await.expect("capture server");

    assert_eq!(output, Resolution::Cancelled);
    let requests = captured.lock_recover();
    assert!(
        requests[0].contains("idempotency-key: stable-envelope-hash"),
        "explicit effect replay identity must reach Restate: {}",
        requests[0]
    );
}

#[tokio::test]
async fn restate_process_attach_calls_await_terminal_ingress() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":"attached"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, process_registry(), continuation_store());

    let output = runner
        .await_terminal("process-1")
        .await
        .expect("attach await");
    server.await.expect("capture server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("attached"),
            control: None,
        }
    );
}

#[tokio::test]
async fn cancel_during_successor_boundary_routes_root_and_await_terminal_resolves() {
    assert_eq!(
        terminal_completion_workflow_key("retained-terminal", 2),
        Some("retained-terminal".to_string())
    );
    let registry = process_registry();
    registry
        .register_process(external_registration("retained-terminal"))
        .await
        .expect("register");
    let expected = ProcessAwaitOutput::Cancelled {
        message: "cancelled after a long chain".to_string(),
        raw: None,
        control: None,
    };
    registry
        .complete_process(
            "retained-terminal",
            expected.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    let runner =
        RestateProcessIngressRunner::new("http://127.0.0.1:1", registry, continuation_store());

    assert_eq!(
        runner
            .await_terminal("retained-terminal")
            .await
            .expect("registry terminal bypasses expired workflow key"),
        expected
    );
}

#[tokio::test]
async fn restate_process_attach_maps_ingress_error_to_plugin_error() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"boom"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, process_registry(), continuation_store());

    let err = runner
        .await_terminal("process-1")
        .await
        .expect_err("attach error");
    server.await.expect("capture server");

    assert!(
        err.to_string()
            .contains("ingress await for process `process-1` failed")
    );
    assert!(err.to_string().contains("status 500"));
    assert!(err.to_string().contains("boom"));
}

#[tokio::test]
async fn restate_process_attach_preserves_re_attach_signal_on_ceiling() {
    let (base_url, black_hole) = spawn_restate_http_black_hole().await;
    let runner = RestateProcessIngressRunner::new(
        RestateConnection::with_config(base_url, short_restate_timeouts(100, 25)),
        process_registry(),
        continuation_store(),
    );

    let error = runner
        .await_terminal("process-1")
        .await
        .expect_err("attach ceiling must be typed for host re-attachment");
    black_hole.abort();
    let _ = black_hole.await;

    assert!(matches!(
        error,
        PluginError::ProcessAttachCeilingElapsed { ref process_id }
            if process_id == "process-1"
    ));
}

#[tokio::test]
async fn restate_turn_attach_preserves_re_attach_code_on_ceiling() {
    let (base_url, black_hole) = spawn_restate_http_black_hole().await;
    let attach = RestateTurnAttach::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(100, 25),
    ));

    let error = attach
        .await_terminal(&TurnAddress::new("session-1", "turn-1"))
        .await
        .expect_err("attach ceiling must be coded for host re-attachment");
    black_hole.abort();
    let _ = black_hole.await;

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed
    );
    assert!(error.is_retryable());
}

/// Like [`spawn_restate_http_capture`], but holds each accepted connection open
/// for `delay` before responding, modeling a durable promise that resolves only
/// once the workflow's `run` completes.
async fn spawn_restate_http_capture_delayed(
    responses: Vec<MockHttpResponse>,
    delay: std::time::Duration,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            captured_server.lock_recover().push(request);
            tokio::time::sleep(delay).await;
            let body = response.body.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status,
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(body).await.expect("write response body");
            socket.flush().await.expect("flush");
        }
    });
    (format!("http://{addr}"), captured, server)
}

#[tokio::test]
async fn restate_attach_before_run_resolves_with_delayed_workflow_output() {
    // The ingress attach is a synchronous long-hold call issued while the
    // workflow's `run` is still in flight; it resolves only when the durable
    // promise does. A delayed mock stands in for that hold, and the eventual
    // output flows back through the driver's attach.
    let delay = std::time::Duration::from_millis(300);
    let (base_url, captured, server) = spawn_restate_http_capture_delayed(
        vec![MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":{"eventual":true}}"#,
        }],
        delay,
    )
    .await;
    let registry = process_registry();
    let deployment =
        RestateProcessDeployment::new(base_url, Arc::clone(&registry), continuation_store());
    let driver = deployment.process_work_driver();
    // A non-terminal process routes await_terminal through the ingress attach
    // rather than the registry short-circuit.
    driver
        .process_registry()
        .register_process(external_registration("process-1"))
        .await
        .expect("register non-terminal process");

    let started = std::time::Instant::now();
    let output = driver
        .await_terminal("process-1")
        .await
        .expect("attach await resolves with the eventual output");
    let elapsed = started.elapsed();
    server.await.expect("capture server");

    assert_eq!(
        output,
        ProcessAwaitOutput::Success {
            value: serde_json::json!({ "eventual": true }),
            control: None,
        }
    );
    assert!(
        elapsed >= delay,
        "the attach must block on the durable promise until run resolves (waited {elapsed:?})"
    );
    let requests = captured.lock_recover();
    assert_eq!(
        requests.len(),
        1,
        "await_terminal issues exactly one ingress call"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/process-1/await_terminal "),
        "unexpected request: {}",
        requests[0]
    );
}

#[tokio::test]
async fn restate_driver_short_circuits_terminal_without_ingress_call() {
    // Empty response set: the capture server accepts nothing, so any ingress
    // call would fail. The registry terminal short-circuit must fire first, so
    // the attach is never consulted for an already-terminal process.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![]).await;
    let registry = process_registry();
    let deployment =
        RestateProcessDeployment::new(base_url, Arc::clone(&registry), continuation_store());
    let driver = deployment.process_work_driver();
    let output = ProcessAwaitOutput::Success {
        value: serde_json::json!("already-terminal"),
        control: None,
    };
    driver
        .process_registry()
        .register_process(external_registration("process-1"))
        .await
        .expect("register");
    driver
        .process_registry()
        .complete_process(
            "process-1",
            output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let resolved = driver
        .await_terminal("process-1")
        .await
        .expect("terminal short-circuit resolves without ingress");
    server.await.expect("capture server");

    assert_eq!(resolved, output);
    assert!(
        captured.lock_recover().is_empty(),
        "a terminal short-circuit must not issue any ingress call"
    );
}

#[tokio::test]
async fn restate_process_attach_maps_malformed_ingress_body_to_plugin_error() {
    // A 2xx response whose body does not decode into ProcessAwaitOutput must
    // surface as a PluginError, not a panic — complementing the non-2xx case.
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: "this-is-not-valid-json",
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, process_registry(), continuation_store());

    let err = runner
        .await_terminal("process-1")
        .await
        .expect_err("a malformed ingress body must surface as an error");
    server.await.expect("capture server");

    assert!(
        err.to_string()
            .contains("ingress await for process `process-1` failed"),
        "unexpected error: {err}"
    );
}

/// Records each pushed event's `(event_type, sequence)` in emit order, and
/// every worker fault the handle reports.
#[derive(Clone, Default)]
struct RecordingProcessEventSink {
    events: Arc<Mutex<Vec<(String, u64)>>>,
    faults: Arc<Mutex<Vec<lash_core::facade_support::ProcessWorkerFault>>>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for RecordingProcessEventSink {
    async fn emit(&self, event: &lash_core::ProcessEvent) {
        self.events
            .lock_recover()
            .push((event.event_type.clone(), event.sequence));
    }

    async fn emit_worker_fault(&self, fault: &lash_core::facade_support::ProcessWorkerFault) {
        self.faults.lock_recover().push(fault.clone());
    }
}

#[tokio::test]
async fn restate_deployment_sink_funnel_feeds_appended_events() {
    // ADR 0017 names `RestateProcessDeployment::new_with_sink` as the durable
    // hosts' wrap funnel: a sink installed there observes every append made
    // through the deployment's shared registry, including terminal events.
    let sink = RecordingProcessEventSink::default();
    let deployment = RestateProcessDeployment::new_with_sink(
        "http://127.0.0.1:8080",
        process_registry(),
        continuation_store(),
        Some(Arc::new(sink.clone())),
    );
    let registry = deployment.process_work_driver().process_registry();
    registry
        .register_process(
            external_registration("sink-funnel").with_extra_event_types([
                lash_core::ProcessEventType {
                    name: "producer.tick".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
        )
        .await
        .expect("register");
    registry
        .append_event(
            "sink-funnel",
            lash_core::ProcessEventAppendRequest::new("producer.tick", serde_json::json!({})),
        )
        .await
        .expect("append");
    registry
        .complete_process(
            "sink-funnel",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let events = sink.events.lock_recover().clone();
    assert_eq!(
        events
            .iter()
            .map(|(event_type, _)| event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["producer.tick", "process.completed"],
        "the deployment-wrapped registry feeds every append to the sink"
    );
    assert!(
        events[0].1 < events[1].1,
        "the deployment sink preserves strictly ordered event sequences"
    );
}

#[tokio::test]
async fn restate_process_attach_is_reentrant_across_sequential_awaits() {
    // The shared await_terminal handler is re-entrant: two sequential attaches
    // each issue an independent ingress call and both succeed.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"first"}"#,
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"second"}"#,
        },
    ])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, process_registry(), continuation_store());

    let first = runner
        .await_terminal("process-1")
        .await
        .expect("first attach await");
    let second = runner
        .await_terminal("process-1")
        .await
        .expect("second attach await");
    server.await.expect("capture server");

    assert_eq!(
        first,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("first"),
            control: None,
        }
    );
    assert_eq!(
        second,
        ProcessAwaitOutput::Success {
            value: serde_json::json!("second"),
            control: None,
        }
    );
    assert_eq!(
        captured.lock_recover().len(),
        2,
        "each await issues an independent ingress call"
    );
}

#[tokio::test]
async fn restate_admin_client_cancels_kills_and_queries_invocation_status() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "202 Accepted",
            body: "",
        },
        MockHttpResponse {
            status: "200 OK",
            body: "",
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"rows":[{"id":"inv_123","target":"WorkbenchTurnWorkflow/turn-1/run","target_service_name":"WorkbenchTurnWorkflow","target_service_key":"turn-1","target_handler_name":"run","status":"completed","completion_result":"success","completion_failure":null}]}"#,
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"rows":[{"id":"inv_456","target":"WorkbenchTurnWorkflow/turn-2/run","target_service_name":"WorkbenchTurnWorkflow","target_service_key":"turn-2","target_handler_name":"run","status":"suspended"}]}"#,
        },
    ])
    .await;
    let client = RestateAdminClient::new(base_url);
    let invocation_id = RestateInvocationId::new("inv_123");

    client
        .cancel_invocation(&invocation_id)
        .await
        .expect("cancel");
    client
        .kill_invocation_for_test_cleanup(&invocation_id)
        .await
        .expect("kill");
    let status = client
        .invocation_status(&invocation_id)
        .await
        .expect("status")
        .expect("status row");
    let workflow_status = client
        .workflow_invocation_status("WorkbenchTurnWorkflow", "turn-2", "run")
        .await
        .expect("workflow status")
        .expect("workflow status row");
    server.await.expect("capture server");

    assert!(status.completed_successfully());
    assert_eq!(status.target_service_name, "WorkbenchTurnWorkflow");
    assert!(workflow_status.is_still_active());
    let requests = captured.lock_recover();
    assert!(
        requests[0].starts_with("PATCH /invocations/inv_123/cancel "),
        "unexpected cancel request: {}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("PATCH /invocations/inv_123/kill "),
        "unexpected kill request: {}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("POST /query "),
        "unexpected query request: {}",
        requests[2]
    );
    assert!(requests[2].contains("FROM sys_invocation WHERE id = 'inv_123'"));
    assert!(requests[3].contains(
        "target_service_name = 'WorkbenchTurnWorkflow' AND target_service_key = 'turn-2' AND target_handler_name = 'run'"
    ));
}

/// A submit that fails mid-pass is that row's outcome, not the pass's. Failing
/// the call would throw away the ids that already reached the ingress, so the
/// failure rides back as a typed per-row deferral instead.
#[tokio::test]
async fn a_failed_ingress_submit_defers_its_row_without_discarding_the_pass() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("submit-fails"))
        .await
        .expect("register the row whose submit fails");

    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"ingress unavailable"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuation_store());
    let report = runner
        .claim_and_run_pending()
        .await
        .expect("a per-row submit failure does not fail the pass");
    server.await.expect("mock ingress server task");

    assert!(report.admitted.is_empty());
    assert_eq!(report.deferred.len(), 1, "{report:?}");
    assert_eq!(report.deferred[0].process_id, "submit-fails");
    let ProcessRecoveryAttemptOutcome::BackendError { operation, .. } =
        &report.deferred[0].disposition
    else {
        panic!(
            "expected a typed backend error, got {:?}",
            report.deferred[0]
        );
    };
    assert_eq!(*operation, ProcessRecoveryOperation::SubmitRun);
}

/// A per-row deferral only reaches a host that reads the report, and every
/// in-tree caller discards it. The fault surface is the path that does not
/// depend on anyone reading a return value, so a failed ingress submit has to
/// arrive there too.
#[tokio::test]
async fn a_failed_ingress_submit_reports_a_worker_fault_to_the_sink() {
    let sink = RecordingProcessEventSink::default();
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("submit-fails-loudly"))
        .await
        .expect("register the row whose submit fails");

    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"ingress unavailable"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuation_store())
        .with_event_sink(Some(Arc::new(sink.clone())));
    let report = runner
        .claim_and_run_pending()
        .await
        .expect("a per-row submit failure does not fail the pass");
    server.await.expect("mock ingress server task");
    assert_eq!(report.deferred.len(), 1, "{report:?}");

    let faults = sink.faults.lock_recover().clone();
    assert_eq!(faults.len(), 1, "{faults:?}");
    let lash_core::facade_support::ProcessWorkerFault::RecoveryBackendError {
        process_id,
        operation,
        ..
    } = &faults[0]
    else {
        panic!("expected a recovery backend fault, got {:?}", faults[0]);
    };
    assert_eq!(process_id, "submit-fails-loudly");
    assert_eq!(*operation, ProcessRecoveryOperation::SubmitRun);
}
