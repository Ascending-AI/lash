//! The authoring gate: real SQLite sessions, callback laws and explicit gaps.
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

/// CERTIFICATION-PRESERVES-PROVIDER-ROUTES: the counted tool source and its
/// bound package advertisement must carry the same execution binding.
#[tokio::test]
async fn a_provider_with_a_link_payload_and_bound_packages_certifies() {
    use lash::tools::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Echo {
        definition: ToolDefinition,
        calls: Arc<AtomicUsize>,
    }
    #[lash::async_trait]
    impl ToolProvider for Echo {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(vec![self.definition.manifest.clone()])
        }
        fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            (name == self.definition.manifest.tool())
                .then(|| Arc::new(self.definition.contract.clone()))
        }
        fn readable_binding_formats(&self) -> &'static [FormatVersion] {
            &[FormatVersion::ONE]
        }
        fn link_payload(
            &self,
            _: &ToolManifest,
            _: &OwnerConfigView,
        ) -> Option<(FormatVersion, serde_json::Value)> {
            Some((FormatVersion::ONE, serde_json::json!({"route": "echo"})))
        }
        async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
            assert_eq!(
                call.context
                    .tool_execution_binding()
                    .map(|binding| &binding.payload),
                Some(&serde_json::json!({"route": "echo"})),
                "the admitted route retains the provider's codec payload"
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok(serde_json::json!("echo")).into()
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Echo {
        definition: ToolDefinition::raw(
            "certification-echo",
            "tools.echo",
            "Echo through a bound package provider",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1)),
        calls: Arc::clone(&calls),
    });
    let key = lash::tool_provider_key!("host");
    let mut entries = PluginRegistrar::new("bound-echo");
    entries
        .tools()
        .provider_reading(key, &[FormatVersion::ONE], {
            let provider = Arc::clone(&provider);
            move |_, _| Ok(provider.clone())
        })
        .unwrap();
    entries
        .package_provider_bound(key, move |_, _| {
            Ok(Arc::new(lash::package::ToolPackageProvider::new(
                "bound-echo",
                key,
                [lash::package::Segment::new("tools").unwrap()].into(),
                provider.clone(),
            )))
        })
        .unwrap();
    let factory = Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("bound-echo"),
        PluginRecipe::from_registrar("bound-echo", entries).stateless(),
    ));
    let scenario: Arc<dyn PluginScenario> = Arc::new(CallScenario {
        callback: PluginCallbackIdentity::tool_provider("bound-echo", "host"),
        tool: "tools__echo",
        args: serde_json::json!({}),
        support: None,
    });
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    no_failed_law(&report);
    // Package advertisement is a separate, untraced contribution. The tool
    // provider's scenario must establish its replay and attribution laws.
    for law in [
        PluginLaw::ReplayInvokesNoRecordedCallback,
        PluginLaw::RecordedDecisionsStayAttributable,
    ] {
        assert!(
            report.reports.iter().any(|result| {
                result.callback == PluginCallbackIdentity::tool_provider("bound-echo", "host")
                    && result.law == law
                    && result.result == PluginLawResult::Passed
            }),
            "{report:#?}"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "reopen must not repeat the tool"
    );
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
        tool: "greetings__hello",
        args: serde_json::json!({}),
        support: None,
    });
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    no_failed_law(&report);
    assert_eq!(report.verdict(), PluginCertificationVerdict::Partial);
    assert!(
        report
            .reports
            .iter()
            .any(|report| report.law == PluginLaw::StateSurvivesReopen
                && report.result
                    == PluginLawResult::Unavailable {
                        reason: CertificationUnavailable::ScenarioDidNotPublish,
                    })
    );
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::ReplayInvokesNoRecordedCallback
        && report.result == PluginLawResult::Passed));
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::RecordedDecisionsStayAttributable
        && report.result == PluginLawResult::Passed));
}
#[tokio::test]
async fn prepared_policy_decision_replays_without_another_invocation() {
    let write = lash::tools::ToolDefinition::raw("write", "tools.write", "write",
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
        tool: "tools__write",
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
                tool: "greetings__hello",
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
            _: &lash_vocabulary::identity::RuntimeOwner,
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
            lash::tools::DeferredToolProviderRegistration::new(Arc::new(Resolver(Arc::clone(
                &resolutions,
            )))),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Resolver(Arc<AtomicUsize>);
    fn definition() -> ToolDefinition {
        ToolDefinition::raw(
            "remote:send_email",
            "tools.send_email",
            "send email",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1))
    }
    #[lash::async_trait]
    impl lash::package::DeferredPackageProvider for Resolver {
        fn validate_grant(
            &self,
            grant: &lash::package::DeferredGrant,
            registry: RegistryKind,
        ) -> Result<(), lash::package::AdmissionRefusal> {
            validate_resolver_grant(grant, registry)
        }
        async fn resolve(
            &self,
            _: &DeferredResolveContext<'_>,
            _: &lash::package::DeferredGrant,
            requested: &lash::package::QualifiedName,
        ) -> Result<lash::package::DeferredAnswer, lash::package::ResolverFault> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if requested.to_string() != "tools.send_email" {
                return Ok(lash::package::DeferredAnswer::Unavailable { note: None });
            }
            let definition = definition();
            let route = lash::tools::OperationRoute::resolved(
                ToolProviderHandle::new(
                    RegistryKind::Session,
                    "host",
                    lash::tool_provider_key!("tools"),
                ),
                definition.clone(),
                None,
            );
            Ok(lash::package::DeferredAnswer::Resolved {
                definition: Box::new(definition),
                route: Box::new(route),
            })
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
            resolver_callback()
        }
        async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
            harness
                .install_protocol(Arc::new(
                    lash::rlm::RlmProtocolPluginFactory::new(
                        lash::rlm::RlmProtocolPluginConfig::builder()
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
                    deferred_host(Arc::new(Host(Arc::clone(&self.executed)))),
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
                        input_json: serde_json::json!({"code":"await tools.sendEmail({});"})
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
        PluginSpec::new().with_deferred_package_provider(
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
        "the selected executable ran once across reopen; resolved={}; reports={:#?}",
        resolved.load(Ordering::SeqCst),
        report.reports
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

#[cfg(feature = "rlm")]
mod deferred_laws {
    use super::*;
    use lash::tools::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Resolver {
        empty: bool,
        resolutions: Arc<AtomicUsize>,
        definition: ToolDefinition,
    }
    fn definition(id: &str, name: &str) -> Result<ToolDefinition, PluginError> {
        ToolDefinition::raw(
            id,
            format!(
                "{}.{name}",
                if name == "lookup" { "local" } else { "tools" }
            ),
            name,
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .map(|draft| draft.with_execution(std::time::Duration::from_secs(1)))
        .map_err(|error| PluginError::Registration(error.to_string()))
    }
    #[lash::async_trait]
    impl lash::package::DeferredPackageProvider for Resolver {
        fn validate_grant(
            &self,
            grant: &lash::package::DeferredGrant,
            registry: RegistryKind,
        ) -> Result<(), lash::package::AdmissionRefusal> {
            validate_resolver_grant(grant, registry)
        }
        async fn resolve(
            &self,
            _: &DeferredResolveContext<'_>,
            _: &lash::package::DeferredGrant,
            requested: &lash::package::QualifiedName,
        ) -> Result<lash::package::DeferredAnswer, lash::package::ResolverFault> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            if self.empty || requested.to_string() != "tools.send_email" {
                return Ok(lash::package::DeferredAnswer::Unavailable { note: None });
            }
            Ok(lash::package::DeferredAnswer::Resolved {
                route: Box::new(lash::tools::OperationRoute::resolved(
                    ToolProviderHandle::new(
                        RegistryKind::Session,
                        "host",
                        lash::tool_provider_key!("tools"),
                    ),
                    self.definition.clone(),
                    None,
                )),
                definition: Box::new(self.definition.clone()),
            })
        }
    }
    struct Host;
    #[lash::async_trait]
    impl ToolProvider for Host {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(vec![definition("host:lookup", "lookup")?.manifest])
        }
        fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            if name != "local.lookup" {
                return None;
            }
            Some(Arc::new(definition("host:lookup", "lookup").ok()?.contract))
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            ToolOutcome::ok(serde_json::json!("ok")).into()
        }
    }
    struct Scenario {
        code: &'static str,
    }
    #[lash::async_trait]
    impl PluginScenario for Scenario {
        fn callback(&self) -> PluginCallbackIdentity {
            resolver_callback()
        }
        async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
            let protocol: Arc<dyn PluginFactory> = Arc::new(
                lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(lash::vm::WorkerService::default()),
            );
            harness.install_protocol(protocol).await?;
            harness
                .install(Arc::new(StaticPluginFactory::new(
                    PluginDeclaration::initial("host"),
                    deferred_host(Arc::new(Host)),
                )))
                .await?;
            harness.create(PluginOptions::default()).await
        }
        fn input(&self) -> lash::TurnInput {
            lash::TurnInput::text("exercise the cell")
        }
        fn responses(&self) -> Vec<lash::provider::LlmResponse> {
            vec![
                lash::provider::LlmResponse {
                    parts: vec![lash::direct::LlmOutputPart::ToolCall {
                        call_id: "certification-cell".into(),
                        tool_name: "execute_code".into(),
                        input_json: serde_json::json!({"code": self.code}).to_string(),
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
    fn factory(
        empty: bool,
        resolutions: Arc<AtomicUsize>,
    ) -> Result<Arc<dyn PluginFactory>, PluginError> {
        Ok(Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial("resolver"),
            PluginSpec::new().with_deferred_package_provider(
                lash::tool_provider_key!("deferred"),
                Arc::new(Resolver {
                    empty,
                    resolutions,
                    definition: definition("remote:send_email", "send_email")?,
                }),
            ),
        )))
    }

    /// STATIC-CALL-IS-NOT-DEFERRED-EVIDENCE: an offered call cannot supply
    /// attribution for an empty resolver, even when a deferred call exercises it.
    #[tokio::test]
    async fn static_call_cannot_certify_an_empty_deferred_resolver() {
        let resolutions = Arc::new(AtomicUsize::new(0));
        let scenario: Arc<dyn PluginScenario> = Arc::new(Scenario {
            code: "await local.lookup({}); await tools.sendEmail({});",
        });
        let report = certify_plugin_with(
            factory(true, Arc::clone(&resolutions)).unwrap(),
            HostPolicy::standard(),
            &[scenario],
        )
        .await
        .unwrap();
        assert_eq!(
            resolutions.load(Ordering::SeqCst),
            1,
            "the resolver was exercised but selected nothing"
        );
        no_failed_law(&report);
        assert_eq!(
            report.verdict(),
            PluginCertificationVerdict::Partial,
            "{:#?}",
            report.reports
        );
        assert!(report.reports.iter().any(|law| law.law
            == PluginLaw::RecordedDecisionsStayAttributable
            && law.result
                == PluginLawResult::Unavailable {
                    reason: CertificationUnavailable::AttributionNotRecorded
                }));
    }
}

/// STATE-PUBLICATION-MUST-BE-EXERCISED: a hook publishing on its second turn
/// cannot claim its empty first turn as evidence that state survived a crash.
#[tokio::test]
async fn a_state_publishing_hook_requires_a_published_state_scenario() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let invocations = Arc::new(AtomicUsize::new(0));
    let mut entries = PluginRegistrar::new("late-state");
    let counted = Arc::clone(&invocations);
    entries
        .turn()
        .before(lash::hook_key!("publish-later"), move |_, _| {
            let counted = Arc::clone(&counted);
            Ok(Arc::new(move |_| {
                let counted = Arc::clone(&counted);
                Box::pin(async move {
                    Ok(TurnContributions {
                        state: if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                            StateCommands::new()
                        } else {
                            StateCommands::new().set("counter", serde_json::json!(1))
                        },
                        ..Default::default()
                    })
                })
            }))
        })
        .unwrap();
    let factory: Arc<dyn PluginFactory> = Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("late-state"),
        PluginRecipe::from_registrar("late-state", entries),
    ));
    let report = certify_plugin(Arc::clone(&factory)).await.unwrap();
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let state = report
        .reports
        .iter()
        .find(|law| law.law == PluginLaw::StateSurvivesReopen)
        .unwrap();
    assert_eq!(
        state.result,
        PluginLawResult::Unavailable {
            reason: CertificationUnavailable::ScenarioDidNotPublish,
        },
        "{state}"
    );
    assert_ne!(report.verdict(), PluginCertificationVerdict::Full);

    // The same hook really can publish; its unexercised branch is the gap.
    let mut harness = PluginHarness::new(factory, HostPolicy::standard())
        .await
        .unwrap();
    harness.create(PluginOptions::default()).await.unwrap();
    harness.script([text_response("done")]);
    harness
        .publish_then_reopen(lash::TurnInput::text("publish now"))
        .await
        .unwrap();
    assert_eq!(
        harness.plugin_state().await.unwrap().plugins["late-state"].values["counter"],
        serde_json::json!(1)
    );
}

/// An HTTP-style lookup provider whose body publishes nothing, declared
/// stateless when `stateless`.
fn http_lookup(stateless: bool) -> Result<Arc<dyn PluginFactory>, Box<dyn std::error::Error>> {
    struct Lookup;
    #[lash::async_trait]
    impl lash::tools::StaticToolExecute for Lookup {
        async fn execute(&self, _: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
            lash::tools::ToolOutcome::ok(serde_json::json!("sunny")).into()
        }
    }
    let lookup = lash::tools::ToolDefinition::raw(
        "lookup",
        "tools.lookup",
        "Fetch the forecast",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    )?
    .with_execution(std::time::Duration::from_secs(1));
    let mut entries = PluginRegistrar::new("weather");
    entries
        .tools()
        .provider(lash::tool_provider_key!("http"), move |_, _| {
            Ok(Arc::new(lash::tools::StaticToolProvider::new(
                vec![lookup.clone()],
                Lookup,
            )))
        })?;
    let recipe = PluginRecipe::from_registrar("weather", entries);
    Ok(Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("weather"),
        if stateless {
            recipe.stateless()
        } else {
            recipe
        },
    )))
}
/// FIG-6551: advisory lifecycle observers are exercised without weakening
/// the recorded tool laws or requiring an author scenario for observation.
#[tokio::test]
async fn tools_with_a_lifecycle_observer_certify_full() {
    use std::sync::Mutex;

    struct Lookup;
    #[lash::async_trait]
    impl lash::tools::StaticToolExecute for Lookup {
        async fn execute(&self, _: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
            lash::tools::ToolOutcome::ok(serde_json::json!("sunny")).into()
        }
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut entries = PluginRegistrar::new("weather");
    entries
        .tools()
        .provider(lash::tool_provider_key!("http"), |_, _| {
            let definition = lash::tools::ToolDefinition::raw(
                "lookup",
                "tools.lookup",
                "Fetch the forecast",
                serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
                serde_json::json!({"type":"string"}),
            )
            .unwrap()
            .with_execution(std::time::Duration::from_secs(1));
            Ok(Arc::new(lash::tools::StaticToolProvider::new(
                vec![definition],
                Lookup,
            )))
        })
        .unwrap();
    entries
        .lifecycle()
        .on_event(lash::hook_key!("settle"), {
            let observed = Arc::clone(&observed);
            move |_, _| {
                let observed = Arc::clone(&observed);
                Ok(Arc::new(move |event| {
                    let observed = Arc::clone(&observed);
                    Box::pin(async move {
                        let name = match event {
                            PluginLifecycleEvent::SessionRestored { .. } => "restored",
                            PluginLifecycleEvent::TurnFinalized(_) => "finalized",
                            PluginLifecycleEvent::SessionConfigChanged(_) => "config-changed",
                        };
                        observed.lock().unwrap().push(name);
                        Ok(())
                    })
                }))
            }
        })
        .unwrap();
    let factory = Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("weather"),
        PluginRecipe::from_registrar("weather", entries).stateless(),
    ));
    let scenario: Arc<dyn PluginScenario> = Arc::new(CallScenario {
        callback: PluginCallbackIdentity::tool_provider("weather", "http"),
        tool: "tools__lookup",
        args: serde_json::json!({}),
        support: None,
    });
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    assert_eq!(
        report.verdict(),
        PluginCertificationVerdict::Full,
        "{:#?}",
        report.reports
    );
    assert!(
        report.reports.iter().any(|law| {
            law.callback.key == "runtime_event:settle"
                && law.law == PluginLaw::LifecycleObserverCompletes
                && law.result == PluginLawResult::Passed
        }),
        "the observer has its own completed certification law"
    );
    let observed = observed.lock().unwrap();
    assert!(
        observed.contains(&"restored"),
        "the observer saw session materialization"
    );
    assert!(
        observed.contains(&"finalized"),
        "the observer completed a finalized turn"
    );
}

