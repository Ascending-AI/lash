use super::*;

impl RuntimeEffectLocalExecutor<'_> {
    pub(crate) fn plugin_state_session(&self) -> Option<Arc<crate::PluginSession>> {
        match &self.state {
            RuntimeEffectLocalExecutorState::Runner(runner) => runner.plugin_state_session(),
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                runner.plugin_state_session()
            }
            RuntimeEffectLocalExecutorState::Target(LocalTarget::Presentation(execution)) => {
                Some(Arc::clone(&execution.plugins))
            }
            _ => None,
        }
    }
}

pub(super) async fn record_plugin_state<F>(
    plugins: Option<Arc<crate::PluginSession>>,
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
