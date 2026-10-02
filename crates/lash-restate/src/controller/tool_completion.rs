use super::*;

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    pub(super) async fn arm_tool_completion(
        &self,
        key: AwaitEventKey,
        timeout_ms: Option<u64>,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, &key) {
            return Err(RuntimeEffectControllerError::from(
                restate_unknown_or_revoked(),
            ));
        }
        let options = local_executor.into_await_event_options()?;
        let deadline = timeout_ms
            .map(|timeout| options.clock.now() + std::time::Duration::from_millis(timeout));
        let request = journaled_restate_durable_wait_request(
            &self.context,
            &key,
            deadline,
            options.clock.as_ref(),
        )
        .await
        .map_err(|err| {
            crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
        })?;
        let deadline_ms = request.deadline.map(|deadline| deadline.unix_epoch_ms);
        self.context
            .arm_tool_completion(
                &self.namespace,
                key,
                deadline_ms,
                options.clock.timestamp_ms(),
            )
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
            })?;
        Ok(RuntimeEffectOutcome::ArmToolCompletion { deadline_ms })
    }
    pub(super) async fn await_tool_completions(
        &self,
        invocation: RuntimeEffectInvocation,
        waits: Vec<lash_core::ToolCompletionWait>,
        dispatch: Option<lash_core::ToolDispatchCursor>,
        transferable: bool,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        for wait in &waits {
            if !restate_await_event_key_is_valid_for_authority(&self.authority_id, &wait.key) {
                return Err(RuntimeEffectControllerError::from(
                    restate_unknown_or_revoked(),
                ));
            }
        }
        let options = local_executor.into_await_event_options()?;
        let turn_cancel = restate_await_event_turn_cancel_wait_request(
            &self.authority_id,
            &invocation,
            options.observe_turn_cancel,
            options.turn_cancel_scope.as_ref(),
        )?;
        let outcome = self
            .context
            .await_tool_completions(
                &self.namespace,
                waits,
                dispatch,
                turn_cancel,
                transferable.then(|| self.build_generation.clone()),
                self.options.process_cancel,
            )
            .await
            .map_err(|err| {
                crate::wire::lash_terminal(&err, RuntimeErrorCode::EngineEffectController)
            })?;
        match outcome {
            RestateTurnCancelRaceOutcome::Completed(event) => {
                Ok(RuntimeEffectOutcome::AwaitToolCompletions { event })
            }
            RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::ProcessCancelled => {
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    "the deferred tool round was cancelled",
                ))
            }
            RestateTurnCancelRaceOutcome::SessionRevoked { .. } => Err(
                RuntimeEffectControllerError::from(restate_unknown_or_revoked()),
            ),
        }
    }
}
