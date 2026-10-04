//! Proxying an effect controller across an owned channel: the task request
//! enum, the task-side controller and the driver loop.
//!
//! Native Run steps stay borrowed by their caller. The owner journals an
//! owned relay and requests the step only when the journal needs execution;
//! a served record never invokes the borrowed step.

use super::*;

use std::task::Poll;

type EffectControllerTaskFuture<'run> = Pin<Box<dyn Future<Output = ()> + Send + 'run>>;

pub enum EffectControllerTaskRequest {
    RecordRun {
        name: String,
        step: RunRecordStep<'static>,
        schedule: bool,
        response:
            oneshot::Sender<Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError>>,
    },
    StartRunAttempt {
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'static>,
        response:
            oneshot::Sender<Result<crate::tool_run::RunAttemptEntry, RuntimeEffectControllerError>>,
    },
    StartRunRetry {
        backoff_ms: u64,
        response: oneshot::Sender<Result<(), RuntimeEffectControllerError>>,
    },
    ArmRunSource {
        descriptor: Box<crate::tool_run::SourceDescriptor>,
        response: oneshot::Sender<Result<(), RuntimeEffectControllerError>>,
    },
    AttachRunProcessTerminal {
        descriptor: Box<crate::tool_run::SourceDescriptor>,
        response: oneshot::Sender<Result<(), RuntimeEffectControllerError>>,
    },
    AwaitRunSources {
        subscriptions: Vec<crate::tool_run::SourceSubscription>,
        cancel: TurnCancelWait,
        response: oneshot::Sender<
            Result<(usize, crate::tool_run::SourceSeal), RuntimeEffectControllerError>,
        >,
    },
    CancelRunSource {
        descriptor: Box<crate::tool_run::SourceDescriptor>,
        response:
            oneshot::Sender<Result<crate::tool_run::SourceSeal, RuntimeEffectControllerError>>,
    },
    Execute {
        scope: ExecutionScope,
        envelope: Box<RuntimeEffectEnvelope>,
        local_executor: Box<RuntimeEffectLocalExecutor<'static>>,
        response: oneshot::Sender<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
    },
    AwaitEventKey {
        scope: ExecutionScope,
        wait: AwaitEventWaitIdentity,
        response: oneshot::Sender<Result<AwaitEventKey, RuntimeError>>,
    },
    ResolveAwaitEvent {
        key: AwaitEventKey,
        resolution: Resolution,
        response: oneshot::Sender<Result<ResolveOutcome, RuntimeError>>,
    },
    PublishAwaitEvent {
        key: AwaitEventKey,
        resolution: Resolution,
        response: oneshot::Sender<Result<Option<ResolveOutcome>, RuntimeError>>,
    },
    PrepareCompletionKey {
        scope: ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
        response: oneshot::Sender<Result<CompletionKeyPreparation, RuntimeError>>,
    },
    ReadRecordedJournal {
        range: crate::RecordedKeyRange,
        response: oneshot::Sender<Result<RecordedJournal, RuntimeEffectControllerError>>,
    },
}

