//! The core's [`ToolChildContextSource`]: the context a group tool child of
//! one of this core's sessions runs under when its opener is not live where
//! the child runs (FIG-3712, decision 46).
//!
//! The context comes from the core's own wiring and the child's recorded
//! execution environment, the way a process worker builds a process's runtime:
//! a plugin host of the core's plugin factories (its own, because a plugin
//! host admits one session of a given id and the child's session may be open
//! in this process), the core's models and work ports, and the recorded
//! policy and plugin options, under the session's recorded tool access and
//! subagent context. The runtime is storeless: it persists nothing, and the
//! driver refuses any session read or change the child makes on it.
//! Everything the child produces rides its settlement to the opener, as it
//! does on the live path. What a particular open or turn added (overlay
//! tools, per-open plugins, forked plugins, plugin state) is not
//! part of the core's wiring: a child whose request records one is refused
//! before this source is asked, and waits for its live opener.

use lash_core::core_internal::ToolChildHostRuntimeOps as _;
use std::sync::Arc;

use lash_core::facade_support::{
    DeploymentToolChildContext, LashRuntime, PluginFactory, RuntimeEnvironment,
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
                lash_core::ProcessWorkWiring,
                Arc<dyn lash_core::SessionWorkEngine>,
            ),
        > + Send
        + Sync,
>;

pub(crate) struct CoreToolChildContextSource {
    env: RuntimeEnvironment,
    protocol_factory: Option<Arc<dyn PluginFactory>>,
    plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    process_lifecycle_available: bool,
    work_ports: CoreWorkPorts,
    drive_owner: lash_core::LeaseOwnerIdentity,
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
        process_lifecycle_available: bool,
        work_ports: CoreWorkPorts,
        drive_owner: lash_core::LeaseOwnerIdentity,
    ) -> Arc<dyn ToolChildContextSource> {
        let source: Arc<dyn ToolChildContextSource> = Arc::new(Self {
            env: env.clone(),
            protocol_factory,
            plugin_factories,
            process_lifecycle_available,
            work_ports,
            drive_owner,
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
        let plugin_host = super::build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            &env.core.tracing,
        )
        .map_err(|error| PluginError::Session(error.to_string()))?;
        env.core = plugin_host
            .install_process_engine_contributions(
                env.core.clone(),
                self.process_lifecycle_available,
            )
            .map_err(|error| PluginError::Session(error.to_string()))?;
        let plugin_host = Arc::new(plugin_host);
        env.plugin_host = Some(Arc::clone(&plugin_host));
        let (process, queued) = (self.work_ports)().await;
        env = env.with_work_ports(process, queued);
        // A child a process opened runs under that process's runtime, keyed
        // by its id: it has no session to open.
        let session_id = match &request.scope.owner {
            lash_core::ExecutionOwner::SessionFrame { session_id, .. } => session_id.clone(),
            lash_core::ExecutionOwner::Process { process_id } => {
                let runtime = lash_core::core_internal::ProcessRuntimeContext::for_tool_child(
                    &env,
                    plugin_host,
                    process_id.clone(),
                    execution_env.clone(),
                    self.drive_owner.clone(),
                )?;
                let mut dispatch = runtime.tool_child_dispatch(lent_controller)?;
                dispatch.process_lineage = enclosing_lineage(&env, request).await?;
                return Ok(DeploymentToolChildContext::new(dispatch, Arc::new(runtime)));
            }
        };
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
        // ...and the plugin configuration the child was admitted under, at
        // its revision (FIG-4379).
        state.authority.plugin_config = execution_env.plugin_config.config.as_ref().clone();
        state.config_revision = execution_env.plugin_config.revision;
        let runtime =
            LashRuntime::from_environment(&env, policy, state, None, self.drive_owner.clone())
                .await
                .map_err(|error| {
                    PluginError::Session(format!(
                        "build the context of tool child `{}` in session `{session_id}`: {error}",
                        request.call.call_id
                    ))
                })?;
        let mut dispatch = runtime.tool_child_dispatch(lent_controller)?;
        dispatch.process_lineage = enclosing_lineage(&env, request).await?;
        Ok(DeploymentToolChildContext::new(
            dispatch,
            Arc::new(std::sync::Mutex::new(runtime)),
        ))
    }
}

/// A child a process body opened runs inside that process: its starts record
/// the process's lineage, read back from the process's own row since no live
/// body lends it here (FIG-3607 R2). `None` for a child no process encloses.
async fn enclosing_lineage(
    env: &RuntimeEnvironment,
    request: &lash_core::facade_support::ToolChildRequest,
) -> Result<Option<lash_core::ProcessLineage>, PluginError> {
    let Some(process_id) = request.enclosing_process() else {
        return Ok(None);
    };
    let registry = env.process_registry().ok_or_else(|| {
        PluginError::Session(format!(
            "tool child `{}` runs inside process `{process_id}` and this deployment has \
             no process registry to read its lineage from",
            request.call.call_id
        ))
    })?;
    let enclosing = registry.get_process(process_id).await?.ok_or_else(|| {
        PluginError::Session(format!(
            "tool child `{}` runs inside process `{process_id}`, which has no row to \
             read its lineage from",
            request.call.call_id
        ))
    })?;
    Ok(Some(enclosing.lineage()))
}
