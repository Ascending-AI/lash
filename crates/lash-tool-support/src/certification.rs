//! FIG-6093: selected tools run through admission, recording and reopen.
use lash::plugins::{PluginCallbackIdentity, PluginFactory, PluginOptions};
use lash::testing::*;
use std::sync::Arc;
struct Echo;
#[lash::async_trait]
impl crate::StaticToolExecute for Echo {
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("echo")).into()
    }
}
struct Scenario;
#[lash::async_trait]
impl PluginScenario for Scenario {
    fn callback(&self) -> PluginCallbackIdentity {
        PluginCallbackIdentity::tool_provider("selected", "tools")
    }
    async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
        harness.create(PluginOptions::default()).await
    }
    fn input(&self) -> lash::TurnInput {
        lash::TurnInput::text("echo")
    }
    fn responses(&self) -> Vec<lash::provider::LlmResponse> {
        vec![
            lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::ToolCall {
                    call_id: "echo".into(),
                    tool_name: "tools__echo".into(),
                    input_json: "{}".into(),
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
#[tokio::test]
async fn selected_tools_pass_certification() {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "echo",
        "tools.echo",
        "echo",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(1));
    let factory: Arc<dyn PluginFactory> = Arc::new(crate::SelectedToolsPlugin::new(
        "selected",
        Arc::new(crate::StaticToolProvider::new(vec![definition], Echo)),
    ));
    let scenario: Arc<dyn PluginScenario> = Arc::new(Scenario);
    let report = certify_plugin_with(factory, HostPolicy::standard(), &[scenario])
        .await
        .unwrap();
    let failures: Vec<_> = report.failures().map(ToString::to_string).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(report.reports.iter().any(|report| report.law
        == PluginLaw::ReplayInvokesNoRecordedCallback
        && report.result == PluginLawResult::Passed));
}

/// PKG-JOIN: a shared package must dispatch through each original runner,
/// even when the providers reuse a tool ID. Duplicate operations are refused
/// whether their manifests agree or conflict.
#[tokio::test]
async fn joined_package_preserves_runners_and_refuses_duplicate_operations() {
    use lash::package::*;
    use lash::plugins::{
        PluginDeclaration, PluginSessionRequest, PluginSpec, RegistryKind, SessionAuthorityContext,
        StaticPluginFactory, ToolProviderHandle,
    };
    use lash::tools::{ExecutionPolicy, OwnerConfigView, ToolDefinition, ToolProvider};

    struct Answer(&'static str);
    #[lash::async_trait]
    impl crate::StaticToolExecute for Answer {
        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            assert_eq!(call.name(), format!("cenote.{}", self.0));
            lash_core::ToolOutcome::ok(serde_json::json!(self.0)).into()
        }
    }

    let config = OwnerConfigView::default();
    let root = Segment::new("cenote").unwrap();
    let domain = PublicationDomain("joined-law".into());
    let context = PublicationContext {
        domain: domain.clone(),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("packages/code".into()),
        root: root.clone(),
    };
    let publisher = ToolProviderHandle::new(
        RegistryKind::Session,
        "packages",
        lash::tool_provider_key!("code"),
    );
    let key = lash::tool_provider_key!("tools");
    let mut factories: Vec<Arc<dyn PluginFactory>> = Vec::new();
    let mut sources = Vec::new();
    let mut expected = Vec::new();
    for (owner, policy) in [
        ("knowledge", ExecutionPolicy::Once),
        (
            "decide",
            ExecutionPolicy::repeatable(std::num::NonZeroU32::new(2).unwrap(), 1, 2),
        ),
    ] {
        let definition = ToolDefinition::raw(
            "shared-tool-id",
            format!("cenote.{owner}"),
            owner,
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1))
        .with_execution_policy(policy);
        let provider: Arc<dyn ToolProvider> = Arc::new(crate::StaticToolProvider::new(
            vec![definition.clone()],
            Answer(owner),
        ));
        sources.push(crate::ToolPackageProvider::new(
            owner,
            key,
            [root.clone()].into(),
            provider.clone(),
        ));
        factories.push(Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial(owner),
            PluginSpec::new().with_tool_provider(key, provider),
        )));
        expected.push((
            ToolProviderHandle::new(RegistryKind::Session, owner, key),
            definition,
        ));
    }
    let joined = Arc::new(crate::JoinedToolPackageProvider::new(sources));
    factories.push(Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("packages"),
        PluginSpec::new().with_package_provider(lash::tool_provider_key!("code"), joined.clone()),
    )));
    let session = lash_core::testing::test_plugin_host(factories)
        .build_session(PluginSessionRequest::creation(
            "joined-package",
            SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let observations = session
        .environment_sources(Arc::new(|_| {}))
        .unwrap()
        .collect(&Default::default(), [], &config)
        .unwrap();
    let fragment = observations
        .iter()
        .map(|s| &s.fragment)
        .find(|f| {
            f.package_roots
                .iter()
                .any(|p| p.publication.path.to_string() == "cenote")
        })
        .unwrap();
    assert_eq!(fragment.package_roots.len(), 1);
    let PackageAdvertisement::Enumerated(package) = &fragment.package_roots[0].advertisement else {
        panic!("an operation package is enumerated")
    };
    assert_eq!(
        package
            .members
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["decide", "knowledge"]
    );
    let fixture = lash_core::testing::ToolCallFixture::mock();
    let attempt = fixture.attempt("joined-call");
    let args = serde_json::json!({});
    for (runner, definition) in &expected {
        let candidate = fragment
            .routes
            .iter()
            .find(|r| r.operation == definition.manifest.operation)
            .unwrap();
        assert_eq!(&candidate.route.provider, runner);
        assert_eq!(candidate.route.policy, definition.manifest.execution_policy);
        assert_eq!(&candidate.definition, definition);
        let provider = session
            .composition()
            .tool_registry()
            .provider(&candidate.route.provider)
            .unwrap()
            .clone();
        let lash_core::ToolAttemptOutcome::Done { result, .. } = provider
            .execute(lash_core::ToolCall::new(
                &candidate.definition.manifest,
                &candidate.definition.contract,
                &args,
                &attempt,
            ))
            .await
        else {
            panic!("the selected runner completes")
        };
        assert_eq!(
            result.into_output(),
            lash_core::ToolCallOutput::success(serde_json::json!(runner.owner()))
        );
    }
    // Explicit namespace authoring and captured environment projection agree.
    let advertised = joined.advertise(&config, &context).unwrap();
    assert_eq!(advertised.len(), 1);
    assert_eq!(
        &advertised[0].advertisement,
        &fragment.package_roots[0].advertisement
    );
    for candidate in &fragment.routes {
        assert!(advertised[0].routes.contains(candidate));
    }
    assert_eq!(advertised[0].routes.len(), fragment.routes.len());
    let fallback = joined
        .environment_fragment(&config, &domain, &PublicationScope::Host, &publisher, None)
        .unwrap();
    for candidate in &fragment.routes {
        assert!(fallback.routes.contains(candidate));
    }
    assert_eq!(fallback.routes.len(), fragment.routes.len());

    for conflicting_metadata in [false, true] {
        let mut entries = fragment.tools.clone();
        entries[1].definition.manifest.operation = entries[0].definition.manifest.operation.clone();
        if !conflicting_metadata {
            entries[1].definition = entries[0].definition.clone();
        }
        let error = joined
            .environment_fragment(
                &config,
                &domain,
                &PublicationScope::Host,
                &publisher,
                Some(&entries),
            )
            .unwrap_err();
        assert!(matches!(error, lash::plugins::PluginError::Package(error)
            if matches!(&*error, EnginePortError::Refused(AdmissionRefusal::PathConflict { path })
                if *path == PackagePath::parse("cenote").unwrap())));
    }
    // A disjoint child avoids a path collision, isolating root ownership.
    let advertisement = PackageAdvertisement::Enumerated(Package {
        path: PackagePath::parse("cenote.foreign").unwrap(),
        members: Default::default(),
    });
    let foreign = PublicationContext {
        publisher: PublisherInstance("foreign/code".into()),
        ..context.clone()
    };
    let mut prior = RecordedPackages::default();
    prior.snapshot.advertisements.push(AcceptedAdvertisement {
        publication: lash_core::plugin::packages::operation_publication(&foreign, &advertisement)
            .unwrap(),
        advertisement,
    });
    assert!(
        matches!(session.composition().advertise_packages(&config, &prior, &domain, PublicationScope::Host),
        Err(lash::plugins::PluginError::Package(error))
            if matches!(*error, EnginePortError::Refused(AdmissionRefusal::NotOwner { .. })))
    );
}
