//! FIG-7183: saved callable reach and recorded per-run routes share normal start admission.
use super::*;
use lash::package::*;
use lash::workflow::document::{CaptureValue, Export, FunctionValue};
use lash_core::plugin::packages::operation_publication;
use std::collections::{BTreeMap, BTreeSet};

const ROUTES: lash::process::ProcessConfigKey<Config> =
    lash::process::ProcessConfigKey::new("workflow-routes", lash::plugins::FormatVersion::ONE);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Config {
    root: String,
    provider: String,
}
struct Runner(&'static str);
#[lash::async_trait]
impl lash::tools::StaticToolExecute for Runner {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(serde_json::json!([self.0, call.args["value"]])).into()
    }
}
fn handle(key: &'static str) -> lash::plugins::ToolProviderHandle {
    ROUTES.tool_provider(if key == "first" {
        lash::tool_provider_key!("first")
    } else {
        lash::tool_provider_key!("second")
    })
}
fn tool_id(provider: &str) -> lash::tools::ToolId {
    lash::tools::ToolId::new(format!("tool:{provider}-echo"))
}
fn definitions(provider: &str) -> Vec<lash::tools::ToolDefinition> {
    let mut definitions = echo_definitions();
    let distinct = lash::tools::ToolDefinition::raw(
        tool_id(provider),
        "tools.echo",
        "Answers its value.",
        serde_json::json!({"type": "object"}),
        serde_json::json!({}),
    )
    .unwrap()
    .declared_contract(lash_vocabulary::operations::ContractId::new("1"))
    .with_execution(std::time::Duration::from_secs(120));
    definitions.push(distinct);
    definitions
}

