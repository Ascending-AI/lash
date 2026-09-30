//! The runtime a process execution runs under, keyed by its minted id.
//!
//! A process is not a session: it has no session state, no session store, no
//! agent frame and no catalog row. Its runtime is built from the host's ports,
//! the environment its start captured and the controller the worker admitted
//! for it. Every session-only service refuses it with
//! [`crate::PluginError::NotASessionRuntime`].

use std::sync::Arc;

use super::session_manager::{ProcessServicesPorts, RuntimeSessionServices};

/// The host's ports a process runtime is built from.
pub struct ProcessRuntimePorts {
    /// Clock, stores, effect host, tool children and engines. Its attachment
    /// store is rebound to the process as holder when the runtime is built.
    pub host: crate::RuntimeHostConfig,
    pub plugin_host: Arc<crate::PluginHost>,
    pub process_work: crate::ProcessWorkWiring,
    pub queued_work: Arc<dyn crate::SessionWorkEngine>,
    /// The host owner identity the process's session work runs under.
    pub lease_owner: crate::LeaseOwnerIdentity,
    pub turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

/// What a process runs, read from its admitted registration.
enum ProcessRuntimeBody {
    /// An engine or tool-call process: the environment loaded from its
    /// recorded `env_ref`, with its plugin session built for
    /// `RuntimeOwner::Process(id)`.
    Captured {
        environment: crate::ProcessExecutionEnvSpec,
    },
    /// A session-turn process: ports only. Its child session is created
    /// through the host's session work, never admitted by the worker.
    SessionTurn {
        default_policy: crate::SessionPolicy,
    },
}

/// A process execution's runtime, keyed by its minted id.
pub struct ProcessRuntimeContext {
    process_id: crate::ProcessId,
    services: Arc<RuntimeSessionServices>,
}

impl ProcessRuntimeContext {
    /// Build the runtime `admitted` runs under. An engine or tool-call
    /// process loads the environment its start captured; a session-turn
    /// process runs on `default_policy`, which fills the provider its
    /// recorded create request omitted.
    pub async fn for_admitted(
        ports: ProcessRuntimePorts,
        admitted: &crate::runtime::effect::AdmittedProcess,
        default_policy: crate::SessionPolicy,
    ) -> Result<Self, crate::PluginError> {
        let process_id = admitted.process_id.clone();
        let registration = &admitted.registration;
        let body = match registration.input.as_ref() {
            crate::ProcessInput::Engine { .. } => {
                let Some(env_ref) = registration.env_ref.as_ref() else {
                    return Err(crate::PluginError::Session(format!(
                        "process `{process_id}` is missing a captured execution env"
                    )));
                };
                let environment = crate::runtime::load_process_execution_env(
                    ports.host.durability.process_env_store.as_ref(),
                    env_ref,
                )
                .await?;
                ProcessRuntimeBody::Captured { environment }
            }
            crate::ProcessInput::SessionTurn { .. } => {
                ProcessRuntimeBody::SessionTurn { default_policy }
            }
            // Externally-owned rows are rejected before dispatch (ADR 0110):
            // lash never executes them, so they have no runtime.
            crate::ProcessInput::External { .. } => {
                return Err(crate::PluginError::Session(format!(
                    "process `{process_id}` is externally-owned and has no execution runtime"
                )));
            }
            // Registration refuses an unresolved start by id, so no row
            // holds one.
            crate::ProcessInput::Definition { definition_id, .. } => {
                return Err(crate::PluginError::Session(format!(
                    "process `{process_id}` names definition `{definition_id}` unresolved"
                )));
            }
        };
        let (environment, policy, plugin_options) = match body {
            ProcessRuntimeBody::Captured { environment } => {
                let policy = environment.policy.clone();
                let plugin_options = environment.plugin_options.clone();
                (Some(environment), policy, plugin_options)
            }
            ProcessRuntimeBody::SessionTurn { default_policy } => {
                (None, default_policy, crate::PluginOptions::default())
            }
        };
        Self::build(ProcessRuntimeBuild {
            process_id,
            environment,
            policy,
            plugin_options,
            host: ports.host,
            work: super::host::RuntimeWork::processes(ports.process_work, ports.queued_work),
            plugin_host: ports.plugin_host,
            lease_owner: ports.lease_owner,
            turn_phase_probe: ports.turn_phase_probe,
        })
    }

