//! Process engines, linked tool twins, and launches used by in-tree fixtures.

use super::{fixture_engine_launch, process_execution_env_fixture_ref};
use std::sync::Arc;

/// The kind [`HeldProcessEngine`] runs.
#[cfg(any(test, feature = "testing"))]
pub const HELD_PROCESS_ENGINE_KIND: &str = "testing-held";

/// An engine input of [`HeldProcessEngine`]'s kind: the trivial process a law
/// registers when it is about the row rather than about what runs it.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_input(payload: serde_json::Value) -> crate::ProcessInput {
    crate::ProcessInput::Engine {
        kind: HELD_PROCESS_ENGINE_KIND.to_string(),
        payload,
        launch: fixture_engine_launch(),
    }
}

/// `launch` as a declaration states it once its environment is held: what a
/// fixture declares when no start-policy round produced it.
#[cfg(any(test, feature = "testing"))]
pub fn declared_launch_for_testing(launch: crate::EngineLaunch) -> crate::DeclaredLaunch {
    crate::DeclaredLaunch::held(launch)
}

/// A process component of `key`'s id whose only contribution is
/// `provider`'s tools, its config `()` (FIG-6034 §3.2): what a fixture
/// registers so a process whose environment names `key` can call them.
#[cfg(any(test, feature = "testing"))]
#[expect(clippy::expect_used, reason = "a tool provider is a process component")]
pub fn tool_component(
    key: crate::ProcessConfigKey<()>,
    provider: std::sync::Arc<dyn crate::ToolProvider>,
) -> std::sync::Arc<dyn crate::plugin::ProcessComponentFactory> {
    std::sync::Arc::new(
        crate::plugin::StaticPluginFactory::new(
            crate::plugin::PluginDeclaration::initial(key.component()),
            crate::plugin::PluginSpec::new()
                .with_tool_provider(crate::tool_provider_key!("tools"), provider),
        )
        .as_component(key.format())
        .expect("a tool provider is a process component"),
    )
}

/// The session twin of `tool_component`: the same declared owner and provider key.
#[cfg(any(test, feature = "testing"))]
pub fn tool_plugin(
    key: crate::ProcessConfigKey<()>,
    provider: Arc<dyn crate::ToolProvider>,
) -> Arc<dyn crate::plugin::PluginFactory> {
    Arc::new(crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial(key.component()),
        crate::plugin::PluginSpec::new()
            .with_tool_provider(crate::tool_provider_key!("tools"), provider),
    ))
}

/// A start policy that inherits inside a process and, outside one, states
/// the starting turn's run naming each of `keys` with config `()`.
#[cfg(any(test, feature = "testing"))]
pub fn run_policy_naming(
    keys: &[crate::ProcessConfigKey<()>],
) -> std::sync::Arc<dyn crate::ProcessEnvPolicy> {
    let keys = keys.to_vec();
    crate::runtime::env_policy::policy::restrict_or(crate::runtime::env_policy::policy::from_run(
        move |_| {
            keys.iter()
                .map(|key| crate::ProcessComponentEntry::new(*key, &(), std::iter::empty()))
                .collect()
        },
    ))
}

