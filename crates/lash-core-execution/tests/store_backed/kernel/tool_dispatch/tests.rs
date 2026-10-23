// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::plugin::PluginSession;
use crate::plugin::PluginSessionRequest;
use crate::plugin::StaticPluginFactory;
use crate::testing::MockSessionManager;
use crate::tool_dispatch::{ToolDispatchContext, dispatch_tool_call};
use crate::{ToolCall, ToolOutcome, ToolProvider};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod composition_laws;
mod live_fence_sweep;
mod protocol_version_refusal;

fn named_beta_tool(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        "",
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

fn manifests(
    definitions: Vec<lash_vocabulary::tool_contract::ToolDefinition>,
) -> Vec<lash_vocabulary::tool_contract::ToolManifest> {
    definitions
        .into_iter()
        .map(|tool| tool.manifest())
        .collect()
}

/// A tool the provider resolves by name but advertises in no manifest: not
/// in the session's catalog.
struct ExactDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
    /// Whether the provider resolves the tool's contract by name.
    contract_available: bool,
    /// The execution binding and contract each call the body ran saw, when
    /// the law watches them.
    observed_calls: Option<ObservedCalls>,
}

#[async_trait::async_trait]
impl ToolProvider for ExactDispatchTools {
    fn tool_manifests(
        &self,
        _config: &crate::plugin::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_manifest(
        &self,
        name: &str,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
        (name == "tools.host_only").then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_vocabulary::tool_contract::ToolId,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
        (id == &lash_vocabulary::tool_contract::ToolId::from("tool:host_only"))
            .then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (self.contract_available && name == "tools.host_only")
            .then(|| Arc::new(named_beta_tool("host_only").contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        if let Some(calls) = &self.observed_calls {
            calls.lock().expect("the call witness").push((
                call.context.tool_execution_binding().cloned(),
                call.contract().clone(),
            ));
        }
        ToolOutcome::ok(json!("host")).into()
    }
}

fn provider_plugins(
    provider: Arc<dyn ToolProvider>,
    authority: crate::plugin::SessionAuthorityContext,
) -> Arc<PluginSession> {
    crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("test_tools"),
        crate::PluginSpec::new()
            .with_tool_provider(lash_core_execution::tool_provider_key!("tools"), provider),
    ))])
    .build_session(PluginSessionRequest::creation("root", authority))
    .expect("plugin session")
}

/// A dispatch context whose effects are unavailable: what it dispatches must
/// end before any attempt runs.
async fn refusing_dispatch_context(plugins: Arc<PluginSession>) -> ToolDispatchContext<'static> {
    let resolved =
        lash_core_execution::testing::environment_projection(&plugins).expect("tool surface");
    let routes = crate::SurfaceTools::of_surface(&resolved.surface, plugins.tool_registry());
    let tools: Arc<dyn ToolProvider> = Arc::new(routes.clone());
    let tool_catalog = resolved.catalog;
    ToolDispatchContext {
        fleet_format: lash_core_execution::FleetFormat::current(),
        composition: (plugins).composition(),
        config: crate::plugin::OwnerConfigView::session((plugins).admitted_plugin_config()),
        execution_budgets: (plugins).execution_budgets(),
        state: plugins,
        tools,
        routes,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::plugin::ServiceBindings::default()),
        process_engines: Default::default(),
        effect_controller: crate::ActorContext::unavailable(),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        observation_call_key: None,
        facts: crate::plugin::ExecutionFacts::of_process(
            &lash_execution_contracts::testing::process_env_with_limit(1024),
        ),
        owner: crate::ExecutionOwner::SessionFrame {
            session_id: lash_vocabulary::identity::SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        },
        observer: crate::engine::NullObservationSink::arc(),
        attachment_store: Arc::new(crate::RuntimeAttachmentStore::ephemeral(
            crate::support::sqlite_memory_store_backend()
                .await
                .attachment_store(),
            crate::AttachmentPolicy::standard(),
        )),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
        process_lineage: None,
        process_originator: None,
    }
}

