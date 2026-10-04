use super::*;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_f4c0;

/// A plugin with a before-turn observer that prefaces the turn and a tool
/// before-check that answers every call with `decision`.
pub(super) fn tool_policy_plugin(
    decision: fn() -> lash_core::plugin::BeforeToolDecision,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("tool-policy"),
        lash_core::facade_support::PluginSpec::new()
            .with_before_turn(
                lash_core::hook_key!("preface"),
                Arc::new(|_| {
                    Box::pin(async {
                        Ok(lash_core::plugin::TurnContributions {
                            messages: vec![lash_core::PluginMessage::text(
                                lash_core::MessageRole::System,
                                "plugin preface",
                            )],
                            events: Vec::new(),
                            state: Default::default(),
                            session: Default::default(),
                        })
                    })
                }),
            )
            .with_tool_args_check(
                lash_core::hook_key!("policy"),
                Arc::new(move |_| Box::pin(async move { Ok(decision()) })),
            ),
    ))
}

pub(super) fn echo_call(call_id: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: call_id.to_string(),
                tool_name: "echo_tool".to_string(),
                input_json: r#"{"value":"x"}"#.to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

fn text_call(text: &str) -> MockCall {
    MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

async fn run_policy_turn(
    seed: u64,
    turn_id: &'static str,
    decision: fn() -> lash_core::plugin::BeforeToolDecision,
    calls: Vec<MockCall>,
) -> lash_core::facade_support::AssembledTurn {
    let double = kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![tool_policy_plugin(decision)],
        Arc::new(EchoTool),
        mock_provider(calls),
        test_host_config(&backend),
    )
    .await;
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from(turn_id),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "call the tool".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");
    turn
}

/// A tool check's AbortRun stops the owning Run (ADR 0128): the turn stops
/// with the plugin-abort cause after the aborted call's result is recorded,
/// and no further model call runs (the provider serves exactly one). What the
/// turn accepted before the abort, the observer's preface, stays.
#[tokio::test]
pub(super) async fn a_tool_check_abort_run_stops_the_run() {
    let turn = run_policy_turn(
        SEED + 1,
        "tool-check-abort-run",
        || {
            lash_core::plugin::BeforeToolDecision::AbortRun(lash_core::plugin::PluginAbort::new(
                "stop",
                "policy stopped the run",
            ))
        },
        vec![echo_call("call-abort")],
    )
    .await;

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Stopped(TurnStop::PluginAbort)
    ));
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == lash_core::TurnFailureKind::Plugin)
        .expect("the abort's typed plugin cause");
    assert_eq!(issue.message, "policy stopped the run");
    assert_eq!(
        issue.code.as_ref().map(|code| code.namespaced()).as_deref(),
        Some("tool-policy:stop")
    );
    assert_eq!(turn.tool_calls.len(), 1);
    let output = &turn.tool_calls[0].output;
    assert!(!output.is_success(), "the aborted call fails");
    assert!(matches!(
        output.control,
        Some(lash_core::ToolControl::AbortRun { .. })
    ));
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| {
                message
                    .parts
                    .iter()
                    .any(|part| part.content().contains("plugin preface"))
            })
    );
}

/// A Deny fails only the call: the Run continues to the model's next answer.
#[tokio::test]
pub(super) async fn a_tool_check_deny_fails_only_the_call() {
    let turn = run_policy_turn(
        SEED + 3,
        "tool-check-deny",
        || {
            lash_core::plugin::BeforeToolDecision::Deny(lash_core::ToolFailure::tool(
                lash_core::ToolFailureClass::PermissionDenied,
                "denied",
                "policy denied the call",
            ))
        },
        vec![echo_call("call-deny"), text_call("done")],
    )
    .await;

    assert!(matches!(&turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(turn.tool_calls.len(), 1);
    let output = &turn.tool_calls[0].output;
    assert_eq!(output.value_for_projection()["code"], "denied");
    assert!(output.control.is_none());
}
