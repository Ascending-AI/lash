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

impl RuntimeEffectLocalExecutor<'_> {
    /// Binds the captured provider route a trigger delivery's start restores
    /// inside its recorded admission, so the host's restorer is asked on the
    /// step's first execution and never on its replay (FIG-4554).
    pub fn with_trigger_route(mut self, route: crate::TriggerRouteRestore) -> Self {
        if let RuntimeEffectLocalExecutorState::Target(LocalTarget::Process(execution)) =
            &mut self.state
        {
            execution.trigger_route = Some(route);
        }
        self
    }
}
