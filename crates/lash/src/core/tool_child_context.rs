//! The core's [`ToolChildContextSource`]: the context a group tool child of
//! one of this core's sessions runs under when its opener is not live where
//! the child runs (FIG-3712, decision 46).
//!
//! The context comes from the core's own wiring and the child's recorded
//! execution environment, the way a process worker builds a process's runtime:
//! a plugin host of the core's plugin factories (its own, because a plugin
//! host admits one session of a given id and the child's session may be open
//! in this process), the core's provider and work ports, and the recorded
//! policy and plugin options, under the session's recorded tool access and
//! subagent context. The runtime is storeless: it persists nothing, and the
//! driver refuses any session read or change the child makes on it.
//! Everything the child produces rides its settlement to the opener, as it
//! does on the live path. What a particular open or turn added (overlay
//! tools, per-open plugins or provider, forked plugins, plugin state) is not
//! part of the core's wiring: a child whose request records one is refused
//! before this source is asked, and waits for its live opener.

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
    /// host. The core and each of its sessions keep the returned source
    /// alive; the host holds it weakly.
    ///
    /// One core per backend is what makes the rebuilt path usable. While
    /// another live core's source is installed on the same host, the host is
    /// ambiguous: no child is rebuilt under either core's wiring, and a child
    /// with no live opener waits for its opener, refused typed, as it did
    /// before FIG-3712. That is logged here, never a build failure.
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
        let installed = env
            .core
            .control
            .tool_children
            .as_ref()
            .map(|tool_children| tool_children.install_context_source(&source));
        if let Some(lash_core::facade_support::ContextSourceInstall::Ambiguous { live }) = installed
        {
            tracing::warn!(
                live,
                "another live core already rebuilds this backend's tool children; while both \
                 live, a tool child with no live opener waits for its opener"
            );
        }
        source
    }
}

#[async_trait::async_trait]
impl ToolChildContextSource for CoreToolChildContextSource {
    async fn tool_child_context(
        &self,
        request: &lash_core::facade_support::ToolChildRequest,
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
        let mut state = RuntimeSessionState {
            session_id: session_id.clone(),
            policy: policy.clone(),
            ..RuntimeSessionState::new(policy.clone())
        };
        // The session's recorded authority, never the fresh session's
        // default: the plugins built below see the child's tool access and
        // subagent depth, as its opener's plugins did (FIG-3712).
        state.authority.tool_access = request.session.tool_access.clone();
        state.authority.subagent = request.session.subagent.clone();
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
