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
    /// Clock, stores, effect host and engines. Its attachment
    /// store is rebound to the process as holder when the runtime is built.
    pub host: crate::RuntimeHostConfig,
    pub plugin_host: Arc<crate::PluginHost>,
    pub process_work: crate::ProcessWorkWiring,
    /// The host owner identity the process's session work runs under.
    pub lease_owner: crate::LeaseOwnerIdentity,
    pub turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

/// A process execution's runtime, keyed by its minted id.
pub struct ProcessRuntimeContext {
    process_id: crate::ProcessId,
    services: Arc<RuntimeSessionServices>,
}

impl ProcessRuntimeContext {
    /// Build the runtime `process` runs under: the environment its start
    /// captured — its starter's policy, plugin config and tool authority — and this
    /// worker's ports. An engine process and a session-turn process alike run
    /// under the facts their start recorded; the worker supplies no default
    /// for either (FIG-4396).
    pub async fn for_record(
        ports: ProcessRuntimePorts,
        process: &crate::ProcessRecord,
    ) -> Result<Self, crate::PluginError> {
        let process_id = process.id.clone();
        let Some(env_ref) = process.env_ref.as_ref() else {
            return Err(crate::PluginError::Session(format!(
                "process `{process_id}` is missing a captured execution env"
            )));
        };
        let environment = crate::runtime::load_process_execution_env(
            ports.host.durability.process_env_store.as_ref(),
            env_ref,
        )
        .await?;
        Self::build(ProcessRuntimeBuild {
            process_id,
            environment,
            host: ports.host,
            work: super::host::RuntimeWork::processes(ports.process_work),
            plugin_host: ports.plugin_host,
            lease_owner: ports.lease_owner,
            turn_phase_probe: ports.turn_phase_probe,
        })
    }

    fn build(build: ProcessRuntimeBuild) -> Result<Self, crate::PluginError> {
        let ProcessRuntimeBuild {
            process_id,
            environment,
            host,
            work,
            plugin_host,
            lease_owner,
            turn_phase_probe,
        } = build;
        // The process runs under the budgets its host config records,
        // whatever the supplied plugin host was constructed with.
        let plugins = Arc::unwrap_or_clone(plugin_host)
            .with_execution_budgets(host.control.execution_budgets.clone())
            .isolated_registry()
            .defer_session(crate::plugin::PluginSessionRequest::process_creation(
                process_id.clone(),
                // Every rebuild reads the authority the process captured at creation.
                crate::plugin::SessionAuthorityContext {
                    tool_access: environment.tool_access.clone(),
                    plugin_config: environment.plugin_config.clone(),
                },
            ))?;
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
                host_attachments.policy(),
            )
            .with_reclamation_retry(host_attachments.reclamation_retry()),
        );
        let host = super::host::RuntimeHost { core, work };
        Ok(Self {
            services: Arc::new(RuntimeSessionServices::for_process(ProcessServicesPorts {
                process_id: process_id.clone(),
                environment,
                host,
                plugins,
                runtime_lease_owner: lease_owner,
                turn_phase_probe,
            })),
            process_id,
        })
    }

    /// Adopt `admission`, the plugin admission the running segment's start
    /// recorded (FIG-4747): what the process writes, and every session it
    /// creates, records plugin namespaces in those formats.
    pub fn adopt_plugin_admission(&self, admission: crate::store::plugin_writers::PluginAdmission) {
        self.services.adopt_plugin_admission(admission);
    }
}

impl ProcessRuntimeContext {
    /// The plugin session this runtime's process runs under.
    #[cfg(any(test, feature = "testing"))]
    pub fn plugin_session(&self) -> &Arc<crate::PluginSession> {
        self.services.plugin_session()
    }

    /// The catalog this runtime's process's steps resolve against: its own
    /// plugin session's tools.
    ///
    /// # Errors
    ///
    /// The surface does not resolve.
    pub fn step_catalog(&self) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        self.services.process_step_catalog()
    }

    /// The tools `process`'s steps run under `cx`, the process actor's
    /// claimed context, inside `process`.
    ///
    /// # Errors
    ///
    /// `process` is not this runtime's, or its surface does not resolve.
    pub async fn step_tools(
        &self,
        cx: crate::ActorContext,
        process: &crate::ProcessRecord,
    ) -> Result<lash_core_execution::runtime::process::ProcessStepTools, crate::PluginError> {
        if process.id != self.process_id {
            return Err(crate::PluginError::attempt_fault(format!(
                "the runtime of process `{}` cannot run process `{}`",
                self.process_id, process.id
            )));
        }
        self.services.process_step_tools(cx, process).await
    }
}

/// Everything one process runtime is built from.
struct ProcessRuntimeBuild {
    process_id: crate::ProcessId,
    environment: crate::ProcessExecutionEnvSpec,
    host: crate::RuntimeHostConfig,
    work: super::host::RuntimeWork,
    plugin_host: Arc<crate::PluginHost>,
    lease_owner: crate::LeaseOwnerIdentity,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

impl ProcessRuntimeContext {
    /// The `SessionTurn` input of this runtime's process, or why it is not one.
    fn session_turn<'a>(
        &self,
        process: &'a crate::ProcessRecord,
    ) -> Result<
        (
            &'a crate::SessionCreateRequest,
            &'a crate::TurnInput,
            &'a crate::SessionTurnOutcome,
        ),
        crate::PluginError,
    > {
        if process.id != self.process_id {
            return Err(crate::PluginError::attempt_fault(format!(
                "the runtime of process `{}` cannot run process `{}`",
                self.process_id, process.id
            )));
        }
        match process.input.as_ref() {
            crate::ProcessInput::SessionTurn {
                create_request,
                turn_input,
                result,
                ..
            } => Ok((create_request, turn_input, result)),
            crate::ProcessInput::Engine { kind, .. } => Err(crate::PluginError::Invoke(format!(
                "process `{}` runs engine `{kind}`, which its process actor drives by advance",
                process.id
            ))),
        }
    }

    /// Create `process`'s child session, or find it, and mail its turn's
    /// input to it under the child turn's id (FIG-5208).
    ///
    /// # Errors
    ///
    /// A failure another pass may not meet.
    pub async fn mail_session_turn(
        &self,
        process: &crate::ProcessRecord,
    ) -> Result<lash_core_execution::runtime::actor::process::SessionTurnMail, crate::PluginError>
    {
        let (create_request, turn_input, _) = self.session_turn(process)?;
        self.services
            .mail_process_session_turn(&process.id, create_request.clone(), turn_input.clone())
            .await
    }

    /// Withdraw `process`'s child turn input, or request the child turn's
    /// cancel once a run took it.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn cancel_session_turn(
        &self,
        process: &crate::ProcessRecord,
    ) -> Result<lash_core_execution::runtime::actor::process::SessionTurnCancel, crate::PluginError>
    {
        let (create_request, _, _) = self.session_turn(process)?;
        let requester = process
            .cancel_request
            .as_deref()
            .map(|request| request.requester.clone());
        self.services
            .cancel_process_session_turn(&process.id, create_request.clone(), requester)
            .await
    }

    /// `process`'s answer from its child turn's committed end.
    ///
    /// # Errors
    ///
    /// A store failure, or a child turn that has not ended.
    pub async fn session_turn_outcome(
        &self,
        process: &crate::ProcessRecord,
    ) -> Result<crate::ProcessOutcome, crate::PluginError> {
        let (create_request, _, result) = self.session_turn(process)?;
        self.services
            .process_session_turn_outcome(&process.id, create_request.clone(), result)
            .await
    }
}
