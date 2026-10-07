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
}

fn manifests(definitions: Vec<crate::ToolDefinition>) -> Vec<crate::ToolManifest> {
    definitions
        .into_iter()
        .map(|tool| tool.manifest())
        .collect()
}

struct HiddenDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for HiddenDispatchTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![named_beta_tool("hidden")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (name == "hidden").then(|| Arc::new(named_beta_tool("hidden").contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("hidden")).into()
    }
}

mod single_gate;

/// A tool the provider resolves by name but advertises in no manifest: not
/// in the session's catalog.
struct ExactDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for ExactDispatchTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest(&self, name: &str) -> Option<crate::ToolManifest> {
        (name == "host_only").then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (name == "host_only").then(|| Arc::new(named_beta_tool("host_only").contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("host")).into()
    }
}

fn provider_plugins(
    provider: Arc<dyn ToolProvider>,
    authority: crate::plugin::SessionAuthorityContext,
) -> Arc<PluginSession> {
    crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("test_tools"),
        crate::PluginSpec::new().with_tool_provider(provider),
    ))])
    .build_session(PluginSessionRequest::creation("root", authority))
    .expect("plugin session")
}

/// A dispatch context whose effects are unavailable: what it dispatches must
/// end before any attempt runs.
async fn refusing_dispatch_context(plugins: Arc<PluginSession>) -> ToolDispatchContext<'static> {
    let tools = plugins.tools();
    let tool_catalog = plugins.resolved_tool_catalog().expect("tool catalog");
    ToolDispatchContext {
        fleet_format: lash_core_execution::FleetFormat::current(),
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_engines: Default::default(),
        effect_controller: crate::ActorContext::unavailable(),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        observation_call_key: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
        owner: crate::ExecutionOwner::SessionFrame {
            session_id: crate::SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        },
        observer: crate::engine::NullObservationSink::arc(),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::RuntimeAttachmentStore::ephemeral(
            crate::support::sqlite_memory_store_backend()
                .await
                .attachment_store(),
        )),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
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
    });
    let context = refusing_dispatch_context(provider_plugins(provider, Default::default())).await;
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

/// A tool the session's authority hides is unavailable before its contract
/// resolves, though the registry still holds it.
#[tokio::test]
async fn dispatch_rejects_hidden_tool_before_contract_resolution() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(HiddenDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
    });
    let tool_access = crate::SessionToolAccess::ambient()
        .with_hidden_tools(["hidden"])
        .expect("valid hidden name");
    let plugins = provider_plugins(
        provider,
        crate::plugin::SessionAuthorityContext {
            tool_access,
            ..Default::default()
        },
    );
    assert!(
        plugins
            .tool_registry()
            .export_state()
            .iter()
            .find(|(_, entry)| entry.manifest().name == "hidden")
            .is_some_and(|(_, entry)| entry.is_member()),
        "authority hiding must not rewrite the registry's curation bit"
    );
    let context = refusing_dispatch_context(plugins).await;
    let outcome =
        dispatch_tool_call(&context, "hidden".to_string(), json!({ "value": "ok" })).await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        outcome.record.output.value_for_projection()["message"],
        json!("Tool is unavailable in this session")
    );
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
}
