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
mod protocol_version_refusal;

fn named_beta_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
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

fn manifests(definitions: Vec<crate::ToolDefinition>) -> Vec<crate::ToolManifest> {
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
    /// The execution bindings the body saw, when the law watches them.
    observed_execution_bindings:
        Option<Arc<std::sync::Mutex<Vec<Option<crate::ExecutionBinding>>>>>,
}

#[async_trait::async_trait]
impl ToolProvider for ExactDispatchTools {
    fn tool_manifests(
        &self,
        _config: &crate::plugin::OwnerConfigView,
    ) -> std::result::Result<Vec<crate::ToolManifest>, crate::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_manifest(
        &self,
        name: &str,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<crate::ToolManifest> {
        (name == "host_only").then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &crate::ToolId,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<crate::ToolManifest> {
        (id == &crate::ToolId::from("tool:host_only"))
            .then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (self.contract_available && name == "host_only")
            .then(|| Arc::new(named_beta_tool("host_only").contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        if let Some(bindings) = &self.observed_execution_bindings {
            bindings
                .lock()
                .expect("the binding witness")
                .push(call.context.tool_execution_binding().cloned());
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
    let resolved = plugins.pin_resolved_tool_surface().expect("tool surface");
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
            &lash_core_execution::testing::process_env_with_limit(1024),
        ),
        owner: crate::ExecutionOwner::SessionFrame {
            session_id: crate::SessionId::from("session"),
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
        observed_execution_bindings: None,
    });
    let context = refusing_dispatch_context(provider_plugins(
        provider,
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .await;
    let outcome =
        dispatch_tool_call(&context, "host_only".to_string(), json!({ "value": "ok" })).await;

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
/// contract, runs once, and its body sees the grant's execution binding.
#[tokio::test]
async fn explicit_execution_grant_runs_non_catalog_tool_with_binding() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: false,
        observed_execution_bindings: Some(Arc::clone(&observed_execution_bindings)),
    });
    let context = refusing_dispatch_context(provider_plugins(
        provider,
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .await;
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::ToolProviderHandle::new(
            crate::plugin::RegistryKind::Session,
            "test_tools",
            crate::tool_provider_key!("tools"),
        ),
        named_beta_tool("host_only"),
    )
    .with_execution_binding(Some(granted_binding()));
    let pending = crate::sansio::PendingToolCall {
        call_id: lash_core_execution::ToolCallId::fixture("grant-call"),
        provider_call_id: None,
        tool_name: "host_only".to_string(),
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
        .execution_binding(grant.binding.clone());
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
        *observed_execution_bindings
            .lock()
            .expect("the binding witness"),
        vec![Some(granted_binding())]
    );
}

/// The binding the grant in these tests carries, in its provider's format.
fn granted_binding() -> crate::ExecutionBinding {
    crate::ExecutionBinding {
        codec: crate::plugin::ToolProviderHandle::new(
            crate::plugin::RegistryKind::Session,
            "test_tools",
            crate::tool_provider_key!("tools"),
        ),
        format: crate::FormatVersion::ONE,
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

    fn definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:mcp__appworld__venmo_show_transactions",
            "mcp__appworld__venmo_show_transactions",
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
        ) -> std::result::Result<Vec<crate::ToolManifest>, crate::PluginError> {
            Ok(manifests(vec![definition()]))
        }

        fn resolve_contract(
            &self,
            name: &str,
            _config: &crate::plugin::OwnerConfigView,
        ) -> Option<Arc<crate::ToolContract>> {
            (name == "mcp__appworld__venmo_show_transactions")
                .then(|| Arc::new(definition().contract()))
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
        "mcp__appworld__venmo_show_transactions".to_string(),
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

    fn definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:seedy",
            "seedy",
            "Seed-aware",
            crate::ToolDefinition::default_input_schema(),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_argument_projection(
            crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
        )
    }

    #[async_trait::async_trait]
    impl ToolProvider for ProjectionPolicyTools {
        fn tool_manifests(
            &self,
            _config: &crate::plugin::OwnerConfigView,
        ) -> std::result::Result<Vec<crate::ToolManifest>, crate::PluginError> {
            Ok(manifests(vec![definition()]))
        }

        fn resolve_contract(
            &self,
            name: &str,
            _config: &crate::plugin::OwnerConfigView,
        ) -> Option<Arc<crate::ToolContract>> {
            (name == "seedy").then(|| Arc::new(definition().contract()))
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
        "seedy".to_string(),
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
        Some(crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"))
    );
}
