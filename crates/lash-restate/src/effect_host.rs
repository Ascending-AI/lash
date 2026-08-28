//! Deployment-level Restate effect host.
//!
//! One responsibility: give a long-lived Lash core a durable await-event
//! boundary when no Restate handler context is in scope. Real effect execution
//! needs a handler, so this host resolves, peeks, awaits, durably cancels, and
//! revokes waits through the ingress and fails loudly for anything else instead
//! of falling back to native execution.

use std::sync::Arc;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    EffectGroupHandle, EffectHost, ExecutionScope, GroupSettlement, LoserPolicy, Resolution,
    ResolveOutcome, RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectFailureDisposition, RuntimeEffectGroup,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode,
    ScopedEffectController, ToolIntentOutcomeSink, ToolIntentPreparation, TurnControlParticipation,
    facade_support::RuntimeAwaitEventOptions,
};

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitResolveRequest, durable_wait_index_object_key,
    restate_await_event_key, restate_await_event_key_is_valid, restate_durable_wait_request,
    restate_unknown_or_revoked,
};
use crate::effect_group::{
    EffectGroupCloseDisposition, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupPayloadGetResponse, EffectGroupProbeResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupSettlementTerminal, EffectGroupShape,
    EffectGroupWaitResolution, decode_wait_resolution, group_shape_error, payload_key,
    rank_wait_request, ready_wait_request, settlement_from_payload,
};
use crate::ingress::{RestateConnection, RestateIngressClient};

/// Deployment-level Restate effect host for long-lived Lash cores.
///
/// Restate's real effect execution requires a handler context, so this host is
/// intentionally a durable boundary, not an executor. HTTP/API code should
/// enter a Restate workflow/object first and then pass
/// [`RestateRuntimeEffectController::scoped_effect_controller`](crate::RestateRuntimeEffectController::scoped_effect_controller)
/// into Lash. If a caller tries to execute through this deployment host
/// directly, it fails loudly instead of falling back to native execution.
#[derive(Clone)]
pub struct RestateEffectHost {
    controller: Arc<RestateEffectHostController>,
}

impl RestateEffectHost {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        Self {
            controller: Arc::new(RestateEffectHostController {
                await_event_ingress: RestateAwaitEventIngress {
                    ingress: RestateIngressClient::new(connection),
                },
            }),
        }
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHost {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.controller.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.controller.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.controller.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl EffectHost for RestateEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate()?;
        ScopedEffectController::shared(self.controller.clone(), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scope.validate()?;
        Ok(Some(ScopedEffectController::shared(
            self.controller.clone(),
            scope,
        )?))
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> Result<ToolIntentPreparation, RuntimeError> {
        Ok(ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    async fn retire_effect_journal(
        &self,
        _retirement: lash_core::EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        // Restate owns invocation-journal retention natively. There is no
        // Lash-side replay ledger to delete at this lifecycle boundary.
        Ok(0)
    }
}
#[derive(Clone)]
struct RestateAwaitEventIngress {
    ingress: RestateIngressClient,
}

async fn resolve_restate_await_event_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<ResolveOutcome, RuntimeError> {
    let address = RestateDurableWaitAddress::for_key(key);
    let request = RestateDurableWaitResolveRequest {
        key: key.clone(),
        resolution,
    };
    let index_key = durable_wait_index_object_key(&address);
    let outcome = ingress
        .ingress
        .call_object_json::<_, ResolveOutcome>(
            "LashDurableWaitIndex",
            &index_key,
            "resolve",
            &request,
        )
        .await;
    outcome.map_err(|err| {
        RuntimeError::new(
            lash_core::RuntimeErrorCode::RestateAwaitEventResolve,
            err.to_string(),
        )
    })
}

async fn update_restate_session_waits_via_ingress(
    ingress: &RestateAwaitEventIngress,
    session_id: &str,
    revoke: bool,
) -> Result<(), RuntimeError> {
    let handler = if revoke { "revoke_all" } else { "cancel_all" };
    ingress
        .ingress
        .call_object_empty("LashDurableWaitIndex", session_id, handler)
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RestateAwaitEventSessionUpdate,
                err.to_string(),
            )
        })
}

async fn restate_session_is_revoked_via_ingress(
    ingress: &RestateAwaitEventIngress,
    session_id: &str,
) -> Result<bool, RuntimeError> {
    ingress
        .ingress
        .call_object_json::<_, bool>("LashDurableWaitIndex", session_id, "is_revoked", &())
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RestateAwaitEventRevocationRead,
                err.to_string(),
            )
        })
}

