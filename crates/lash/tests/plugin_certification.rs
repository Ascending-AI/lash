//! FIG-6093's authoring gate: real SQLite sessions, four laws and explicit gaps.
#[path = "../examples/plugin_authoring.rs"]
mod examples;
use lash::plugins::*;
use lash::testing::*;
use std::sync::Arc;

struct CallScenario {
    callback: PluginCallbackIdentity,
    tool: &'static str,
    args: serde_json::Value,
    support: Option<Arc<dyn PluginFactory>>,
}
#[lash::async_trait]
impl PluginScenario for CallScenario {
    fn callback(&self) -> PluginCallbackIdentity {
        self.callback.clone()
    }
    async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
        if let Some(support) = &self.support {
            harness.install(Arc::clone(support)).await?;
        }
        harness.create(PluginOptions::default()).await
    }
    fn input(&self) -> lash::TurnInput {
        lash::TurnInput::text("exercise tool")
    }
    fn responses(&self) -> Vec<lash::provider::LlmResponse> {
        vec![
            lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::ToolCall {
                    call_id: "certification-call".into(),
                    tool_name: self.tool.into(),
                    input_json: self.args.to_string(),
                    replay: None,
                }],
                ..Default::default()
            },
            text_response("done"),
        ]
    }
    fn pause_after_responses(&self) -> usize {
        1
    }
}
fn no_failed_law(report: &PluginCertification) {
    assert!(
        !report.reports.is_empty(),
        "inventory must enumerate callbacks"
    );
    let failures: Vec<_> = report.failures().map(ToString::to_string).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
/// NO-CERTIFIABLE-CONTRIBUTIONS: prompt inventory has no callback laws to run.
#[tokio::test]
async fn prompt_only_plugins_have_no_certifiable_contributions() {
    let mut entries = PluginRegistrar::new("prompt-only");
    entries
        .prompt()
        .section(
            PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new("instructions").unwrap(),
                lash::prompt::PromptPlacement::InitialInstructions,
            ),
            |_, _| {
                Ok(Arc::new(|_: &PromptInput<'_>| {
                    Ok(SectionText::text("Answer concisely."))
                }))
            },
        )
        .unwrap();
    let factory = Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("prompt-only"),
        PluginRecipe::from_registrar("prompt-only", entries),
    ));
    let report = certify_plugin(factory).await.unwrap();
    assert!(report.inventory.entries.iter().any(|entry| {
        entry.key.owner() == "prompt-only" && entry.kind == ContributionKind::PromptSection
    }));
    assert!(report.reports.is_empty());
    assert_eq!(
        report.verdict(),
        PluginCertificationVerdict::NoCertifiableContributions
    );
}
#[tokio::test]
async fn two_tools_with_recorded_session_config_are_certified() {
    let factory = examples::greetings().unwrap();
    let scenario: Arc<dyn PluginScenario> = Arc::new(CallScenario {
        callback: PluginCallbackIdentity::tool_provider("greetings", "tools"),
        tool: "hello",
        args: serde_json::json!({}),
        support: None,
    });
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    no_failed_law(&report);
    assert_eq!(report.verdict(), PluginCertificationVerdict::Full);
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::ReplayInvokesNoRecordedCallback
        && report.result == PluginLawResult::Passed));
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::RecordedDecisionsStayAttributable
        && report.result == PluginLawResult::Passed));
}
#[tokio::test]
async fn prepared_policy_decision_replays_without_another_invocation() {
    let write = lash::tools::ToolDefinition::raw("write", "write", "write",
        serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}), serde_json::json!({"type":"string"}))
        .unwrap().with_execution(std::time::Duration::from_secs(1));
    struct Write;
    #[lash::async_trait]
    impl lash::tools::StaticToolExecute for Write {
        async fn execute(&self, _: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
            panic!("the prepared policy must deny before the body")
        }
    }
    let support = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("writer"),
        PluginSpec::new().with_tool_provider(
            lash::tool_provider_key!("tools"),
            Arc::new(lash::tools::StaticToolProvider::new(vec![write], Write)),
        ),
    ));
    let scenario: Arc<dyn PluginScenario> = Arc::new(CallScenario {
        callback: PluginCallbackIdentity {
            owner: "protected-path".into(),
            key: "tool_args_check:policy".into(),
        },
        tool: "write",
        args: serde_json::json!({"path":".env"}),
        support: Some(support),
    });
    let report = certify_plugin_with(
        examples::protected_path(),
        HostPolicy::standard(),
        &[scenario],
    )
    .await
    .unwrap();
    no_failed_law(&report);
    assert_eq!(report.verdict(), PluginCertificationVerdict::Partial);
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::ReplayInvokesNoRecordedCallback
        && report.result == PluginLawResult::Passed));
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::RecordedDecisionsStayAttributable
        && report.result
            == PluginLawResult::Unavailable {
                reason: CertificationUnavailable::AttributionNotRecorded
            }));
}
#[tokio::test]
async fn state_counter_in_the_prompt_survives_reopen() {
    let report = certify_plugin(examples::counter().unwrap()).await.unwrap();
    no_failed_law(&report);
    assert!(
        report
            .reports
            .iter()
            .any(|report| report.law == PluginLaw::StateSurvivesReopen
                && report.result == PluginLawResult::Passed)
    );
    let mut harness = PluginHarness::new(examples::counter().unwrap(), HostPolicy::standard())
        .await
        .unwrap();
    harness.create(PluginOptions::default()).await.unwrap();
    harness.script([text_response("done")]);
    harness
        .publish_then_reopen(lash::TurnInput::text("count"))
        .await
        .unwrap();
    assert!(
        harness.requests().iter().any(|request| request
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("Accepted turns: 1"))),
        "the model reads the committed state through the prompt section"
    );
}
#[tokio::test]
async fn host_admitted_64_kib_publication_survives_a_fresh_runtime() {
    examples::large_state().await.unwrap();
    let mut policy = HostPolicy::standard();
    policy.data_limits.plugin_state.max_value_bytes = 128 * 1024;
    let report = certify_plugin_with(examples::large_state_factory().unwrap(), policy, &[])
        .await
        .unwrap();
    no_failed_law(&report);
    assert!(
        report
            .reports
            .iter()
            .any(|report| report.law == PluginLaw::StateSurvivesReopen
                && report.result == PluginLawResult::Passed)
    );
}
#[tokio::test]
async fn unavailable_callbacks_are_partial_not_a_certification_pass() {
    let report = certify_plugin(examples::greetings().unwrap())
        .await
        .unwrap();
    assert_eq!(report.verdict(), PluginCertificationVerdict::Partial);
    assert!(report.reports.iter().any(|report| report.result
        == PluginLawResult::Unavailable {
            reason: CertificationUnavailable::NoScenario
        }));

    // REPLAY-EVIDENCE-BELONGS-TO-THE-CUT: a completed setup turn cannot
    // certify a callback the turn being crashed never invokes.
    struct SetupOnly;
    #[lash::async_trait]
    impl PluginScenario for SetupOnly {
        fn callback(&self) -> PluginCallbackIdentity {
            PluginCallbackIdentity::tool_provider("greetings", "tools")
        }
        async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
            let seed = CallScenario {
                callback: self.callback(),
                tool: "hello",
                args: serde_json::json!({}),
                support: None,
            };
            seed.prepare(harness).await?;
            harness.script(seed.responses());
            harness.send(seed.input()).await?.output().await?;
            Ok(())
        }
        fn input(&self) -> lash::TurnInput {
            lash::TurnInput::text("answer without a tool")
        }
        fn responses(&self) -> Vec<lash::provider::LlmResponse> {
            vec![text_response("done")]
        }
        fn pause_after_responses(&self) -> usize {
            0
        }
    }
    let scenario: Arc<dyn PluginScenario> = Arc::new(SetupOnly);
    let report = certify_plugin_with(
        examples::greetings().unwrap(),
        HostPolicy::standard(),
        &[scenario],
    )
    .await
    .unwrap();
    assert!(
        report.reports.iter().any(|report| report.law
            == PluginLaw::ReplayInvokesNoRecordedCallback
            && report.result
                == PluginLawResult::Unavailable {
                    reason: CertificationUnavailable::ScenarioNotExercised {
                        supplied: true,
                        invocations_before_cut: 0,
                        invocations_after_reopen: 0,
                    }
                }),
        "completed setup work must not invent replay evidence: {:?}",
        report.reports
    );
}

