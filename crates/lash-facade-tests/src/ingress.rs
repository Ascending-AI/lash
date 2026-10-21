use super::*;
use lash_execution_contracts::process::ProcessRegistry;

use lash_vocabulary::identity::ProcessId;

use lash_vocabulary::identity::SessionId;

pub const SESSION: &str = "intent-ingress-session";
pub const SCOPE: &str = "intent-ingress-turn";

/// The id of the process a start intent registered, read off the handle its
/// executed outcome answers.
pub fn started_process_id(outcome: &lash::tools::ToolIntentIngressOutcome) -> ProcessId {
    let lash::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed {
                realized:
                    lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::StartProcess(
                        result,
                    ),
                ..
            },
        ..
    } = outcome
    else {
        panic!("the start executed: {outcome:?}");
    };
    result.process_id.clone()
}

/// Every process the registry holds.
pub async fn registered_process_count(registry: &Arc<dyn ProcessRegistry>) -> Result<usize> {
    Ok(lash_core::testing::process_roster_records_for_fixture(
        registry.as_ref(),
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            ..lash_execution_contracts::process::ProcessListFilter::default()
        },
    )
    .await?
    .len())
}

/// A held process start under the fixture environment.
pub fn start_intent(session_id: &SessionId) -> lash_execution_contracts::tool_intent::ToolIntent {
    lash_execution_contracts::tool_intent::ToolIntent::StartProcess(Box::new(
        lash_execution_contracts::tool_intent::StartProcessIntent {
            owner: lash_vocabulary::identity::RuntimeOwner::Session(session_id.clone()),
            declaration: lash_execution_contracts::testing::held_engine_declaration(
                serde_json::Value::Null,
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::Lifetime::Detached,
            ),
        },
    ))
}

pub const INGRESS_ENGINE_KIND: &str = "ingress-admission-engine";

/// Engine registered on the ingress host, so a submitted start can be checked
/// against a kind that exists and one that does not. With `defaults` it
/// takes settings and answers those as its node's own.
struct IngressAdmissionEngine {
    defaults: Option<serde_json::Value>,
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for IngressAdmissionEngine {
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
        link: lash_vocabulary::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> std::result::Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link,
            document: lash_vocabulary::DocumentId::from_bytes([0; 32]),
            selector: selector.clone(),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "generic fixture has no linked code".into(),
            },
        )
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
        INGRESS_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Vec<lash_core::ArtifactName>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError>
    {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        unreachable!("the ingress engine stores no artifacts")
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
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match (settings, &self.defaults) {
            (None, _) | (Some(_), Some(_)) => Ok(()),
            (Some(_), None) => Err(
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
        Ok(self
            .defaults
            .clone()
            .map(lash_execution_contracts::process::EngineSettings::new))
    }

    fn advance(
        &self,
        state: lash_execution_contracts::process::EngineState,
        event: lash_execution_contracts::process::EngineEvent,
    ) -> std::result::Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        // The `refuse` program's first transition is refused: its process parks.
        if let lash_execution_contracts::process::EngineEvent::Started { payload } = &event
            && payload.get("program").and_then(serde_json::Value::as_str) == Some("refuse")
        {
            return Err(lash_execution_contracts::process::ProcessInfraError::new(
                lash_execution_contracts::plugin_error::PluginError::Session(
                    "the ingress engine refuses this program".into(),
                ),
            ));
        }
        Ok((
            state,
            lash_execution_contracts::process::EngineAction::Terminal(
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(
                        serde_json::json!({"ingress_engine": "ran"}),
                    ),
                ),
            ),
        ))
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> std::result::Result<
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

pub fn admit_ingress_engine(
    _kind: &'static str,
    payload: &serde_json::Value,
) -> std::result::Result<
    lash_execution_contracts::process::ProcessIdentity,
    lash_execution_contracts::plugin_error::PluginError,
> {
    if payload.get("program").and_then(serde_json::Value::as_str) == Some("invalid") {
        return Err(
            lash_execution_contracts::plugin_error::PluginError::Session(
                "invalid ingress engine program".to_owned(),
            ),
        );
    }
    let draft = lash_execution_contracts::process::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({ "payload": payload }),
        [],
    )
    .map_err(|error| {
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string())
    })?;
    let mut identity = lash_execution_contracts::process::ProcessIdentity::labelled(
        INGRESS_ENGINE_KIND,
        payload.get("program").and_then(serde_json::Value::as_str),
    );
    identity.definition_id = Some(draft.id());
    Ok(identity)
}