struct Packages;
#[lash::async_trait]
impl lash::package::PackageProvider for Packages {
    fn roots(&self, config: &lash::tools::OwnerConfigView) -> BTreeSet<Segment> {
        let config = config
            .decode::<Config>(ROUTES.component())
            .unwrap()
            .unwrap();
        [
            Segment::new("tools").unwrap(),
            Segment::new(config.root).unwrap(),
        ]
        .into()
    }
    fn advertise(
        &self,
        config: &lash::tools::OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, lash::plugins::PluginError> {
        let settings = config
            .decode::<Config>(ROUTES.component())
            .unwrap()
            .unwrap();
        if context.root.as_str() != "tools" {
            let advertisement = PackageAdvertisement::Enumerated(Package {
                path: Vec1::new(context.root.clone()),
                members: Default::default(),
            });
            return Ok(vec![PackageContribution {
                publication: operation_publication(context, &advertisement).unwrap(),
                advertisement,
                routes: Vec::new(),
            }]);
        }
        let provider = lash::tools::StaticToolProvider::new(echo_definitions(), Runner("first"));
        let packages = lash::package::ToolPackageProvider::new(
            ROUTES.component(),
            lash::tool_provider_key!("first"),
            [Segment::new("tools").unwrap()].into(),
            Arc::new(provider),
        );
        let mut contributions = packages.advertise(config, context)?;
        for contribution in &mut contributions {
            for candidate in &mut contribution.routes {
                let provider = if settings.provider == "second" {
                    "second"
                } else {
                    "first"
                };
                candidate.route.provider = handle(provider);
                candidate.definition =
                    definitions(provider).remove(usize::from(settings.provider == "both-distinct"));
                candidate.route.tool = candidate.definition.manifest.id.clone();
            }
            if settings.provider.starts_with("both") {
                let mut second = contribution.routes[0].clone();
                second.route.provider = handle("second");
                second.definition =
                    definitions("second").remove(usize::from(settings.provider == "both-distinct"));
                second.route.tool = second.definition.manifest.id.clone();
                contribution.routes.push(second);
            }
        }
        Ok(contributions)
    }
    async fn hold(
        &self,
        _: &lash::persistence::ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), lash::package::EnginePortError> {
        Ok(())
    }
}
struct Component(lash::process::StaticProcessComponent);
impl lash::process::ProcessComponentFactory for Component {
    fn declaration(&self) -> lash::process::ProcessComponentDeclaration {
        self.0.declaration()
    }
    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Config>()
    }
    fn validate(
        &self,
        entry: &lash::process::ProcessComponentEntry,
        _: &lash::process::ProcessEnv,
    ) -> Result<(), lash::process::ProcessEnvRefusal> {
        entry.decode(ROUTES).map(|_: Config| ()).map_err(|error| {
            lash::process::ProcessEnvRefusal::ConfigRefused {
                id: ROUTES.component().into(),
                reason: error.to_string(),
            }
        })
    }
    fn recipe(self: Arc<Self>) -> Result<lash::plugins::ProcessRecipe, lash::plugins::PluginError> {
        Arc::new(self.0.clone()).recipe()
    }
}
async fn setup() -> lash::LashCore {
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let factory = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(ROUTES.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(
                lash::tool_provider_key!("first"),
                Arc::new(lash::tools::StaticToolProvider::new(
                    definitions("first"),
                    Runner("first"),
                )),
            )
            .with_tool_provider(
                lash::tool_provider_key!("second"),
                Arc::new(lash::tools::StaticToolProvider::new(
                    definitions("second"),
                    Runner("second"),
                )),
            )
            .with_package_provider(lash::tool_provider_key!("packages"), Arc::new(Packages)),
    )
    .as_component(ROUTES.format())
    .unwrap();
    builder_with_process_tools(stores, false, EchoComponent::Installed)
        .await
        .process_component(Arc::new(Component(factory)), ROUTES)
        .build(node_identity("callable-env"))
        .unwrap()
}
fn base(root: &str, provider: &str) -> lash::process::ProcessEnv {
    let config = Config {
        root: root.into(),
        provider: provider.into(),
    };
    // Reconstruct the exact recorded config, rather than consulting mutable host state.
    let recorded: Config = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(ROUTES, &recorded)
        .build()
        .unwrap()
}
async fn callable(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    request: &lash::vm::WorkflowEnvironmentRequest,
    captured: Option<FunctionValue>,
) -> FunctionValue {
    let body = if captured.is_some() {
        "let direct = perform 0({value: \"root\"}) as Any let nested = apply helper() return [direct,nested]"
    } else {
        "return perform 0({value: \"captured\"}) as Any"
    };
    // A closure states helper as a frozen free variable, independently of its defining Link.
    let text = format!(
        "kernel 1\nnumbers float\n{}\nmain {{ let saved = fn() {{ {body} }} }}",
        echo_import_header(0)
    );
    let mut document = lash_kernel_doc::parse_document(&text).unwrap();
    export_closure(&mut document, "saved", &[], lash::schema::Type::Json);
    let Export::Function(export) = document.exports[&Name::new("saved")].clone() else {
        panic!("function")
    };
    let start = core
        .host_artifacts()
        .publish_workflow(
            pin,
            &document.clone().into(),
            &StartSelector::Main,
            request,
            None,
            None,
        )
        .await
        .unwrap();
    FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: Vec::new(),
        callable: CallableId {
            link: start.link,
            function: export.function,
        },
        captures: captured
            .map(|function| {
                (
                    Name::new("helper"),
                    CaptureValue::Function(Box::new(function)),
                )
            })
            .into_iter()
            .collect(),
        signature: export.signature,
        convention: lash::workflow::document::StartConvention::Direct,
        representation: lash::workflow::document::FunctionRepresentation::Bare,
        creation: None,
        token: None,
        captions: lash::workflow::document::Captions::empty(document.identity()),
    }
}
async fn saved(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    request: &lash::vm::WorkflowEnvironmentRequest,
) -> (lash::process::ProcessDefinition, FunctionValue) {
    let helper = callable(core, pin, request, None).await;
    let function = callable(core, pin, request, Some(helper.clone())).await;
    let origin = CallableOrigin {
        authored: [helper.callable.link, function.callable.link].into(),
        publications: request
            .snapshot
            .advertisements
            .iter()
            .map(|ad| ad.publication.key())
            .collect(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(pin, &function, request, &origin, None)
        .await
        .unwrap();
    (definition, function)
}

/// K-REACH-REQUIREMENTS: pre-save and published inspection expose exactly the uses routing demands.
#[tokio::test]
async fn a_callable_reports_root_and_captured_operation_uses_before_save_and_link() {
    let core = setup().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let base = base("tenant", "both");
    let (packages, request) = namespace(&core, &pin, &base).await;
    let helper = callable(&core, &pin, &request, None).await;
    let function = callable(&core, &pin, &request, Some(helper.clone())).await;
    assert_ne!(function.callable.link, helper.callable.link);
    let origin = CallableOrigin {
        authored: [helper.callable.link, function.callable.link].into(),
        publications: request
            .snapshot
            .advertisements
            .iter()
            .map(|ad| ad.publication.key())
            .collect(),
    };
    let (definition, inspection) = artifacts
        .inspect_callable(&function, &request, &origin)
        .await
        .unwrap();
    // Inspection must not publish the callable's process definition.
    assert!(
        artifacts
            .get_definition(&definition.id)
            .await
            .unwrap()
            .is_none()
    );
    let root_use = OperationUse {
        link: function.callable.link,
        slot: ImportSlot(0),
    };
    let capture_use = OperationUse {
        link: helper.callable.link,
        slot: ImportSlot(0),
    };
    let mut expected = [root_use, capture_use];
    expected.sort();
    assert_eq!(
        inspection.operations.keys().copied().collect::<Vec<_>>(),
        expected
    );
    for need in inspection.operations.values() {
        assert!(matches!(need, OperationNeed::Fixed { name, .. }
            if name.package == Vec1::new(Segment::new("tools").unwrap())
                && name.member == Segment::new("echo").unwrap()));
    }
    let linked = artifacts
        .inspect_target(&lash::process::LinkTarget::Linked(LinkedStart {
            link: helper.callable.link,
            selector: StartSelector::Export(Name::new("saved")),
        }))
        .await
        .unwrap();
    assert_eq!(linked.operations.len(), 1);
    assert_eq!(
        linked.operations[&capture_use],
        inspection.operations[&capture_use]
    );
    let published = artifacts
        .publish_callable(&pin, &function, &request, &origin, None)
        .await
        .unwrap();
    assert_eq!(published, definition);
    let target = lash::process::LinkTarget::Definition(published.id);
    assert_eq!(artifacts.inspect_target(&target).await.unwrap(), inspection);
    let selected: BTreeMap<_, _> = expected
        .into_iter()
        .map(|use_| {
            (
                use_,
                (handle("second"), lash::tools::ToolId::new("tool:echo")),
            )
        })
        .collect();
    for use_ in inspection.operations.keys() {
        let mut missing = selected.clone();
        missing.remove(use_);
        let error = artifacts
            .link_process_env(&base, &target, &packages, &missing)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, lash::EmbedError::Plugin(lash::plugins::PluginError::Package(cause)) if matches!(&**cause, EnginePortError::Refused(AdmissionRefusal::InvalidDocument { message }) if message.contains(&format!("{use_:?}")))),
            "the omitted ambiguous OperationUse is named: {error:?}"
        );
    }
    let env = artifacts
        .link_process_env(&base, &target, &packages, &selected)
        .await
        .unwrap();
    assert_eq!(
        env.execution().uses.keys().copied().collect::<Vec<_>>(),
        expected
    );
    core.shutdown().await.unwrap();
}

