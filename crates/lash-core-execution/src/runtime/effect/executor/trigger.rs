use super::*;

pub struct TriggerLocalExecution {
    pub store: Arc<dyn crate::TriggerStore>,
}

impl TriggerLocalExecution {
    pub async fn execute(
        self,
        operation_id: &str,
        command: crate::TriggerCommand,
    ) -> Result<crate::TriggerEffectResult, RuntimeEffectControllerError> {
        self.store
            .execute_command(operation_id, command)
            .await
            .map_err(RuntimeEffectControllerError::from)
    }
}
