//! Native plugin views produced by the engine's existing journal.
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

pub(super) async fn record_native_transition(
    controller: &crate::ScopedEffectController<'_>,
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
        .execute_effect(
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
        let crate::RuntimeEffectCommand::TransitionPlugins { mut request } = envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "native transition requires its recorded request",
            ));
        };
        if request.target.is_empty() {
            request.target = crate::store::plugin_writers::PluginAdmission::from_plugins(
                self.host
                    .factories()
                    .iter()
                    .map(|factory| {
                        let declaration = factory.declaration();
                        crate::store::plugin_writers::AdmittedPlugin {
                            plugin: factory.id().into(),
                            behavior_revision: declaration.behavior_revision,
                            writer: declaration.format_version,
                        }
                    })
                    .collect(),
            );
        }
        Ok(crate::RuntimeEffectOutcome::TransitionPlugins {
            record: Box::new(
                self.host
                    .transition_plugins(*request, &self.state, &self.config),
            ),
        })
    }
}