/// K-CALLABLE-ROUTES: root and captured defining Links each require an explicit ambiguous-route selection.
#[tokio::test]
async fn a_saved_callable_selects_routes_for_root_and_captured_links() {
    let core = setup().await;
    let pin = lash::process::HostArtifactPin::mint();
    let base = base("tenant", "both");
    let (packages, request) = namespace(&core, &pin, &base).await;
    let (definition, function) = saved(&core, &pin, &request).await;
    let target = lash::process::LinkTarget::Definition(definition.id.clone());
    let helper = match &function.captures[&Name::new("helper")] {
        CaptureValue::Function(value) => value,
        _ => panic!("function capture"),
    };
    let root_use = OperationUse {
        link: function.callable.link,
        slot: ImportSlot(0),
    };
    let capture_use = OperationUse {
        link: helper.callable.link,
        slot: ImportSlot(0),
    };
    let mut selected = BTreeMap::new();
    let mut uses = [root_use, capture_use];
    uses.sort();
    for use_ in uses {
        let error = core
            .host_artifacts()
            .link_process_env(&base, &target, &packages, &selected)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, lash::EmbedError::Plugin(lash::plugins::PluginError::Package(cause)) if matches!(&**cause, lash::package::EnginePortError::Refused(lash::package::AdmissionRefusal::InvalidDocument { message }) if message.contains(&format!("{use_:?}")))),
            "the ambiguous OperationUse is named: {error:?}"
        );
        selected.insert(
            use_,
            (handle("second"), lash::tools::ToolId::new("tool:echo")),
        );
    }
    let env = core
        .host_artifacts()
        .link_process_env(&base, &target, &packages, &selected)
        .await
        .unwrap();
    assert!(env.execution().authored.contains(&root_use.link));
    assert!(env.execution().authored.contains(&capture_use.link));
    assert_eq!(env.execution().uses.len(), 2);
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&pin, &env)
        .await
        .unwrap();
    let process = start_with(&core, &env_ref, &definition, "selected", Default::default()).await;
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!([["second", "root"], ["second", "captured"]])
    );
    core.shutdown().await.unwrap();
}

