//! This target's twins on the Restate server double and the storage-only
//! store set (D1 F3/F7, PR-S2).

use crate::plugin::PluginSessionRequest;
use std::sync::Arc;

const SEED: u64 = 0x5_2d20;

use crate::SessionId;

/// An open handler on this target's double lends a turn-scoped controller
/// and closes cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_handler_lends_its_scope_on_the_double() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let admitted = crate::AdmittedScope::turn(SessionId::from("root"), crate::TurnId::from("t"));
    let handler = double
        .open_handler(admitted.clone())
        .await
        .expect("open the handler");
    assert_eq!(handler.scoped().admitted_scope(), &admitted);
    handler.close().await.expect("close the handler");
}

/// A tool that answers with the text it was called with, counting its runs.
struct EchoTool {
    executed: Arc<std::sync::atomic::AtomicUsize>,
}

fn echo_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:echo",
        "echo",
        "",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
}

#[async_trait::async_trait]
impl crate::ToolProvider for EchoTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![echo_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "echo" || name == "tool:echo").then(|| Arc::new(echo_tool().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::ToolOutcome::ok(call.args.clone()).into()
    }
}

/// A dispatch context over `ports`, the shape the dispatch fixtures build.
fn echo_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    executed: Arc<std::sync::atomic::AtomicUsize>,
) -> crate::tool_dispatch::ToolDispatchContext<'h> {
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(EchoTool { executed });
    let plugins =
        crate::support::plugin_host(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core_execution::plugin::PluginDeclaration::initial("echo_tools"),
            crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
        ))])
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .expect("plugin session");
    let tool_catalog = plugins.resolved_tool_catalog().expect("tool catalog");
    crate::tool_dispatch::ToolDispatchContext {
        tool_receipts: None,
        tools: plugins.tools(),
        plugins,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_engines: Default::default(),
        effect_controller: ports.controller,
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
            session_id: SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").expect("a test frame id"),
        },
        observer: crate::engine::NullObservationSink::arc(),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: ports.attachment_store,
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
        process_lineage: None,
        process_originator: None,
    }
}

/// A tool call dispatched over [`crate::support::double_dispatch_ports`]
/// runs its attempt as an effect on the controller the double's handler
/// lent, answers, and the handler then closes cleanly. The test runs on a
/// current-thread runtime, as the dispatch laws do.
#[tokio::test]
async fn a_tool_call_on_the_doubles_lent_dispatch_ports_runs_in_the_handler() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let context = echo_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        Arc::clone(&executed),
    );
    let outcome = crate::tool_dispatch::dispatch_tool_call(
        &context,
        "echo".to_string(),
        serde_json::json!({ "text": "in the handler" }),
    )
    .await;
    assert!(
        outcome.record.output.is_success(),
        "the call answers: {:?}",
        outcome.record.output
    );
    assert_eq!(
        executed.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the tool body runs once"
    );
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}