impl EffectControllerTaskRequest {
    pub(crate) fn into_future<'run>(
        self,
        controller: &'run dyn RuntimeEffectController,
    ) -> EffectControllerTaskFuture<'run> {
        match self {
            Self::RecordRun {
                name,
                step,
                schedule,
                response,
            } => Box::pin(async move {
                let result = if schedule {
                    controller.record_run_schedule(name, step).await
                } else {
                    controller.record_run_record(name, step).await
                };
                let _ = response.send(result);
            }),
            Self::StartRunAttempt {
                name,
                step,
                response,
            } => {
                // Register in request order, before polling any response.
                let attempt = controller.start_run_attempt(name, step);
                Box::pin(async move {
                    let _ = response.send(attempt.await);
                })
            }
            Self::StartRunRetry {
                backoff_ms,
                response,
            } => {
                let timer = controller.start_run_retry(backoff_ms);
                Box::pin(async move {
                    let _ = response.send(timer.await);
                })
            }
            Self::ArmRunSource {
                descriptor,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.arm_run_source(*descriptor).await);
            }),
            Self::AttachRunProcessTerminal {
                descriptor,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.attach_run_process_terminal(*descriptor).await);
            }),
            Self::AwaitRunSources {
                subscriptions,
                cancel,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.await_run_sources(subscriptions, cancel).await);
            }),
            Self::CancelRunSource {
                descriptor,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.cancel_run_source(*descriptor).await);
            }),
            Self::Execute {
                scope,
                envelope,
                local_executor,
                response,
            } => Box::pin(async move {
                let result = if envelope.invocation.execution_scope() != &scope {
                    Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RuntimeEffectScopeMismatch,
                        format!(
                            "proxied effect address scope {:?} does not match admitted controller scope {scope:?}",
                            envelope.invocation.execution_scope()
                        ),
                    ))
                } else {
                    let publication = local_executor.plugin_state_session().map(|plugins| {
                        crate::plugin::EffectPublication::begin(
                            plugins,
                            envelope.invocation.address().clone(),
                        )
                    });
                    let result = controller.execute_effect(*envelope, *local_executor).await;
                    match publication {
                        Some(publication) => {
                            result.and_then(|outcome| publication.publish(outcome))
                        }
                        None => result,
                    }
                };
                let _ = response.send(result);
            }),
            Self::AwaitEventKey {
                scope,
                wait,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.await_event_key(&scope, wait).await);
            }),
            Self::ResolveAwaitEvent {
                key,
                resolution,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.resolve_await_event(&key, resolution).await);
            }),
            Self::PublishAwaitEvent {
                key,
                resolution,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.publish_await_event(&key, resolution).await);
            }),
            Self::PrepareCompletionKey {
                scope,
                wait,
                may_defer,
                response,
            } => Box::pin(async move {
                let _ = response.send(
                    controller
                        .prepare_completion_key(&scope, wait, may_defer)
                        .await,
                );
            }),
            Self::ReadRecordedJournal { range, response } => Box::pin(async move {
                let _ = response.send(controller.read_recorded_journal(&range).await);
            }),
        }
    }
}

pub(in crate::runtime::effect::executor) struct RemoteLocalExecutionRequest {
    pub(in crate::runtime::effect::executor) envelope: RuntimeEffectEnvelope,
    pub(in crate::runtime::effect::executor) effect_attempt: Option<crate::EffectAttempt>,
    pub(in crate::runtime::effect::executor) response:
        oneshot::Sender<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
}

type NativeRunStep<'step, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'step>>;
type NativeRunExecution<T> = oneshot::Receiver<oneshot::Sender<Result<T, String>>>;

fn native_run_task_closed(message: &str) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectControllerTaskClosed, message)
}

/// The owned journal closure contains no caller borrow. Only executing it
/// requests the borrowed body; replay drops it and serves the recorded answer.
fn native_run_step<T: Send + 'static>() -> (NativeRunStep<'static, T>, NativeRunExecution<T>) {
    let (execute_tx, execute_rx) = oneshot::channel();
    let step = Box::pin(async move {
        let (reply_tx, reply_rx) = oneshot::channel();
        execute_tx
            .send(reply_tx)
            .map_err(|_| "native Run step caller was dropped".to_owned())?;
        reply_rx
            .await
            .map_err(|_| "native Run step response was dropped".to_owned())?
    });
    (step, execute_rx)
}

async fn native_run_response<T>(
    response: oneshot::Receiver<Result<T, RuntimeEffectControllerError>>,
) -> Result<T, RuntimeEffectControllerError> {
    response
        .await
        .map_err(|_| native_run_task_closed("native Run controller response was dropped"))?
}

async fn finish_native_run_step<T: Send + 'static>(
    step: NativeRunStep<'_, T>,
    execute: NativeRunExecution<T>,
    mut response: oneshot::Receiver<Result<T, RuntimeEffectControllerError>>,
) -> Result<T, RuntimeEffectControllerError> {
    let execute = tokio::select! {
        result = &mut response => {
            return result.map_err(|_| native_run_task_closed("native Run controller response was dropped"))?;
        }
        execute = execute => execute,
    };
    if let Ok(reply) = execute {
        let _ = reply.send(step.await);
    }
    // A replay closes `execute` without asking for the body. Its recorded
    // response can arrive later; a closed body channel is not a task fault.
    native_run_response(response).await
}

