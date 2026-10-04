//! L19 Q5: cancelling unrecorded A preserves durable B, including overlapping keys.

use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
use lash_core_store::session_state::SessionPluginStateSource as _;
use lash_sansio::sync::MutexExt as _;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PLUGIN: &str = "q5";

struct Tools {
    entered: tokio::sync::mpsc::UnboundedSender<CancellationToken>,
    release: Arc<tokio::sync::Notify>,
}

fn definition() -> crate::ToolDefinition {
    use lash_sansio::ToolDefinitionBindingExt as _;
    crate::ToolDefinition::raw(
        "q5:append",
        "append",
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({"type":"string"}),
    )
    .unwrap()
    .with_tool_binding(lash_sansio::ToolBinding::new([PLUGIN], "append"))
}

#[async_trait::async_trait]
impl crate::ToolProvider for Tools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let symbol = call.args["symbol"].as_str().unwrap();
        if symbol == "A" {
            self.entered
                .send(
                    call.context
                        .cancellation_token()
                        .cloned()
                        .unwrap_or_default(),
                )
                .unwrap();
            self.release.notified().await;
        }
        // Even a body that returns success after its stop cannot publish A.
        crate::ToolAttemptOutcome::done_without_intents(
            crate::ToolOutcomeDone::ok(json!(symbol)).with_state(
                crate::StateCommands::new().apply(
                    call.args["key"].as_str().unwrap(),
                    "append",
                    json!(symbol),
                ),
            ),
        )
    }
}

async fn cancelled_sibling(disjoint: bool) {
    let double =
        crate::support::kernel_double(0x4936, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let scope = crate::AdmittedScope::turn("q5-session", "test-turn");
    let handler = double.open_handler(scope).await.unwrap();
    let host = backend.effect_host();
    let resolver = host.await_event_resolver();
    let control = Arc::new(
        crate::runtime::turn_control::ActiveTurnControl::new(
            resolver,
            crate::TurnAddress::new("q5-session", "test-turn"),
        )
        .await
        .unwrap(),
    );
    let (entered, mut body) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let reductions = Arc::new(Mutex::new(Vec::new()));
    let reduced = reductions.clone();
    let factory = crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial(PLUGIN),
        crate::PluginSpec::new()
            .with_state_reducer(
                "append",
                Arc::new(move |input| {
                    reduced.lock_recover().push(input.input.clone());
                    Ok(Some(json!(format!(
                        "{}{}",
                        input
                            .current
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(""),
                        input.input.as_str().unwrap()
                    ))))
                }),
            )
            .with_tool_provider(Arc::new(Tools {
                entered,
                release: release.clone(),
            })),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(factory));
    let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
        .session_id("q5-session")
        .borrowed_effect_controller(handler.scoped())
        .plugin_factories(factories)
        .build()
        .into_runtime()
        .with_recorded_turn_cancel(
            false,
            control.clone(),
            backend.effect_host(),
            CancellationToken::new(),
        );
    let plugins = context.dispatch().plugins.clone();
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::PluginRevision::new(PLUGIN, crate::plugin::BehaviorRevision::ONE),
        definition(),
    );
    let calls = [
        ("A", if disjoint { "a" } else { "value" }),
        ("B", if disjoint { "b" } else { "value" }),
    ]
    .into_iter()
    .map(|(symbol, key)| {
        crate::session::ToolInvocation::new(
            crate::ToolCallId::fixture(symbol),
            crate::ToolId::new("q5:append"),
            json!({"symbol":symbol,"key":key}),
        )
        .with_execution_grant(grant.clone())
    })
    .collect();
    let run = context.drive_tool_run(None, |context| async move {
        let replies = context.call_tool_batch(calls).await;
        assert!(!context.has_nested_effect_error());
        context.close_opener_groups().await.unwrap();
        replies
    });
    let cancel = async {
        let stop = tokio::time::timeout(Duration::from_secs(2), body.recv())
            .await
            .expect("A enters its native inline body")
            .unwrap();
        // Read the double's independent journal, rather than a reducer or
        // presentation callback: B's final decision must already be durable.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let durable = double.server().invocations().iter().any(|view| {
                    double.server().journal(&view.id).unwrap().iter().any(|entry| {
                        let Some(Ok(bytes)) = entry.run_completion() else { return false; };
                        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false; };
                        value.get("record").and_then(|record| serde_json::from_value::<crate::tool_run::RunRecord>(record.clone()).ok()).is_some_and(|record| record.events.iter().any(|event| matches!(event, crate::tool_run::RunEvent::Decided { call_id, decision: crate::tool_run::CallDecision::Final { .. }, .. } if *call_id == crate::ToolCallId::fixture("B"))))
                    })
                });
                if durable { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("B's final decision is durable while A is unrecorded");
        control
            .request_local_stop(resolver, crate::TurnCancelMode::Immediate, None)
            .await
            .unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(1), stop.cancelled())
            .await
            .is_ok();
        // Release also on the unfixed side, so the red measures leaked state
        // instead of leaving a live body or handler behind.
        release.notify_one();
        observed
    };
    let (replies, observed) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, cancel) })
            .await
            .unwrap();
    let replies = replies.unwrap();
    let state = plugins.capture_plugin_state().unwrap();
    drop(context);
    handler.close().await.unwrap();
    assert_eq!(reductions.lock_recover().as_slice(), [json!("B")]);
    let namespace = &state.plugins[PLUGIN];
    assert_eq!(
        namespace.values[if disjoint { "b" } else { "value" }],
        json!("B")
    );
    assert!(!namespace.values.contains_key("a"));
    assert_eq!(namespace.publication.receipts.len(), 1);
    assert!(observed, "native A never observed its durable cancellation");
    assert!(!replies.replies[0].output.is_success());
    assert!(replies.replies[1].output.is_success());
}

#[tokio::test]
async fn l19_native_inline_cancel_retains_only_the_same_key_durable_sibling() {
    cancelled_sibling(false).await;
}

#[tokio::test]
async fn l19_native_inline_cancel_retains_only_the_disjoint_key_durable_sibling() {
    cancelled_sibling(true).await;
}
