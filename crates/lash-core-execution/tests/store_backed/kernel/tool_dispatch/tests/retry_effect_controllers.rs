use super::*;

#[derive(Default)]
pub(super) struct SleepRecordingEffectController {
    pub(super) sleeps: Arc<std::sync::Mutex<Vec<crate::RuntimeInvocation>>>,
}

impl crate::AwaitEventResolver for SleepRecordingEffectController {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for SleepRecordingEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
            self.sleeps
                .lock_recover()
                .push(envelope.invocation.into_runtime_invocation());
            Ok(crate::RuntimeEffectOutcome::Sleep)
        } else {
            local_executor.execute(envelope).await
        }
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "SleepRecordingEffectController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "SleepRecordingEffectController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "SleepRecordingEffectController",
        ))
    }

    async fn commit_group_child_final(
        &self,
        _commit: crate::runtime::effect::GroupChildFinalCommit,
    ) -> Result<
        crate::runtime::effect::EffectGroupChildCommitOutcome,
        crate::RuntimeEffectControllerError,
    > {
        Ok(crate::runtime::effect::EffectGroupChildCommitOutcome::Ungrouped)
    }
}

pub(super) struct FailingSleepEffectController;

impl crate::AwaitEventResolver for FailingSleepEffectController {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for FailingSleepEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
            Err(crate::RuntimeEffectControllerError::foreign(
                "test_sleep_rejected",
                crate::TurnFailureCause::Outcome,
                format!("rejected {}", envelope.command.kind().as_str()),
            ))
        } else {
            local_executor.execute(envelope).await
        }
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "FailingSleepEffectController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "FailingSleepEffectController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported(
            "FailingSleepEffectController",
        ))
    }

    async fn commit_group_child_final(
        &self,
        _commit: crate::runtime::effect::GroupChildFinalCommit,
    ) -> Result<
        crate::runtime::effect::EffectGroupChildCommitOutcome,
        crate::RuntimeEffectControllerError,
    > {
        Ok(crate::runtime::effect::EffectGroupChildCommitOutcome::Ungrouped)
    }
}
