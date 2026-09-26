//! This target's twins on the Restate server double and the storage-only
//! store set (D1 F3/F7, PR-S2).

use std::sync::Arc;

use crate::process_registry::{ProcessDefinitionRegistration, ProcessDefinitionRegistry};
use crate::{SessionId, TriggerOwnerScope};

const SEED: u64 = 0x5_2d20;

fn reference(payload: &str) -> crate::ProcessDefinitionRef {
    crate::ProcessDefinitionRef::unclaimed(
        "test-engine",
        serde_json::json!({"definition": payload}),
    )
}

/// `kernel::process_registry::tests::registration_is_cas_fenced` in its F3
/// form: the registry comes from a storage-only store set, no engine.
#[tokio::test]
async fn a_definition_registration_is_cas_fenced_on_the_store_set() {
    let registry: Arc<dyn ProcessDefinitionRegistry> = crate::support::memory_store_set()
        .await
        .process_definition_registry();
    let scope = TriggerOwnerScope::session(SessionId::from("session-a"));
    let first = registry
        .register_definition(
            "op-1",
            scope.clone(),
            "nightly-scan",
            reference("one"),
            None,
        )
        .await
        .expect("the first registration");
    let ProcessDefinitionRegistration::Admitted(admitted) = &first else {
        panic!("the first registration admits: {first:?}");
    };
    assert_eq!(admitted.revision, 1);
    let error = registry
        .register_definition(
            "op-2",
            scope,
            "nightly-scan",
            reference("one-bis"),
            Some(
                &crate::process_registry::ProcessDefinitionExpectation::observed(
                    7,
                    admitted.fingerprint.clone(),
                ),
            ),
        )
        .await
        .expect_err("a stale expected revision conflicts")
        .to_string();
    assert!(error.contains("conflicts with revision 1"), "{error}");
}

/// The store-only backend reaches the same kind of store port.
#[tokio::test]
async fn the_store_only_backend_serves_its_store_ports() {
    let backend = crate::support::memory_store_backend().await;
    let registration = backend
        .process_definition_registry()
        .register_definition(
            "op-1",
            TriggerOwnerScope::session(SessionId::from("session-b")),
            "hourly",
            reference("two"),
            None,
        )
        .await
        .expect("the store-only backend's registry registers");
    assert!(matches!(
        registration,
        ProcessDefinitionRegistration::Admitted(_)
    ));
}

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
    ports: crate::support::DoubleDispatchPorts<'h>,
    executed: Arc<std::sync::atomic::AtomicUsize>,
) -> crate::tool_dispatch::ToolDispatchContext<'h> {
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(EchoTool { executed });
    let plugins =
        crate::support::plugin_host(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            "echo_tools",
            crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
        ))])
        .build_session("root")
        .expect("plugin session");
    let tool_catalog = plugins
        .resolved_tool_catalog(&SessionId::from("session"))
        .expect("tool catalog");
    crate::tool_dispatch::ToolDispatchContext {
        tools: plugins.tools(),
        plugins,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_definitions: None,
        process_engines: Default::default(),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(
            ports.controller,
        ),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        observation_call_key: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: SessionId::from("session"),
        agent_frame_id: crate::FrameNodeId::new("test-frame").expect("a test frame id"),
        observer: crate::engine::NullObservationSink::arc(),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: ports.attachment_store,
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
    }
}

/// A tool call dispatched over [`crate::support::double_dispatch_ports`]
/// runs its attempt as an effect on the controller the double's handler
/// lent, answers, and the handler then closes cleanly. The test runs on a
/// current-thread runtime, as the dispatch laws do.
#[tokio::test]
async fn a_tool_call_on_the_doubles_lent_dispatch_ports_runs_in_the_handler() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(crate::support::dispatch_scope())
        .await
        .expect("open the dispatch handler");
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