struct IngressAdmissionEnginePlugin;

impl lash_core::plugin::SessionPlugin for IngressAdmissionEnginePlugin {
    fn id(&self) -> &'static str {
        "ingress-admission-engine-factory"
    }
}
impl IngressAdmissionEnginePlugin {
    fn declare(
        _reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        Ok(())
    }
}

pub struct IngressAdmissionEngineFactory {
    pub defaults: Option<serde_json::Value>,
}

impl lash_core::plugin::PluginFactory for IngressAdmissionEngineFactory {
    fn id(&self) -> &'static str {
        "ingress-admission-engine-factory"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> = {
            let mut reg = lash::plugins::PluginRegistrar::<IngressAdmissionEnginePlugin>::new(
                lash::plugins::PluginFactory::id(self.as_ref()),
            );
            IngressAdmissionEnginePlugin::declare(&mut reg)?;
            let id = self.id();
            Ok(lash::plugins::PluginRecipe::new(
                id,
                reg,
                move |_ctx, _state| Ok(Arc::new(IngressAdmissionEnginePlugin)),
            ))
        };
        let mut recipe = recipe?;
        let engine_factory = Arc::clone(&recipe_factory);
        recipe = recipe.with_process_engine(INGRESS_ENGINE_KIND, move |context| {
            engine_factory.bind_process_engine(context)
        });
        Ok(recipe)
    }
}
impl IngressAdmissionEngineFactory {
    pub fn bind_process_engine(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<
        lash_core::ProcessEngineRegistration,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        lash_core::ProcessEngineRegistration::new(
            Arc::new(IngressAdmissionEngine {
                defaults: self.defaults.clone(),
            }),
            lash_execution_contracts::process::ProcessEngineAdmission::new(
                INGRESS_ENGINE_KIND,
                admit_ingress_engine,
            ),
        )
    }
}

impl IngressAdmissionEngineFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("ingress-admission-engine-factory")
    }
}
impl lash_core::plugin::PluginMetadata for IngressAdmissionEngineFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

pub async fn ingress_engine_core(
    backend: lash_execution_contracts::backend::Backend,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    ingress_engine_core_with(backend, |builder| builder).await
}

/// A core over `backend` serving the ingress engine, whose node defaults are
/// `defaults`.
pub fn ingress_node_builder(
    backend: &lash_execution_contracts::backend::Backend,
    defaults: Option<serde_json::Value>,
) -> lash::LashCoreBuilder {
    explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(IngressAdmissionEngineFactory { defaults }))
}

/// [`ingress_engine_core`] with the host's own additions to its builder.
pub async fn ingress_engine_core_with(
    backend: lash_execution_contracts::backend::Backend,
    configure: impl FnOnce(lash::LashCoreBuilder) -> lash::LashCoreBuilder,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    let registry = backend.process_registry();
    let core =
        configure(ingress_node_builder(&backend, None)).build(super::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_execution_contracts::testing::process_env_fixture(),
        )
        .await?;
    let _session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse(SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    Ok((core, registry))
}

pub fn engine_start_intent(
    kind: &str,
    payload: serde_json::Value,
) -> lash_execution_contracts::tool_intent::ToolIntent {
    lash_execution_contracts::tool_intent::ToolIntent::StartProcess(Box::new(
        lash_execution_contracts::tool_intent::StartProcessIntent {
            owner: lash_vocabulary::identity::RuntimeOwner::Session(SessionId::fixture(
                SESSION.to_string(),
            )),
            declaration: lash_execution_contracts::process::ProcessStartDeclaration::engine(
                kind,
                payload,
                lash_execution_contracts::testing::declared_launch_for_testing(
                    lash_execution_contracts::testing::fixture_engine_launch(),
                ),
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::Lifetime::Detached,
            ),
        },
    ))
}