    /// The runtime a group tool child a process opened runs under when the
    /// process's body is not live where the child runs: the environment the
    /// child was admitted under, and this environment's wiring.
    pub fn for_tool_child(
        runtime_env: &super::RuntimeEnvironment,
        plugin_host: Arc<crate::PluginHost>,
        process_id: crate::ProcessId,
        environment: crate::ProcessExecutionEnvSpec,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, crate::PluginError> {
        let policy = environment.policy.clone();
        let plugin_options = environment.plugin_options.clone();
        Self::build(ProcessRuntimeBuild {
            process_id,
            environment: Some(environment),
            policy,
            plugin_options,
            host: runtime_env.core.clone(),
            work: runtime_env.work.clone(),
            plugin_host,
            lease_owner,
            turn_phase_probe: None,
        })
    }

    fn build(build: ProcessRuntimeBuild) -> Result<Self, crate::PluginError> {
        let ProcessRuntimeBuild {
            process_id,
            environment,
            policy,
            plugin_options,
            host,
            work,
            plugin_host,
            lease_owner,
            turn_phase_probe,
        } = build;
        let plugins = plugin_host.isolated_registry().build_session(
            crate::plugin::PluginSessionRequest::process_creation(
                process_id.clone(),
                crate::plugin::SessionCreationConfig {
                    authority: crate::plugin::SessionAuthorityContext {
                        plugin_options,
                        ..Default::default()
                    },
                    protocol_turn_options: Default::default(),
                },
            ),
        )?;
        // The process's attachments are held by its own record: a put it
        // makes names `ProcessRecord(id)` as its referrer, and the cleanup its
        // terminal publication plans ends that edge (ADR 0124).
        let mut core = host;
        let host_attachments = Arc::clone(&core.durability.attachment_store);
        core.durability.attachment_store = Arc::new(
            crate::RuntimeAttachmentStore::new_with_clock(
                Arc::clone(host_attachments.backend()),
                core.backend().attachment_referrers(),
                crate::RuntimeOwner::Process(process_id.clone()),
                Arc::clone(&core.clock),
            )
            .with_max_attachment_bytes(host_attachments.max_attachment_bytes())
            .with_upload_expiry_ms(host_attachments.upload_expiry_ms())
            .with_output_retention(host_attachments.output_retention()),
        );
        let host = super::host::RuntimeHost { core, work };
        Ok(Self {
            services: Arc::new(RuntimeSessionServices::for_process(ProcessServicesPorts {
                process_id: process_id.clone(),
                environment,
                policy,
                host,
                plugins,
                runtime_lease_owner: lease_owner,
                turn_phase_probe,
            })),
            process_id,
        })
    }

    /// The dispatch context of a group tool child this process opened, its
    /// controller slots filled by `lent_controller` until the driver rebinds
    /// them to the child's own.
    pub fn tool_child_dispatch(
        &self,
        lent_controller: crate::ScopedEffectController<'static>,
    ) -> Result<crate::tool_dispatch::ToolDispatchContext<'static>, crate::PluginError> {
        self.services.tool_child_dispatch(lent_controller)
    }
}

/// Everything one process runtime is built from.
struct ProcessRuntimeBuild {
    process_id: crate::ProcessId,
    environment: Option<crate::ProcessExecutionEnvSpec>,
    policy: crate::SessionPolicy,
    plugin_options: crate::PluginOptions,
    host: crate::RuntimeHostConfig,
    work: super::host::RuntimeWork,
    plugin_host: Arc<crate::PluginHost>,
    lease_owner: crate::LeaseOwnerIdentity,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

#[async_trait::async_trait]
impl crate::runtime::effect::ProcessRunner for ProcessRuntimeContext {
    async fn run_process(
        &self,
        admitted: crate::runtime::effect::AdmittedProcess,
        execution_context: crate::ProcessExecutionContext,
        registry: Arc<dyn crate::ProcessRegistry>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: tokio_util::sync::CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        if admitted.process_id != self.process_id {
            return Err(crate::ProcessInfraError::new(crate::PluginError::Session(
                format!(
                    "the runtime of process `{}` cannot run process `{}`",
                    self.process_id, admitted.process_id
                ),
            )));
        }
        self.services
            .run_admitted_process(
                admitted,
                execution_context,
                registry,
                scoped_effect_controller,
                cancellation,
                handover,
            )
            .await
    }
}