/// LIFECYCLE-OBSERVER-COMPLETES: advisory runtime failures cannot veto a
/// turn, but certification must report the observer's failure under its key.
#[tokio::test]
async fn a_failing_lifecycle_observer_fails_certification() {
    let factory = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("broken-observer"),
        PluginSpec::new().with_runtime_event(
            lash::hook_key!("settle"),
            Arc::new(|event| {
                Box::pin(async move {
                    if matches!(event, PluginLifecycleEvent::TurnFinalized(_)) {
                        tokio::task::yield_now().await;
                        Err(PluginError::Session("settle sink unavailable".into()))
                    } else {
                        Ok(())
                    }
                })
            }),
        ),
    ));
    let report = certify_plugin(factory).await.unwrap();
    assert_eq!(
        report.verdict(),
        PluginCertificationVerdict::Failed,
        "{:#?}",
        report.reports
    );
    let failure = report
        .failures()
        .next()
        .expect("the observer failure is reported");
    assert_eq!(failure.callback.owner, "broken-observer");
    assert_eq!(failure.callback.key, "runtime_event:settle");
    assert_eq!(failure.law, PluginLaw::LifecycleObserverCompletes);
    assert!(failure.to_string().contains("settle sink unavailable"));
}

async fn certify_http_lookup(
    stateless: bool,
) -> Result<PluginCertification, Box<dyn std::error::Error>> {
    let scenario: Arc<dyn PluginScenario> = Arc::new(CallScenario {
        callback: PluginCallbackIdentity::tool_provider("weather", "http"),
        tool: "tools__lookup",
        args: serde_json::json!({}),
        support: None,
    });
    let report =
        certify_plugin_with(http_lookup(stateless)?, HostPolicy::standard(), &[scenario]).await?;
    no_failed_law(&report);
    assert_eq!(report.reports.len(), 4, "{report:#?}");
    Ok(report)
}
fn state_law(report: &PluginCertification) -> Option<&PluginLawResult> {
    report
        .reports
        .iter()
        .find(|report| report.law == PluginLaw::StateSurvivesReopen)
        .map(|report| &report.result)
}
/// STATELESS-TOOL-BODY-CERTIFIES-FULL (FIG-6278): a provider declared
/// stateless cannot publish, so its tool body's state law has no subject.
#[tokio::test]
async fn a_stateless_http_provider_certifies_full() {
    let report = certify_http_lookup(true).await.unwrap();
    assert_eq!(
        state_law(&report),
        Some(&PluginLawResult::NotApplicable {
            reason: CertificationNotApplicable::NoPublishedState
        })
    );
    assert_eq!(
        report.verdict(),
        PluginCertificationVerdict::Full,
        "{report:#?}"
    );
}
/// UNPUBLISHED-STATE-CAPABLE-BODY-IS-UNAVAILABLE (FIG-6264): without the
/// declaration the same body could publish, so a scenario that leaves the
/// namespace empty leaves the state law unexercised.
#[tokio::test]
async fn a_state_capable_provider_that_does_not_publish_stays_unavailable() {
    let report = certify_http_lookup(false).await.unwrap();
    assert_eq!(
        state_law(&report),
        Some(&PluginLawResult::Unavailable {
            reason: CertificationUnavailable::ScenarioDidNotPublish
        })
    );
    assert_eq!(
        report.verdict(),
        PluginCertificationVerdict::Partial,
        "{report:#?}"
    );
}

