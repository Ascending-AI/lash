use super::*;
const RECORDED: crate::ProcessConfigKey<()> =
    crate::ProcessConfigKey::new("z-recorded", lash_vocabulary::providers::FormatVersion::ONE);
const OTHER: crate::ProcessConfigKey<()> =
    crate::ProcessConfigKey::new("a-other", lash_vocabulary::providers::FormatVersion::ONE);

fn tool() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        "search",
        "search.run",
        "Search",
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["q"], "properties":{"q":{"type":"string"}}}),
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["hits"], "properties":{"hits":{"type":"string"}}}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(60))
}
struct Answer(lash_vocabulary::tool_contract::ToolDefinition);
#[async_trait::async_trait]
impl crate::ToolProvider for Answer {
    fn tool_manifests(
        &self,
        _: &crate::OwnerConfigView,
    ) -> Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![self.0.manifest.clone()])
    }
    fn resolve_contract(
        &self,
        name: &str,
        _: &crate::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == self.0.manifest.tool()).then(|| Arc::new(self.0.contract.clone()))
    }
    async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(serde_json::json!({})).into()
    }
}
/// How the recorded component serves the tool on this node.
#[derive(Clone, Copy, PartialEq)]
enum Served {
    AsRecorded,
    /// Under another retry policy: a fact of the link, not the contract.
    OtherPolicy,
    /// Under another output schema: another contract.
    OtherContract,
}
/// `definition` with another output schema: the tool under its next
/// contract.
fn next_contract(
    mut definition: lash_vocabulary::tool_contract::ToolDefinition,
) -> lash_vocabulary::tool_contract::ToolDefinition {
    definition.contract.output_schema = lash_vocabulary::schema_contract::SchemaContract::admit(
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["hits"], "properties":{"hits":{"type":"string"}, "next":{"type":"string"}}}),
    )
    .unwrap();
    definition
}
fn registry(served: Served, recorded: bool) -> crate::ProcessEngineRegistry {
    registry_of(served, recorded, tool())
}
fn registry_of(
    change: Served,
    recorded: bool,
    served: lash_vocabulary::tool_contract::ToolDefinition,
) -> crate::ProcessEngineRegistry {
    let mut components = crate::plugin::ProcessComponentRegistry::new();
    for key in [OTHER, RECORDED] {
        if key.component() == RECORDED.component() && !recorded {
            continue;
        }
        let mut definition = served.clone();
        if key.component() == RECORDED.component() {
            match change {
                Served::AsRecorded => {}
                Served::OtherPolicy => {
                    definition = definition.with_execution_policy(
                        lash_vocabulary::tool_contract::ExecutionPolicy::repeatable(
                            std::num::NonZeroU32::new(2).unwrap(),
                            1,
                            1,
                        ),
                    );
                }
                Served::OtherContract => definition = next_contract(definition),
            }
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
        definition_id: lash_execution_contracts::process::ProcessDefinitionDraft::new(
            lash_execution_contracts::testing::HELD_PROCESS_ENGINE_KIND,
            serde_json::json!({}),
            [],
        )
        .unwrap()
        .id(),
        engine_kind: lash_execution_contracts::testing::HELD_PROCESS_ENGINE_KIND.into(),
        signature: lash_execution_contracts::process::ProcessSignature::Unknown,
        payload: serde_json::json!({}),
        args: Default::default(),
        performed: std::collections::BTreeMap::from([(
            lash_vocabulary::legacy_names::EffectName::new("search.run").unwrap(),
            lash_vocabulary::operations::OperationRequirement::new(
                lash_vocabulary::json_schema::tool_signature(
                    &tool().contract.input_schema.canonical,
                    &tool().contract.output_schema.canonical,
                ),
                tool().contract_id(),
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
/// CAPTURE-IS-IDENTITY: a second provider cannot replace the recorded key
/// or link, and the captured link keeps the policy the run recorded; a
/// component serving the tool under another contract is refused (OP-LINK-ONE).
#[tokio::test]
async fn capture_keeps_recorded_provider_and_link() {
    let run = ResolvedRun::snapshot(crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(4),
        crate::NoProgressBudget::bounded(4),
    ));
    let surface = SurfaceTools::of_entries(
        [crate::SurfaceEntry::advertised(
            lash_vocabulary::providers::ToolProviderHandle::new(
                lash_vocabulary::claims::RegistryKind::Session,
                RECORDED.component(),
                crate::tool_provider_key!("tools"),
            ),
            tool(),
            None,
        )],
        Arc::new(crate::ToolRegistry::default()),
    );
    let target = target();
    let starter = crate::ExecutionOwner::Process {
        process_id: crate::ProcessIdMint::sequential_id_for_testing(1),
    };
    for served in [Served::AsRecorded, Served::OtherPolicy] {
        let engines = registry(served, true);
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
        let link = &captured.effects()
            [&lash_vocabulary::legacy_names::EffectName::new("search.run").unwrap()];
        assert_eq!(
            link.provider,
            RECORDED.tool_provider(crate::tool_provider_key!("tools"))
        );
        assert_eq!(link.tool, tool().manifest.id);
        assert_eq!(link.policy, tool().manifest.execution_policy);
    }
    let engines = registry(Served::OtherContract, true);
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
        Err(ProcessEnvRefusal::ContractChanged { .. })
    ));
    let engines = registry(Served::AsRecorded, false);
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
/// OP-CONTRACT-PIN: a child start is judged against the requirement its own
/// code states, never the contract its parent's link was captured under.
/// The parent recorded the tool at its first contract and its component
/// now serves the next: a child whose code requires the next contract
/// links, and one whose code still requires the first is refused, naming
/// both ids.
#[tokio::test]
async fn a_child_start_is_judged_against_its_own_requirement() {
    let run = ResolvedRun::snapshot(crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(4),
        crate::NoProgressBudget::bounded(4),
    ));
    let surface = SurfaceTools::of_entries(
        [crate::SurfaceEntry::advertised(
            lash_vocabulary::providers::ToolProviderHandle::new(
                lash_vocabulary::claims::RegistryKind::Session,
                RECORDED.component(),
                crate::tool_provider_key!("tools"),
            ),
            tool(),
            None,
        )],
        Arc::new(crate::ToolRegistry::default()),
    );
    let starter = crate::ExecutionOwner::Process {
        process_id: crate::ProcessIdMint::sequential_id_for_testing(1),
    };
    let engines = registry(Served::OtherContract, true);
    let next = next_contract(tool());
    let mut child = target();
    child.performed = std::collections::BTreeMap::from([(
        lash_vocabulary::legacy_names::EffectName::new("search.run").unwrap(),
        lash_vocabulary::operations::OperationRequirement::new(
            lash_vocabulary::json_schema::tool_signature(
                &next.contract.input_schema.canonical,
                &next.contract.output_schema.canonical,
            ),
            next.contract_id(),
        ),
    )]);
    for (code, linked) in [(&child, true), (&target(), false)] {
        let cx = ProcessEnvCx::new(
            &starter,
            code,
            None,
            Some(&run),
            Some(&surface),
            None,
            &engines,
        );
        let captured = cx.capture(&run, &surface, components(true)).await;
        if linked {
            assert!(captured.is_ok(), "{captured:?}");
        } else {
            assert!(
                matches!(
                    &captured,
                    Err(ProcessEnvRefusal::ContractChanged { pinned, live, .. })
                        if *pinned == tool().contract_id() && *live == next.contract_id()
                ),
                "{captured:?}"
            );
        }
    }
}
/// P1: an invalid pair refuses at the hold before a declaration can be minted.
#[tokio::test]
async fn launch_holding_checks_links_and_exact_fit() {
    let engines = registry(Served::AsRecorded, true);
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
    let wider = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "search", "search.run", "Search",
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["q"], "properties":{"q":{"type":"string"}, "limit":{"type":"integer"}}}),
        serde_json::json!({"type":"object", "additionalProperties":false, "required":["hits"], "properties":{"hits":{"type":"string"}, "extra":{"type":"string"}}}),
    ).unwrap()
    // The wider tool states the contract the code requires: only its fit differs.
    .declared_contract(tool().contract_id())
    .with_execution(std::time::Duration::from_secs(60));
    let engines = registry_of(Served::AsRecorded, true, wider.clone());
    let effect = lash_vocabulary::legacy_names::EffectName::new("search.run").unwrap();
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
    engines
        .check_links(
            &target.engine_kind,
            &target.payload,
            &Arc::new(env.clone()),
            &performed,
            &Default::default(),
        )
        .await
        .expect("the wider tool is compatible");
    let choice = ProcessStartChoice {
        fits: std::collections::BTreeMap::from([(
            effect,
            lash_execution_contracts::process::FitPolicy::Exact,
        )]),
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
impl lash_execution_contracts::process::ProcessEngine for SearchEngine {
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

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_execution_contracts::process::PerformedEffects, crate::ProcessEnvRefusal> {
        Ok(std::collections::BTreeMap::from([(
            lash_vocabulary::legacy_names::EffectName::new("search.run").unwrap(),
            lash_vocabulary::operations::OperationRequirement::new(
                lash_vocabulary::json_schema::tool_signature(
                    &tool().contract.input_schema.canonical,
                    &tool().contract.output_schema.canonical,
                ),
                tool().contract_id(),
            ),
        )]))
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
        lash_execution_contracts::testing::HELD_PROCESS_ENGINE_KIND
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