/// The session's catalog is the dispatch authority: a tool outside it is
/// unavailable before its provider resolves a contract or runs it.
#[tokio::test]
async fn dispatch_rejects_non_catalog_tool_before_provider_resolution() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: true,
        observed_calls: None,
    });
    let context = refusing_dispatch_context(provider_plugins(
        provider,
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .await;
    let outcome = dispatch_tool_call(
        &context,
        "tools.host_only".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        outcome.record.output.value_for_projection()["message"],
        json!("Tool is unavailable in this session")
    );
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
}

/// A grant the caller holds runs a tool the session's catalog does not list:
/// the call prepares from the grant without resolving the provider's
/// contract, runs once, and its body sees the grant's execution binding and,
/// as [`ToolCall::contract`], the grant's contract (FIG-6295 law 2).
#[tokio::test]
async fn explicit_execution_grant_runs_non_catalog_tool_with_binding() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let observed_calls = ObservedCalls::default();
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: false,
        observed_calls: Some(Arc::clone(&observed_calls)),
    });
    let context = refusing_dispatch_context(provider_plugins(
        provider,
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .await;
    let grant = crate::ToolExecutionGrant::from_definition(
        lash_vocabulary::providers::ToolProviderHandle::new(
            lash_vocabulary::claims::RegistryKind::Session,
            "test_tools",
            crate::tool_provider_key!("tools"),
        ),
        named_beta_tool("host_only"),
    )
    .with_execution_binding(Some(granted_binding()));
    let grant_contract = grant.contract().clone();
    let pending = crate::sansio::PendingToolCall {
        call_id: lash_core_execution::ToolCallId::fixture("grant-call"),
        provider_call_id: None,
        tool_name: "tools.host_only".to_string(),
        args: json!({ "value": "ok" }),
        replay: None,
    };
    let prepared = match crate::tool_dispatch::prepare_granted_tool_call_with_context(
        &context, &grant, pending,
    )
    .await
    {
        crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => *prepared,
        crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
            panic!("grant should prepare, got {:?}", outcome.record.output)
        }
    };
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .execution_binding(grant.payload().cloned());
    let launch = crate::coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        Some(Box::new(grant)),
        tool_context,
    )
    .await;
    let crate::tool_dispatch::ToolCallLaunch::Done(outcome) = launch else {
        panic!("grant call should complete");
    };

    assert!(outcome.record.output.is_success());
    assert_eq!(outcome.record.output.value_for_projection(), json!("host"));
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
    assert_eq!(
        *observed_calls.lock().expect("the call witness"),
        vec![(Some(granted_binding()), grant_contract)]
    );
}

/// What each call a provider ran saw: its execution binding and contract.
type ObservedCalls = Arc<
    std::sync::Mutex<
        Vec<(
            Option<lash_vocabulary::providers::ExecutionBinding>,
            lash_vocabulary::tool_contract::ToolContract,
        )>,
    >,
>;

/// The binding the grant in these tests carries, in its provider's format.
fn granted_binding() -> lash_vocabulary::providers::ExecutionBinding {
    lash_vocabulary::providers::ExecutionBinding {
        codec: lash_vocabulary::providers::ToolProviderHandle::new(
            lash_vocabulary::claims::RegistryKind::Session,
            "test_tools",
            crate::tool_provider_key!("tools"),
        ),
        format: lash_vocabulary::providers::FormatVersion::ONE,
        payload: json!({ "kind": "test", "route": "deferred" }),
    }
}