#[cfg(feature = "rlm")]
#[path = "../src/testing/plugin_harness/certification/packages.rs"]
mod packages;
#[cfg(feature = "rlm")]
use packages::*;

/// ENV-UNCHANGED (FIG-7376): a warm session's clean model rounds enter no
/// tool or bound package method, including assembly and dependency queries.
/// FIG-7407: re-applying identical owner config also stays silent; a grant
/// removal refreshes its dependent source without asking unrelated owners.
#[tokio::test]
async fn env_unchanged_provider_methods_across_clean_model_rounds() {
    use lash::tools::*;
    struct Grants;
    impl ConfigOwner for Grants {
        type Create = u32;
        type Recorded = u32;
        type Refusal = String;
        type RunOptions = u32;
        fn create(&self, input: Option<u32>) -> Result<Option<u32>, String> {
            Ok(Some(input.unwrap_or(0)))
        }
        fn validate(&self, _: &u32, _: Option<&u32>, _: &CandidateFacts<'_>) -> Result<(), String> {
            Ok(())
        }
        fn apply_run_options(&self, _: &u32, options: u32) -> Result<u32, String> {
            Ok(options)
        }
    }
    #[derive(Clone, serde::Serialize, serde::Deserialize, lash::plugins::schemars::JsonSchema)]
    #[schemars(crate = "lash::plugins::schemars")]
    struct SetGrant {
        granted: bool,
    }
    impl ConfigCommand for SetGrant {
        type Owner = Grants;
        type Output = ();
        const NAME: &'static str = "set_grant";
    }
    struct Tools(&'static str, Vec<ToolDefinition>);
    #[lash::async_trait]
    impl ToolProvider for Tools {
        fn tool_manifests(
            &self,
            config: &OwnerConfigView,
        ) -> Result<Vec<ToolManifest>, PluginError> {
            let omitted = config.decode::<u32>(self.0).unwrap().unwrap();
            Ok(self
                .1
                .iter()
                .enumerate()
                .filter(|(i, _)| *i >= omitted as usize)
                .map(|(_, d)| d.manifest.clone())
                .collect())
        }
        fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            self.1
                .iter()
                .find(|d| d.manifest.tool() == name)
                .map(|d| Arc::new(d.contract.clone()))
        }
        fn readable_binding_formats(&self) -> &'static [FormatVersion] {
            &[FormatVersion::ONE]
        }
        fn link_payload(
            &self,
            _: &ToolManifest,
            _: &OwnerConfigView,
        ) -> Option<(FormatVersion, serde_json::Value)> {
            Some((FormatVersion::ONE, serde_json::json!({"route":"counted"})))
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            panic!("no tool call in this law")
        }
    }
    // Keep the trait's default snapshot on this adapter, while counting the
    // real per-tool calls it makes on its runner. Forwarding a snapshot alone
    // cannot observe virtual calls made inside the inner provider.
    struct Snapshots(Arc<dyn ToolProvider>);
    #[lash::async_trait]
    impl ToolProvider for Snapshots {
        fn tool_manifests(
            &self,
            config: &OwnerConfigView,
        ) -> Result<Vec<ToolManifest>, PluginError> {
            self.0.tool_manifests(config)
        }
        fn resolve_contract(
            &self,
            name: &str,
            config: &OwnerConfigView,
        ) -> Option<Arc<ToolContract>> {
            self.0.resolve_contract(name, config)
        }
        fn readable_binding_formats(&self) -> &'static [FormatVersion] {
            self.0.readable_binding_formats()
        }
        fn link_payload(
            &self,
            manifest: &ToolManifest,
            config: &OwnerConfigView,
        ) -> Option<(FormatVersion, serde_json::Value)> {
            self.0.link_payload(manifest, config)
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            panic!("no tool call in this law")
        }
    }
    // Figments' bound publisher uses the default fragment -> advertise path,
    // which observes the same runner again. Both paths must stay silent.
    struct Publisher(Arc<dyn lash::package::PackageProvider>);
    #[lash::async_trait]
    impl lash::package::PackageProvider for Publisher {
        fn environment_tool_sources(
            &self,
            key: &ToolProviderHandle,
        ) -> std::collections::BTreeSet<ToolProviderHandle> {
            self.0.environment_tool_sources(key)
        }
        fn roots(
            &self,
            config: &OwnerConfigView,
        ) -> std::collections::BTreeSet<lash::package::Segment> {
            self.0.roots(config)
        }
        fn advertise(
            &self,
            config: &OwnerConfigView,
            context: &lash::package::PublicationContext,
        ) -> Result<Vec<lash::package::PackageContribution>, PluginError> {
            self.0.advertise(config, context)
        }
        async fn hold(
            &self,
            claim: &lash_core_store::artifact_referrer::ReferrerClaim,
            advertisements: &[&lash::package::PackageAdvertisement],
        ) -> Result<(), lash::package::EnginePortError> {
            self.0.hold(claim, advertisements).await
        }
    }
    fn counted_factory(
        owner: &'static str,
        tool_count: usize,
        counts: &lash_core::testing::CallbackCounts,
    ) -> Arc<dyn PluginFactory> {
        let key = lash::tool_provider_key!("tools");
        let tools = counts.tool_provider(
            PluginCallbackIdentity::tool_provider(owner, "tools"),
            Arc::new(Tools(
                owner,
                (0..tool_count)
                    .map(|i| {
                        ToolDefinition::raw(
                            format!("{owner}-tool-{i}"),
                            format!("{owner}.work{i}"),
                            "counted",
                            serde_json::json!({"type":"object"}),
                            serde_json::json!({"type":"null"}),
                        )
                        .unwrap()
                        .with_execution(std::time::Duration::from_secs(1))
                    })
                    .collect(),
            )),
        );
        let tools = counts.tool_provider(
            PluginCallbackIdentity::tool_provider(owner, "snapshot"),
            Arc::new(Snapshots(tools)),
        );
        let packages = counts.package_provider(
            PluginCallbackIdentity {
                owner: owner.into(),
                key: "package_provider:tools".into(),
            },
            Arc::new(lash::package::ToolPackageProvider::new(
                owner,
                key,
                [lash::package::Segment::new(owner).unwrap()].into(),
                tools.clone(),
            )),
        );
        let packages = counts.package_provider(
            PluginCallbackIdentity {
                owner: owner.into(),
                key: "package_provider:fragment".into(),
            },
            Arc::new(Publisher(packages)),
        );
        let mut entries = PluginRegistrar::new(owner);
        entries
            .tools()
            .provider_reading(key, &[FormatVersion::ONE], move |_, _| Ok(tools.clone()))
            .unwrap();
        entries
            .package_provider_bound(key, move |_, _| Ok(packages.clone()))
            .unwrap();
        let mut configuration = ConfigRegistrar::new(owner);
        configuration
            .owner_with_run_options(Grants, lash::RunOptionsKey::<u32>::new(owner))
            .unwrap();
        configuration
            .command::<SetGrant>(|_, command| {
                Ok(OwnerChange {
                    recorded: u32::from(!command.granted),
                    output: (),
                })
            })
            .unwrap();
        Arc::new(PluginRecipeFactory::new(
            PluginDeclaration::initial(owner),
            PluginRecipe::from_registrar(owner, entries)
                .stateless()
                .with_configuration(configuration),
        ))
    }
    async fn clean_turn(
        tool_count: usize,
        rounds: usize,
        grant_commits: &[bool],
        overrides: &[u32],
    ) -> std::collections::BTreeMap<(PluginCallbackIdentity, String), u64> {
        let counts = lash_core::testing::CallbackCounts::default();
        let factory = counted_factory("unchanged", tool_count, &counts);
        let mut harness = PluginHarness::new(factory, HostPolicy::standard())
            .await
            .unwrap();
        harness
            .install(counted_factory("unrelated", 1, &counts))
            .await
            .unwrap();
        harness.create(PluginOptions::default()).await.unwrap();
        harness.script([text_response("warm")]);
        harness
            .send(lash::TurnInput::text("warm the session"))
            .await
            .unwrap()
            .outcome()
            .await
            .unwrap();
        for (i, granted) in grant_commits.iter().enumerate() {
            let session = harness
                .core()
                .unwrap()
                .session(harness.session_id().clone())
                .open()
                .await
                .unwrap();
            let config = session.admin().config();
            let revision = config.revision().await.unwrap();
            let transaction = lash::config::ConfigTransaction::new()
                .then_entry(lash::config::ConfigCommandEntry {
                    owner: "unchanged".into(), command: "set_grant".into(),
                    args: serde_json::json!({"granted": granted}),
                })
                // A touched but content-identical owner must remain clean too.
                .then_entry(lash::config::ConfigCommandEntry {
                    owner: "unrelated".into(), command: "set_grant".into(),
                    args: serde_json::json!({"granted": true}),
                });
            let outcome = config
                .apply(
                    lash::config::ConfigWrite::new(format!("grant-{i}"), revision),
                    transaction,
                )
                .await
                .unwrap()
                .await_outcome(&config)
                .await
                .unwrap();
            assert!(
                matches!(
                    outcome,
                    lash::config::ConfigTransactionOutcome::Applied { .. }
                ),
                "{outcome:?}"
            );
        }
        fn run_spec(omitted: u32) -> lash::RunSpec {
            let mut overrides = lash::RunOverrides::default();
            overrides
                .owners
                .set(lash::RunOptionsKey::<u32>::new("unchanged"), omitted)
                .unwrap();
            lash::RunSpec::overrides(overrides)
        }
        if let Some(omitted) = overrides.first() {
            harness.script([text_response("override warm")]);
            harness
                .session()
                .await
                .unwrap()
                .send(lash::TurnInput::text("warm override"))
                .run(run_spec(*omitted))
                .await
                .unwrap()
                .outcome()
                .await
                .unwrap();
        }
        let baseline = counts.provider_snapshot();
        harness.script((0..rounds).map(|_| text_response("counted")));
        if rounds > 1 {
            harness.pause_model_after(0);
        }
        let turn = harness
            .session()
            .await
            .unwrap()
            .send(lash::TurnInput::text("counted"))
            .id("clean-rounds".into());
        let turn = match overrides.last() {
            Some(omitted) => turn.run(run_spec(*omitted)),
            None => turn,
        }
        .await
        .unwrap();
        if rounds > 1 {
            harness.wait_for_model_pause().await.unwrap();
            let steering = harness
                .session()
                .await
                .unwrap()
                .send(lash::TurnInput::text("reply counted"))
                .ingress(lash::persistence::TurnInputIngress::active_turn(
                    "clean-rounds",
                    lash::persistence::TurnInputCheckpointBoundary::BeforeCompletion,
                ))
                .await
                .unwrap();
            harness.release_model();
            steering.outcome().await.unwrap();
        }
        let answer = turn.outcome().await.unwrap();
        assert_eq!(answer.status(), lash::TurnStatus::Answered, "{answer:?}");
        assert_eq!(
            harness.requests().len(),
            rounds + 1 + usize::from(!overrides.is_empty()),
            "one warm call followed by the requested clean rounds"
        );
        let omitted = overrides
            .last()
            .copied()
            .unwrap_or_else(|| u32::from(!grant_commits.last().copied().unwrap_or(true)));
        let requests = harness.requests();
        let expected_tools: std::collections::BTreeSet<_> = (0..tool_count)
            .filter(|i| *i >= omitted as usize)
            .map(|i| format!("unchanged__work{i}"))
            .chain(["unrelated__work0".to_string()])
            .collect();
        assert_eq!(
            requests
                .last()
                .unwrap()
                .tools
                .iter()
                .map(|t| t.name.clone())
                .filter(|name| name.starts_with("unchanged__") || name.starts_with("unrelated__"))
                .collect::<std::collections::BTreeSet<_>>(),
            expected_tools,
            "the next model request must reflect the committed grants"
        );
        let after = counts.provider_snapshot();
        after
            .iter()
            .filter_map(|(method, n)| {
                let calls = n - baseline.get(method).copied().unwrap_or(0);
                (calls != 0).then_some((method.clone(), calls))
            })
            .collect()
    }
    let mut failures = Vec::new();
    for (tools, rounds, commits) in [
        (1, 1, vec![]),
        (31, 1, vec![]),
        (31, 2, vec![]),
        (31, 1, vec![true]),
    ] {
        let calls = clean_turn(tools, rounds, &commits, &[]).await;
        eprintln!(
            "ENV-UNCHANGED tools={tools} rounds={rounds} commits={commits:?} provider calls: {calls:?}"
        );
        if !calls.is_empty() {
            failures.push((tools, rounds, calls));
        }
    }
    // Separate lifetime owners still hold their package closures. The clean
    // override must make no provider observation or metadata query.
    let repeated_override = clean_turn(31, 1, &[], &[1, 1]).await;
    assert!(
        repeated_override.keys().all(|(_, method)| method == "hold"),
        "FIG-7407: identical non-default run override re-asked providers: {repeated_override:?}"
    );
    // A changed candidate still validates bound codecs for every owner.
    // Source collection itself must touch only the changed owner.
    let changed_override = clean_turn(31, 1, &[], &[1, 2]).await;
    assert!(
        changed_override
            .keys()
            .filter(|(_, method)| !matches!(method.as_str(), "hold" | "readable_binding_formats"))
            .all(|(callback, _)| callback.owner == "unchanged"),
        "changed override re-collected an unrelated source: {changed_override:?}"
    );
    for method in ["environment_snapshot", "tool_manifests", "link_payload"] {
        assert!(
            changed_override
                .iter()
                .any(|((_, called), n)| called == method && *n > 0),
            "changed run override must re-collect {method}: {changed_override:?}"
        );
    }
    let calls = clean_turn(31, 1, &[false], &[]).await;
    // A new version holds its whole package closure, including unchanged
    // sources. Only collection of the changed owner is permitted here.
    assert!(
        calls
            .keys()
            .filter(|(_, method)| method != "hold")
            .all(|(callback, _)| callback.owner == "unchanged"),
        "a grant change must ask exactly the dependent owner: {calls:?}"
    );
    for method in ["environment_snapshot", "tool_manifests", "link_payload"] {
        assert!(
            calls
                .iter()
                .any(|((_, called), n)| called == method && *n > 0),
            "changed config must re-collect {method}: {calls:?}"
        );
    }
    assert!(
        failures.is_empty(),
        "ENV-UNCHANGED: clean rounds re-asked providers: {failures:?}"
    );
}
