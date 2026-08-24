//! The `RestateControllerContext` seam over Restate's context shapes.
//!
//! One responsibility: expose the durable primitives the controller needs
//! (timer, `ctx.run`, workflow scheduling, durable wait, awakeable races) over
//! every Restate context shape a handler can hold, and hold the SDK's
//! suspension protocol — including the one-shot fusing of a context future that
//! wakes synchronously and then returns `Pending`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;
use std::time::Duration;

use lash_core::{
    ProcessAwaitOutput, ProcessExecutionContext, ProcessRegistration, Resolution, ResolveOutcome,
};
use lash_sansio::sync::MutexExt;
use restate_sdk::context::{
    Context as RestateContext, ContextAwakeables, ContextClient, InvocationHandle, ObjectContext,
    RunRetryPolicy, SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, TerminalError};
use restate_sdk::serde::Json;
use serde::{Serialize, de::DeserializeOwned};

use crate::durable_wait::{
    LashDurableWaitIndexClient, LashDurableWaitWorkflowClient, RestateDurableWaitAddress,
    RestateDurableWaitAwaitRequest, RestateDurableWaitResolveRequest, RestateTurnCancelGate,
    RestateTurnCancelRaceOutcome, RestateTurnCancelWake, durable_wait_index_object_key,
    register_turn_cancel_gate, retire_turn_cancel_gate,
};
use crate::process::{
    LashProcessWorkflowClient, RestateProcessAwaitRequest, RestateProcessCancelRequest,
    RestateProcessWorkflowInput,
};

/// Fuse a Restate context future across both of its terminal poll shapes.
///
/// `DurableFutureImpl` returns `Ready` on success. When the SDK records a
/// terminal handler state (including a genuine suspension), it synchronously
/// wakes the task and returns `Pending`; the SDK's outer
/// `HandlerStateAwareFuture` consumes that state on the next poll. In both
/// shapes the SDK future has produced its terminal outcome for this attempt and
/// must never be polled again.
pub(crate) struct RestateContextFuture<F> {
    future: Option<Pin<Box<F>>>,
    /// The relay a guarded `ctx.run` closure's own future wakes through, when
    /// there is one. The guard publishes its parent waker here before each poll
    /// so those wakes bypass the tracker entirely and can never be mistaken for
    /// the SDK's terminal park.
    closure_relay: Option<Arc<ClosureWakeRelay>>,
    tracked: Option<TrackedWaker>,
}

/// The synchronous-wake tracker and the waker derived from it, kept together so
/// the pair can be lent to one poll and handed back without ever being half
/// present. The guard sits on the streaming-hot path, so the pair is reused
/// across polls and only rebuilt when the parent waker or the polling thread
/// changes - the tracker's verdict is scoped to one thread.
struct TrackedWaker {
    tracker: Arc<SynchronousWakeTracker>,
    waker: Waker,
}

impl TrackedWaker {
    fn new(parent: &Waker, polling_thread: ThreadId) -> Self {
        let tracker = Arc::new(SynchronousWakeTracker {
            parent: parent.clone(),
            polling_thread,
            polling: AtomicBool::new(false),
            woke_during_poll: AtomicBool::new(false),
        });
        let waker = Waker::from(Arc::clone(&tracker));
        Self { tracker, waker }
    }

    fn matches(&self, parent: &Waker, polling_thread: ThreadId) -> bool {
        self.tracker.parent.will_wake(parent) && self.tracker.polling_thread == polling_thread
    }

    fn begin_poll(&self) {
        self.tracker
            .woke_during_poll
            .store(false, Ordering::Release);
        self.tracker.polling.store(true, Ordering::Release);
    }

    /// End the poll and report whether the guarded future woke this task
    /// synchronously while it was being polled.
    fn end_poll(&self) -> bool {
        self.tracker.polling.store(false, Ordering::Release);
        self.tracker.woke_during_poll.load(Ordering::Acquire)
    }
}

