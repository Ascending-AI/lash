//! The core's [`ToolChildContextSource`]: the context a group tool child of
//! one of this core's sessions runs under when its opener is not live where
//! the child runs (FIG-3712, decision 46).
//!
//! The context comes from the core's own wiring and the child's recorded
//! execution environment, the way a process worker builds a process's runtime:
//! a plugin host of the core's plugin factories (its own, because a plugin
//! host admits one session of a given id and the child's session may be open
//! in this process), the core's provider and work ports, and the recorded
//! policy and plugin options. The runtime is storeless: it reads no session state and
//! persists none, so a child never writes session state under an opener that
//! is not running. Everything the child produces rides its settlement to the
//! opener, as it does on the live path. Session-scoped plugin overlays a
//! particular open added are not part of the core's wiring and are absent
//! here, exactly as they are for a process runtime.

use std::sync::Arc;

use lash_core::facade_support::{
    DeploymentToolChildContext, LashRuntime, PluginFactory, ProviderHandle, RuntimeEnvironment,
    ToolChildContextSource,
};
use lash_core::{PluginError, RuntimeSessionState, ScopedEffectController};

/// The core's work ports, resolved when a context is built: the process work
/// wiring and the queued-work substrate a session runtime of this core runs
/// with.
pub(crate) type CoreWorkPorts = Arc<
    dyn Fn() -> futures_util::future::BoxFuture<
            'static,
            (
                Option<lash_core::ProcessWorkWiring>,
                Arc<dyn lash_core::QueuedWorkSubstrate>,
            ),
        > + Send
        + Sync,
>;

pub(crate) struct CoreToolChildContextSource {
    env: RuntimeEnvironment,
    protocol_factory: Option<Arc<dyn PluginFactory>>,
    plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    provider: Option<ProviderHandle>,
    process_lifecycle_available: bool,
    work_ports: CoreWorkPorts,
    session_execution_owner: lash_core::LeaseOwnerIdentity,
}

impl CoreToolChildContextSource {
    /// Builds the core's source and installs it on the backend's tool-child
    /// host. The core keeps the returned source alive; the host holds it
    /// weakly.
    pub(crate) fn install(
        env: &RuntimeEnvironment,
        protocol_factory: Option<Arc<dyn PluginFactory>>,
        plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
        provider: Option<ProviderHandle>,
        process_lifecycle_available: bool,
        work_ports: CoreWorkPorts,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
    ) -> Arc<dyn ToolChildContextSource> {
        let source: Arc<dyn ToolChildContextSource> = Arc::new(Self {
            env: env.clone(),
            protocol_factory,
            plugin_factories,
            provider,
            process_lifecycle_available,
            work_ports,
            session_execution_owner,
        });
        if let Some(tool_children) = env.core.control.tool_children.as_ref() {
            tool_children.install_context_source(&source);
        }
        source
    }
}

#[async_trait::async_trait]
impl ToolChildContextSource for CoreToolChildContextSource {
    async fn tool_child_context(
        &self,
        request: &lash_core::runtime::effect::ToolChildRequest,
        execution_env: &lash_core::ProcessExecutionEnvSpec,
        lent_controller: ScopedEffectController<'static>,
    ) -> Result<DeploymentToolChildContext, PluginError> {
        let mut env = self.env.clone();
        if let Some(provider) = self.provider.clone() {
            env.core.providers.provider_resolver = Arc::new(
                lash_core::facade_support::SingleProviderResolver::new(provider),
            );
        }
        let plugin_host = super::build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            Vec::new(),
        )
        .map_err(|error| PluginError::Session(error.to_string()))?;
        env.core = plugin_host
            .install_process_engine_contributions(
                env.core.clone(),
                self.process_lifecycle_available,
            )
            .map_err(|error| PluginError::Session(error.to_string()))?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let (process, queued) = (self.work_ports)().await;
        env = env.with_work_ports(process, queued);
        let session_id = request.scope.session_id.clone();
        let policy = execution_env.policy.clone();
        let state = RuntimeSessionState {
            session_id: session_id.clone(),
            policy: policy.clone(),
            ..RuntimeSessionState::new(policy.clone())
        };
        let runtime = LashRuntime::from_environment_with_plugin_options(
            &env,
            policy,
            state,
            None,
            execution_env.plugin_options.clone(),
            self.session_execution_owner.clone(),
        )
        .await
        .map_err(|error| {
            PluginError::Session(format!(
                "build the context of tool child `{}` in session `{session_id}`: {error}",
                request.call.call_id
            ))
        })?;
        let dispatch = runtime.tool_child_dispatch(lent_controller)?;
        Ok(DeploymentToolChildContext::new(
            dispatch,
            Arc::new(std::sync::Mutex::new(runtime)),
        ))
    }
}
