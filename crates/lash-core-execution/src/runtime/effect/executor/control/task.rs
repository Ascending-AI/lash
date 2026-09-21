//! Proxying an effect controller across an owned channel: the task request
//! enum, the task-side controller and the driver loop.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

type EffectControllerTaskFuture<'run> = Pin<Box<dyn Future<Output = ()> + Send + 'run>>;

pub enum EffectControllerTaskRequest {
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
    AcquireQueuedLane {
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
        response: oneshot::Sender<Result<QueuedLaneAcquisition, RuntimeError>>,
    },
    PrepareCompletionKey {
        scope: ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
        response: oneshot::Sender<Result<CompletionKeyPreparation, RuntimeError>>,
    },
    RuntimeEffectFailureDisposition {
        code: RuntimeErrorCode,
        response: oneshot::Sender<Result<RuntimeEffectFailureDisposition, RuntimeError>>,
    },
    TurnControlParticipation {
        response: oneshot::Sender<Result<TurnControlParticipation, RuntimeError>>,
    },
}

impl EffectControllerTaskRequest {
    pub(super) fn into_future<'run>(
        self,
        controller: &'run dyn RuntimeEffectController,
    ) -> EffectControllerTaskFuture<'run> {
        match self {
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
                    controller.execute_effect(*envelope, *local_executor).await
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
            Self::AcquireQueuedLane {
                lane,
                cancel,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.acquire_queued_lane(lane, cancel).await);
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
            Self::RuntimeEffectFailureDisposition { code, response } => Box::pin(async move {
                let _ = response.send(controller.runtime_effect_failure_disposition(code).await);
            }),
            Self::TurnControlParticipation { response } => Box::pin(async move {
                let _ = response.send(controller.turn_control_participation().await);
            }),
        }
    }
}

pub(in crate::runtime::effect::executor) struct RemoteLocalExecutionRequest {
    pub(in crate::runtime::effect::executor) envelope: RuntimeEffectEnvelope,
    pub(in crate::runtime::effect::executor) response:
        oneshot::Sender<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
}

#[derive(Clone)]
pub struct EffectTaskController {
    requests: mpsc::UnboundedSender<EffectControllerTaskRequest>,
    scope: ExecutionScope,
    supports_concurrent_effects: bool,
    owns_commit_backpressure: bool,
    await_event_authority_binding_id: Option<String>,
}

impl EffectTaskController {
    pub fn scoped(
        controller: &dyn RuntimeEffectController,
        scope: ExecutionScope,
    ) -> Result<
        (
            ScopedEffectController<'static>,
            mpsc::UnboundedReceiver<EffectControllerTaskRequest>,
        ),
        RuntimeError,
    > {
        let (requests, request_rx) = mpsc::unbounded_channel();
        let proxy = Self {
            requests,
            scope: scope.clone(),
            supports_concurrent_effects: controller.supports_concurrent_effects(),
            owns_commit_backpressure: controller.owns_commit_backpressure(),
            await_event_authority_binding_id: controller.await_event_authority_binding_id(),
        };
        Ok((
            ScopedEffectController::shared(Arc::new(proxy), scope)?,
            request_rx,
        ))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for EffectTaskController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.await_event_authority_binding_id.clone()
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AcquireQueuedLane {
                lane,
                cancel,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "queued-lane controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "queued-lane controller response was dropped",
            )
        })?
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
}

#[async_trait::async_trait]
impl RuntimeEffectController for EffectTaskController {
    fn owns_commit_backpressure(&self) -> bool {
        self.owns_commit_backpressure
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.supports_concurrent_effects
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(
                EffectControllerTaskRequest::RuntimeEffectFailureDisposition {
                    code,
                    response: response_tx,
                },
            )
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "effect-failure disposition controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "effect-failure disposition controller response was dropped",
            )
        })?
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::TurnControlParticipation {
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "turn-control participation controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "turn-control participation controller response was dropped",
            )
        })?
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
                    let result = executor.execute_forwarded(request.envelope).await;
                    let _ = request.response.send(result);
                }
            }
        }
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        // Not forwarded, on purpose. This proxy relays execute_effect and
        // three other calls over an mpsc channel, so forwarding groups needs
        // new request variants that round-trip `&mut EffectGroupHandle` and a
        // long-lived cancellation token — a design change, not a delegation.
        // Until FIG-3415 does that, a controller reached through this proxy
        // has no groups, and now says so instead of inheriting a default that
        // looked identical to a host which had simply never considered them.
        Err(crate::effect_groups_unsupported("EffectTaskController"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("EffectTaskController"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("EffectTaskController"))
    }
}

pub async fn drive_effect_controller_task(
    controller: &dyn RuntimeEffectController,
    scope: ExecutionScope,
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'static>,
    mut requests: mpsc::UnboundedReceiver<EffectControllerTaskRequest>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    let (root_tx, root_rx) = oneshot::channel();
    let root = EffectControllerTaskRequest::Execute {
        scope,
        envelope: Box::new(envelope),
        local_executor: Box::new(local_executor),
        response: root_tx,
    };
    let mut stack = vec![root.into_future(controller)];
    let mut requests_open = true;
    tokio::pin!(root_rx);

    loop {
        let Some(active) = stack.last_mut() else {
            return root_rx.await.map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "root effect controller response was dropped",
                )
            })?;
        };
        tokio::select! {
            biased;
            response = &mut root_rx => {
                return response.map_err(|_| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                        "root effect controller response was dropped",
                    )
                })?;
            }
            () = active => {
                stack.pop();
            }
            request = async {
                if requests_open {
                    requests.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match request {
                    Some(request) => stack.push(request.into_future(controller)),
                    None => requests_open = false,
                }
            }
        }
    }
}