async fn ensure_restate_key_access_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
) -> Result<(), RuntimeError> {
    if !restate_await_event_key_is_valid(key) {
        return Err(restate_unknown_or_revoked());
    }
    if let Some(session_id) = key.scope.session_id()
        && restate_session_is_revoked_via_ingress(ingress, session_id).await?
    {
        return Err(restate_unknown_or_revoked());
    }
    Ok(())
}

async fn await_restate_await_event_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
    cancel: tokio_util::sync::CancellationToken,
    deadline: Option<std::time::Instant>,
    effect_replay_key: Option<&str>,
) -> Result<Resolution, RuntimeError> {
    let request =
        restate_durable_wait_request(key, deadline, &lash_core::facade_support::SystemClock);
    let workflow_key = RestateDurableWaitAddress::for_key(&request.key).workflow_key;
    tokio::select! {
        result = async {
            match effect_replay_key {
                Some(replay_key) => ingress.ingress.call_workflow_json_idempotent::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &request,
                    replay_key,
                ).await,
                None => ingress.ingress.call_workflow_json::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &request,
                ).await,
            }
        } => result.map_err(|err| {
            RuntimeError::new(lash_core::RuntimeErrorCode::RestateAwaitEventAwait, err.to_string())
        }),
        _ = cancel.cancelled() => {
            let outcome = resolve_restate_await_event_via_ingress(
                ingress,
                key,
                Resolution::Cancelled,
            ).await?;
            Ok(match outcome {
                ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked => {
                    Resolution::Cancelled
                }
                ResolveOutcome::AlreadyResolved { terminal } => terminal,
            })
        },
    }
}
struct RestateEffectHostController {
    await_event_ingress: RestateAwaitEventIngress,
}

