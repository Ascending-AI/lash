//! Proxying an effect controller across an owned channel: the task request
//! enum, the task-side controller and the driver loop.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

use futures_util::stream::{FuturesUnordered, StreamExt};

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
    OpenEffectGroup {
        scope: ExecutionScope,
        group: Box<RuntimeEffectGroup>,
        response: oneshot::Sender<Result<EffectGroupHandle, RuntimeEffectControllerError>>,
    },
    AwaitNextSettlement {
        handle: EffectGroupHandle,
        cancel: CancellationToken,
        response: oneshot::Sender<(
            EffectGroupHandle,
            Result<GroupSettlement, RuntimeEffectControllerError>,
        )>,
    },
    CloseEffectGroup {
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
        response: oneshot::Sender<Result<(), RuntimeEffectControllerError>>,
    },
    ReadGroupSettlement {
        group_key: String,
        rank: u64,
        response: oneshot::Sender<
            Result<
                Option<crate::runtime::effect::RankedGroupSettlement>,
                RuntimeEffectControllerError,
            >,
        >,
    },
    CommitGroupChildFinal {
        commit: crate::runtime::effect::group_journal::GroupChildFinalCommit,
        response: oneshot::Sender<
            Result<
                crate::runtime::effect::group_journal::EffectGroupChildCommitOutcome,
                RuntimeEffectControllerError,
            >,
        >,
    },
    AwaitGroupChildDrainAdmission {
        group_key: String,
        commit_seq: u64,
        response: oneshot::Sender<Result<(), RuntimeEffectControllerError>>,
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
            Self::OpenEffectGroup {
                scope,
                group,
                response,
            } => Box::pin(async move {
                let result = match group.validate_execution_scope(&scope) {
                    Ok(()) => controller.open_effect_group(*group).await,
                    Err(error) => Err(error),
                };
                let _ = response.send(result);
            }),
            Self::AwaitNextSettlement {
                mut handle,
                cancel,
                response,
            } => Box::pin(async move {
                let result = controller.await_next_settlement(&mut handle, cancel).await;
                // The cursor of record rides back with the outcome: whatever
                // the controller advanced is exactly what the caller's handle
                // becomes — nothing on a refusal or a cancellation.
                let _ = response.send((handle, result));
            }),
            Self::CloseEffectGroup {
                handle,
                disposition,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.close_effect_group(handle, disposition).await);
            }),
            Self::ReadGroupSettlement {
                group_key,
                rank,
                response,
            } => Box::pin(async move {
                let _ = response.send(controller.read_group_settlement(&group_key, rank).await);
            }),
            Self::CommitGroupChildFinal { commit, response } => Box::pin(async move {
                let _ = response.send(controller.commit_group_child_final(commit).await);
            }),
            Self::AwaitGroupChildDrainAdmission {
                group_key,
                commit_seq,
                response,
            } => Box::pin(async move {
                let _ = response.send(
                    controller
                        .await_group_child_drain_admission(&group_key, commit_seq)
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
    pub(in crate::runtime::effect::executor) response:
        oneshot::Sender<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
}

#[derive(Clone)]
pub struct EffectTaskController {
    requests: mpsc::UnboundedSender<EffectControllerTaskRequest>,
    scope: ExecutionScope,
    owns_commit_backpressure: bool,
    effect_journaling: EffectJournaling,
    await_event_authority_binding_id: Option<String>,
}

impl EffectTaskController {
    pub fn scoped(
        controller: &dyn RuntimeEffectController,
        admitted: AdmittedScope,
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
            scope: admitted.scope().clone(),
            owns_commit_backpressure: controller.owns_commit_backpressure(),
            effect_journaling: controller.effect_journaling(),
            await_event_authority_binding_id: controller.await_event_authority_binding_id(),
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

    fn effect_journaling(&self) -> EffectJournaling {
        self.effect_journaling
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
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        group.validate_execution_scope(&self.scope)?;
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::OpenEffectGroup {
                scope: self.scope.clone(),
                group: Box::new(group),
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "group-open controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "group-open controller response was dropped",
            )
        })?
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        // A `&mut` cannot ride the channel, so the cursor travels as a copy:
        // the task side advances its own handle and returns it with the
        // outcome, and this handle — still the sole cursor of record — is
        // written back to exactly what the controller delivered. A send or
        // response failure therefore leaves this handle untouched, matching a
        // cancelled await.
        let cursor = EffectGroupHandle::restored(
            handle.group_key().to_string(),
            handle.children(),
            handle.consumed(),
        )?;
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AwaitNextSettlement {
                handle: cursor,
                cancel,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "group-settlement controller task is no longer running",
                )
            })?;
        let (returned, result) = response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "group-settlement controller response was dropped",
            )
        })?;
        *handle = returned;
        result
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::CloseEffectGroup {
                handle,
                disposition,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "group-close controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "group-close controller response was dropped",
            )
        })?
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<crate::runtime::effect::RankedGroupSettlement>, RuntimeEffectControllerError>
    {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::ReadGroupSettlement {
                group_key: group_key.to_string(),
                rank,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "group-settlement-read controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "group-settlement-read controller response was dropped",
            )
        })?
    }

    async fn commit_group_child_final(
        &self,
        commit: crate::runtime::effect::group_journal::GroupChildFinalCommit,
    ) -> Result<
        crate::runtime::effect::group_journal::EffectGroupChildCommitOutcome,
        RuntimeEffectControllerError,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::CommitGroupChildFinal {
                commit,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "group-child commit controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "group-child commit controller response was dropped",
            )
        })?
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(EffectControllerTaskRequest::AwaitGroupChildDrainAdmission {
                group_key: group_key.to_string(),
                commit_seq,
                response: response_tx,
            })
            .map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "drain-barrier controller task is no longer running",
                )
            })?;
        response_rx.await.map_err(|_| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                "drain-barrier controller response was dropped",
            )
        })?
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
    mut requests: mpsc::UnboundedReceiver<EffectControllerTaskRequest>,
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
    // the root is free.
    let mut in_flight = FuturesUnordered::new();
    in_flight.push(root.into_future(controller));
    let mut requests_open = true;
    tokio::pin!(root_rx);

    loop {
        if in_flight.is_empty() {
            return root_rx.await.map_err(|_| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
                    "root effect controller response was dropped",
                )
            })?;
        }
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
            _ = in_flight.next() => {}
            request = async {
                if requests_open {
                    requests.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match request {
                    Some(request) => in_flight.push(request.into_future(controller)),
                    None => requests_open = false,
                }
            }
        }
    }
}
