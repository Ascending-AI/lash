//! The runtimes a process execution runs under, keyed by its minted id.
//!
//! A process is not a session: it has no session state, no session store, no
//! agent frame and no catalog row. Every session-only service refuses it with
//! [`lash_execution_contracts::plugin_error::PluginError::NotASessionRuntime`].
//!
//! - An engine process runs under [`EngineProcessRuntime`]: the environment
//!   its start stated, built only from the process components that
//!   environment names, in its order (FIG-6034). Nothing of a session, no
//!   session plugin and no protocol factory builds.
//! - A session-turn process runs under [`SessionTurnRuntime`]: the host's
//!   session plugin host and the plugin admission its segment adopted, from
//!   which its child session is created (D5). It has no environment and no
//!   composition.

use std::sync::Arc;

use super::session_manager::{EngineProcessParts, ProcessServicesPorts, RuntimeSessionServices};

/// The host's ports a process runtime is built from.
pub struct ProcessRuntimePorts {
    /// Clock, stores, effect host, engines and the process components. Its
    /// attachment store is rebound to the process as holder when the
    /// runtime is built.
    pub host: crate::RuntimeHostConfig,
    /// The deployment's session plugin host: what a session the process
    /// creates resolves against.
    pub plugin_host: Arc<crate::PluginHost>,
    pub process_work: crate::ProcessWorkWiring,
    /// The host owner identity the process's session work runs under.
    pub lease_owner: crate::LeaseOwnerIdentity,
    pub turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

/// An engine process's runtime: its recorded environment's composition.
pub struct EngineProcessRuntime {
    process_id: lash_vocabulary::identity::ProcessId,
    services: Arc<RuntimeSessionServices>,
}

impl EngineProcessRuntime {
    /// Build the runtime `process` runs under (§5.1): its recorded
    /// environment, verified against its reference, and the composition only
    /// that environment's components make, in its order. No policy, helper
    /// or default runs, and no session is read (§6.1).
    ///
    /// # Errors
    ///
    /// `process` is not an engine process, its environment does not load,
    /// or a component it names does not build here.
    pub async fn for_record(
        ports: ProcessRuntimePorts,
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<Self, lash_execution_contracts::plugin_error::PluginError> {
        let process_id = process.id.clone();
        let Some(env_ref) = process.env_ref() else {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "process `{process_id}` runs a session turn, which has no environment"
                )),
            );
        };
        use lash_core_execution::runtime::actor::process::component_formats::{
            recorded_component_cut, recorded_component_state,
        };
        let backend = ports.host.backend();
        let cut = recorded_component_cut(backend.durable().as_ref(), process)
            .await
            .map_err(lash_execution_contracts::plugin_error::registry_error)?;
        let (env, snapshot) = match cut {
            Some(cut) => (cut.env, Some(cut.state)),
            None => (
                lash_execution_contracts::process::load_process_env(
                    ports.host.durability.process_env_store.as_ref(),
                    env_ref,
                )
                .await?,
                None,
            ),
        };
        let snapshot =
            recorded_component_state(backend.durable().as_ref(), process, &env, snapshot)
                .await
                .map_err(lash_execution_contracts::plugin_error::registry_error)?;
        let env = Arc::new(env);
        let components = ports.host.process_engines.components();
        let composition = components
            .compose_with_state(&env, Some(&process_id), &snapshot)
            .map_err(|refusal| {
                lash_execution_contracts::plugin_error::PluginError::ProcessEnvRefused { refusal }
            })?;
        let state = Arc::new(crate::plugin::ComponentState::new(
            process_id.clone(),
            &composition,
            ports.host.tracing.evidence_limits(),
            ports.host.tracing.diagnostic_limits(),
            ports.host.data_limits,
        ));
        let policy = direct_policy(&env);
        let engine = Arc::new(EngineProcessParts {
            resolutions: Arc::default(),
            env,
            composition,
            state,
        });
        Ok(Self {
            services: process_services(&process_id, ports, Some(engine), Some(policy)),
            process_id,
        })
    }

    /// The composition's bindings and format requirements, without building
    /// its tool catalog. A successor checks these before committing its env.
    pub fn step_context(
        &self,
    ) -> Arc<dyn lash_core_execution::runtime::process::steps::ProcessStepContext> {
        Arc::clone(&self.services) as _
    }

    /// The tools `process`'s steps run under `cx`, the process actor's
    /// claimed context, inside `process`.
    ///
    /// # Errors
    ///
    /// `process` is not this runtime's, or its catalog does not build.
    pub async fn step_tools(
        &self,
        cx: crate::ActorContext,
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<
        lash_core_execution::runtime::process::ProcessStepTools,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        require_own(&self.process_id, process)?;
        self.services.process_step_tools(cx, process).await
    }
}

/// A session-turn process's runtime: the host's session plugins, from which
/// its child session is created (D5).
pub struct SessionTurnRuntime {
    process_id: lash_vocabulary::identity::ProcessId,
    services: Arc<RuntimeSessionServices>,
}