/// BATTERY-REFUSES-OVER-LIMIT-STATE: an author's broken publication is
/// reported under its callback and the state law, with the runtime cause.
#[tokio::test]
async fn the_battery_names_the_state_law_for_an_over_limit_plugin() {
    struct Broken;
    impl PluginMetadata for Broken {
        fn plugin_declaration(&self) -> PluginDeclaration {
            PluginDeclaration::initial("broken-state")
        }
    }
    impl PluginFactory for Broken {
        fn id(&self) -> &'static str {
            "broken-state"
        }
        fn initialize_state(
            &self,
            _: &lash::RuntimeOwner,
            _: &PluginConfig,
        ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, PluginError> {
            Ok(std::collections::BTreeMap::from([(
                "value".into(),
                serde_json::json!("x".repeat(64 * 1024)),
            )]))
        }
        fn recipe(self: Arc<Self>) -> Result<PluginRecipe, PluginError> {
            let mut entries = PluginRegistrar::new(self.id());
            entries
                .turn()
                .before(lash::hook_key!("too-large"), |_, _| {
                    Ok(Arc::new(|_| {
                        Box::pin(async { Ok(TurnContributions::default()) })
                    }))
                })?;
            Ok(PluginRecipe::from_registrar(self.id(), entries))
        }
    }
    let factory = Arc::new(Broken);
    let report = certify_plugin(factory).await.unwrap();
    let failure = report
        .failures()
        .find(|report| report.law == PluginLaw::StateSurvivesReopen)
        .expect("the state law refuses oversized publication");
    let message = failure.to_string();
    assert!(
        message.contains("broken-state/before_turn:too-large"),
        "{message}"
    );
    assert!(
        message.contains("TooLarge") || message.contains("too_large"),
        "the typed cause stays readable: {message}"
    );
}

