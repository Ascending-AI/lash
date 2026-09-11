use super::*;

#[derive(Default)]
pub(super) struct SleepRecordingEffectController {
    pub(super) sleeps: Arc<std::sync::Mutex<Vec<crate::RuntimeInvocation>>>,
}

impl crate::AwaitEventResolver for SleepRecordingEffectController {}

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
}

pub(super) struct FailingSleepEffectController;

impl crate::AwaitEventResolver for FailingSleepEffectController {}

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
                format!("rejected {}", envelope.command.kind().as_str()),
            ))
        } else {
            local_executor.execute(envelope).await
        }
    }
}