/// The relay a `ctx.run` closure's own future wakes through.
///
/// The guarded `ctx.run` future polls arbitrary lash code inside the run
/// closure, and a wake from that code - `yield_now`, a `FuturesUnordered`
/// re-arm, a provider stream woken cross-thread by the I/O driver - is benign:
/// it means the closure has more work, not that the attempt is over. Such a
/// wake must therefore never reach the guard's synchronous-wake tracker.
///
/// Attribution is by construction rather than by arithmetic. The guard
/// publishes its own parent waker here before each poll, and the waker
/// [`relay_closure_wakes`] installs beneath the closure forwards straight to
/// that parent - so a closure wake bypasses the tracker whatever thread it
/// comes from and whenever it lands. Counting instead would be racy: a
/// cross-thread closure wake is invisible to the tracker's same-thread gate,
/// so subtracting it would cancel out the SDK's terminal park and leave the
/// guard unfused.
///
/// What the tracker still sees is exactly the wake the closure cannot account
/// for: the SDK recording a terminal handler state, including the synthetic
/// `wake_by_ref` `InterceptErrorFuture` issues after `ctx.fail`. That holds on
/// the replay path too, where the SDK never invokes the closure at all.
#[derive(Default)]
pub(crate) struct ClosureWakeRelay {
    /// The guard's parent waker, republished on every guard poll.
    parent: StdMutex<Option<Waker>>,
}

impl ClosureWakeRelay {
    /// Publish the waker the guard was polled with, so closure wakes reach the
    /// task without passing through the guard's tracker.
    fn publish_parent(&self, parent: &Waker) {
        let mut slot = self.parent.lock_recover();
        match slot.as_ref() {
            Some(existing) if existing.will_wake(parent) => {}
            _ => *slot = Some(parent.clone()),
        }
    }

    fn parent(&self) -> Option<Waker> {
        self.parent.lock_recover().clone()
    }
}

/// The waker handed to a run closure's future. It forwards to whatever parent
/// the guard last published, never to the guard's tracked waker.
struct RelayedWaker {
    relay: Arc<ClosureWakeRelay>,
    /// Used only before the guard's first poll has published a parent, which
    /// cannot happen while the closure is being polled by the guarded future.
    fallback: Waker,
}

impl RelayedWaker {
    fn relay_wake(&self) {
        match self.relay.parent() {
            Some(parent) => parent.wake(),
            None => self.fallback.wake_by_ref(),
        }
    }
}

impl Wake for RelayedWaker {
    fn wake(self: Arc<Self>) {
        self.relay_wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.relay_wake();
    }
}

/// Poll a run closure's future under a waker that relays its wakes past the
/// guard's tracker (see [`ClosureWakeRelay`]).
pub(crate) struct RelayedWakeFuture<F> {
    future: Pin<Box<F>>,
    relay: Arc<ClosureWakeRelay>,
    waker: Option<(Arc<RelayedWaker>, Waker)>,
}

impl<F> Future for RelayedWakeFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let (relayed, waker) = match this.waker.take() {
            Some((relayed, waker)) if relayed.fallback.will_wake(cx.waker()) => (relayed, waker),
            _ => {
                let relayed = Arc::new(RelayedWaker {
                    relay: Arc::clone(&this.relay),
                    fallback: cx.waker().clone(),
                });
                let waker = Waker::from(Arc::clone(&relayed));
                (relayed, waker)
            }
        };
        let result = this.future.as_mut().poll(&mut Context::from_waker(&waker));
        this.waker = Some((relayed, waker));
        result
    }
}

pub(crate) fn relay_closure_wakes<F>(
    future: F,
    relay: Arc<ClosureWakeRelay>,
) -> RelayedWakeFuture<F>
where
    F: Future,
{
    RelayedWakeFuture {
        future: Box::pin(future),
        relay,
        waker: None,
    }
}

impl<F> RestateContextFuture<F> {
    fn is_fused(&self) -> bool {
        self.future.is_none()
    }
}

impl<F> Future for RestateContextFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let polling_thread = std::thread::current().id();
        let tracked = match this.tracked.take() {
            Some(tracked) if tracked.matches(cx.waker(), polling_thread) => tracked,
            _ => TrackedWaker::new(cx.waker(), polling_thread),
        };
        let Some(future) = this.future.as_mut() else {
            return Poll::Pending;
        };
        // Publish this poll's parent waker before entering the guarded future,
        // so a wake from inside the run closure - on any thread, at any time -
        // reaches the task directly instead of registering as a synchronous
        // wake the closure cannot account for.
        if let Some(relay) = this.closure_relay.as_ref() {
            relay.publish_parent(cx.waker());
        }
        tracked.begin_poll();
        let result = future
            .as_mut()
            .poll(&mut Context::from_waker(&tracked.waker));
        // Every wake the tracker saw is the SDK's own: the closure's wakes were
        // routed past it by construction.
        let woke_during_poll = tracked.end_poll();

        if result.is_ready() || woke_during_poll {
            this.future = None;
        } else {
            this.tracked = Some(tracked);
        }
        result
    }
}