#[derive(Clone)]
pub struct EffectTaskController {
    requests: mpsc::UnboundedSender<EffectControllerTaskRequest>,
    scope: ExecutionScope,
    owns_commit_backpressure: bool,
    hands_over_turns: bool,
    await_event_authority_binding_id: Option<String>,
    attempt_observation: Option<lash_trace::AttemptObservation>,
}

/// The request stream a scoped task controller's caller hands to
/// [`drive_effect_controller_task`]: the receiving end of the proxy's queue.
pub type EffectControllerTaskRequests = mpsc::UnboundedReceiver<EffectControllerTaskRequest>;

impl EffectTaskController {
    async fn record_native_run(
        &self,
        name: String,
        step: RunRecordStep<'_>,
        schedule: bool,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        let (remote, execute) = native_run_step();
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::RecordRun {
                name,
                step: remote,
                schedule,
                response: response_tx,
            })
            .map_err(|_| {
                native_run_task_closed("native Run controller task is no longer running")
            })?;
        finish_native_run_step(step, execute, response_rx).await
    }

    pub fn scoped(
        controller: &dyn RuntimeEffectController,
        admitted: AdmittedScope,
    ) -> Result<
        (
            ScopedEffectController<'static>,
            EffectControllerTaskRequests,
        ),
        RuntimeError,
    > {
        let (requests, request_rx) = mpsc::unbounded_channel();
        let proxy = Self {
            requests,
            scope: admitted.scope().clone(),
            owns_commit_backpressure: controller.owns_commit_backpressure(),
            hands_over_turns: controller.hands_over_turns(),
            await_event_authority_binding_id: controller.await_event_authority_binding_id(),
            attempt_observation: controller.attempt_observation(),
        };
        Ok((
            ScopedEffectController::shared(Arc::new(proxy), admitted)?,
            request_rx,
        ))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for EffectTaskController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.await_event_authority_binding_id.clone()
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::PrepareCompletionKey {
                scope: scope.clone(),
                wait,
                may_defer,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "completion-key controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "completion-key controller response was dropped",
            )
        })?
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AwaitEventKey {
                scope: scope.clone(),
                wait,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "await-event key controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "await-event key controller response was dropped",
            )
        })?
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::ResolveAwaitEvent {
                key: key.clone(),
                resolution,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "await-event resolution controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "await-event resolution controller response was dropped",
            )
        })?
    }

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::PublishAwaitEvent {
                key: key.clone(),
                resolution,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "await-event publication controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "await-event publication controller response was dropped",
            )
        })?
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for EffectTaskController {
    async fn record_run_record(
        &self,
        name: String,
        step: RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.record_native_run(name, step, false).await
    }

    async fn record_run_schedule(
        &self,
        name: String,
        step: RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.record_native_run(name, step, true).await
    }

    fn start_run_attempt<'run>(
        &'run self,
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        let (remote, execute) = native_run_step();
        let (response_tx, response_rx) = oneshot::channel();
        // Queue now: callers can register all X commands before awaiting one.
        if self
            .requests
            .send(EffectControllerTaskRequest::StartRunAttempt {
                name,
                step: remote,
                response: response_tx,
            })
            .is_err()
        {
            return Box::pin(async {
                Err(native_run_task_closed(
                    "native Run controller task is no longer running",
                ))
            });
        }
        Box::pin(finish_native_run_step(step, execute, response_rx))
    }

    fn start_run_retry(&self, backoff_ms: u64) -> crate::tool_dispatch::RunRetryTimer<'_> {
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .requests
            .send(EffectControllerTaskRequest::StartRunRetry {
                backoff_ms,
                response: response_tx,
            })
            .is_err()
        {
            return Box::pin(async {
                Err(native_run_task_closed(
                    "native Run controller task is no longer running",
                ))
            });
        }
        Box::pin(native_run_response(response_rx))
    }

    async fn arm_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::ArmRunSource {
                descriptor: Box::new(descriptor),
                response: response_tx,
            })
            .map_err(|_| {
                native_run_task_closed("native Run controller task is no longer running")
            })?;
        native_run_response(response_rx).await
    }

    async fn attach_run_process_terminal(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AttachRunProcessTerminal {
                descriptor: Box::new(descriptor),
                response: response_tx,
            })
            .map_err(|_| {
                native_run_task_closed("native Run controller task is no longer running")
            })?;
        native_run_response(response_rx).await
    }

    async fn await_run_sources(
        &self,
        subscriptions: Vec<crate::tool_run::SourceSubscription>,
        cancel: TurnCancelWait,
    ) -> Result<(usize, crate::tool_run::SourceSeal), RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AwaitRunSources {
                subscriptions,
                cancel,
                response: response_tx,
            })
            .map_err(|_| {
                native_run_task_closed("native Run controller task is no longer running")
            })?;
        native_run_response(response_rx).await
    }

    async fn cancel_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<crate::tool_run::SourceSeal, RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::CancelRunSource {
                descriptor: Box::new(descriptor),
                response: response_tx,
            })
            .map_err(|_| {
                native_run_task_closed("native Run controller task is no longer running")
            })?;
        native_run_response(response_rx).await
    }

    fn owns_commit_backpressure(&self) -> bool {
        self.owns_commit_backpressure
    }

    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        self.attempt_observation.clone()
    }

    fn hands_over_turns(&self) -> bool {
        self.hands_over_turns
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if envelope.invocation.execution_scope() != &self.scope {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectScopeMismatch,
                format!(
                    "proxied effect address scope {:?} does not match admitted controller scope {:?}",
                    envelope.invocation.execution_scope(),
                    self.scope
                ),
            ));
        }
        let (local_executor, mut local_execution) = local_executor.into_remote_execution();
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::Execute {
                scope: self.scope.clone(),
                envelope: Box::new(envelope),
                local_executor: Box::new(local_executor),
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "effect controller task is no longer running",
                )
            })?;

        tokio::pin!(response_rx);
        loop {
            tokio::select! {
                response = &mut response_rx => {
                    return response.map_err(|_| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                            "effect controller response was dropped",
                        )
                    })?;
                }
                request = async {
                    match local_execution.as_mut() {
                        Some((_, requests)) => requests.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(request) = request else {
                        // Replay-aware controllers may return a recorded
                        // outcome without invoking local execution. Dropping
                        // the remote executor closes this channel by design;
                        // keep waiting for the controller response.
                        local_execution = None;
                        continue;
                    };
                    let Some((executor, _)) = local_execution.take() else {
                        unreachable!("local execution request requires a local executor");
                    };
                    let result = executor
                        .execute_forwarded(request.envelope, request.effect_attempt)
                        .await;
                    let _ = request.response.send(result);
                }
            }
        }
    }

    async fn read_recorded_journal(
        &self,
        range: &crate::RecordedKeyRange,
    ) -> Result<RecordedJournal, RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::ReadRecordedJournal {
                range: range.clone(),
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "recorded-journal controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "recorded-journal controller response was dropped",
            )
        })?
    }
}