/// A host start of [`held_engine_input`] under the fixture environment,
/// which the host must have published (see [`process_execution_env_fixture`]).
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_request(
    payload: serde_json::Value,
    originator: crate::ProcessOriginator,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessStartRequest {
    crate::ProcessStartRequest::engine(
        HELD_PROCESS_ENGINE_KIND,
        payload,
        fixture_engine_launch(),
        originator,
        lifetime,
    )
}

/// A declared start of [`held_engine_input`]: the declaration a recorded
/// start of the held engine journals, its launch the fixture environment.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_declaration(
    payload: serde_json::Value,
    originator: crate::ProcessOriginator,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessStartDeclaration {
    crate::ProcessStartDeclaration::engine(
        HELD_PROCESS_ENGINE_KIND,
        payload,
        crate::DeclaredLaunch::held(fixture_engine_launch()),
        originator,
        lifetime,
    )
}

/// Hold the fixture environment under `context`'s execution claim for a
/// start of [`held_engine_input`] with `payload`, as a start policy's choice
/// is held (D10): the launch a tool body declares the held engine under.
///
/// # Errors
///
/// The hold's refusal or fault.
#[cfg(any(test, feature = "testing"))]
pub async fn held_engine_launch(
    context: &crate::AttemptContext<'_>,
    payload: &serde_json::Value,
) -> Result<crate::DeclaredLaunch, crate::HoldEnvError> {
    context
        .declare_engine_launch(
            &crate::ProcessStartChoice {
                fits: Default::default(),
                env: crate::ProcessEnvChoice::Published(process_execution_env_fixture_ref()),
                engine_settings: None,
            },
            HELD_PROCESS_ENGINE_KIND,
            payload,
        )
        .await
}

/// A registration of [`held_engine_input`], whose launch names the fixture
/// environment. Publish it with [`process_execution_env_fixture`] in the
/// store receiving the registration.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_registration(
    payload: serde_json::Value,
    provenance: crate::ProcessProvenance,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessRegistration {
    crate::ProcessRegistration::new(held_engine_input(payload), provenance, lifetime)
}

/// Engine fixture whose process runs until it is cancelled, and then answers
/// its cancellation: a process that stays open until the law ends it.
#[cfg(any(test, feature = "testing"))]
pub struct HeldProcessEngine;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl crate::ProcessEngine for HeldProcessEngine {
    fn settled_ran(
        &self,
        _kind: &crate::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
    }

    fn trace_class(&self) -> crate::ProcessTraceClass {
        crate::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<crate::PerformedEffects, crate::ProcessEnvRefusal> {
        Ok(crate::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &crate::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: crate::ArgsMode,
    ) -> std::result::Result<(), crate::ArgsMismatch> {
        Err(crate::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        HELD_PROCESS_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the held engine stores no artifact `{artifact_ref}`"
        )))
    }

    fn state_format(&self) -> crate::EngineStateFormat {
        crate::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&crate::EngineSettings>,
    ) -> std::result::Result<(), crate::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(crate::PluginError::Registration(format!(
                "process engine `{}` takes no settings",
                self.kind()
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Option<crate::EngineSettings>, crate::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: crate::EngineState,
        event: crate::EngineEvent,
    ) -> Result<(crate::EngineState, crate::EngineAction), crate::ProcessInfraError> {
        let action = match event {
            crate::EngineEvent::Cancelled { .. } => crate::EngineAction::Terminal(
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                    crate::ToolCancellation::runtime("the held process was cancelled"),
                )),
            ),
            _ => crate::EngineAction::Idle,
        };
        Ok((state, action))
    }

    async fn resolve(
        &self,
        _reference: &crate::ProcessDefinitionRef,
    ) -> Result<crate::ProcessDefinitionResolution, crate::ProcessDefinitionError> {
        Ok(crate::ProcessDefinitionResolution::new(
            crate::ProcessSignature::Unknown,
        ))
    }
}

/// Engine fixture for start tests that need to exercise the real
/// engine-only start contract without publishing unrelated language artifacts.
#[cfg(any(test, feature = "testing"))]
pub struct FixtureProcessEngine;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl crate::ProcessEngine for FixtureProcessEngine {
    fn settled_ran(
        &self,
        _kind: &crate::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
    }

    fn trace_class(&self) -> crate::ProcessTraceClass {
        crate::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<crate::PerformedEffects, crate::ProcessEnvRefusal> {
        Ok(crate::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &crate::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: crate::ArgsMode,
    ) -> std::result::Result<(), crate::ArgsMismatch> {
        Err(crate::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        "testing-fixture"
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the fixture engine stores no artifact `{artifact_ref}`"
        )))
    }

    fn state_format(&self) -> crate::EngineStateFormat {
        crate::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&crate::EngineSettings>,
    ) -> std::result::Result<(), crate::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(crate::PluginError::Registration(format!(
                "process engine `{}` takes no settings",
                self.kind()
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Option<crate::EngineSettings>, crate::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: crate::EngineState,
        _event: crate::EngineEvent,
    ) -> Result<(crate::EngineState, crate::EngineAction), crate::ProcessInfraError> {
        Ok((
            state,
            crate::EngineAction::Terminal(crate::ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::success(serde_json::json!({ "fixture": "complete" })),
            )),
        ))
    }

    async fn resolve(
        &self,
        _reference: &crate::ProcessDefinitionRef,
    ) -> Result<crate::ProcessDefinitionResolution, crate::ProcessDefinitionError> {
        Ok(crate::ProcessDefinitionResolution::new(
            crate::ProcessSignature::Unknown,
        ))
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn process_engine_fixture() -> crate::ProcessEngineRegistry {
    crate::ProcessEngineRegistry::new()
        .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
            FixtureProcessEngine,
        )))
        .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
            HeldProcessEngine,
        )))
}

#[cfg(any(test, feature = "testing"))]
struct FixtureProcessEnginePlugin;

#[cfg(any(test, feature = "testing"))]
impl crate::SessionPlugin for FixtureProcessEnginePlugin {
    fn id(&self) -> &'static str {
        "testing-fixture-process-engine"
    }
}
impl FixtureProcessEnginePlugin {
    fn declare(
        _reg: &mut crate::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), crate::plugin::PluginError> {
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
struct FixtureProcessEngineFactory;

#[cfg(any(test, feature = "testing"))]
impl crate::PluginFactory for FixtureProcessEngineFactory {
    fn id(&self) -> &'static str {
        "testing-fixture-process-engine"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<crate::plugin::PluginRecipe, crate::plugin::PluginError> {
        let _recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<crate::plugin::PluginRecipe, crate::plugin::PluginError> = {
            let mut reg = crate::plugin::PluginRegistrar::<FixtureProcessEnginePlugin>::new(
                crate::plugin::PluginFactory::id(self.as_ref()),
            );
            FixtureProcessEnginePlugin::declare(&mut reg)?;
            let id = self.id();
            Ok(crate::plugin::PluginRecipe::new(
                id,
                reg,
                move |_context, _state| Ok(Arc::new(FixtureProcessEnginePlugin)),
            ))
        };
        let mut recipe = recipe?;
        recipe = recipe.with_process_engine("testing-fixture", |_| {
            Ok(crate::ProcessEngineRegistration::accepting(Arc::new(
                FixtureProcessEngine,
            )))
        });
        recipe = recipe.with_process_engine(HELD_PROCESS_ENGINE_KIND, |_| {
            Ok(crate::ProcessEngineRegistration::accepting(Arc::new(
                HeldProcessEngine,
            )))
        });
        Ok(recipe)
    }
}

#[cfg(any(test, feature = "testing"))]
impl FixtureProcessEngineFactory {
    pub fn declaration() -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial("testing-fixture-process-engine")
    }
}
impl crate::plugin::PluginMetadata for FixtureProcessEngineFactory {
    fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
        Self::declaration()
    }
}

/// Plugin factory that contributes [`FixtureProcessEngine`] and
/// [`HeldProcessEngine`] to a facade host.
#[cfg(any(test, feature = "testing"))]
pub fn process_engine_plugin_fixture() -> Arc<dyn crate::PluginFactory> {
    Arc::new(FixtureProcessEngineFactory)
}
