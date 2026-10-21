//! Process engines, linked tool twins, and launches used by in-tree fixtures.

use lash_execution_contracts::testing::HELD_PROCESS_ENGINE_KIND;
use lash_execution_contracts::testing::process_execution_env_fixture_ref;
use std::sync::Arc;

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
    struct Named(Vec<crate::ProcessConfigKey<()>>);
    #[async_trait::async_trait]
    impl crate::ProcessEnvPolicy for Named {
        async fn choose(
            &self,
            cx: &crate::ProcessEnvCx<'_>,
        ) -> Result<crate::ProcessStartChoice, crate::ProcessEnvPolicyError> {
            let run = cx.run.ok_or_else(|| {
                crate::ProcessEnvPolicyError::Fault("fixture requires a recorded run".into())
            })?;
            let mut builder = crate::ProcessEnv::builder(run.config().max_tool_calls);
            for key in &self.0 {
                builder = builder.entry(
                    crate::ProcessComponentEntry::new(*key, &(), std::iter::empty())
                        .map_err(crate::ProcessEnvPolicyError::Refused)?,
                );
            }
            Ok(crate::ProcessStartChoice {
                env: crate::ProcessEnvChoice::New(
                    builder
                        .build()
                        .map_err(crate::ProcessEnvPolicyError::Refused)?,
                ),
                engine_settings: cx.engine_defaults()?,
                initial_resolutions: Vec::new(),
            })
        }
    }
    Arc::new(Named(keys.to_vec()))
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
) -> Result<lash_execution_contracts::process::DeclaredLaunch, crate::HoldEnvError> {
    context
        .declare_engine_launch(
            &crate::ProcessStartChoice {
                initial_resolutions: Default::default(),
                env: crate::ProcessEnvChoice::Published(process_execution_env_fixture_ref()),
                engine_settings: None,
            },
            HELD_PROCESS_ENGINE_KIND,
            payload,
        )
        .await
}

/// Engine fixture whose process runs until it is cancelled, and then answers
/// its cancellation: a process that stays open until the law ends it.
#[cfg(any(test, feature = "testing"))]
pub struct HeldProcessEngine;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for HeldProcessEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        start: &lash_vocabulary::LinkedStart,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link: start.link,
            document: lash_vocabulary::DocumentId::from_bytes([0; 32]),
            selector: lash_execution_contracts::process::linking::ReachSelector::Start(
                start.selector.clone(),
            ),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        HELD_PROCESS_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, lash_execution_contracts::plugin_error::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the held engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
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
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_execution_contracts::process::EngineState,
        event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        let action = match event {
            lash_execution_contracts::process::EngineEvent::Cancelled { .. } => {
                lash_execution_contracts::process::EngineAction::Terminal(
                    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                        crate::ToolCallOutput::cancelled(crate::ToolCancellation::runtime(
                            "the held process was cancelled",
                        )),
                    ),
                )
            }
            _ => lash_execution_contracts::process::EngineAction::Idle,
        };
        Ok((state, action))
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}

/// Engine fixture for start tests that need to exercise the real
/// engine-only start contract without publishing unrelated language artifacts.
#[cfg(any(test, feature = "testing"))]
pub struct FixtureProcessEngine;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for FixtureProcessEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        start: &lash_vocabulary::LinkedStart,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link: start.link,
            document: lash_vocabulary::DocumentId::from_bytes([0; 32]),
            selector: lash_execution_contracts::process::linking::ReachSelector::Start(
                start.selector.clone(),
            ),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        "testing-fixture"
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, lash_execution_contracts::plugin_error::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the fixture engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
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
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_execution_contracts::process::EngineState,
        _event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        Ok((
            state,
            lash_execution_contracts::process::EngineAction::Terminal(
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success(serde_json::json!({ "fixture": "complete" })),
                ),
            ),
        ))
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
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
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
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
    ) -> std::result::Result<
        crate::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let _recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<
            crate::plugin::PluginRecipe,
            lash_execution_contracts::plugin_error::PluginError,
        > = {
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