fn ingress_group_error(
    operation: &str,
    error: crate::RestateHttpError,
) -> RuntimeEffectControllerError {
    let service_unregistered = error.is_service_unregistered();
    let message = format!("Restate effect-group operation {operation} failed: {error}");
    if service_unregistered {
        RuntimeEffectControllerError::new(RuntimeErrorCode::RestateServiceUnregistered, message)
    } else {
        group_shape_error(message)
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHostController {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate()?;
        let ingress = &self.await_event_ingress;
        if let Some(session_id) = scope.session_id()
            && restate_session_is_revoked_via_ingress(ingress, session_id).await?
        {
            return Err(restate_unknown_or_revoked());
        }
        restate_await_event_key(scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event_via_ingress(&self.await_event_ingress, key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        let ingress = &self.await_event_ingress;
        ensure_restate_key_access_via_ingress(ingress, key).await?;
        let workflow_key = RestateDurableWaitAddress::for_key(key).workflow_key;
        ingress
            .ingress
            .call_workflow_empty::<Option<Resolution>>(
                "LashDurableWaitWorkflow",
                &workflow_key,
                "peek",
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::RestateAwaitEventPeek,
                    err.to_string(),
                )
            })
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        let ingress = &self.await_event_ingress;
        ensure_restate_key_access_via_ingress(ingress, key).await?;
        await_restate_await_event_via_ingress(ingress, key, cancel, deadline, None).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let ingress = &self.await_event_ingress;
        update_restate_session_waits_via_ingress(ingress, session_id, true).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let ingress = &self.await_event_ingress;
        update_restate_session_waits_via_ingress(ingress, session_id, false).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RestateEffectHostController {
    fn supports_concurrent_effects(&self) -> bool {
        false
    }

    fn supports_effect_groups(&self) -> bool {
        true
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let ingress = &self.await_event_ingress.ingress;
        let group_key = group.group_key().to_string();
        let handle = EffectGroupHandle::new(&group);
        let shape = EffectGroupShape::from_group(&group)?;
        let probe = ingress
            .call_object_empty_json::<EffectGroupProbeResponse>(
                "EffectGroupIndex",
                &group_key,
                "probe",
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/probe", error))?;
        if matches!(probe, EffectGroupProbeResponse::Absent)
            && let Some(position) = ingress
                .call_workflow_json::<_, Option<usize>>(
                    "EffectGroupDispatch",
                    &group_key,
                    "preflight",
                    &group.children(),
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupDispatch/preflight", error))?
        {
            let replay_key = shape.replay_keys.get(position).ok_or_else(|| {
                group_shape_error(format!(
                    "effect group {group_key} preflight named child {position}, outside the {} children its shape carries",
                    shape.replay_keys.len()
                ))
            })?;
            return Err(group_shape_error(format!(
                "effect group {group_key} child {position} ({replay_key}) has no registered executor; refusing before group state is created"
            )));
        }
        let opened = ingress
            .call_object_json::<_, EffectGroupOpenResponse>(
                "EffectGroupIndex",
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/open", error))?;
        match opened {
            EffectGroupOpenResponse::OpenedFresh | EffectGroupOpenResponse::ReopenedPreparing => {
                ingress
                    .send_workflow_json(
                        "EffectGroupDispatch",
                        &group_key,
                        "run",
                        &EffectGroupDispatchRequest {
                            group_key: group_key.clone(),
                            shape: shape.clone(),
                            children: group.children().to_vec(),
                        },
                    )
                    .await
                    .map_err(|error| ingress_group_error("EffectGroupDispatch/run", error))?;
                let request = ready_wait_request(&shape.wait_scope, &group_key)?;
                let address = RestateDurableWaitAddress::for_key(&request.key);
                let resolution = ingress
                    .call_workflow_json::<_, Resolution>(
                        "LashDurableWaitWorkflow",
                        &address.workflow_key,
                        "await_resolution",
                        &request,
                    )
                    .await
                    .map_err(|error| {
                        ingress_group_error(
                            "LashDurableWaitWorkflow/await_resolution(READY)",
                            error,
                        )
                    })?;
                match decode_wait_resolution(resolution)? {
                    EffectGroupWaitResolution::Ready => Ok(handle),
                    EffectGroupWaitResolution::Refused { reason } => Err(group_shape_error(
                        format!("effect group {group_key} routing was refused: {reason:?}"),
                    )),
                    EffectGroupWaitResolution::Retired => Err(group_shape_error(format!(
                        "effect group {group_key} was retired before it became ready"
                    ))),
                    other => Err(group_shape_error(format!(
                        "effect group {group_key} READY wait resolved as {other:?}"
                    ))),
                }
            }
            EffectGroupOpenResponse::ReopenedReady => Ok(handle),
            EffectGroupOpenResponse::ReopenedClosed { effective } => match effective {
                EffectGroupCloseDisposition::Refused { reason } => Err(group_shape_error(format!(
                    "effect group {group_key} routing was refused: {reason:?}"
                ))),
                EffectGroupCloseDisposition::RunToCompletion
                | EffectGroupCloseDisposition::Cancel => Ok(handle),
            },
            EffectGroupOpenResponse::Retired => Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            ))),
            EffectGroupOpenResponse::ShapeMismatch => Err(group_shape_error(format!(
                "effect group {group_key} was reopened with a different durable shape"
            ))),
        }
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        if handle.is_exhausted() {
            return Err(group_shape_error(format!(
                "effect group {} has no settlement after its {} children",
                handle.group_key(),
                handle.children()
            )));
        }
        let ingress = &self.await_event_ingress.ingress;
        let rank = u64::try_from(handle.consumed() + 1).map_err(|error| {
            group_shape_error(format!("effect group rank does not fit u64: {error}"))
        })?;
        let mut read = ingress
            .call_object_json::<_, EffectGroupReadRankResponse>(
                "EffectGroupIndex",
                handle.group_key(),
                "read_rank",
                &EffectGroupReadRankRequest { rank },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        if matches!(read, EffectGroupReadRankResponse::NotSettled) {
            let scope = ExecutionScope::runtime_operation(handle.group_key());
            let request = rank_wait_request(&scope, handle.group_key(), rank)?;
            let address = RestateDurableWaitAddress::for_key(&request.key);
            let wait = ingress.call_workflow_json::<_, Resolution>(
                "LashDurableWaitWorkflow",
                &address.workflow_key,
                "await_resolution",
                &request,
            );
            tokio::pin!(wait);
            let resolution = tokio::select! {
                result = &mut wait => Some(result.map_err(|error| ingress_group_error(
                    "LashDurableWaitWorkflow/await_resolution(RANK)", error
                ))?),
                _ = cancel.cancelled() => None,
            };
            let Some(resolution) = resolution else {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    format!(
                        "awaiting effect group {} rank {rank} was cancelled",
                        handle.group_key()
                    ),
                ));
            };
            match decode_wait_resolution(resolution)? {
                EffectGroupWaitResolution::Rank => {}
                EffectGroupWaitResolution::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {} was retired while awaiting rank {rank}",
                        handle.group_key()
                    )));
                }
                other => {
                    return Err(group_shape_error(format!(
                        "effect group {} rank {rank} wait resolved as {other:?}",
                        handle.group_key()
                    )));
                }
            }
            read = ingress
                .call_object_json::<_, EffectGroupReadRankResponse>(
                    "EffectGroupIndex",
                    handle.group_key(),
                    "read_rank",
                    &EffectGroupReadRankRequest { rank },
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        }
        let record = match read {
            EffectGroupReadRankResponse::Settled { settlement } => settlement,
            EffectGroupReadRankResponse::NotSettled => {
                return Err(group_shape_error(format!(
                    "effect group {} rank {rank} remained unsettled after its notification",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::Closed => {
                return Err(group_shape_error(format!(
                    "effect group {} is closed to this caller",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::UnknownGroup => {
                return Err(group_shape_error(format!(
                    "effect group {} is unknown",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {} is retired",
                    handle.group_key()
                )));
            }
        };
        let payload = if matches!(
            record.terminal,
            EffectGroupSettlementTerminal::StoredPayload
        ) {
            match ingress
                .call_object_empty_json::<EffectGroupPayloadGetResponse>(
                    "EffectGroupPayload",
                    &payload_key(handle.group_key(), record.position),
                    "get",
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupPayload/get", error))?
            {
                EffectGroupPayloadGetResponse::Stored { bytes } => Some(bytes),
                EffectGroupPayloadGetResponse::Missing => {
                    return Err(group_shape_error(format!(
                        "effect group {} rank {rank} refers to a missing payload",
                        handle.group_key()
                    )));
                }
                EffectGroupPayloadGetResponse::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {} payload was retired",
                        handle.group_key()
                    )));
                }
            }
        } else {
            None
        };
        let settlement = settlement_from_payload(record, payload)?;
        handle.advance()?;
        Ok(settlement)
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let group_key = handle.group_key().to_string();
        let response = self
            .await_event_ingress
            .ingress
            .call_object_json::<_, EffectGroupCloseResponse>(
                "EffectGroupIndex",
                &group_key,
                "close",
                &EffectGroupCloseRequest { disposition },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/close", error))?;
        match response {
            EffectGroupCloseResponse::Closed | EffectGroupCloseResponse::AlreadyClosed => Ok(()),
            EffectGroupCloseResponse::WidenRefused => Err(group_shape_error(format!(
                "effect group {group_key} close attempted to widen its declared loser disposition"
            ))),
            EffectGroupCloseResponse::NotReady => Err(group_shape_error(format!(
                "effect group {group_key} cannot close before registration"
            ))),
            EffectGroupCloseResponse::UnknownGroup => Err(group_shape_error(format!(
                "effect group {group_key} is unknown"
            ))),
            EffectGroupCloseResponse::Retired => Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            ))),
        }
    }

    async fn runtime_effect_failure_disposition(
        &self,
        _code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        Ok(TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let effect_replay_key = envelope.stable_hash()?;
        if let RuntimeEffectCommand::AwaitEvent { key } = &envelope.command {
            if !restate_await_event_key_is_valid(key) {
                return Err(RuntimeEffectControllerError::from(
                    restate_unknown_or_revoked(),
                ));
            }
            let ingress = &self.await_event_ingress;
            let RuntimeAwaitEventOptions {
                cancellation,
                deadline,
                ..
            } = local_executor.into_await_event_options()?;
            let resolution = await_restate_await_event_via_ingress(
                ingress,
                key,
                cancellation,
                deadline,
                Some(&effect_replay_key),
            )
            .await
            .map_err(RuntimeEffectControllerError::from)?;
            return Ok(RuntimeEffectOutcome::AwaitEvent { resolution });
        }
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateEffectHostRequiresHandlerScope,
            format!(
                "effect `{}` must enter a Restate handler and use RestateRuntimeEffectController::scoped_effect_controller",
                envelope
                    .invocation
                    .effect_id()
                    .unwrap_or_else(|| envelope.command.kind().as_str())
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_call_error(status: u16) -> crate::RestateHttpError {
        crate::RestateHttpError::Status {
            operation: "Restate object call",
            url: "https://restate.invalid/EffectGroupIndex/group/probe".to_string(),
            status,
            body: "not found".to_string(),
        }
    }

    #[test]
    fn effect_group_ingress_404_is_restate_service_unregistered() {
        let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(404));

        assert_eq!(error.code, RuntimeErrorCode::RestateServiceUnregistered);
        assert!(error.message.contains("EffectGroupIndex/probe"));
    }

    #[test]
    fn effect_group_ingress_non_registration_failure_stays_a_shape_error() {
        let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(503));

        assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectGroupShape);
    }
}
