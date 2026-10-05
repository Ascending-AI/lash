//! An after-check's declared messages are recorded with the decision that
//! makes them eligible, and replay serves them without calling the check
//! (hook-composition ruling 6; K3/K10).

use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PLUGIN: &str = "check-contributions";
const NOTE: &str = "after-check note 4905";

struct Tools;

fn definition() -> crate::ToolDefinition {
    use lash_sansio::ToolDefinitionBindingExt as _;
    crate::ToolDefinition::raw(
        "check-contributions:run",
        "run",
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({"type":"string"}),
    )
    .unwrap()
    .with_tool_binding(lash_sansio::ToolBinding::new([PLUGIN], "run"))
}

#[async_trait::async_trait]
impl crate::ToolProvider for Tools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(json!("done")).into()
    }
}

#[tokio::test]
async fn an_after_check_message_is_recorded_with_its_decision_and_replays_without_the_check() {
    use crate::session::{
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateOutcome, ToolAggregateRequest,
    };

    let double =
        crate::support::kernel_double(0x4905, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let checks = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let backend = backend.clone();
        let checks = checks.clone();
        let delivered = delivered.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let checks = checks.clone();
            let delivered = delivered.clone();
            Box::pin(async move {
                let counted = checks.clone();
                let spec = crate::PluginSpec::new()
                    .with_tool_provider(Arc::new(Tools))
                    .with_tool_result_check(
                        lash_core_execution::hook_key!("note"),
                        Arc::new(move |_| {
                            counted.fetch_add(1, Ordering::SeqCst);
                            Box::pin(async move {
                                Ok(crate::plugin::AfterToolContributions {
                                    messages: vec![crate::PluginMessage::text(
                                        crate::MessageRole::System,
                                        NOTE,
                                    )],
                                    ..Default::default()
                                })
                            })
                        }),
                    );
                let mut factories = crate::testing::test_standard_protocol_factories();
                factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
                    crate::plugin::PluginDeclaration::initial(PLUGIN),
                    spec,
                )));
                let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
                    .session_id("check-contributions-session")
                    .borrowed_effect_controller(scoped)
                    .plugin_factories(factories)
                    .build()
                    .into_runtime();
                let grant = crate::ToolExecutionGrant::from_definition(
                    crate::plugin::PluginRevision::new(
                        PLUGIN,
                        crate::plugin::BehaviorRevision::ONE,
                    ),
                    definition(),
                );
                let leaves = vec![ToolAggregateLeaf::Tool(
                    crate::session::ToolInvocation::new(
                        crate::ToolCallId::fixture("noted"),
                        crate::ToolId::new("check-contributions:run"),
                        json!({}),
                    )
                    .with_execution_grant(grant),
                )];
                context
                    .drive_tool_run(None, |context| async move {
                        let outcome = context
                            .call_tool_aggregate(ToolAggregateRequest {
                                leaves,
                                consumer: ToolAggregateConsumer::All,
                                settled_value_after: None,
                                command: crate::CommandReplayKey::new("check-contributions"),
                            })
                            .await;
                        match outcome {
                            ToolAggregateOutcome::AllResults(_) => {}
                            ToolAggregateOutcome::HostControl(fault) => panic!(
                                "a decision carrying after-check contributions faulted: {fault}"
                            ),
                            _ => panic!("a decision carrying after-check contributions decides"),
                        }
                        context.close_tool_run().await.unwrap();
                    })
                    .await
                    .unwrap();
                assert!(!context.has_nested_effect_error());
                delivered.lock().unwrap().push(
                    context
                        .dispatch()
                        .checkpoint_messages
                        .drain()
                        .iter()
                        .map(|message| format!("{:?}", message.parts))
                        .collect(),
                );
                if crash {
                    panic!("the owner dies after the noted call decided and was incorporated");
                }
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        double.run_crashed_then_redriven(
            crate::AdmittedScope::turn("check-contributions-session", "test-turn"),
            attempt(true),
            attempt(false),
        ),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(
        checks.load(Ordering::SeqCst),
        1,
        "replay serves the recorded decision without calling the check again"
    );
    let delivered = delivered.lock().unwrap();
    assert_eq!(delivered.len(), 2, "one fresh owner and one cold replay");
    assert_eq!(
        delivered[0].len(),
        1,
        "the after-check message is incorporated exactly once"
    );
    assert!(delivered[0][0].contains(NOTE));
    assert_eq!(
        delivered[1], delivered[0],
        "cold replay incorporates the recorded message again, not a fresh one"
    );
}