struct SynchronousWakeTracker {
    // Deliberately redundant: the Restate SDK also wakes the handler through
    // its output channel when it records suspension. Forwarding preserves the
    // ordinary Future/Waker contract for other synchronous wake paths, but the
    // suspension fix does not depend on this parent wake; the tracker flag is
    // what fuses the one-shot SDK future.
    parent: Waker,
    polling_thread: ThreadId,
    polling: AtomicBool,
    woke_during_poll: AtomicBool,
}

impl Wake for SynchronousWakeTracker {
    fn wake(self: Arc<Self>) {
        self.record_wake();
        self.parent.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.record_wake();
        self.parent.wake_by_ref();
    }
}

impl SynchronousWakeTracker {
    fn record_wake(&self) {
        if self.polling.load(Ordering::Acquire)
            && std::thread::current().id() == self.polling_thread
        {
            self.woke_during_poll.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn guard_restate_context_future<F>(future: F) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
        closure_relay: None,
        tracked: None,
    }
}

/// Guard a `ctx.run` future whose closure polls arbitrary lash code.
///
/// `closure_relay` is the relay the closure's own future wakes through (see
/// [`relay_closure_wakes`]). Those wakes are routed to the guard's parent waker
/// by construction, so a wake from inside the closure - on any thread - leaves
/// the run pollable, while any synchronous wake that does reach the tracker -
/// the SDK's terminal park, on a live attempt or on a replay that never invokes
/// the closure - fuses it.
pub(crate) fn guard_restate_run_future<F>(
    future: F,
    closure_relay: Arc<ClosureWakeRelay>,
) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
        closure_relay: Some(closure_relay),
        tracked: None,
    }
}
/// The future every turn-cancel race returns across this seam.
///
/// `T` is what the guarded wait produces when it wins: `()` for a timer, a
/// `Resolution` for an await-event, the process output for a process await.
type TurnCancelRaceFuture<'run, T> = Pin<
    Box<dyn Future<Output = Result<RestateTurnCancelRaceOutcome<T>, TerminalError>> + Send + 'run>,
>;