pub async fn drive_effect_controller_task(
    controller: &dyn RuntimeEffectController,
    scope: ExecutionScope,
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'static>,
    mut requests: EffectControllerTaskRequests,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    let (root_tx, root_rx) = oneshot::channel();
    let root = EffectControllerTaskRequest::Execute {
        scope,
        envelope: Box::new(envelope),
        local_executor: Box::new(local_executor),
        response: root_tx,
    };
    // Every in-flight request is polled, not just the newest: a suspended
    // frame can hold a store transaction's locks, and burying it under a newer
    // request that needs the same lock deadlocks the task — the root effect's
    // parked-wait claim suspended mid-transaction while a `ResolveAwaitEvent`
    // needed its scope lock was exactly that shape. Requests are independent
    // RPCs whose callers already await their own response, so progress under
    // the run is free.
    //
    // Each in-flight request is polled at most once per poll of this task. A
    // host future may wake its task and park to hand a terminal state to an
    // enclosing future that is only consulted on the task's next poll: the
    // Restate SDK does exactly that when it records a suspension, and polling
    // the same future again before the task re-polls from the top resumes an
    // SDK future that has already completed. A `FuturesUnordered` re-polls a
    // self-woken future inside one `poll_next`, and a loop that re-enters the
    // poll after another request settles does the same, so neither is used
    // here: one pass polls every request once and returns.
    let mut root_rx = root_rx;
    let mut in_flight = vec![root.into_future(controller)];
    let mut requests_open = true;
    let root_response = |response: Result<
        Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
        oneshot::error::RecvError,
    >| {
        response
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "root effect controller response was dropped",
                )
            })
            .and_then(|outcome| outcome)
    };
    std::future::poll_fn(|cx| {
        if let Poll::Ready(response) = Pin::new(&mut root_rx).poll(cx) {
            return Poll::Ready(root_response(response));
        }
        while requests_open {
            match requests.poll_recv(cx) {
                Poll::Ready(Some(request)) => in_flight.push(request.into_future(controller)),
                Poll::Ready(None) => requests_open = false,
                Poll::Pending => break,
            }
        }
        in_flight.retain_mut(|request| request.as_mut().poll(cx).is_pending());
        match Pin::new(&mut root_rx).poll(cx) {
            Poll::Ready(response) => Poll::Ready(root_response(response)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Run `body` as the owner of every effect it issues (K8, binding Q2): the
/// body gets a `'static` proxy of `controller`, bound to `admitted`, and
/// every request it sends is served here, on the invocation `controller`
/// journals in, so that invocation is the one journal owner of the body's
/// work.
///
/// The owner is a Run lifecycle. While the body runs it is `Live` and serves
/// every request the body issues. Once the body returned (its explicit
/// completion) it is `Closing`: it admits nothing new, so a request sent
/// after it is refused [`RuntimeEffectControllerTaskClosed`], and it keeps
/// serving every request issued before it until each has settled. Only then
/// is it `Settled` and the body's output returned: the owner never finishes
/// while it still owns live work. An aborted body (its cancellation token
/// fired) is a body that returned: its issued work drains the same way.
///
/// Requests are polled as [`drive_effect_controller_task`] polls them: every
/// in-flight request once per poll of the owner, never re-entered within one.
///
/// [`RuntimeEffectControllerTaskClosed`]: crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed
pub async fn own_effect_controller_task<Body, Work, T>(
    controller: &dyn RuntimeEffectController,
    admitted: AdmittedScope,
    body: Body,
) -> Result<T, RuntimeError>
where
    Body: FnOnce(ScopedEffectController<'static>) -> Work,
    Work: Future<Output = T>,
{
    let (proxy, mut requests) = EffectTaskController::scoped(controller, admitted)?;
    let mut body = std::pin::pin!(body(proxy));
    let mut lifecycle = lash_core_store::tool_run::RunLifecycle::Live;
    let mut output = None;
    let mut in_flight: Vec<EffectControllerTaskFuture<'_>> = Vec::new();
    let mut requests_open = true;
    std::future::poll_fn(|cx| {
        if lifecycle == lash_core_store::tool_run::RunLifecycle::Live
            && let Poll::Ready(value) = body.as_mut().poll(cx)
        {
            output = Some(value);
            // Explicit completion: no new request is admitted, and the
            // ones already queued are still the owner's to serve.
            requests.close();
            lifecycle = lash_core_store::tool_run::RunLifecycle::Closing;
        }
        while requests_open {
            match requests.poll_recv(cx) {
                Poll::Ready(Some(request)) => in_flight.push(request.into_future(controller)),
                Poll::Ready(None) => requests_open = false,
                Poll::Pending => break,
            }
        }
        in_flight.retain_mut(|request| request.as_mut().poll(cx).is_pending());
        if lifecycle == lash_core_store::tool_run::RunLifecycle::Closing
            && !requests_open
            && in_flight.is_empty()
        {
            lifecycle = lash_core_store::tool_run::RunLifecycle::Settled;
            return Poll::Ready(output.take());
        }
        Poll::Pending
    })
    .await
    .ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
            "the effect owner settled without its body's output",
        )
    })
}
