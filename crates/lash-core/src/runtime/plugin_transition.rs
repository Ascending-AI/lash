//! Native plugin views produced by the engine's existing journal.
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

/// With no fleet store, admit the validated composition with each plugin's
/// native writer before resolving creation config or recording a transition.
pub(super) fn native_plugin_admission(
    host: &crate::PluginHost,
) -> Result<crate::store::plugin_writers::PluginAdmission, crate::PluginError> {
    let composition = host.composition()?;
    Ok(crate::store::plugin_writers::PluginAdmission::from_plugins(
        composition
            .declarations()
            .iter()
            .map(|declaration| crate::store::plugin_writers::AdmittedPlugin {
                plugin: declaration.id.as_str().into(),
                behavior_revision: declaration.behavior_revision,
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
) -> Result<crate::plugin::PluginTransitionRecord, crate::PluginError> {
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
    /// Build the session a turn run (fresh or resumed) runs in, under the
    /// host's [`ToolSourcePolicy`](crate::ToolSourcePolicy): under `Require`
    /// a run whose restore would lose a recorded member is refused, typed,
    /// before anything is installed (FIG-5134). Its preparation's terminal
    /// error ends the run as `Refused`, in the run's own commit.
    pub(in crate::runtime) async fn materialize_turn_session(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
    ) -> Result<(), crate::RuntimeError> {
        let policy = self.host.core.control.tool_source_policy;
        self.materialize_run_session(scoped_effect_controller, policy)
            .await
    }

    /// Build the session a command run applies against. A command run
    /// tolerates tool loss whatever the host's policy: a host's restore
    /// answers its report instead of being refused (ADR 0119).
    pub(in crate::runtime) async fn materialize_command_session(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
    ) -> Result<(), crate::RuntimeError> {
        self.materialize_run_session(scoped_effect_controller, crate::ToolSourcePolicy::Tolerate)
            .await
    }

    /// Publish the native plugin view before resolving a run's protocol config.
    /// A deferred session supplies no driver or renderer until this activation.
    ///
    /// A store-backed session runs the plugins its head recorded: its
    /// session actor materializes them as admitted, and a build that does
    /// not serve that admission is refused typed (ADR 0132 §4).
    async fn materialize_run_session(
        &mut self,
        scoped_effect_controller: &crate::ActorContext,
        policy: crate::ToolSourcePolicy,
    ) -> Result<(), crate::RuntimeError> {
        if self.session.is_none() {
            if self.is_store_backed() {
                self.require_tool_sources(policy)?;
                return Box::pin(self.materialize_published_session())
                    .await
                    .map_err(|error| match error {
                        crate::SessionError::Plugin(error) => {
                            crate::RuntimeEffectControllerError::from(error).into_runtime_error()
                        }
                        error => crate::RuntimeError::new(
                            crate::RuntimeErrorCode::SessionHeadRefresh,
                            error.to_string(),
                        ),
                    });
            }
            let plugins = &self.services.plugins;
            let target = match plugins.plugin_admission() {
                Some(admission) => admission,
                None => crate::runtime::plugin_transition::native_plugin_admission(plugins.host())
                    .map_err(|error| {
                        crate::RuntimeEffectControllerError::from(error).into_runtime_error()
                    })?,
            };
            let address = crate::EffectAddress::new(
                scoped_effect_controller.execution_scope().clone(),
                "plugin-transition",
            )
            .map_err(crate::RuntimeError::from)?;
            let request = crate::plugin::PluginTransitionRequest {
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
            };
            let record = crate::runtime::plugin_transition::record_native_transition(
                scoped_effect_controller,
                plugins.host().clone(),
                request,
                plugins.export_state(),
                self.state.authority.plugin_config.clone(),
            )
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })?;
            plugins.adopt_plugin_transition(&record).map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })?;
            if let Some(bytes) = plugins.native_view(self.fleet_format()).map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })? {
                self.state.set_plugin_admission_snapshot(bytes);
            }
            self.state.authority.plugin_config = (*plugins.admitted_plugin_config().config).clone();
            self.require_tool_sources(policy)?;
            Box::pin(self.materialize_published_session())
                .await
                .map_err(|error| {
                    crate::RuntimeError::new(
                        crate::RuntimeErrorCode::SessionHeadRefresh,
                        error.to_string(),
                    )
                })?;
        }
        Ok(())
    }

    /// Under `Require`, refuse when restoring the session's recorded tool
    /// state over the materialized plugins' sources would lose a member. It
    /// previews the restore without changing the registry, so a refused run
    /// installs nothing, restores no protocol session and emits no
    /// `SessionRestored`. Parked opt-outs and superseded identities never
    /// refuse: neither is a capability the session lost.
    fn require_tool_sources(
        &self,
        policy: crate::ToolSourcePolicy,
    ) -> Result<(), crate::RuntimeError> {
        let (crate::ToolSourcePolicy::Require, Some(snapshot)) =
            (policy, self.state.tool_state_snapshot())
        else {
            return Ok(());
        };
        let plugin_error =
            |error| crate::RuntimeEffectControllerError::from(error).into_runtime_error();
        self.services.plugins.materialize().map_err(plugin_error)?;
        let report = self
            .services
            .plugins
            .tool_registry()
            .preview_restore(snapshot)
            .map_err(|error| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::Plugin,
                    format!("tool restore preview failed: {error}"),
                )
            })?;
        if report.has_lost_members() {
            return Err(
                crate::RuntimeEffectControllerError::tool_sources_unavailable(
                    &self.state.session_id,
                    report,
                )
                .into_runtime_error(),
            );
        }
        Ok(())
    }
}