/// An MCP tool whose input schema does not forbid unknown properties admits
/// arguments it does not name, and its body runs.
#[tokio::test]
async fn dispatch_allows_unknown_mcp_args_when_schema_does_not_forbid_them() {
    struct StrictMcpTools {
        executed: Arc<AtomicUsize>,
    }

    fn definition() -> lash_vocabulary::tool_contract::ToolDefinition {
        lash_vocabulary::tool_contract::ToolDefinition::raw(
            "tool:mcp__appworld__venmo_show_transactions",
            "appworld.venmo_show_transactions",
            "Show Venmo transactions",
            json!({
                "type": "object",
                "properties": {
                    "min_created_at": { "type": "string" },
                    "max_created_at": { "type": "string" },
                    "limit": { "type": "integer", "maximum": 100 }
                },
                "required": ["limit"]
            }),
            json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
    }

    #[async_trait::async_trait]
    impl ToolProvider for StrictMcpTools {
        fn tool_manifests(
            &self,
            _config: &crate::plugin::OwnerConfigView,
        ) -> std::result::Result<
            Vec<lash_vocabulary::tool_contract::ToolManifest>,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            Ok(manifests(vec![definition()]))
        }

        fn resolve_contract(
            &self,
            name: &str,
            _config: &crate::plugin::OwnerConfigView,
        ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
            (name == "appworld.venmo_show_transactions").then(|| Arc::new(definition().contract()))
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            self.executed.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok(json!({ "executed": true })).into()
        }
    }

    let executed = Arc::new(AtomicUsize::new(0));
    let context = refusing_dispatch_context(provider_plugins(
        Arc::new(StrictMcpTools {
            executed: Arc::clone(&executed),
        }),
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .await;
    let outcome = dispatch_tool_call(
        &context,
        "appworld.venmo_show_transactions".to_string(),
        json!({
            "min_datetime": "2024-01-01T00:00:00Z",
            "limit": 20
        }),
    )
    .await;

    assert!(
        outcome.record.output.is_success(),
        "{:?}",
        outcome.record.output
    );
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

/// The tool hooks of a call read the argument projection policy its
/// manifest resolves to.
#[tokio::test]
async fn before_tool_hook_receives_resolved_argument_projection_policy() {
    struct ProjectionPolicyTools;

    fn definition() -> lash_vocabulary::tool_contract::ToolDefinition {
        lash_vocabulary::tool_contract::ToolDefinition::raw(
            "tool:seedy",
            "tools.seedy",
            "Seed-aware",
            lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_argument_projection(
            lash_vocabulary::tool_contract::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
        )
    }

    #[async_trait::async_trait]
    impl ToolProvider for ProjectionPolicyTools {
        fn tool_manifests(
            &self,
            _config: &crate::plugin::OwnerConfigView,
        ) -> std::result::Result<
            Vec<lash_vocabulary::tool_contract::ToolManifest>,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            Ok(manifests(vec![definition()]))
        }

        fn resolve_contract(
            &self,
            name: &str,
            _config: &crate::plugin::OwnerConfigView,
        ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
            (name == "tools.seedy").then(|| Arc::new(definition().contract()))
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("ok")).into()
        }
    }

    let captured = Arc::new(std::sync::Mutex::new(None));
    let hook_captured = Arc::clone(&captured);
    let hook: crate::plugin::ToolArgsTransformHook = Arc::new(move |input| {
        let hook_captured = Arc::clone(&hook_captured);
        Box::pin(async move {
            *hook_captured.lock().expect("the policy witness") =
                Some(input.context.argument_projection.clone());
            Ok(input.current)
        })
    });
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("projection_policy_tools"),
        crate::PluginSpec::new()
            .with_tool_provider(
                lash_core_execution::tool_provider_key!("tools"),
                Arc::new(ProjectionPolicyTools),
            )
            .with_tool_args_transform(lash_core_execution::hook_key!("capture"), hook),
    ))])
    .build_session(PluginSessionRequest::creation(
        "root",
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .expect("plugin session");
    let outcome = dispatch_tool_call(
        &refusing_dispatch_context(plugins).await,
        "tools.seedy".to_string(),
        json!({}),
    )
    .await;

    assert!(
        outcome.record.output.is_success(),
        "{:?}",
        outcome.record.output
    );
    assert_eq!(
        captured.lock().expect("the policy witness").clone(),
        Some(lash_vocabulary::tool_contract::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"))
    );
}

/// L-DEFERRED-PIN (§15.1–15.2): a published deferred package resolves through
/// its registered resolver, but a retained target executes without rediscovery.
/// Only its executor and readable payload are needed when the cell is restored.
#[tokio::test]
async fn a_resolved_package_cell_call_needs_its_executor_and_codec_without_rediscovery() {
    use crate::plugin::packages::{
        DeferredAnswer, DeferredPackageProvider, PackageContribution, PackageProvider,
        ResolverFault, operation_publication,
    };
    use crate::runtime::actor::round::{self, MemberBodies as _};
    use crate::tool_dispatch::{CellCall, CellMember, CellMembers};
    use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};
    use lash_execution_contracts::process::linking::{AdmissionRefusal, EnginePortError};
    use lash_vocabulary::{ProviderName, names::*, package::*};

    #[derive(Default)]
    struct DeferredOnly {
        executed: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ToolProvider for DeferredOnly {
        fn tool_manifests(
            &self,
            _config: &crate::plugin::OwnerConfigView,
        ) -> Result<
            Vec<lash_vocabulary::tool_contract::ToolManifest>,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            Ok(Vec::new())
        }

        fn resolve_contract(
            &self,
            _name: &str,
            _config: &crate::plugin::OwnerConfigView,
        ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
            None
        }

        fn readable_binding_formats(&self) -> &'static [lash_vocabulary::providers::FormatVersion] {
            &[lash_vocabulary::providers::FormatVersion::ONE]
        }

        async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            assert_eq!(
                call.context.tool_execution_binding(),
                Some(&granted_binding())
            );
            self.executed.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok(json!("sent")).into()
        }
    }

    #[async_trait::async_trait]
    impl PackageProvider for DeferredOnly {
        fn roots(&self, _: &crate::plugin::OwnerConfigView) -> std::collections::BTreeSet<Segment> {
            [Segment::new("tools").unwrap()].into()
        }

        fn advertise(
            &self,
            _: &crate::plugin::OwnerConfigView,
            context: &PublicationContext,
        ) -> Result<Vec<PackageContribution>, lash_execution_contracts::PluginError> {
            let advertisement = PackageAdvertisement::Deferred(Box::new(DeferredPackage {
                path: PackagePath::parse("tools").unwrap(),
                visibility: Visibility::Public,
                provider: ProviderName {
                    owner: "test_tools".into(),
                    key: "deferred".into(),
                },
                config: ProviderConfig {
                    component: None,
                    format: lash_vocabulary::providers::FormatVersion::ONE,
                    value: serde_json::Value::Null,
                    payload: None,
                },
                resolution_policy: Default::default(),
            }));
            Ok(vec![PackageContribution {
                publication: operation_publication(context, &advertisement).unwrap(),
                advertisement,
                routes: Vec::new(),
            }])
        }

        async fn hold(
            &self,
            _: &lash_core_store::artifact_referrer::ReferrerClaim,
            _: &[&PackageAdvertisement],
        ) -> Result<(), EnginePortError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl DeferredPackageProvider for DeferredOnly {
        fn validate_grant(
            &self,
            grant: &DeferredGrant,
            registry: crate::plugin::RegistryKind,
        ) -> Result<(), AdmissionRefusal> {
            assert_eq!(registry, crate::plugin::RegistryKind::Session);
            assert_eq!(grant.provider.owner, "test_tools");
            assert_eq!(grant.path, PackagePath::parse("tools").unwrap());
            Ok(())
        }

        async fn resolve(
            &self,
            _: &crate::DeferredResolveContext<'_>,
            _: &DeferredGrant,
            requested: &QualifiedName,
        ) -> Result<DeferredAnswer, ResolverFault> {
            assert_eq!(requested.member.as_str(), "send_email");
            let definition = named_beta_tool("send_email");
            Ok(DeferredAnswer::Resolved {
                route: Box::new(crate::OperationRoute::resolved(
                    crate::testing::test_tools_provider(),
                    definition.clone(),
                    Some(granted_binding()),
                )),
                definition: Box::new(definition),
            })
        }
    }

    let stores = crate::support::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores.clone());
    let database = backend.durable().clone();
    let actor = ActorKey::session("test-session").unwrap();
    let formats = FormatSet::new("deferred-cell-law");
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), formats.clone());
    database
        .commit_mail(mail, CommitLabel::new("law.create"))
        .await
        .unwrap();
    let node = database
        .register_node(&NodeSpec {
            node: NodeId::new("deferred-cell-law"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    let claims = database.claim(&node, 1).await.unwrap();
    assert_eq!(claims.len(), 1);
    let controller = crate::ActorContext::new(
        backend.clone(),
        actor,
        claims[0].epoch,
        crate::AdmittedScope::runtime_operation("deferred-cell-law"),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    );
    let provider = Arc::new(DeferredOnly::default());
    let mut registrar = crate::plugin::PluginRegistrar::new("test_tools");
    registrar
        .tools()
        .provider_reading(
            crate::tool_provider_key!("tools"),
            &[lash_vocabulary::providers::FormatVersion::ONE],
            {
                let provider = provider.clone();
                move |_, _| Ok(provider.clone() as Arc<dyn ToolProvider>)
            },
        )
        .unwrap();
    registrar
        .package_provider(crate::tool_provider_key!("packages"), provider.clone())
        .unwrap();
    registrar
        .deferred_package_provider(crate::tool_provider_key!("deferred"), provider.clone())
        .unwrap();
    let session =
        crate::support::plugin_host(vec![Arc::new(crate::plugin::PluginRecipeFactory::new(
            crate::plugin::PluginDeclaration::initial("test_tools"),
            crate::plugin::PluginRecipe::from_registrar("test_tools", registrar),
        ))])
        .build_session(PluginSessionRequest::creation(
            "test-session",
            crate::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let surface = lash_core_execution::testing::environment_projection(&session).unwrap();
    assert!(
        surface.catalog.entries().is_empty(),
        "deferred packages add no model tools"
    );
    let accepted = surface
        .surface
        .packages()
        .snapshot
        .advertisements
        .iter()
        .find(|accepted| accepted.publication.path == PackagePath::parse("tools").unwrap())
        .expect("the registered tools publication");
    let PackageAdvertisement::Deferred(package) = &accepted.advertisement else {
        panic!("the registered provider advertises a deferred package");
    };
    let deferred_grant = DeferredGrant {
        provider: package.provider.clone(),
        context: accepted.publication.context.clone(),
        path: package.path.clone(),
        visibility: package.visibility,
        signature: lash_vocabulary::Signature {
            params: Vec::new(),
            result: lash_vocabulary::Type::Json,
        },
        config: package.config.clone(),
        resolution_policy: package.resolution_policy,
    };
    let composition = session.composition();
    composition
        .validate_deferred_grant(&deferred_grant, crate::plugin::RegistryKind::Session)
        .unwrap();
    let deferred_key = lash_vocabulary::providers::ToolProviderHandle::from_name(
        crate::plugin::RegistryKind::Session,
        &deferred_grant.provider,
    )
    .unwrap();
    let context = crate::testing::TestExecutionContextBuilder::new(
        crate::testing::TestExecutionPorts::of(&backend),
    )
    .provider(provider.clone())
    .routes(crate::SurfaceTools::of_surface(
        &surface.surface,
        composition.tool_registry().clone(),
    ))
    .shared_effect_controller(controller.clone())
    .build()
    .into_runtime();
    let capabilities = Default::default();
    let owner = crate::ExecutionOwner::SessionFrame {
        session_id: lash_vocabulary::identity::SessionId::from("test-session"),
        agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
    };
    let config = crate::plugin::OwnerConfigView::default();
    let requested = QualifiedName {
        package: deferred_grant.path.clone(),
        member: Segment::new("send_email").unwrap(),
        type_member: None,
    };
    let resolved = composition
        .deferred_package_provider(&deferred_grant.provider)
        .unwrap()
        .resolve(
            &crate::DeferredResolveContext::new(
                &deferred_key,
                &owner,
                None,
                &capabilities,
                &config,
            ),
            &deferred_grant,
            &requested,
        )
        .await
        .unwrap();
    let DeferredAnswer::Resolved { definition, route } = resolved else {
        panic!("the registered resolver serves send_email");
    };
    composition
        .validate_resolved_route(
            &deferred_grant,
            &requested,
            &definition,
            &route,
            crate::plugin::RegistryKind::Session,
        )
        .unwrap();
    let grant = crate::ToolExecutionGrant::of_link(&route).unwrap();
    let member = CellMember::Tool(CellCall {
        id: crate::ToolCallId::fixture("deferred-cell"),
        tool_id: definition.manifest.id.clone(),
        args: json!({ "value": "email" }),
        contract: definition.contract_id(),
        execution_grant: Some(Box::new(grant)),
        issuing_language_node_id: None,
    });
    let opener = crate::EffectOpener::turn("test-session", "test-turn");
    let members = CellMembers::new(&context, opener.clone()).unwrap();
    members
        .require_capable(&member)
        .expect("a recorded deferred match must not park as catalogue drift");
    let pin = members.pin(&member, controller.now().0 as u64);
    let pending = crate::sansio::PendingToolCall {
        call_id: member.id().clone(),
        provider_call_id: None,
        tool_name: "tools.send_email".to_owned(),
        args: json!({ "value": "email" }),
        replay: None,
    };
    let draft = round::call_draft(&opener, &pending, pin).unwrap();
    let ledger_owner = lash_durable::domain::OwnerKey::Turn(
        lash_vocabulary::identity::SessionId::from("test-session"),
        lash_vocabulary::identity::TurnId::parse("test-turn").unwrap(),
    );
    let mut tx = controller.begin().await.unwrap();
    let admitted = round::admit(&mut tx, &ledger_owner, round::RunSeq(1), vec![draft]).unwrap();
    controller
        .commit(tx, CommitLabel::new("law.admit"))
        .await
        .unwrap();
    members.register(member.clone());
    let outcome = members.body(&admitted[0])(tokio_util::sync::CancellationToken::new()).await;
    assert!(
        matches!(
            outcome.output,
            lash_core_store::tool_run::SettledOutput::Completed(_)
        ),
        "{outcome:?}"
    );
    let reply = members.reply(&member, &outcome.output);
    assert!(reply.output.is_success(), "{reply:?}");
    assert_eq!(reply.output.value_for_projection(), json!("sent"));
    assert_eq!(provider.executed.load(Ordering::SeqCst), 1);
    let mut tx = controller.begin().await.unwrap();
    round::settle(&mut tx, &admitted[0], outcome.output, Vec::new(), None).unwrap();
    controller
        .commit(tx, CommitLabel::new("law.settle"))
        .await
        .unwrap();

    // Binding readability is a node capability, checked again when a
    // retained cell member is restored, before its next admission.
    let mut unreadable = member.clone();
    let CellMember::Tool(call) = &mut unreadable;
    let grant = call.execution_grant.as_mut().unwrap();
    let payload = lash_vocabulary::providers::ExecutionBinding {
        format: lash_vocabulary::providers::FormatVersion::new(2).unwrap(),
        ..grant.payload().unwrap().clone()
    };
    **grant = (**grant).clone().with_execution_binding(Some(payload));
    let error = members
        .require_capable(&unreadable)
        .expect_err("an unreadable binding parks");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::ExecutionBindingUnreadable
    );

    // A restored target needs its executor and codec, without the original
    // resolver or its publication being present on this node.
    let restored_session = provider_plugins(
        provider.clone(),
        crate::plugin::SessionAuthorityContext::fixture(),
    );
    let restored_composition = restored_session.composition();
    assert!(
        restored_composition
            .validate_deferred_grant(&deferred_grant, crate::plugin::RegistryKind::Session)
            .is_err(),
        "an unresolved use still needs its registered resolver"
    );
    restored_composition
        .validate_resolved_route(
            &deferred_grant,
            &requested,
            &definition,
            &route,
            crate::plugin::RegistryKind::Session,
        )
        .expect("the retained target is valid without rediscovery");
    let restored_surface =
        lash_core_execution::testing::environment_projection(&restored_session).unwrap();
    let restored = crate::testing::TestExecutionContextBuilder::new(
        crate::testing::TestExecutionPorts::of(&backend),
    )
    .provider(provider.clone())
    .routes(crate::SurfaceTools::of_surface(
        &restored_surface.surface,
        restored_composition.tool_registry().clone(),
    ))
    .build()
    .into_runtime();
    let restored = CellMembers::new(&restored, opener.clone()).unwrap();
    restored
        .require_capable(&member)
        .expect("a retained target does not require its original resolver");
    restored
        .require_capable(&unreadable)
        .expect_err("restoration still checks payload readability");

    // Resolver presence cannot replace the target's executor registration.
    let mut unserved = member.clone();
    let CellMember::Tool(call) = &mut unserved;
    let grant = call.execution_grant.as_mut().unwrap();
    **grant = crate::ToolExecutionGrant::of_link(&crate::OperationRoute {
        provider: crate::plugin::ToolProviderHandle::new(
            crate::plugin::RegistryKind::Session,
            "absent",
            crate::tool_provider_key!("tools"),
        ),
        ..grant.link().clone()
    })
    .unwrap();
    let error = members
        .require_capable(&unserved)
        .expect_err("a missing execution provider parks");
    assert_eq!(error.code, crate::RuntimeErrorCode::LashVmCellBindingDrift);
    assert!(error.message.contains("missing from"), "{error:?}");
    assert_eq!(provider.executed.load(Ordering::SeqCst), 1);
}
