//! Existing local-effect recording used by the plugin-state fixtures.
use super::*;
use std::future::Future;

struct FixtureRunner<F> {
    body: F,
}

#[async_trait::async_trait]
impl<F> crate::core_internal::RuntimeEffectLocalRunner for FixtureRunner<F>
where
    F: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>> + Send + 'static,
{
    async fn execute(
        self: Box<Self>,
        _: RuntimeEffectEnvelope,
        _: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.body.await
    }
}

/// Publish `commands` to the fixture plugin's namespace the way a body of its
/// tool does: reduced inside a recorded body named `name`, and published once
/// that body's outcome returns.
#[expect(
    clippy::unwrap_used,
    reason = "fixture effect identities and outcomes are asserted"
)]
pub(super) async fn publish(
    plugins: &Arc<crate::PluginSession>,
    plugin: &str,
    name: &str,
    commands: lash_core::StateCommands,
) -> RuntimeEffectOutcome {
    let address = crate::EffectAddress::new(
        crate::ExecutionScope::turn(
            SessionId::fixture(owner_key(plugins.owner())),
            "plugin-state-law",
        ),
        name,
    )
    .unwrap();
    lash_core::testing::runtime_internals::publish_plugin_state(plugins, plugin, address, commands)
        .await
        .unwrap()
}

#[expect(
    clippy::unwrap_used,
    reason = "fixture transition identities and outcomes are asserted"
)]
pub(super) async fn transition(
    host: &crate::PluginHost,
    id: &str,
    state: &crate::PluginState,
    config: &crate::PluginConfig,
) -> crate::plugin::PluginTransitionRecord {
    let request = crate::plugin::PluginTransitionRequest {
        id: crate::plugin::PluginTransitionId(
            crate::EffectAddress::new(
                crate::ExecutionScope::turn(SessionId::fixture(id), "plugin-state-law"),
                "plugin-transition",
            )
            .unwrap(),
        ),
        owner: crate::RuntimeOwner::Session(SessionId::fixture(id)),
        base: crate::plugin::PluginTransitionBase::Session {
            head: crate::store::SessionHeadRef {
                generation: 0,
                revision: 0,
                leaf: None,
                checkpoint: None,
            },
        },
        target: crate::store::plugin_writers::PluginAdmission::from_plugins(
            host.factories()
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
        ),
    };
    let address = request.id.0.clone();
    let host = host.clone();
    let state = state.clone();
    let config = config.clone();
    let envelope = RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            address,
            crate::RuntimeAttribution::none(),
            "plugin-transition",
        ),
        RuntimeEffectCommand::TransitionPlugins {
            request: Box::new(request.clone()),
        },
    );
    let outcome = crate::testing::execute_effect_locally(
        envelope,
        crate::core_internal::owned_runner_executor(
            Box::new(FixtureRunner {
                body: async move {
                    Ok(RuntimeEffectOutcome::TransitionPlugins {
                        record: Box::new(host.transition_plugins(request, &state, &config)),
                    })
                },
            }),
            None,
        ),
    )
    .await
    .unwrap();
    let RuntimeEffectOutcome::TransitionPlugins { record } = outcome else {
        panic!("transition outcome")
    };
    *record
}

#[expect(
    clippy::unwrap_used,
    reason = "fixture transition and construction outcomes are asserted"
)]
pub(super) async fn construct(
    host: &crate::PluginHost,
    id: &str,
    snapshot: Option<&crate::PluginState>,
    authority: SessionAuthorityContext,
) -> Arc<crate::PluginSession> {
    let config = authority.plugin_config.config.as_ref().clone();
    let record = transition(host, id, snapshot.unwrap_or(&Default::default()), &config).await;
    let request = match snapshot {
        Some(state) => {
            PluginSessionRequest::rematerialization(SessionId::fixture(id), state, authority)
        }
        None => PluginSessionRequest::creation(SessionId::fixture(id), authority),
    };
    let plugins = host.defer_session(request).unwrap();
    plugins.adopt_plugin_transition(&record).unwrap();
    plugins.materialize().unwrap();
    plugins
}
