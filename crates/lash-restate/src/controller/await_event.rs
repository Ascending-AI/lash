//! Signed completion keys and process-segment signal waits.
use super::*;
use crate::durable_wait::RestateDurableWaitResolveRequest;
use lash_core::{AwaitEventResolver, CompletionKeyPreparation, ResolveOutcome};

async fn resolve_restate_await_event<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<ResolveOutcome, RuntimeError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    context
        .resolve_event(
            namespace,
            RestateDurableWaitResolveRequest {
                key: key.clone(),
                resolution,
            },
        )
        .await
        .map_err(|err| {
            crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
                .into_runtime_error()
        })?
        .into_result()
}
#[async_trait::async_trait]
impl<'ctx, C> AwaitEventResolver for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.authority_id.binding_id().to_string())
    }

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
        restate_await_event_key_for_authority(&self.authority_id, scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event(&self.context, &self.namespace, key, resolution).await
    }

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.publish_resolve(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        if turn_gate::is_turn_cancel_gate(key) {
            if let Some(resolution) = self.mirrored_turn_gate(key).await? {
                return Ok(Some(resolution));
            }
        } else {
            self.require_active_session(key.scope.session_id()).await?;
        }
        self.context
            .peek_event(
                &self.namespace,
                RestateDurableWaitAddress::for_key(key),
                key.key_id.clone(),
            )
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
                    .into_runtime_error()
            })
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Resolution, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        self.require_active_session(key.scope.session_id()).await?;
        let replay_key = key.key_id.clone();
        let request = restate_durable_wait_request(key);
        self.context
            .await_event(&self.namespace, request, replay_key, cancel)
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
                    .into_runtime_error()
            })
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(&self.namespace, session_id.clone(), true)
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
                    .into_runtime_error()
            })
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(&self.namespace, session_id.clone(), false)
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
                    .into_runtime_error()
            })
    }
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// A process segment's signal wait (FIG-3673, FIG-3799): the event
    /// raced against the segment's cancel promise and the drain's hand-over.
    /// A hand-over is not a wait outcome the body sees: it answers
    /// [`RuntimeErrorCode::ProcessSignalWaitHandedOver`], on which the body
    /// stops at the wait and the segment hands it to its successor.
    pub(super) async fn await_segment_signal(
        &self,
        _invocation: &RuntimeEffectInvocation,
        request: crate::durable_wait::RestateDurableWaitAwaitRequest,
        replay_key: String,
        generation: lash_core::engine::BuildGeneration,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let outcome = self
            .context
            .await_signal_or_segment_end(&self.namespace, request, replay_key, generation.clone())
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
            })?;
        match outcome {
            RestateTurnCancelRaceOutcome::Completed(context::SignalWaitOutcome::Resolved(
                resolution,
            )) => Ok(RuntimeEffectOutcome::AwaitEvent { resolution }),
            RestateTurnCancelRaceOutcome::ProcessCancelled => {
                Ok(RuntimeEffectOutcome::AwaitEvent {
                    resolution: Resolution::Cancelled,
                })
            }
            RestateTurnCancelRaceOutcome::Completed(context::SignalWaitOutcome::HandedOver) => {
                tracing::info!(
                    target: "lash::restate",
                    event = "restate.signal_wait_handed_over",
                    generation = generation.as_str(),
                    "a process segment's signal wait was handed over to its successor"
                );
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::ProcessSignalWaitHandedOver,
                    format!(
                        "the drain of generation {} handed this signal wait to a successor segment",
                        generation.as_str()
                    ),
                ))
            }
            RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::SessionRevoked { .. } => {
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EngineEffectController,
                    "a process signal wait observes no turn",
                ))
            }
        }
    }
}
