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
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;
use std::time::Duration;

use lash_core::{
    ProcessAwaitOutput, ProcessExecutionContext, ProcessRegistration, Resolution, ResolveOutcome,
};
use restate_sdk::context::{
    Context as RestateContext, ContextAwakeables, ContextClient, InvocationHandle, ObjectContext,
    RequestTarget, RunRetryPolicy, SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, TerminalError};
use restate_sdk::serde::Json;
use serde::{Serialize, de::DeserializeOwned};

use crate::durable_wait::{
    RestateAwaitEventRaceOutcome, RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    RestateDurableWaitAwakeableKind, RestateDurableWaitAwakeableRequest,
    RestateDurableWaitRegistration, RestateDurableWaitResolveRequest, RestateProcessAwaitWake,
    RestateSleepRaceOutcome, RestateTurnAwaitEventWaitRequest, durable_wait_index_object_key,
};
use crate::process::{
    RestateProcessAwaitRequest, RestateProcessCancelRequest, RestateProcessWorkflowInput,
};

use super::RestateProcessAwaitRaceOutcome;

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

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(future) = self.future.as_mut() else {
            return Poll::Pending;
        };
        let tracker = Arc::new(SynchronousWakeTracker {
            parent: cx.waker().clone(),
            polling_thread: std::thread::current().id(),
            polling: AtomicBool::new(true),
            woke_during_poll: AtomicBool::new(false),
        });
        let tracked_waker = Waker::from(Arc::clone(&tracker));
        let mut tracked_context = Context::from_waker(&tracked_waker);
        let result = future.as_mut().poll(&mut tracked_context);
        tracker.polling.store(false, Ordering::Release);

        if result.is_ready() || tracker.woke_during_poll.load(Ordering::Acquire) {
            self.future = None;
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
    }
}
#[doc(hidden)]
pub trait RestateControllerContext<'ctx>: Send + Sync + 'ctx {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    /// Race a sleep against cancellation.
    ///
    /// Implementations backed by a real Restate SDK context MUST override this
    /// method and obey the SDK suspension protocol: journal the timer
    /// deterministically, fuse terminal wake-then-`Pending` futures, and poll
    /// the timer before cancellation. The default is only suitable for
    /// non-SDK test contexts.
    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<RestateSleepRaceOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async move {
            let Some(turn_cancel) = turn_cancel else {
                return tokio::select! {
                    result = self.sleep_send(duration) => {
                        result.map(|()| RestateSleepRaceOutcome::Slept)
                    }
                    _ = cancellation.cancelled() => Ok(RestateSleepRaceOutcome::Cancelled),
                };
            };
            tokio::select! {
                result = self.sleep_send(duration) => {
                    result.map(|()| RestateSleepRaceOutcome::Slept)
                }
                result = self.await_event(
                    turn_cancel,
                    tokio_util::sync::CancellationToken::new(),
                ) => {
                    result.map(|_| RestateSleepRaceOutcome::Cancelled)
                }
                _ = cancellation.cancelled() => Ok(RestateSleepRaceOutcome::Cancelled),
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
    ) -> Pin<
        Box<dyn Future<Output = Result<RestateAwaitEventRaceOutcome, TerminalError>> + Send + 'run>,
    >
    where
        'ctx: 'run,
    {
        Box::pin(async move {
            let Some(turn_cancel) = turn_cancel else {
                return self
                    .await_event(request, cancellation)
                    .await
                    .map(RestateAwaitEventRaceOutcome::Event);
            };
            tokio::select! {
                result = self.await_event(request, cancellation.clone()) => {
                    result.map(RestateAwaitEventRaceOutcome::Event)
                }
                result = self.await_event(
                    turn_cancel,
                    tokio_util::sync::CancellationToken::new(),
                ) => {
                    result.map(|_| RestateAwaitEventRaceOutcome::TurnCancelled)
                }
            }
        })
    }

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
    ///
    /// Implementations backed by a real Restate SDK context MUST override this
    /// method and obey the SDK suspension protocol. The default is only
    /// suitable for non-SDK test contexts.
    fn await_process_terminal_or_turn_cancel<'run>(
        &'run self,
        process_id: String,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<RestateProcessAwaitRaceOutcome, TerminalError>>
                + Send
                + 'run,
        >,
    >
    where
        'ctx: 'run,
    {
        let _ = turn_cancel;
        Box::pin(async move {
            self.await_process_terminal(process_id)
                .await
                .map(Box::new)
                .map(RestateProcessAwaitRaceOutcome::Terminal)
        })
    }

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
                ) -> Pin<Box<dyn Future<Output = Result<RestateSleepRaceOutcome, TerminalError>> + Send + 'run>>
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
                                        result.map(|()| RestateSleepRaceOutcome::Slept),
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
                                                RestateSleepRaceOutcome::Cancelled,
                                            ))
                                        }
                                        Poll::Pending => Poll::Pending,
                                    },
                                }
                            })
                            .await;
                        };

                        let Some(session_id) = turn_cancel.address.session_id.clone() else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        let (awakeable_id, awakeable) = self.awakeable::<Json<Resolution>>();
                        let registration_request = RestateDurableWaitAwakeableRequest {
                            address: turn_cancel.address,
                            awakeable_id,
                            kind: RestateDurableWaitAwakeableKind::default(),
                        };
                        let register: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitAwakeableRequest>,
                            Json<RestateDurableWaitRegistration>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id.clone(),
                                "register_awakeable",
                            ),
                            Json(registration_request.clone()),
                        );
                        let Json(registration) = register.call().await?;
                        if registration == RestateDurableWaitRegistration::Revoked {
                            return Ok(RestateSleepRaceOutcome::Cancelled);
                        }

                        let timer = restate_sdk::context::ContextTimers::sleep(self, duration);
                        restate_sdk::select! {
                            result = timer => {
                                result?;
                                let unregister: restate_sdk::context::Request<
                                    '_,
                                    Json<RestateDurableWaitAwakeableRequest>,
                                    Json<()>,
                                > = ContextClient::request(
                                    self,
                                    RequestTarget::object(
                                        "LashDurableWaitIndex",
                                        session_id,
                                        "unregister_awakeable",
                                    ),
                                    Json(registration_request),
                                );
                                let Json(()) = unregister.call().await?;
                                Ok(RestateSleepRaceOutcome::Slept)
                            },
                            result = awakeable => {
                                let _ = result?;
                                Ok(RestateSleepRaceOutcome::Cancelled)
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
                        let run = restate_sdk::context::ContextSideEffects::run(self, move || async move {
                            Ok::<Json<T>, HandlerError>(Json(future.await))
                        });
                        let run = restate_sdk::context::RunFuture::name(run, effect_name);
                        let run = match retry_policy {
                            Some(policy) => restate_sdk::context::RunFuture::retry_policy(run, policy),
                            None => run,
                        };
                        run.await
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
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessWorkflowInput>,
                        Json<ProcessAwaitOutput>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            workflow_key.clone(),
                            "run",
                        ),
                        Json(RestateProcessWorkflowInput {
                            registration,
                            execution_context,
                            segment_ordinal: 0,
                            execution_id: None,
                        }),
                    );
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
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCancelRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            workflow_key.clone(),
                            "cancel",
                        ),
                        Json(request),
                    );
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
                        let start: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitAwaitRequest>,
                            Json<Resolution>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::workflow(
                                "LashDurableWaitWorkflow",
                                request.address.workflow_key.clone(),
                                "await_resolution",
                            ),
                            Json(request.clone()),
                        );
                        let call = start.call();
                        restate_sdk::select! {
                            result = call => {
                                let Json(resolution) = result?;
                                Ok(resolution)
                            },
                            on_cancel => {
                                let address = request.address;
                                let target = RequestTarget::object(
                                    "LashDurableWaitIndex",
                                    durable_wait_index_object_key(&address),
                                    "resolve",
                                );
                                let resolve_request: restate_sdk::context::Request<
                                    '_,
                                    Json<RestateDurableWaitResolveRequest>,
                                    Json<ResolveOutcome>,
                                > = ContextClient::request(
                                    self,
                                    target,
                                    Json(RestateDurableWaitResolveRequest {
                                        address,
                                        resolution: Resolution::Cancelled,
                                    }),
                                );
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
                ) -> Pin<
                    Box<
                        dyn Future<Output = Result<RestateAwaitEventRaceOutcome, TerminalError>>
                            + Send
                            + 'run,
                    >,
                >
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            return self
                                .await_event(request, cancellation)
                                .await
                                .map(RestateAwaitEventRaceOutcome::Event);
                        };

                        let cancel_workflow_key = turn_cancel.address.workflow_key;
                        let race: restate_sdk::context::Request<
                            '_,
                            Json<RestateTurnAwaitEventWaitRequest>,
                            Json<RestateAwaitEventRaceOutcome>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::workflow(
                                "LashDurableWaitWorkflow",
                                cancel_workflow_key,
                                "await_event_or_turn_cancel",
                            ),
                            Json(RestateTurnAwaitEventWaitRequest { event: request }),
                        );
                        let Json(outcome) = race.call().await?;
                        Ok(outcome)
                    })
                }

                fn peek_event<'run>(
                    &'run self,
                    address: RestateDurableWaitAddress,
                ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request: restate_sdk::context::Request<
                        '_,
                        (),
                        Json<Option<Resolution>>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashDurableWaitWorkflow",
                            address.workflow_key,
                            "peek",
                        ),
                        (),
                    );
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
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessAwaitRequest>,
                        Json<ProcessAwaitOutput>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "await_terminal",
                        ),
                        Json(RestateProcessAwaitRequest { process_id }),
                    );
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
                ) -> Pin<
                    Box<
                        dyn Future<
                                Output = Result<RestateProcessAwaitRaceOutcome, TerminalError>,
                            > + Send
                            + 'run,
                    >,
                >
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            return self
                                .await_process_terminal(process_id)
                                .await
                                .map(Box::new)
                                .map(RestateProcessAwaitRaceOutcome::Terminal);
                        };
                        let Some(session_id) = turn_cancel.address.session_id.clone() else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        // `Request::call()` emits its CallCommand synchronously in
                        // Restate SDK 0.10. Construct this call first so a suspended
                        // pre-FIG-790 journal remains the exact prefix of every
                        // redrive after the cancellation adjudicator was added.
                        let process: restate_sdk::context::Request<
                            '_,
                            Json<RestateProcessAwaitRequest>,
                            Json<ProcessAwaitOutput>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::workflow(
                                "LashProcessWorkflow",
                                process_id.clone(),
                                "await_terminal",
                            ),
                            Json(RestateProcessAwaitRequest {
                                process_id: process_id.clone(),
                            }),
                        );
                        let process = process.call();
                        let (awakeable_id, awakeable) =
                            self.awakeable::<Json<RestateProcessAwaitWake>>();
                        let registration_request = RestateDurableWaitAwakeableRequest {
                            address: turn_cancel.address,
                            awakeable_id,
                            kind: RestateDurableWaitAwakeableKind::ProcessAwait,
                        };
                        let register: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitAwakeableRequest>,
                            Json<RestateDurableWaitRegistration>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id.clone(),
                                "register_awakeable",
                            ),
                            Json(registration_request.clone()),
                        );
                        let Json(registration) = register.call().await?;
                        if registration == RestateDurableWaitRegistration::Revoked {
                            tracing::info!(
                                target: "lash::restate",
                                event = "restate.process_await_adjudicated",
                                process_id = %process_id,
                                registration_state = "revoked",
                                winning_branch = "session_revoked",
                                "Restate process-await adjudication"
                            );
                            return Ok(RestateProcessAwaitRaceOutcome::SessionRevoked {
                                session_id,
                            });
                        }

                        restate_sdk::select! {
                            result = process => {
                                let Json(output) = result?;
                                let unregister: restate_sdk::context::Request<
                                    '_,
                                    Json<RestateDurableWaitAwakeableRequest>,
                                    Json<()>,
                                > = ContextClient::request(
                                    self,
                                    RequestTarget::object(
                                        "LashDurableWaitIndex",
                                        session_id,
                                        "unregister_awakeable",
                                    ),
                                    Json(registration_request),
                                );
                                let Json(()) = unregister.call().await?;
                                tracing::info!(
                                    target: "lash::restate",
                                    event = "restate.process_await_adjudicated",
                                    process_id = %process_id,
                                    registration_state = "registered",
                                    winning_branch = "process_terminal",
                                    "Restate process-await adjudication"
                                );
                                Ok(RestateProcessAwaitRaceOutcome::Terminal(Box::new(output)))
                            },
                            result = awakeable => {
                                let Json(wake) = result?;
                                match wake {
                                    RestateProcessAwaitWake::TurnCancelled => {
                                        tracing::info!(
                                            target: "lash::restate",
                                            event = "restate.process_await_adjudicated",
                                            process_id = %process_id,
                                            registration_state = "registered",
                                            winning_branch = "turn_cancelled",
                                            "Restate process-await adjudication"
                                        );
                                        Ok(RestateProcessAwaitRaceOutcome::TurnCancelled)
                                    }
                                    RestateProcessAwaitWake::SessionRevoked => {
                                        tracing::info!(
                                            target: "lash::restate",
                                            event = "restate.process_await_adjudicated",
                                            process_id = %process_id,
                                            registration_state = "registered_then_revoked",
                                            winning_branch = "session_revoked",
                                            "Restate process-await adjudication"
                                        );
                                        Ok(RestateProcessAwaitRaceOutcome::SessionRevoked {
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
                        let target = RequestTarget::object(
                            "LashDurableWaitIndex",
                            durable_wait_index_object_key(&request.address),
                            "resolve",
                        );
                        let resolve: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitResolveRequest>,
                            Json<ResolveOutcome>,
                        > = ContextClient::request(self, target, Json(request));
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
                    let handler = if revoke { "revoke_all" } else { "cancel_all" };
                    // Zero-input handlers require an empty payload; `()`
                    // serializes to empty bytes while `Json(())` would send a
                    // JSON `null` body that Restate's input validation rejects.
                    let request: restate_sdk::context::Request<'_, (), Json<()>> =
                        ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id,
                                handler,
                            ),
                            (),
                        );
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
                    let request: restate_sdk::context::Request<'_, Json<()>, Json<bool>> =
                        ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id,
                                "is_revoked",
                            ),
                            Json(()),
                        );
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
