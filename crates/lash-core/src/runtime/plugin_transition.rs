//! Native plugin views produced by the engine's existing journal.
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

/// With no fleet store, admit the validated composition with each plugin's
/// native writer before resolving creation config or recording a transition.
pub(super) fn native_plugin_admission(
    host: &crate::PluginHost,
) -> Result<
    crate::store::plugin_writers::PluginAdmission,
    lash_execution_contracts::plugin_error::PluginError,
> {
    let composition = host.composition()?;
    Ok(crate::store::plugin_writers::PluginAdmission::from_plugins(
        composition
            .declarations()
            .iter()
            .map(|declaration| crate::store::plugin_writers::AdmittedPlugin {
                plugin: declaration.id.as_str().into(),
                writer: declaration.format_version,
            })
            .collect(),
    ))
}

pub(super) async fn record_native_transition(
    controller: &crate::ActorContext,
    host: crate::PluginHost,
    request: crate::plugin::PluginTransitionRequest,
    state: crate::PluginState,
    config: crate::PluginConfig,
) -> Result<
    crate::plugin::PluginTransitionRecord,
    lash_execution_contracts::plugin_error::PluginError,
> {
    let request = request
        .clone_in(config.allocation_budget())
        .map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::from(
                crate::StoreError::Allocation(error),
            )
        })?;
    let attribution = match &request.owner {
        crate::RuntimeOwner::Session(id) => crate::RuntimeAttribution::for_session(id.clone()),
        crate::RuntimeOwner::Process(_) => crate::RuntimeAttribution::none(),
    };
    let invocation =
        crate::RuntimeEffectInvocation::new(request.id.0.clone(), attribution, "plugin-transition");
    let answer = controller
        .ingress_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::TransitionPlugins {
                    request: Box::new(request),
                },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(NativeTransitionRunner {
                    host,
                    state,
                    config,
                }),
                None,
            ),
        )
        .await?;
    match answer {
        crate::RuntimeEffectOutcome::TransitionPlugins { record } => {
            record.candidate()?;
            Ok(*record)
        }
        other => Err(crate::RuntimeEffectControllerError::wrong_outcome(
            crate::RuntimeEffectKind::TransitionPlugins,
            other.kind(),
        )
        .into()),
    }
}

struct NativeTransitionRunner {
    host: crate::PluginHost,
    state: crate::PluginState,
    config: crate::PluginConfig,
}
#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for NativeTransitionRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::TransitionPlugins { request } = envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "native transition requires its recorded request",
            ));
        };
        Ok(crate::RuntimeEffectOutcome::TransitionPlugins {
            record: Box::new(
                self.host
                    .transition_plugins(*request, &self.state, &self.config),
            ),
        })
    }
}

impl crate::runtime::LashRuntime {
    /// Record the plugin transition a turn run (fresh or resumed) runs
    /// under, before its configuration is resolved or installed: the
    /// resident config is then the run's snapshot. Nothing is activated
    /// (FIG-6021): [`Self::activate_turn_session`] builds the session after.
    pub(in crate::runtime) async fn transition_turn_plugins(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
    ) -> Result<(), crate::RuntimeError> {
        self.transition_run_plugins(scoped_effect_controller).await
    }

    /// Build the session a turn run runs in over its recorded transition.
    /// Its preparation's terminal error ends the run as `Refused`, in the
    /// run's own commit.
    pub(in crate::runtime) async fn activate_turn_session(
        &mut self,
    ) -> Result<(), crate::RuntimeError> {
        self.activate_run_session().await?;
        self.deliver_undelivered_config_change().await;
        Ok(())
    }

    /// Build the session a command run applies against.
    ///
    /// Either build delivers the config change the session's head still
    /// owes its observers (FIG-5397), and the run's head commit retires it.
    pub(in crate::runtime) async fn materialize_command_session(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
    ) -> Result<(), crate::RuntimeError> {
        self.materialize_run_session(scoped_effect_controller)
            .await?;
        self.deliver_undelivered_config_change().await;
        Ok(())
    }