#[doc(hidden)]
pub trait RestateControllerContext<'ctx>: Send + Sync + 'ctx {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    /// Race a sleep against cancellation.
    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TurnCancelRaceFuture<'run, ()>
    where
        'ctx: 'run;

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run;

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> TurnCancelRaceFuture<'run, Resolution>
    where
        'ctx: 'run;

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_process_terminal<'run>(
        &'run self,
        process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    /// Race a process terminal wait against durable turn cancellation.
    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        process_id: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    ) -> TurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
    where
        'ctx: 'run;

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn update_session_waits<'run>(
        &'run self,
        session_id: String,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn session_is_revoked<'run>(
        &'run self,
        _session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(false) })
    }
}
macro_rules! impl_restate_controller_context {
    ($($context:ident),+ $(,)?) => {
        $(
            impl<'ctx> RestateControllerContext<'ctx> for $context<'ctx> {
                fn sleep_send<'run>(
                    &'run self,
                    duration: Duration,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        restate_sdk::context::ContextTimers::sleep(self, duration).await
                    })
                }

                fn sleep_or_turn_cancel<'run>(
                    &'run self,
                    duration: Duration,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    cancellation: tokio_util::sync::CancellationToken,
                ) -> TurnCancelRaceFuture<'run, ()>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            // `sleep()` journals `sys_sleep` synchronously at
                            // construction. Construct it unconditionally so
                            // every replay emits the same command.
                            let timer = guard_restate_context_future(
                                restate_sdk::context::ContextTimers::sleep(self, duration),
                            );
                            let cancelled = cancellation.cancelled();
                            tokio::pin!(timer);
                            tokio::pin!(cancelled);
                            return std::future::poll_fn(|cx| {
                                // Poll the timer first on every cycle. A
                                // recorded suspension fuses the timer and wins
                                // immediately; HandlerStateAwareFuture must
                                // consume it before cancellation is polled.
                                match timer.as_mut().poll(cx) {
                                    Poll::Ready(result) => Poll::Ready(
                                        result.map(RestateTurnCancelRaceOutcome::Completed),
                                    ),
                                    Poll::Pending if timer.as_ref().get_ref().is_fused() => {
                                        Poll::Pending
                                    }
                                    Poll::Pending => match cancelled.as_mut().poll(cx) {
                                        Poll::Ready(()) => {
                                            // The stray timer is harmless only
                                            // because its emission is
                                            // deterministic across attempts.
                                            // OutputCommand + End hides it when
                                            // reached, but a panic, crash, or
                                            // engine kill before then exposes
                                            // it to replay.
                                            Poll::Ready(Ok(
                                                RestateTurnCancelRaceOutcome::TurnCancelled,
                                            ))
                                        }
                                        Poll::Pending => Poll::Pending,
                                    },
                                }
                            })
                            .await;
                        };

                        let Some(session_id) =
                            turn_cancel.key.scope.session_id().map(str::to_string)
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        // Journal order is the deployed contract: the awakeable,
                        // then its registration, then the timer. The gate helpers
                        // own the two index calls; the call site keeps ownership of
                        // when each journaled command is emitted.
                        let (awakeable_id, awakeable) =
                            self.awakeable::<Json<RestateTurnCancelWake>>();
                        let gate = match register_turn_cancel_gate(
                            self,
                            &session_id,
                            turn_cancel.key,
                            awakeable_id,
                        )
                        .await?
                        {
                            RestateTurnCancelGate::Registered(gate) => gate,
                            RestateTurnCancelGate::Revoked => {
                                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                                    session_id,
                                });
                            }
                        };

                        let timer = restate_sdk::context::ContextTimers::sleep(self, duration);
                        restate_sdk::select! {
                            result = timer => {
                                result?;
                                retire_turn_cancel_gate(self, &session_id, gate).await?;
                                Ok(RestateTurnCancelRaceOutcome::Completed(()))
                            },
                            result = awakeable => {
                                let Json(wake) = result?;
                                Ok(match wake {
                                    RestateTurnCancelWake::TurnCancelled => {
                                        RestateTurnCancelRaceOutcome::TurnCancelled
                                    }
                                    RestateTurnCancelWake::SessionRevoked => {
                                        RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
                                    }
                                })
                            }
                        }
                    })
                }

                fn run_json_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    retry_policy: Option<RunRetryPolicy>,
                    future: Fut,
                ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                    T: Serialize + DeserializeOwned + Send + 'static,
                    Fut: Future<Output = T> + Send + 'run,
                {
                    Box::pin(async move {
                        // Wakes from the closure's own future are relayed straight
                        // to the guard's parent waker, so they never reach the
                        // guard's tracker and only the SDK's terminal park can
                        // fuse the run - on a live attempt and on a replay that
                        // never invokes the closure at all.
                        let closure_relay = Arc::new(ClosureWakeRelay::default());
                        let relay = Arc::clone(&closure_relay);
                        let run = restate_sdk::context::ContextSideEffects::run(self, move || async move {
                            let value = relay_closure_wakes(future, relay).await;
                            Ok::<Json<T>, HandlerError>(Json(value))
                        });
                        let run = restate_sdk::context::RunFuture::name(run, effect_name);
                        let run = match retry_policy {
                            Some(policy) => restate_sdk::context::RunFuture::retry_policy(run, policy),
                            None => run,
                        };
                        // An SDK-level run failure is terminal for this attempt:
                        // the SDK records the handler state, wakes
                        // synchronously and returns `Pending` so its outer
                        // `HandlerStateAwareFuture` can consume that state. The
                        // already-resolved run future must never be re-entered,
                        // and pollers above this seam (the turn event pump, the
                        // effect races) can poll their enclosing future again,
                        // so fuse it here rather than trusting every caller.
                        guard_restate_run_future(run, closure_relay).await
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
                    let workflow_key = registration.id.clone();
                    let request = self
                        .workflow_client::<LashProcessWorkflowClient>(workflow_key.clone())
                        .run(Json(RestateProcessWorkflowInput {
                            registration,
                            execution_context,
                            segment_ordinal: 0,
                            execution_id: None,
                        }));
                    let handle = request.send();
                    Box::pin(async move { handle.invocation_id().await })
                }

                fn request_process_workflow_cancel<'run>(
                    &'run self,
                    request: RestateProcessCancelRequest,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let workflow_key = request.process_id.clone();
                    let request = self
                        .workflow_client::<LashProcessWorkflowClient>(workflow_key.clone())
                        .cancel(Json(request));
                    let call = request.call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }

                fn await_event<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    _cancellation: tokio_util::sync::CancellationToken,
                ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let address = RestateDurableWaitAddress::for_key(&request.key);
                        let start = self
                            .workflow_client::<LashDurableWaitWorkflowClient>(
                                address.workflow_key.clone(),
                            )
                            .await_resolution(Json(request.clone()));
                        let call = start.call();
                        restate_sdk::select! {
                            result = call => {
                                let Json(resolution) = result?;
                                Ok(resolution)
                            },
                            on_cancel => {
                                let resolve_request = self
                                    .object_client::<LashDurableWaitIndexClient>(
                                        durable_wait_index_object_key(&address),
                                    )
                                    .resolve(Json(RestateDurableWaitResolveRequest {
                                        key: request.key,
                                        resolution: Resolution::Cancelled,
                                    }));
                                let Json(outcome) = resolve_request.call().await?;
                                Ok(match outcome {
                                    ResolveOutcome::AlreadyResolved { terminal } => terminal,
                                    ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked => {
                                        Resolution::Cancelled
                                    }
                                })
                            }
                        }
                    })
                }

                fn await_event_or_turn_cancel<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    cancellation: tokio_util::sync::CancellationToken,
                ) -> TurnCancelRaceFuture<'run, Resolution>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            return self
                                .await_event(request, cancellation)
                                .await
                                .map(RestateTurnCancelRaceOutcome::Completed);
                        };

                        let Some(session_id) =
                            turn_cancel.key.scope.session_id().map(str::to_string)
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        let event_address = RestateDurableWaitAddress::for_key(&request.key);
                        let event_key = request.key.clone();
                        // Same journal geometry as process await: the guarded
                        // wait's CallCommand is emitted first, then the gate's
                        // awakeable, then the registration.
                        let event = self
                            .workflow_client::<LashDurableWaitWorkflowClient>(
                                event_address.workflow_key.clone(),
                            )
                            .await_resolution(Json(request));
                        let event = event.call();
                        let (awakeable_id, awakeable) =
                            self.awakeable::<Json<RestateTurnCancelWake>>();
                        let gate = match register_turn_cancel_gate(
                            self,
                            &session_id,
                            turn_cancel.key,
                            awakeable_id,
                        )
                        .await?
                        {
                            RestateTurnCancelGate::Registered(gate) => gate,
                            RestateTurnCancelGate::Revoked => {
                                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                                    session_id,
                                });
                            }
                        };

                        restate_sdk::select! {
                            result = event => {
                                let Json(resolution) = result?;
                                retire_turn_cancel_gate(self, &session_id, gate).await?;
                                Ok(RestateTurnCancelRaceOutcome::Completed(resolution))
                            },
                            result = awakeable => {
                                let Json(wake) = result?;
                                match wake {
                                    RestateTurnCancelWake::TurnCancelled => {
                                        // Release the losing event wait. The
                                        // retired nested workflow did this from
                                        // its own journal; on the gate it is the
                                        // waiter's job, or the event workflow
                                        // stays parked with nobody left to
                                        // resolve it.
                                        let resolve = self
                                            .object_client::<LashDurableWaitIndexClient>(
                                                durable_wait_index_object_key(&event_address),
                                            )
                                            .resolve(Json(RestateDurableWaitResolveRequest {
                                                key: event_key,
                                                resolution: Resolution::Cancelled,
                                            }));
                                        let Json(_) = resolve.call().await?;
                                        Ok(RestateTurnCancelRaceOutcome::TurnCancelled)
                                    }
                                    RestateTurnCancelWake::SessionRevoked => {
                                        Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                                            session_id,
                                        })
                                    }
                                }
                            }
                        }
                    })
                }

                fn peek_event<'run>(
                    &'run self,
                    address: RestateDurableWaitAddress,
                ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request = self
                        .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key)
                        .peek();
                    Box::pin(async move {
                        let Json(resolution) = request.call().await?;
                        Ok(resolution)
                    })
                }

                fn await_process_terminal<'run>(
                    &'run self,
                    process_id: String,
                ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request = self
                        .workflow_client::<LashProcessWorkflowClient>(process_id.clone())
                        .await_terminal(Json(RestateProcessAwaitRequest { process_id }));
                    let call = request.call();
                    Box::pin(async move {
                        let Json(output) = call.await?;
                        Ok(output)
                    })
                }

                fn await_process_terminal_or_turn_cancel<'run>(
                    &'run self,
                    process_id: String,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                ) -> TurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            return self
                                .await_process_terminal(process_id)
                                .await
                                .map(Box::new)
                                .map(RestateTurnCancelRaceOutcome::Completed);
                        };
                        let Some(session_id) =
                            turn_cancel.key.scope.session_id().map(str::to_string)
                        else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        // `Request::call()` emits its CallCommand synchronously in
                        // Restate SDK 0.10. Construct this call first so a suspended
                        // pre-FIG-790 journal remains the exact prefix of every
                        // redrive after the cancellation adjudicator was added.
                        let process = self
                            .workflow_client::<LashProcessWorkflowClient>(process_id.clone())
                            .await_terminal(Json(RestateProcessAwaitRequest {
                                process_id: process_id.clone(),
                            }));
                        let process = process.call();
                        let (awakeable_id, awakeable) =
                            self.awakeable::<Json<RestateTurnCancelWake>>();
                        let gate = match register_turn_cancel_gate(
                            self,
                            &session_id,
                            turn_cancel.key,
                            awakeable_id,
                        )
                        .await?
                        {
                            RestateTurnCancelGate::Registered(gate) => gate,
                            RestateTurnCancelGate::Revoked => {
                                tracing::info!(
                                    target: "lash::restate",
                                    event = "restate.process_await_adjudicated",
                                    process_id = %process_id,
                                    registration_state = "revoked",
                                    winning_branch = "session_revoked",
                                    "Restate process-await adjudication"
                                );
                                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                                    session_id,
                                });
                            }
                        };

                        restate_sdk::select! {
                            result = process => {
                                let Json(output) = result?;
                                retire_turn_cancel_gate(self, &session_id, gate).await?;
                                tracing::info!(
                                    target: "lash::restate",
                                    event = "restate.process_await_adjudicated",
                                    process_id = %process_id,
                                    registration_state = "registered",
                                    winning_branch = "process_terminal",
                                    "Restate process-await adjudication"
                                );
                                Ok(RestateTurnCancelRaceOutcome::Completed(Box::new(output)))
                            },
                            result = awakeable => {
                                let Json(wake) = result?;
                                match wake {
                                    RestateTurnCancelWake::TurnCancelled => {
                                        tracing::info!(
                                            target: "lash::restate",
                                            event = "restate.process_await_adjudicated",
                                            process_id = %process_id,
                                            registration_state = "registered",
                                            winning_branch = "turn_cancelled",
                                            "Restate process-await adjudication"
                                        );
                                        Ok(RestateTurnCancelRaceOutcome::TurnCancelled)
                                    }
                                    RestateTurnCancelWake::SessionRevoked => {
                                        tracing::info!(
                                            target: "lash::restate",
                                            event = "restate.process_await_adjudicated",
                                            process_id = %process_id,
                                            registration_state = "registered_then_revoked",
                                            winning_branch = "session_revoked",
                                            "Restate process-await adjudication"
                                        );
                                        Ok(RestateTurnCancelRaceOutcome::SessionRevoked {
                                            session_id,
                                        })
                                    }
                                }
                            }
                        }
                    })
                }

                fn resolve_event<'run>(
                    &'run self,
                    request: RestateDurableWaitResolveRequest,
                ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let address = RestateDurableWaitAddress::for_key(&request.key);
                        let resolve = self
                            .object_client::<LashDurableWaitIndexClient>(
                                durable_wait_index_object_key(&address),
                            )
                            .resolve(Json(request));
                        let Json(outcome) = resolve.call().await?;
                        Ok(outcome)
                    })
                }

                fn update_session_waits<'run>(
                    &'run self,
                    session_id: String,
                    revoke: bool,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let client = self.object_client::<LashDurableWaitIndexClient>(session_id);
                    let request = if revoke {
                        client.revoke_all()
                    } else {
                        client.cancel_all()
                    };
                    let call = request.call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }

                fn session_is_revoked<'run>(
                    &'run self,
                    session_id: String,
                ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request = self
                        .object_client::<LashDurableWaitIndexClient>(session_id)
                        .is_revoked(Json(()));
                    let call = request.call();
                    Box::pin(async move {
                        let Json(revoked) = call.await?;
                        Ok(revoked)
                    })
                }
            }
        )+
    };
}

impl_restate_controller_context!(
    RestateContext,
    SharedObjectContext,
    ObjectContext,
    SharedWorkflowContext,
    WorkflowContext,
);