/// FIG-6247: an author scenario under standard cannot exercise an RLM resolver;
/// its missing execution evidence must be distinguished from absent input.
#[tokio::test]
async fn supplied_but_unexercised_resolver_is_distinct_from_no_scenario() {
    use lash::tools::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Resolver(Arc<AtomicUsize>);
    #[lash::async_trait]
    impl ToolProvider for Resolver {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(Vec::new())
        }
        fn resolve_contract(&self, _: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            None
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            panic!("the scenario must not execute the resolver as a tool")
        }
    }
    #[lash::async_trait]
    impl DeferredToolProvider for Resolver {
        async fn resolve(
            &self,
            _: &DeferredResolveContext<'_>,
            _: &[&str],
        ) -> std::collections::BTreeMap<String, DeferredOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::collections::BTreeMap::new()
        }
    }
    let resolutions = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("unexercised-resolver"),
        PluginSpec::new().with_deferred_provider(
            lash::tool_provider_key!("deferred"),
            Arc::new(Resolver(Arc::clone(&resolutions))),
        ),
    ));
    let absent = certify_plugin(factory.clone()).await.unwrap();
    assert_eq!(absent.verdict(), PluginCertificationVerdict::Partial);
    assert_eq!(absent.reports.len(), 4);
    for law in &absent.reports {
        if law.law != PluginLaw::UnservedIsUnclaimable {
            assert_eq!(
                law.result,
                PluginLawResult::Unavailable {
                    reason: CertificationUnavailable::NoScenario,
                }
            );
        }
    }
    let scenario: Arc<dyn PluginScenario> = Arc::new(CallScenario {
        callback: PluginCallbackIdentity::tool_provider("unexercised-resolver", "deferred"),
        tool: "tools.send_email",
        args: serde_json::json!({}),
        support: None,
    });
    let supplied = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);
    assert_eq!(supplied.verdict(), PluginCertificationVerdict::Partial);
    assert_eq!(supplied.reports.len(), 4);
    for law in &supplied.reports {
        if law.law != PluginLaw::UnservedIsUnclaimable {
            assert_eq!(
                law.result,
                PluginLawResult::Unavailable {
                    reason: CertificationUnavailable::ScenarioNotExercised {
                        supplied: true,
                        invocations_before_cut: 0,
                        invocations_after_reopen: 0,
                    },
                },
                "{law}"
            );
        }
    }
}