impl SessionTurnRuntime {
    /// Build the runtime the session-turn `process` runs under: the host's
    /// ports and session plugin host, with no environment and no
    /// composition.
    ///
    /// # Errors
    ///
    /// `process` is not a session-turn process.
    pub fn for_record(
        ports: ProcessRuntimePorts,
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<Self, lash_execution_contracts::plugin_error::PluginError> {
        let process_id = process.id.clone();
        if let lash_execution_contracts::process::ProcessInput::Engine { kind, .. } =
            process.input.as_ref()
        {
            return Err(lash_execution_contracts::plugin_error::PluginError::Invoke(
                format!(
                    "process `{process_id}` runs engine `{kind}`, which its process actor drives by \
                 advance"
                ),
            ));
        }
        Ok(Self {
            services: process_services(&process_id, ports, None, None),
            process_id,
        })
    }

    /// Adopt `admission`, the plugin admission the running segment's start
    /// recorded (FIG-4747): every session the process creates records plugin
    /// namespaces in those formats.
    pub fn adopt_plugin_admission(&self, admission: crate::store::plugin_writers::PluginAdmission) {
        self.services.adopt_plugin_admission(admission);
    }

    /// The `SessionTurn` input of this runtime's process, or why it is not one.
    fn session_turn<'a>(
        &self,
        process: &'a lash_execution_contracts::process::ProcessRecord,
    ) -> Result<
        (
            &'a lash_execution_contracts::session_types::SessionCreateRequest,
            &'a crate::TurnInput,
            &'a lash_execution_contracts::process::SessionTurnOutcome,
        ),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        require_own(&self.process_id, process)?;
        match process.input.as_ref() {
            lash_execution_contracts::process::ProcessInput::SessionTurn {
                create_request,
                turn_input,
                result,
                ..
            } => Ok((create_request, turn_input, result)),
            lash_execution_contracts::process::ProcessInput::Engine { kind, .. } => Err(
                lash_execution_contracts::plugin_error::PluginError::Invoke(format!(
                    "process `{}` runs engine `{kind}`, which its process actor drives by advance",
                    process.id
                )),
            ),
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
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<
        lash_core_execution::runtime::actor::process::SessionTurnMail,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (create_request, turn_input, _) = self.session_turn(process)?;
        // The child turn joins the process's trace: it runs on the
        // process's behalf.
        self.services
            .mail_process_session_turn(
                &process.id,
                create_request.clone(),
                turn_input.clone(),
                process
                    .trace
                    .as_ref()
                    .and_then(lash_trace::DurableTraceScope::as_parent),
            )
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
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<
        lash_core_execution::runtime::actor::process::SessionTurnCancel,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<
        lash_execution_contracts::process::ProcessOutcome,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (create_request, _, result) = self.session_turn(process)?;
        self.services
            .process_session_turn_outcome(&process.id, create_request.clone(), result)
            .await
    }
}

fn require_own(
    process_id: &lash_vocabulary::identity::ProcessId,
    process: &lash_execution_contracts::process::ProcessRecord,
) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
    if process.id == *process_id {
        return Ok(());
    }
    Err(
        lash_execution_contracts::plugin_error::PluginError::attempt_fault(format!(
            "the runtime of process `{process_id}` cannot run process `{}`",
            process.id
        )),
    )
}

/// The services a process runtime runs through, keyed by `process_id`.
fn process_services(
    process_id: &lash_vocabulary::identity::ProcessId,
    ports: ProcessRuntimePorts,
    engine: Option<Arc<EngineProcessParts>>,
    policy: Option<crate::SessionPolicy>,
) -> Arc<RuntimeSessionServices> {
    let ProcessRuntimePorts {
        host: mut core,
        plugin_host,
        process_work,
        lease_owner,
        turn_phase_probe,
    } = ports;
    // The process's attachments are held by its own record: a put it makes
    // names `ProcessRecord(id)` as its referrer, and the cleanup its terminal
    // publication plans ends that edge (ADR 0124).
    let host_attachments = Arc::clone(&core.durability.attachment_store);
    core.durability.attachment_store = Arc::new(
        crate::RuntimeAttachmentStore::new_with_clock(
            Arc::clone(host_attachments.backend()),
            core.backend().attachment_referrers(),
            lash_vocabulary::identity::RuntimeOwner::Process(process_id.clone()),
            Arc::clone(&core.clock),
            host_attachments.policy(),
        )
        .with_reclamation_retry(host_attachments.reclamation_retry()),
    );
    // A session the process creates runs under the budgets the host config
    // records, whatever the supplied plugin host was constructed with.
    let plugin_host = Arc::new(
        Arc::unwrap_or_clone(plugin_host)
            .with_execution_budgets(core.control.execution_budgets.clone()),
    );
    let host = super::host::RuntimeHost {
        core,
        work: super::host::RuntimeWork::processes(process_work),
    };
    Arc::new(RuntimeSessionServices::for_process(ProcessServicesPorts {
        process_id: process_id.clone(),
        engine,
        policy,
        host,
        plugin_host,
        runtime_lease_owner: lease_owner,
        turn_phase_probe,
    }))
}

/// The policy an engine process's direct calls run under: its environment's
/// model, generation, charge safety, attachment rules and tool-call limit.
/// A process has no turn, so the turn and progress bounds of the policy are
/// never read; they are stated unbounded only to complete the value.
fn direct_policy(env: &crate::ProcessEnv) -> crate::SessionPolicy {
    let mut policy = crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        env.max_tool_calls(),
        crate::NoProgressBudget::Unbounded,
    );
    policy.attachment_acceptance = Arc::clone(env.attachments());
    if let Some(model) = env.model() {
        policy.model = Some(model.profile().clone());
        policy.generation = model.generation().clone();
        policy.charge_safety = model.charge_safety().clone();
    }
    policy
}