/// K-RECORDED-ROUTES: one saved callable runs under each independently recorded config and namespace.
#[tokio::test]
async fn a_saved_callable_starts_under_each_recorded_configs_routes() {
    let core = setup().await;
    let pin = lash::process::HostArtifactPin::mint();
    let first = base("tenant_first", "first");
    let second = base("tenant_second", "second");
    let (first_packages, first_request) = namespace(&core, &pin, &first).await;
    let (definition, _) = saved(&core, &pin, &first_request).await;
    let (second_packages, _) = namespace(&core, &pin, &second).await;
    let mut starts = Vec::new();
    for (base, packages, provider, root) in [
        (&first, &first_packages, "first", "tenant_first"),
        (&second, &second_packages, "second", "tenant_second"),
    ] {
        assert!(
            packages
                .snapshot
                .advertisements
                .iter()
                .any(|ad| ad.publication.path.to_string() == root)
        );
        let env = core
            .host_artifacts()
            .link_process_env(
                base,
                &lash::process::LinkTarget::Definition(definition.id.clone()),
                packages,
                &BTreeMap::new(),
            )
            .await
            .unwrap();
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&pin, &env)
            .await
            .unwrap();
        starts.push((
            start_with(&core, &env_ref, &definition, provider, Default::default()).await,
            provider,
        ));
    }
    for (process, provider) in starts {
        assert_eq!(
            finished(&core, &process).await,
            serde_json::json!([[provider, "root"], [provider, "captured"]])
        );
    }
    core.shutdown().await.unwrap();
}

/// K-DEFINITION-LINK: a saved Linked definition retains the route semantics of its LinkedStart.
#[tokio::test]
async fn a_published_linked_definition_uses_its_original_start() {
    let core = super::core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let base = environment(false);
    let publication = publish(
        &core,
        &pin,
        &guarded(GUARDED).into(),
        &Name::new("guarded"),
        &base,
        None,
    )
    .await
    .unwrap();
    let (packages, _) = namespace(&core, &pin, &base).await;
    let linked = core
        .host_artifacts()
        .link_process_env(
            &base,
            &lash::process::LinkTarget::Definition(publication.definition.id),
            &packages,
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(linked, publication.environment);
    core.shutdown().await.unwrap();
}

/// K-ROUTE-CONSISTENCY: a per-operation surface refuses distinct per-use providers or tools before starting.
#[tokio::test]
async fn a_saved_callable_refuses_distinct_routes_for_the_same_operation() {
    let core = setup().await;
    let pin = lash::process::HostArtifactPin::mint();
    for distinct in [false, true] {
        let base = base("tenant", if distinct { "both-distinct" } else { "both" });
        let (packages, request) = namespace(&core, &pin, &base).await;
        let (definition, function) = saved(&core, &pin, &request).await;
        let helper = match &function.captures[&Name::new("helper")] {
            CaptureValue::Function(value) => value,
            _ => panic!("function capture"),
        };
        let root_use = OperationUse {
            link: function.callable.link,
            slot: ImportSlot(0),
        };
        let capture_use = OperationUse {
            link: helper.callable.link,
            slot: ImportSlot(0),
        };
        let selected = [
            (
                root_use,
                (
                    handle("first"),
                    if distinct {
                        tool_id("first")
                    } else {
                        lash::tools::ToolId::new("tool:echo")
                    },
                ),
            ),
            (
                capture_use,
                (
                    handle("second"),
                    if distinct {
                        tool_id("second")
                    } else {
                        lash::tools::ToolId::new("tool:echo")
                    },
                ),
            ),
        ]
        .into();
        let error = core
            .host_artifacts()
            .link_process_env(
                &base,
                &lash::process::LinkTarget::Definition(definition.id),
                &packages,
                &selected,
            )
            .await
            .unwrap_err();
        assert!(!error.is_retryable());
        assert!(error.is_terminal());
        assert!(
            matches!(error, lash::EmbedError::ConflictingOperationRoutes { operation, first, second } if *operation == echo_definitions()[0].manifest.operation && BTreeSet::from([first, second]) == BTreeSet::from([root_use, capture_use]))
        );
    }
    core.shutdown().await.unwrap();
}
