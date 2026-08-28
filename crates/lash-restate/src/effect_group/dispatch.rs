#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupDispatchRequest {
    pub group_key: String,
    pub shape: EffectGroupShape,
    pub children: Vec<RuntimeEffectEnvelope>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupChildRequest {
    pub group_key: String,
    pub shape: EffectGroupShape,
    pub position: usize,
    pub envelope: RuntimeEffectEnvelope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EffectGroupChildRunOutcome {
    Completed {
        outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    },
    Cancelled,
}

#[derive(Clone)]
pub struct EffectGroupDispatch {
    executors: Arc<dyn GroupExecutors>,
    ingress: RestateIngressClient,
    infinite_retry_policy: RunRetryPolicy,
}

impl std::fmt::Debug for EffectGroupDispatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EffectGroupDispatch")
            .field("infinite_retry_policy", &self.infinite_retry_policy)
            .finish_non_exhaustive()
    }
}

#[restate_sdk::workflow(name = "EffectGroupDispatch")]
impl EffectGroupDispatch {
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<EffectGroupDispatchRequest>,
    ) -> HandlerResult<Json<()>> {
        request.shape.validate_wire()?;
        let own_id = ctx.invocation_id().to_string();
        let Json(adopted) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .probe_and_adopt(Json(EffectGroupAdoptRequest {
                invocation_id: own_id,
            }))
            .call()
            .await?;
        match adopted {
            EffectGroupProbeAdoptResponse::Adopted
            | EffectGroupProbeAdoptResponse::AlreadyAdopted => {}
            EffectGroupProbeAdoptResponse::Ready
            | EffectGroupProbeAdoptResponse::Closed
            | EffectGroupProbeAdoptResponse::Retired => return Ok(Json(())),
            EffectGroupProbeAdoptResponse::DifferentDispatcher
            | EffectGroupProbeAdoptResponse::UnknownGroup => {
                return Err(TerminalError::new(format!(
                    "effect-group dispatcher protocol defect for {}: {adopted:?}",
                    request.group_key
                ))
                .into());
            }
        }

        let executors = Arc::clone(&self.executors);
        let preflight_children = request.children.clone();
        let Json(missing) = ctx
            .run(move || async move {
                let mut missing = None;
                for (position, child) in preflight_children.iter().enumerate() {
                    if executors.executor_for(child).is_none() && missing.is_none() {
                        missing = Some(position);
                    }
                }
                Ok(Json(missing))
            })
            .name("lash:effect-group:dispatch-preflight")
            .retry_policy(self.infinite_retry_policy.clone())
            .await?;
        if let Some(position) = missing {
            let Json(outcome) = ctx
                .object_client::<EffectGroupIndexClient>(request.group_key.clone())
                .register_refusal(Json(EffectGroupRefusalRequest {
                    reason: EffectGroupRefusal::NoExecutor { position },
                }))
                .call()
                .await?;
            return match outcome {
                EffectGroupRegisterRefusalResponse::Refused
                | EffectGroupRegisterRefusalResponse::AlreadyRegistered
                | EffectGroupRegisterRefusalResponse::AlreadyClosed
                | EffectGroupRegisterRefusalResponse::Retired => Ok(Json(())),
                EffectGroupRegisterRefusalResponse::UnknownGroup => {
                    Err(TerminalError::new(format!(
                        "dispatcher refused unknown effect group {}",
                        request.group_key
                    ))
                    .into())
                }
            };
        }

        let mut addresses = BTreeMap::new();
        for (position, envelope) in request.children.into_iter().enumerate() {
            let handle = ctx
                .workflow_client::<EffectGroupDispatchClient>(request.group_key.clone())
                .child(Json(EffectGroupChildRequest {
                    group_key: request.group_key.clone(),
                    shape: request.shape.clone(),
                    position,
                    envelope,
                }))
                .send()
                .await?;
            let invocation_id = handle.invocation_id().to_owned();
            let Json(recorded) = ctx
                .object_client::<EffectGroupIndexClient>(request.group_key.clone())
                .record_dispatch(Json(EffectGroupRecordDispatchRequest {
                    position,
                    invocation_id: invocation_id.clone(),
                }))
                .call()
                .await?;
            match recorded {
                EffectGroupRecordDispatchResponse::Recorded
                | EffectGroupRecordDispatchResponse::Duplicate => {}
                EffectGroupRecordDispatchResponse::Retired => return Ok(Json(())),
                other => {
                    return Err(TerminalError::new(format!(
                        "record dispatch protocol defect for {} child {position}: {other:?}",
                        request.group_key
                    ))
                    .into());
                }
            }
            addresses.insert(position, invocation_id);
        }
        let Json(registered) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .register_children(Json(EffectGroupRegisterRequest { addresses }))
            .call()
            .await?;
        match registered {
            EffectGroupRegisterResponse::Registered
            | EffectGroupRegisterResponse::AlreadyRegistered
            | EffectGroupRegisterResponse::AlreadyClosed
            | EffectGroupRegisterResponse::Retired => Ok(Json(())),
            other => Err(TerminalError::new(format!(
                "register children protocol defect for {}: {other:?}",
                request.group_key
            ))
            .into()),
        }
    }

    #[handler]
    async fn preflight(
        &self,
        _ctx: SharedWorkflowContext<'_>,
        Json(children): Json<Vec<RuntimeEffectEnvelope>>,
    ) -> HandlerResult<Json<Option<usize>>> {
        Ok(Json(children.iter().position(|child| {
            self.executors.executor_for(child).is_none()
        })))
    }

    #[handler]
    async fn child(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<EffectGroupChildRequest>,
    ) -> HandlerResult<Json<()>> {
        request.shape.validate_wire()?;
        let own_id = ctx.invocation_id().to_string();
        let admission_request = EffectGroupAdmissionRequest {
            position: request.position,
            invocation_id: own_id,
        };
        let Json(first) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .admit_child(Json(admission_request.clone()))
            .call()
            .await?;
        let admission = match first {
            EffectGroupAdmissionResponse::Admitted => EffectGroupAdmissionResponse::Admitted,
            EffectGroupAdmissionResponse::Refused | EffectGroupAdmissionResponse::Retired => {
                return Ok(Json(()));
            }
            EffectGroupAdmissionResponse::NotYetRecorded => {
                let key = group_wait_key(
                    &request.shape.wait_scope,
                    &request.group_key,
                    EffectGroupWaitKind::Admit(request.position),
                )?;
                let address = RestateDurableWaitAddress::for_key(&key);
                let Json(_) = ctx
                    .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key)
                    .await_resolution(Json(RestateDurableWaitAwaitRequest {
                        key,
                        timeout_ms: None,
                    }))
                    .call()
                    .await?;
                // ADMIT is notification only. Authorization always comes from
                // this one fresh, mapping-exact call after the wake.
                let Json(fresh) = ctx
                    .object_client::<EffectGroupIndexClient>(request.group_key.clone())
                    .admit_child(Json(admission_request))
                    .call()
                    .await?;
                fresh
            }
        };
        match admission {
            EffectGroupAdmissionResponse::Admitted => {}
            EffectGroupAdmissionResponse::Refused | EffectGroupAdmissionResponse::Retired => {
                return Ok(Json(()));
            }
            EffectGroupAdmissionResponse::NotYetRecorded => {
                return Err(TerminalError::new(format!(
                    "ADMIT notification for {} child {} did not produce a decisive fresh admission",
                    request.group_key, request.position
                ))
                .into());
            }
        }

        let cancellation = tokio_util::sync::CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let envelope = request.envelope.clone();
        let executors = Arc::clone(&self.executors);
        let group_key = request.group_key.clone();
        let position = request.position;
        let mut run = Box::pin(
            ctx.run(move || async move {
                let Some(executor) = executors.executor_for(&envelope) else {
                    return Err(std::io::Error::other(format!(
                        "no executor currently routes effect group {group_key} child {position}; retry on a carrying deployment"
                    ))
                    .into());
                };
                let outcome = tokio::select! {
                    biased;
                    _ = run_cancellation.cancelled() => EffectGroupChildRunOutcome::Cancelled,
                    outcome = executor.execute(envelope) => {
                        EffectGroupChildRunOutcome::Completed { outcome }
                    }
                };
                Ok(Json(outcome))
            })
            .name(format!(
                "lash:effect-group:{}:{}",
                request.group_key, request.position
            ))
            .retry_policy(self.infinite_retry_policy.clone()),
        );
        let cancel_key = group_wait_key(
            &request.shape.wait_scope,
            &request.group_key,
            EffectGroupWaitKind::Cancel(request.shape.replay_key(request.position)?),
        )?;
        let cancel_address = RestateDurableWaitAddress::for_key(&cancel_key);
        let cancel_request = RestateDurableWaitAwaitRequest {
            key: cancel_key,
            timeout_ms: None,
        };
        let cancel_watch = self.ingress.call_workflow_json::<_, Resolution>(
            "LashDurableWaitWorkflow",
            &cancel_address.workflow_key,
            "await_resolution",
            &cancel_request,
        );
        tokio::pin!(cancel_watch);
        let Json(outcome) = tokio::select! {
            biased;
            cancel = &mut cancel_watch => {
                cancel.map_err(|error| std::io::Error::other(format!(
                    "observe effect-group child cancellation: {error}"
                )))?;
                cancellation.cancel();
                run.await?
            }
            outcome = &mut run => outcome?,
        };

        let terminal = match outcome {
            EffectGroupChildRunOutcome::Cancelled => EffectGroupSettlementTerminal::Cancelled,
            EffectGroupChildRunOutcome::Completed {
                outcome: Err(error),
            } => EffectGroupSettlementTerminal::Failed { error },
            EffectGroupChildRunOutcome::Completed {
                outcome: Ok(outcome),
            } => {
                let bytes = serde_json::to_vec(&outcome).map_err(|error| {
                    TerminalError::new(format!(
                        "serialize effect group {} child {} outcome: {error}",
                        request.group_key, request.position
                    ))
                })?;
                let Json(put) = ctx
                    .object_client::<EffectGroupPayloadClient>(payload_key(
                        &request.group_key,
                        request.position,
                    ))
                    .put(Json(EffectGroupPayloadPutRequest { bytes }))
                    .call()
                    .await?;
                match put {
                    EffectGroupPayloadPutResponse::Written
                    | EffectGroupPayloadPutResponse::Duplicate => {
                        EffectGroupSettlementTerminal::StoredPayload
                    }
                    EffectGroupPayloadPutResponse::Retired => return Ok(Json(())),
                    EffectGroupPayloadPutResponse::Conflict => {
                        return Err(TerminalError::new(format!(
                            "payload byte fence conflict for effect group {} child {}",
                            request.group_key, request.position
                        ))
                        .into());
                    }
                }
            }
        };
        let Json(recorded) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .record_settlement(Json(EffectGroupRecordSettlementRequest {
                position: request.position,
                terminal,
            }))
            .call()
            .await?;
        match recorded {
            EffectGroupRecordSettlementResponse::Recorded { .. }
            | EffectGroupRecordSettlementResponse::Duplicate { .. }
            | EffectGroupRecordSettlementResponse::Retired => Ok(Json(())),
            other => Err(TerminalError::new(format!(
                "record settlement protocol defect for {} child {}: {other:?}",
                request.group_key, request.position
            ))
            .into()),
        }
    }

    #[handler]
    async fn retire(
        &self,
        ctx: SharedWorkflowContext<'_>,
        group_key: String,
    ) -> HandlerResult<Json<()>> {
        let Json(retired) = ctx
            .object_client::<EffectGroupIndexClient>(group_key.clone())
            .retire()
            .call()
            .await?;
        let cleanup = match retired {
            EffectGroupRetireResponse::Retired { cleanup }
            | EffectGroupRetireResponse::AlreadyRetired { cleanup } => cleanup,
            EffectGroupRetireResponse::Tombstone | EffectGroupRetireResponse::UnknownGroup => {
                return Ok(Json(()));
            }
        };
        if let EffectGroupDispatchState::Adopted { id, .. } = &cleanup.dispatcher {
            ctx.invocation_handle(id.clone()).cancel();
            match ctx.invocation_handle(id.clone()).attach::<Json<()>>().await {
                Ok(_) | Err(_) => {}
            }
        }
        for invocation_id in cleanup.dispatched.values() {
            ctx.invocation_handle(invocation_id.clone()).cancel();
        }
        let Json(cancelled) = ctx
            .object_client::<EffectGroupIndexClient>(group_key.clone())
            .retirement_cancel()
            .call()
            .await?;
        match cancelled {
            EffectGroupRetirementCancelResponse::Applied
            | EffectGroupRetirementCancelResponse::AlreadyApplied => {}
            other => {
                return Err(TerminalError::new(format!(
                    "effect group {group_key} retirement could not install canceller-side terminals: {other:?}"
                ))
                .into());
            }
        }
        for position in 0..cleanup.children {
            let Json(()) = ctx
                .object_client::<EffectGroupPayloadClient>(payload_key(&group_key, position))
                .retire()
                .call()
                .await?;
        }
        // Wait retirement is retained in the durable-wait index. This shared
        // handler cannot mutate the index object directly without journaling
        // the calls, so resolve every one before deleting payload bytes.
        for (kind, resolution) in std::iter::once((
            EffectGroupWaitKind::Ready,
            EffectGroupWaitResolution::Retired,
        ))
        .chain((1..=cleanup.children as u64).map(|rank| {
            (
                EffectGroupWaitKind::Rank(rank),
                EffectGroupWaitResolution::Retired,
            )
        }))
        .chain(cleanup.replay_keys.iter().map(|replay_key| {
            (
                EffectGroupWaitKind::Cancel(replay_key),
                EffectGroupWaitResolution::Retired,
            )
        }))
        .chain((0..cleanup.children).map(|position| {
            (
                EffectGroupWaitKind::Admit(position),
                EffectGroupWaitResolution::Retired,
            )
        })) {
            let key = group_wait_key(&cleanup.wait_scope, &group_key, kind)?;
            let address = RestateDurableWaitAddress::for_key(&key);
            let Json(()) = ctx
                .object_client::<LashDurableWaitIndexClient>(durable_wait_index_object_key(
                    &address,
                ))
                .retain_resolution(Json(RestateDurableWaitResolveRequest {
                    key,
                    resolution: wait_resolution(resolution)?,
                }))
                .call()
                .await?;
        }
        for position in 0..cleanup.children {
            let Json(()) = ctx
                .object_client::<EffectGroupPayloadClient>(payload_key(&group_key, position))
                .delete_bytes()
                .call()
                .await?;
        }
        let Json(finished) = ctx
            .object_client::<EffectGroupIndexClient>(group_key.clone())
            .finish_retirement()
            .call()
            .await?;
        match finished {
            EffectGroupFinishRetirementResponse::Finished
            | EffectGroupFinishRetirementResponse::AlreadyFinished => Ok(Json(())),
            other => Err(TerminalError::new(format!(
                "effect group {group_key} retirement could not reduce the index to its tombstone: {other:?}"
            ))
            .into()),
        }
    }
}