/// DEFERRED-STATE-FREE-CERTIFICATION: the resolver selects a different,
/// catalogue-free executable under the author's actual protocol.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn deferred_state_free_provider_is_full_under_a_supporting_protocol() {
    use lash::tools::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Resolver(Arc<AtomicUsize>);
    #[lash::async_trait]
    impl ToolProvider for Resolver {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(Vec::new())
        }
        fn resolve_contract(&self, _: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            None
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            panic!("the resolver never executes the selected tool")
        }
    }
    fn definition() -> ToolDefinition {
        ToolDefinition::raw(
            "remote:send_email",
            "send_email",
            "send email",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1))
        .with_tool_binding(ToolBinding::new(["tools"], "send_email"))
    }
    #[lash::async_trait]
    impl DeferredToolProvider for Resolver {
        async fn resolve(
            &self,
            _: &DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> BTreeMap<String, DeferredOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            paths
                .iter()
                .filter(|path| **path == "tools.send_email")
                .map(|path| {
                    (
                        (*path).into(),
                        DeferredOutcome::Resolved {
                            provider: ToolProviderHandle::new(
                                RegistryKind::Session,
                                "host",
                                lash::tool_provider_key!("tools"),
                            ),
                            definition: definition(),
                            binding: None,
                        },
                    )
                })
                .collect()
        }
    }
    struct Host(Arc<AtomicUsize>);
    #[lash::async_trait]
    impl ToolProvider for Host {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(Vec::new())
        }
        fn resolve_contract(&self, _: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            None
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok(serde_json::json!("sent")).into()
        }
    }
    struct DeferredScenario {
        resolved: Arc<AtomicUsize>,
        executed: Arc<AtomicUsize>,
    }
    #[lash::async_trait]
    impl PluginScenario for DeferredScenario {
        fn callback(&self) -> PluginCallbackIdentity {
            PluginCallbackIdentity::tool_provider("resolver", "deferred")
        }
        async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
            harness
                .install_protocol(Arc::new(
                    lash::rlm::RlmProtocolPluginFactory::new(
                        lash::rlm::RlmProtocolPluginConfig::builder()
                            .channel(lash::rlm::RlmChannel::NativeTool)
                            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                            .build(),
                        lash::rlm::CellDialect::typescript(),
                    )
                    .with_worker_service(lash::vm::WorkerService::default()),
                ))
                .await?;
            harness
                .install(Arc::new(StaticPluginFactory::new(
                    PluginDeclaration::initial("host"),
                    PluginSpec::new().with_tool_provider(
                        lash::tool_provider_key!("tools"),
                        Arc::new(Host(Arc::clone(&self.executed))),
                    ),
                )))
                .await?;
            harness.create(PluginOptions::default()).await?;
            // A scenario only installs services. The battery must exercise resolution.
            assert_eq!(self.resolved.load(Ordering::SeqCst), 0);
            Ok(())
        }
        fn input(&self) -> lash::TurnInput {
            lash::TurnInput::text("send email")
        }
        fn responses(&self) -> Vec<lash::provider::LlmResponse> {
            vec![
                lash::provider::LlmResponse {
                    parts: vec![lash::direct::LlmOutputPart::ToolCall {
                        call_id: "deferred-cell".into(),
                        tool_name: "execute_code".into(),
                        input_json: serde_json::json!({"code":"await tools.send_email({});"})
                            .to_string(),
                        replay: None,
                    }],
                    ..Default::default()
                },
                text_response("done"),
            ]
        }
        fn pause_after_responses(&self) -> usize {
            1
        }
    }
    let resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("resolver"),
        PluginSpec::new().with_deferred_provider(
            lash::tool_provider_key!("deferred"),
            Arc::new(Resolver(Arc::clone(&resolved))),
        ),
    ));
    let unavailable = certify_plugin(factory.clone()).await.unwrap();
    assert_eq!(unavailable.verdict(), PluginCertificationVerdict::Partial);
    assert!(unavailable.reports.iter().any(|report| report.result
        == PluginLawResult::Unavailable {
            reason: CertificationUnavailable::NoScenario
        }));
    let scenario: Arc<dyn PluginScenario> = Arc::new(DeferredScenario {
        resolved: Arc::clone(&resolved),
        executed: Arc::clone(&executed),
    });
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    assert_eq!(
        executed.load(Ordering::SeqCst),
        1,
        "the selected executable ran once across reopen"
    );
    assert_eq!(
        resolved.load(Ordering::SeqCst),
        1,
        "the recorded resolution is never repeated"
    );
    assert_eq!(
        report.verdict(),
        PluginCertificationVerdict::Full,
        "{:#?}",
        report.reports
    );
    assert_eq!(report.reports.len(), 4);
    assert!(
        report
            .reports
            .iter()
            .any(|law| law.law == PluginLaw::StateSurvivesReopen
                && law.result
                    == PluginLawResult::NotApplicable {
                        reason: CertificationNotApplicable::NoPublishedState
                    })
    );
}