    /// Publish the native plugin view, then activate the session over it.
    /// A turn run resolves its configuration between the two halves
    /// (FIG-6021), from the transition's config and the built, unreadied
    /// contributions.
    ///
    /// Every run, store-backed or not, records the transition from its
    /// head's namespaces to the plugin set this build admits before any
    /// capability is built: a namespace the head does not record takes its
    /// factory's initial values, so readiness sees what the session's first
    /// commit records (FIG-5324). A run admits the build that runs it, as a
    /// process segment does, never the set its head's view recorded: a
    /// namespace this build does not serve stays byte-for-byte, and a tool
    /// it owned is lost under the run's tool-source policy (FIG-5897). A
    /// build refusal keeps its typed plugin cause.
    ///
    /// Only a session without a store keeps the transition as a native view
    /// in its head. A store-backed head records the namespaces as their own
    /// components and the config in its authority, which every commit keeps
    /// current: a view would copy every namespace into every commit, so a
    /// steady turn would rewrite namespaces it never changed (FIG-5896).
    async fn materialize_run_session(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
    ) -> Result<(), crate::RuntimeError> {
        self.transition_run_plugins(scoped_effect_controller)
            .await?;
        self.activate_run_session().await
    }

    /// The transition half of [`Self::materialize_run_session`]: the run's
    /// plugin namespaces recorded and published to the resident authority,
    /// with no capability built or readied.
    async fn transition_run_plugins(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
    ) -> Result<(), crate::RuntimeError> {
        if self.session.is_some() {
            return Ok(());
        }
        let plugin_error =
            |error| crate::RuntimeEffectControllerError::from(error).into_runtime_error();
        let plugins = &self.services.plugins;
        let target = match &self.services.store {
            Some(store) => plugins
                .host()
                .admit_plugins(store.store().as_ref())
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?,
            None => crate::runtime::plugin_transition::native_plugin_admission(plugins.host())
                .map_err(plugin_error)?,
        };
        let address = crate::EffectAddress::new(
            scoped_effect_controller.execution_scope().clone(),
            "plugin-transition",
        )
        .map_err(crate::RuntimeError::from)?;
        let request = crate::plugin::PluginTransitionRequest::new(
            crate::plugin::PluginTransitionRequestData {
                id: crate::plugin::PluginTransitionId(address),
                owner: crate::RuntimeOwner::Session(self.state.session_id.clone()),
                base: crate::plugin::PluginTransitionBase::Session {
                    head: crate::store::SessionHeadRef {
                        generation: 0,
                        revision: self.state.head_revision,
                        leaf: self.state.session_graph.leaf_node_id.clone(),
                        checkpoint: self.state.checkpoint_ref.clone(),
                    },
                },
                target,
            },
        );
        let record = crate::runtime::plugin_transition::record_native_transition(
            scoped_effect_controller,
            plugins.host().clone(),
            request,
            plugins.export_state(),
            self.state.authority.plugin_config.clone(),
        )
        .await
        .map_err(plugin_error)?;
        if self.is_store_backed() {
            plugins
                .install_plugin_transition(&record)
                .map_err(plugin_error)?;
        } else {
            plugins
                .adopt_plugin_transition(&record)
                .map_err(plugin_error)?;
            if let Some(bytes) = plugins
                .native_view(self.fleet_format())
                .map_err(plugin_error)?
            {
                self.state.set_plugin_admission_snapshot(bytes);
            }
        }
        self.state.authority.plugin_config = (*plugins.admitted_plugin_config().config).clone();
        Ok(())
    }

    /// The activation half of [`Self::materialize_run_session`]: the session
    /// built, readied and restored over the recorded transition.
    async fn activate_run_session(&mut self) -> Result<(), crate::RuntimeError> {
        if self.session.is_some() {
            return Ok(());
        }
        let plugin_error =
            |error| crate::RuntimeEffectControllerError::from(error).into_runtime_error();
        Box::pin(self.materialize_published_session())
            .await
            .map_err(|error| match error {
                crate::SessionError::Plugin(error) => plugin_error(error),
                error => crate::RuntimeError::new(
                    crate::RuntimeErrorCode::SessionHeadRefresh,
                    error.to_string(),
                ),
            })
    }
}
