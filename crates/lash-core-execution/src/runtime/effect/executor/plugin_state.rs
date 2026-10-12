use super::*;

impl RuntimeEffectLocalExecutor<'_> {
    pub(crate) fn execution_state(&self) -> Option<Arc<dyn crate::plugin::ExecutionState>> {
        match &self.state {
            RuntimeEffectLocalExecutorState::Runner(runner) => runner.execution_state(),
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                runner.execution_state()
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Presentation(execution)) => {
                Some(Arc::clone(&execution.state))
            }
            _ => None,
        }
    }
}

pub(super) async fn record_plugin_state<F>(
    plugins: Option<Arc<dyn crate::plugin::ExecutionState>>,
    kind: crate::RuntimeEffectKind,
    address: crate::EffectAddress,
    body: F,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>
where
    F: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
{
    match plugins {
        Some(plugins) => crate::plugin::state::record_effect(plugins, kind, address, body).await,
        None => body.await,
    }
}
