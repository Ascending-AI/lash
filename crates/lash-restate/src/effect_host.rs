//! Deployment-level Restate effect host.
//!
//! One responsibility: give a long-lived Lash core a durable await-event
//! boundary when no Restate handler context is in scope. Real effect execution
//! needs a handler, so this host resolves, peeks, awaits, and revokes waits
//! through the ingress and fails loudly for anything else instead of falling
//! back to inline execution.

use std::sync::Arc;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    Resolution, ResolveOutcome, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode, ScopedEffectController,
    facade_support::RuntimeAwaitEventOptions,
};

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitResolveRequest, durable_wait_index_object_key,
    restate_await_event_key, restate_await_event_key_is_valid, restate_durable_wait_request,
    restate_unknown_or_revoked,
};
use crate::ingress::{RestateConnection, RestateIngressClient};

/// Deployment-level Restate effect host for long-lived Lash cores.
///
/// Restate's real effect execution requires a handler context, so this host is
/// intentionally a durable boundary, not an executor. HTTP/API code should
/// enter a Restate workflow/object first and then pass
/// [`RestateRuntimeEffectController::scoped_effect_controller`](crate::RestateRuntimeEffectController::scoped_effect_controller)
/// into Lash. If a caller tries to execute through this deployment host
/// directly, it fails loudly instead of falling back to inline execution.
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
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::OrdinalAddressed
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
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
        address,
        resolution,
    };
    let index_key = durable_wait_index_object_key(&request.address);
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
    let workflow_key = request.address.workflow_key.clone();
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
        _ = cancel.cancelled() => Err(RuntimeError::new(lash_core::RuntimeErrorCode::RestateAwaitEventCancelled,
            "Restate await-event ingress observation was cancelled locally",
        )),
    }
}
struct RestateEffectHostController {
    await_event_ingress: RestateAwaitEventIngress,
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHostController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::OrdinalAddressed
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
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
