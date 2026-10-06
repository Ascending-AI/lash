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
    pub queued_work: Arc<dyn crate::SessionWorkEngine>,
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
    /// Build the runtime `admitted` runs under: the environment its start
    /// captured — its starter's recorded policy and plugin config — and this
    /// worker's ports. An engine process and a session-turn process alike run
    /// under the facts their start recorded; the worker supplies no default
    /// for either (FIG-4396).
    pub async fn for_admitted(
        ports: ProcessRuntimePorts,
        admitted: &crate::runtime::effect::AdmittedProcess,
    ) -> Result<Self, crate::PluginError> {
        let process_id = admitted.process_id.clone();
        let registration = &admitted.registration;
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
        Self::build(ProcessRuntimeBuild {
            process_id,
            environment,
            host: ports.host,
            work: super::host::RuntimeWork::processes(ports.process_work, ports.queued_work),
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
        let plugins = plugin_host.isolated_registry().defer_session(
            crate::plugin::PluginSessionRequest::process_creation(
                process_id.clone(),
                crate::plugin::SessionAuthorityContext {
                    plugin_config: environment.plugin_config.clone(),
                    ..Default::default()
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
            .with_read_policy(host_attachments.read_policy())
            .with_upload_expiry_ms(host_attachments.upload_expiry_ms())
            .with_output_retention(host_attachments.output_retention()),
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

#[async_trait::async_trait]
impl crate::runtime::effect::ProcessRunner for ProcessRuntimeContext {
    async fn run_process(
        &self,
        admitted: crate::runtime::effect::AdmittedProcess,
        execution_context: crate::ProcessExecutionContext,
        registry: Arc<dyn crate::ProcessRegistry>,
        scoped_effect_controller: crate::ActorContext,
        cancellation: tokio_util::sync::CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        if admitted.process_id != self.process_id {
            return Err(crate::ProcessInfraError::new(
                crate::PluginError::attempt_fault(format!(
                    "the runtime of process `{}` cannot run process `{}`",
                    self.process_id, admitted.process_id
                )),
            ));
        }
        let environment = admitted.registration.env_ref.clone().ok_or_else(|| {
            crate::PluginError::attempt_fault("admitted process has no captured environment")
        })?;
        let plugins = self.services.plugins();
        let target = execution_context.plugin_admission.clone().ok_or_else(|| {
            crate::PluginError::attempt_fault("process segment has no recorded plugin admission")
        })?;
        let address = crate::EffectAddress::new(
            scoped_effect_controller.execution_scope().clone(),
            "plugin-transition",
        )
        .map_err(|error| {
            crate::ProcessInfraError::from(crate::PluginError::Runtime(crate::RuntimeError::from(
                error,
            )))
        })?;
        let request = crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(address),
            owner: crate::RuntimeOwner::Process(self.process_id.clone()),
            base: crate::plugin::PluginTransitionBase::Process {
                environment,
                segment: scoped_effect_controller.execution_scope().clone(),
            },
            target,
        };
        let record = super::plugin_transition::record_native_transition(
            &scoped_effect_controller,
            plugins.host().clone(),
            request,
            plugins.export_state(),
            (*plugins.admitted_plugin_config().config).clone(),
        )
        .await?;
        plugins.adopt_plugin_transition(&record)?;
        let state_segment = u32::try_from(execution_context.segment_ordinal).map_err(|_| {
            crate::PluginError::attempt_fault(
                "process segment exceeds the plugin-state segment range",
            )
        })?;
        plugins.adopt_state_segment(crate::tool_run::SegmentOrdinal(state_segment));
        plugins.materialize()?;
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
