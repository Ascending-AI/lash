use super::*;

#[derive(Clone)]
pub(super) struct RestateAwaitEventIngress {
    pub(super) ingress: RestateIngressClient,
}

pub(super) async fn resolve_restate_await_event_via_ingress(
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
        .call_object_json::<_, RestateDurableWaitResolveResponse>(
            crate::LashService::DurableWaitIndex.name(),
            &index_key,
            "resolve",
            &request,
        )
        .await;
    outcome
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RestateAwaitEventResolve,
                err.to_string(),
            )
        })?
        .into_result()
}

pub(super) async fn update_restate_session_waits_via_ingress(
    ingress: &RestateAwaitEventIngress,
    session_id: &SessionId,
    revoke: bool,
) -> Result<(), RuntimeError> {
    let handler = if revoke { "revoke_all" } else { "cancel_all" };
    ingress
        .ingress
        .call_object_empty(
            crate::LashService::DurableWaitIndex.name(),
            session_id,
            handler,
        )
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RestateAwaitEventSessionUpdate,
                err.to_string(),
            )
        })
}

/// Whether the `LashDurableWaitIndex` object at `index_key` (a session's, or
/// a non-session scope's) has been revoked: the durable fence every mint,
/// resolve, peek, await, effect, and group under it consults.
pub(super) async fn restate_index_is_revoked_via_ingress(
    ingress: &RestateAwaitEventIngress,
    index_key: &str,
) -> Result<bool, RuntimeError> {
    ingress
        .ingress
        .call_object_json::<_, bool>(
            crate::LashService::DurableWaitIndex.name(),
            index_key,
            "is_revoked",
            &(),
        )
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RestateAwaitEventRevocationRead,
                err.to_string(),
            )
        })
}

pub(super) async fn ensure_restate_key_access_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
) -> Result<(), RuntimeError> {
    if !restate_await_event_key_is_valid(key) {
        return Err(restate_unknown_or_revoked());
    }
    let index_key = durable_wait_index_object_key(&RestateDurableWaitAddress::for_key(key));
    if restate_index_is_revoked_via_ingress(ingress, &index_key).await? {
        return Err(restate_unknown_or_revoked());
    }
    Ok(())
}

pub(super) fn restate_scope_not_retirable(scope: &ExecutionScope) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::AwaitEventScopeNotRetirable,
        format!(
            "scope `{}` carries a session and is retired through session revocation, not the scope lever",
            scope.id()
        ),
    )
}

pub(super) async fn update_restate_scope_waits_via_ingress(
    ingress: &RestateAwaitEventIngress,
    scope: &ExecutionScope,
    handler: &str,
) -> Result<(), RuntimeError> {
    let index_key = durable_wait_index_key_for_scope(scope);
    ingress
        .ingress
        .call_object_empty(
            crate::LashService::DurableWaitIndex.name(),
            &index_key,
            handler,
        )
        .await
        .map_err(|err| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RestateAwaitEventSessionUpdate,
                err.to_string(),
            )
        })
}

pub(super) async fn retire_restate_scope_via_ingress(
    ingress: &RestateAwaitEventIngress,
    scope: &ExecutionScope,
    only_if_quiescent: bool,
) -> Result<(), RuntimeError> {
    let index_key = durable_wait_index_key_for_scope(scope);
    let handler = if only_if_quiescent {
        "revoke_all_if_quiescent"
    } else {
        "retire_scope"
    };
    let retired = ingress
        .ingress
        .call_object_json::<_, bool>(
            crate::LashService::DurableWaitIndex.name(),
            &index_key,
            handler,
            &(),
        )
        .await
        .map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::RestateAwaitEventSessionUpdate,
                error.to_string(),
            )
        })?;
    if retired {
        Ok(())
    } else {
        Err(
            lash_core::facade_support::effect_replay_driver::scope_not_quiescent(
                scope.journal_identity()?.key(),
            ),
        )
    }
}

pub(super) async fn await_restate_await_event_via_ingress(
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
                    crate::LashService::DurableWaitWorkflow.name(),
                    &workflow_key,
                    "await_resolution",
                    &request,
                    replay_key,
                ).await,
                None => ingress.ingress.call_workflow_json::<_, Resolution>(
                    crate::LashService::DurableWaitWorkflow.name(),
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
            ).await;
            // A cancel-decided child's key refuses the release (ADR 0099 §4):
            // the waiter was cancelled either way.
            match outcome {
                Ok(ResolveOutcome::AlreadyResolved { terminal }) => Ok(terminal),
                Ok(ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked) => {
                    Ok(Resolution::Cancelled)
                }
                Err(error)
                    if error.code
                        == lash_core::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided =>
                {
                    Ok(Resolution::Cancelled)
                }
                Err(error) => Err(error),
            }
        },
    }
}
