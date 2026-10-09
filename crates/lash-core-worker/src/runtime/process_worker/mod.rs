use std::sync::Arc;

mod steps;

pub use steps::process_steps;

use crate::RuntimeHostConfig;
use crate::{
    DeploymentStore, PluginError, PluginFactory, PluginHost, PluginStack, ProcessRecord,
    ProcessRegistry,
};
use lash_core::core_internal::{ProcessRuntimeContext, ProcessRuntimePorts};
use lash_core_execution::runtime::actor::process::{
    SessionTurnCancel, SessionTurnMail, SessionTurns,
};

/// Deployment-local configuration for rebuilding durable process executions.
///
/// Process rows carry portable process input, provenance and the environment
/// their start captured: the starter's recorded policy, tool authority and plugin config, which
/// every process runtime runs under. Workers provide the physical binding —
/// plugins, providers, stores, secrets and host capabilities — for the
/// deployment that owns those rows, and no behaviour of their own: a worker
/// selects no default for a fact its process's start recorded (FIG-4396).
#[derive(Clone)]
pub struct DurableProcessWorkerConfig {
    pub plugin_host: Arc<PluginHost>,
    /// The host config and its one backend, which supplies the session
    /// catalog this worker reaches (ADR 0102, D2).
    pub runtime_host: RuntimeHostConfig,
    process_work: crate::ProcessWorkWiring,
    /// The host owner identity a process runtime runs under.
    pub lease_owner: crate::LeaseOwnerIdentity,
}

impl DurableProcessWorkerConfig {
    pub fn new(
        plugin_host: Arc<PluginHost>,
        runtime_host: RuntimeHostConfig,
        process_work: crate::ProcessWorkWiring,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        let plugin_host = Arc::new(
            Arc::unwrap_or_clone(plugin_host)
                .with_trace_runtime(runtime_host.tracing.clone())
                .with_execution_budgets(runtime_host.control.execution_budgets.clone()),
        );
        Self {
            plugin_host,
            runtime_host,
            process_work,
            lease_owner,
        }
    }

    /// The backend's session catalog, which every session this worker
    /// creates, opens or reconstructs goes through.
    pub fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.runtime_host.session_store_factory()
    }

    pub fn process_registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.process_work.registry()
    }

    /// The process work wiring a process runtime of this worker is built on.
    pub fn process_work(&self) -> &crate::ProcessWorkWiring {
        &self.process_work
    }

    pub fn from_plugin_factories(
        plugin_factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
        runtime_host: RuntimeHostConfig,
        process_work: crate::ProcessWorkWiring,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self::new(
            Arc::new(PluginHost::new(
                plugin_factories.into_iter().collect(),
                runtime_host.control.execution_budgets.clone(),
                runtime_host.tracing.clone(),
            )),
            runtime_host,
            process_work,
            lease_owner,
        )
    }

    pub fn from_plugin_stack(
        plugin_stack: PluginStack,
        runtime_host: RuntimeHostConfig,
        process_work: crate::ProcessWorkWiring,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self::from_plugin_factories(
            plugin_stack.into_factories(),
            runtime_host,
            process_work,
            lease_owner,
        )
    }
}

/// Runs the `SessionTurn` processes of a durable deployment (FIG-5208): it
/// creates each one's child session and mails its turn there, and answers
/// the process from the child turn's end. The process actor drives it
/// ([`SessionTurns`]); the child's session actor runs the turn.
#[derive(Clone)]
pub struct DurableProcessWorker {
    config: Arc<DurableProcessWorkerConfig>,
}

impl DurableProcessWorker {
    pub fn new(config: DurableProcessWorkerConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    pub fn config(&self) -> &DurableProcessWorkerConfig {
        &self.config
    }

    fn process_wiring(&self) -> crate::ProcessWorkWiring {
        self.config.process_work.clone()
    }

    /// Admit this worker's plugin composition against the fleet record
    /// (FIG-4747): the plugins the child sessions this worker creates record
    /// their namespaces under, and the writer format chosen for each.
    ///
    /// # Errors
    /// The store's typed refusal for a plugin that writes no format the
    /// fleet record permits, and any fault reading the record.
    pub async fn admit_plugins(
        &self,
    ) -> Result<crate::store::plugin_writers::PluginAdmission, PluginError> {
        Ok(self
            .config
            .plugin_host
            .admit_plugins(self.config.session_store_factory().as_ref())
            .await?)
    }

    /// The runtime `process` runs under, its plugins admitted.
    async fn runtime(&self, process: &ProcessRecord) -> Result<ProcessRuntimeContext, PluginError> {
        let runtime = Box::pin(ProcessRuntimeContext::for_record(
            ProcessRuntimePorts {
                host: self.config.runtime_host.clone(),
                plugin_host: Arc::clone(&self.config.plugin_host),
                process_work: self.process_wiring(),
                lease_owner: self.config.lease_owner.clone(),
                turn_phase_probe: process.session_capability.as_ref().and_then(|session| {
                    self.config
                        .runtime_host
                        .turn_phase_probes
                        .get_for_scope(&crate::SessionScope::new(session.clone()))
                }),
            },
            process,
        ))
        .await
        .map_err(|err| {
            PluginError::attempt_fault(format!(
                "failed to build the runtime of process `{}`: {err}",
                process.id
            ))
        })?;
        let plugins = self.admit_plugins().await?;
        self.config
            .plugin_host
            .validate_plugin_admission(&plugins)?;
        runtime.adopt_plugin_admission(plugins);
        Ok(runtime)
    }
}

#[lash_core::async_trait]
impl SessionTurns for DurableProcessWorker {
    async fn mail(&self, process: &ProcessRecord) -> Result<SessionTurnMail, PluginError> {
        self.runtime(process)
            .await?
            .mail_session_turn(process)
            .await
    }

    async fn cancel(&self, process: &ProcessRecord) -> Result<SessionTurnCancel, PluginError> {
        self.runtime(process)
            .await?
            .cancel_session_turn(process)
            .await
    }

    async fn outcome(&self, process: &ProcessRecord) -> Result<crate::ProcessOutcome, PluginError> {
        self.runtime(process)
            .await?
            .session_turn_outcome(process)
            .await
    }
}
