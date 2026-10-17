use super::*;
use lash_protocol_types::ToolDefinitionBindingExt as _;
const RECORDED: crate::ProcessConfigKey<()> =
    crate::ProcessConfigKey::new("z-recorded", crate::FormatVersion::ONE);
const OTHER: crate::ProcessConfigKey<()> =
    crate::ProcessConfigKey::new("a-other", crate::FormatVersion::ONE);

fn tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "search",
        "search",
        "Search",
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["q"], "properties":{"q":{"type":"string"}}}),
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["hits"], "properties":{"hits":{"type":"string"}}}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(60))
    .with_tool_binding(lash_protocol_types::ToolBinding::new(["search"], "run"))
}
struct Answer(crate::ToolDefinition);
#[async_trait::async_trait]
impl crate::ToolProvider for Answer {
    fn tool_manifests(
        &self,
        _: &crate::OwnerConfigView,
    ) -> Result<Vec<crate::ToolManifest>, crate::PluginError> {
        Ok(vec![self.0.manifest.clone()])
    }
    fn resolve_contract(
        &self,
        name: &str,
        _: &crate::OwnerConfigView,
    ) -> Option<Arc<crate::ToolContract>> {
        (name == self.0.manifest.name).then(|| Arc::new(self.0.contract.clone()))
    }
    async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(serde_json::json!({})).into()
    }
}
fn registry(changed: bool, recorded: bool) -> crate::ProcessEngineRegistry {
    registry_of(changed, recorded, tool())
}
fn registry_of(
    changed: bool,
    recorded: bool,
    served: crate::ToolDefinition,
) -> crate::ProcessEngineRegistry {
    let mut components = crate::plugin::ProcessComponentRegistry::new();
    for key in [OTHER, RECORDED] {
        if key.component() == RECORDED.component() && !recorded {
            continue;
        }
        let mut definition = served.clone();
        if key.component() == RECORDED.component() && changed {
            definition = definition.with_execution_policy(crate::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(2).unwrap(),
                1,
                1,
            ));
        }
        components
            .register(
                crate::testing::tool_component(key, Arc::new(Answer(definition))),
                key,
            )
            .unwrap();
    }
    crate::ProcessEngineRegistry::new()
        .with_components(components)
        .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
            SearchEngine,
        )))
}
fn target() -> ResolvedStartTarget {
    ResolvedStartTarget {
        definition_id: crate::ProcessDefinitionDraft::new(
            crate::testing::HELD_PROCESS_ENGINE_KIND,
            serde_json::json!({}),
            [],
        )
        .unwrap()
        .id(),
        engine_kind: crate::testing::HELD_PROCESS_ENGINE_KIND.into(),
        signature: crate::ProcessSignature::Unknown,
        payload: serde_json::json!({}),
        args: Default::default(),
        performed: std::collections::BTreeMap::from([(
            crate::EffectName::new("search.run").unwrap(),
            lash_protocol_types::tool_signature(
                &tool().contract.input_schema.canonical,
                &tool().contract.output_schema.canonical,
            ),
        )]),
    }
}
fn components(recorded: bool) -> Vec<ProcessComponentEntry> {
    [OTHER, RECORDED]
        .into_iter()
        .filter(|key| recorded || key.component() != RECORDED.component())
        .map(|key| ProcessComponentEntry::new(key, &(), []).unwrap())
        .collect()
}
/// CAPTURE-IS-IDENTITY: a second provider cannot replace the recorded key or surface.
#[tokio::test]
async fn capture_keeps_recorded_provider_and_surface() {
    let run = ResolvedRun::snapshot(crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(4),
        crate::NoProgressBudget::bounded(4),
    ));
    let surface = SurfaceTools::of_entries(
        [crate::SurfaceEntry {
            provider: crate::plugin::ToolProviderHandle::new(
                crate::RegistryKind::Session,
                RECORDED.component(),
                crate::tool_provider_key!("tools"),
            ),
            definition: tool(),
        }],
        Arc::new(crate::ToolRegistry::default()),
    );
    let target = target();
    let starter = crate::ExecutionOwner::Process {
        process_id: crate::ProcessIdMint::sequential_id_for_testing(1),
    };
    let engines = registry(false, true);
    let cx = ProcessEnvCx::new(
        &starter,
        &target,
        None,
        Some(&run),
        Some(&surface),
        None,
        &engines,
    );
    let captured = cx.capture(&run, &surface, components(true)).await.unwrap();
    let link = &captured.effects()[&crate::EffectName::new("search.run").unwrap()];
    assert_eq!(
        link.provider,
        RECORDED.tool_provider(crate::tool_provider_key!("tools"))
    );
    assert_eq!(link.surface, crate::ProcessSurface::of(&tool()));
    let engines = registry(true, true);
    let cx = ProcessEnvCx::new(
        &starter,
        &target,
        None,
        Some(&run),
        Some(&surface),
        None,
        &engines,
    );
    assert!(matches!(
        cx.capture(&run, &surface, components(true)).await,
        Err(ProcessEnvRefusal::LinkedSurfaceChanged { .. })
    ));
    let engines = registry(false, false);
    let cx = ProcessEnvCx::new(
        &starter,
        &target,
        None,
        Some(&run),
        Some(&surface),
        None,
        &engines,
    );
    assert!(matches!(
        cx.capture(&run, &surface, components(false)).await,
        Err(ProcessEnvRefusal::CaptureMissing { .. })
    ));
}
/// P1: an invalid pair refuses at the hold before a declaration can be minted.
#[tokio::test]
async fn launch_holding_checks_links_and_exact_fit() {
    let engines = registry(false, true);
    let scope = crate::ExecutionScope::runtime_operation("link-law");
    let target = target();
    let choice = ProcessStartChoice {
        fits: Default::default(),
        env: ProcessEnvChoice::New(
            ProcessEnv::builder(crate::MaxToolCalls::new(4))
                .build()
                .unwrap(),
        ),
        engine_settings: None,
    };
    assert!(matches!(
        super::super::env_hold::hold_launch(
            &engines,
            &scope,
            None,
            &choice,
            &target.engine_kind,
            &target.payload
        )
        .await,
        Err(crate::HoldEnvError::Refused(
            ProcessEnvRefusal::EffectNotLinked { .. }
        ))
    ));
}
/// R7: Exact is enforced while holding a launch, rather than merely advised.
#[tokio::test]
async fn an_exact_fit_is_required_before_a_launch_is_held() {
    let scope = crate::ExecutionScope::runtime_operation("exact-link-law");
    let target = target();
    let wider = crate::ToolDefinition::raw(
        "search", "search", "Search",
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["q"], "properties":{"q":{"type":"string"}, "limit":{"type":"integer"}}}),
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["hits"], "properties":{"hits":{"type":"string"}, "extra":{"type":"string"}}}),
    ).unwrap().with_execution(std::time::Duration::from_secs(60))
        .with_tool_binding(lash_protocol_types::ToolBinding::new(["search"], "run"));
    let engines = registry_of(false, true, wider.clone());
    let effect = crate::EffectName::new("search.run").unwrap();
    let env = ProcessEnv::builder(crate::MaxToolCalls::new(4))
        .component(RECORDED, &())
        .link_tool(
            RECORDED.tool_provider(crate::tool_provider_key!("tools")),
            &wider,
        )
        .build()
        .unwrap();
    let performed = engines
        .performed_effects(&target.engine_kind, &target.payload)
        .await
        .unwrap();
    crate::check_links(&env, &performed, &Default::default())
        .expect("the wider tool is compatible");
    let choice = ProcessStartChoice {
        fits: std::collections::BTreeMap::from([(effect, crate::FitPolicy::Exact)]),
        env: ProcessEnvChoice::New(env),
        engine_settings: None,
    };
    let held = super::super::env_hold::hold_launch(
        &engines,
        &scope,
        None,
        &choice,
        &target.engine_kind,
        &target.payload,
    )
    .await;
    assert!(
        matches!(
            &held,
            Err(crate::HoldEnvError::Refused(
                ProcessEnvRefusal::EffectUnfit { .. }
            ))
        ),
        "{held:?}"
    );
}

struct SearchEngine;
#[async_trait::async_trait]
impl crate::ProcessEngine for SearchEngine {
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
        Ok(std::collections::BTreeMap::from([(
            crate::EffectName::new("search.run").unwrap(),
            lash_protocol_types::tool_signature(
                &tool().contract.input_schema.canonical,
                &tool().contract.output_schema.canonical,
            ),
        )]))
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
        crate::testing::HELD_PROCESS_ENGINE_KIND
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
