//! A tool before-check's decision on the durable turn path (ADR 0128): an
//! `AbortRun` stops the owning run after the aborted call's result is
//! recorded; a `Deny` fails only the call.

use super::*;
use crate::{TurnEvent, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::{TurnOutcome, TurnStop};

const ECHO: &str = "echo_tool";

/// A tool that answers every call with its arguments.
struct EchoTool;

fn echo_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{ECHO}"),
        ECHO,
        "Echo the value.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], ECHO))
}

#[async_trait]
impl ToolProvider for EchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![echo_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == ECHO).then(|| Arc::new(echo_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(call.args.clone()).into()
    }
}

/// A plugin whose tool before-check answers every call with `decision`.
fn tool_policy_plugin(
    decision: fn() -> lash_core::plugin::BeforeToolDecision,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("tool-policy"),
        lash_core::facade_support::PluginSpec::new().with_tool_args_check(
            crate::hook_key!("policy"),
            Arc::new(move |_| Box::pin(async move { Ok(decision()) })),
        ),
    ))
}

/// One policy turn, in which the model calls [`ECHO`] once and then answers
/// `done`, and the turn sent after it.
struct PolicyTurn {
    output: crate::TurnReport,
    completed: Vec<lash_core::ToolCallOutput>,
    calls: usize,
    requests: Vec<String>,
}

async fn run_policy_turn(
    id: &str,
    decision: fn() -> lash_core::plugin::BeforeToolDecision,
) -> Result<PolicyTurn> {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = {
        let (calls, requests) = (Arc::clone(&calls), Arc::clone(&requests));
        crate::testing::TestProvider::builder()
            .kind("tool-policy")
            .complete(move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                requests
                    .lock_recover()
                    .push(format!("{:?}", request.messages));
                async move {
                    if call == 0 {
                        return Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "call-policy".to_string(),
                                tool_name: ECHO.to_string(),
                                input_json: r#"{"value":"x"}"#.to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        });
                    }
                    Ok(text_response("done"))
                }
            })
            .build()
            .into_handle()
    };
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(EchoTool))
    .plugin(tool_policy_plugin(decision))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let events = RecordingEvents::default();
    let output = session
        .send(TurnInput::text("call the tool"))
        .output_into(&events)
        .await?;
    let completed = events
        .snapshot()
        .await
        .into_iter()
        .filter_map(|activity| match activity.event {
            TurnEvent::ToolCallCompleted { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    let calls_in_turn = calls.load(Ordering::SeqCst);
    session.send(TurnInput::text("next")).output().await?;
    drop(session);
    core.shutdown().await?;
    Ok(PolicyTurn {
        output,
        completed,
        calls: calls_in_turn,
        requests: requests.lock_recover().clone(),
    })
}

fn abort_run() -> lash_core::plugin::BeforeToolDecision {
    lash_core::plugin::BeforeToolDecision::AbortRun(lash_core::plugin::PluginAbort::new(
        "stop",
        "policy stopped the run",
    ))
}

/// A tool check's AbortRun stops the owning run (ADR 0128): the turn stops
/// with the plugin-abort cause after the aborted call's result is recorded,
/// and no further model call runs. What the run accepted before the abort,
/// its input, stays: the next turn's prompt carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_check_abort_run_stops_the_run() -> Result<()> {
    let turn = run_policy_turn("tool-check-abort-run", abort_run).await?;
    assert!(
        matches!(
            &turn.output.outcome,
            TurnOutcome::Stopped(TurnStop::PluginAbort)
        ),
        "{:?}",
        turn.output.outcome
    );
    assert_eq!(turn.completed.len(), 1, "{:?}", turn.completed);
    let output = &turn.completed[0];
    assert!(!output.is_success(), "the aborted call fails");
    assert!(
        matches!(
            output.control,
            Some(lash_core::ToolControl::AbortRun { .. })
        ),
        "{output:?}"
    );
    assert_eq!(turn.calls, 1, "no model call after the abort");
    let next = turn
        .requests
        .last()
        .expect("the next turn called the model");
    assert!(
        next.contains("call the tool"),
        "the stopped run's accepted input stays: {next}"
    );
    Ok(())
}

/// The abort's typed plugin cause is the stopped turn's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5334: a turn's report through send() carries no errors"]
async fn a_tool_check_abort_run_reports_its_typed_plugin_cause() -> Result<()> {
    let turn = run_policy_turn("tool-check-abort-cause", abort_run).await?;
    let issue = turn
        .output
        .errors
        .iter()
        .find(|issue| issue.kind == lash_core::TurnFailureKind::Plugin)
        .unwrap_or_else(|| panic!("the abort's typed plugin cause: {:?}", turn.output.errors));
    assert_eq!(issue.message, "policy stopped the run");
    assert_eq!(
        issue.code.as_ref().map(|code| code.namespaced()).as_deref(),
        Some("tool-policy:stop")
    );
    Ok(())
}

/// A Deny fails only the call: the run continues to the model's next answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_check_deny_fails_only_the_call() -> Result<()> {
    let turn = run_policy_turn("tool-check-deny", || {
        lash_core::plugin::BeforeToolDecision::Deny(lash_core::ToolFailure::tool(
            lash_core::ToolFailureClass::PermissionDenied,
            "denied",
            "policy denied the call",
        ))
    })
    .await?;
    assert!(
        matches!(&turn.output.outcome, TurnOutcome::Finished(_)),
        "{:?}",
        turn.output.outcome
    );
    assert_eq!(turn.completed.len(), 1, "{:?}", turn.completed);
    let output = &turn.completed[0];
    assert_eq!(output.value_for_projection()["code"], "denied");
    assert!(output.control.is_none());
    assert_eq!(
        turn.calls, 2,
        "the run continues to the model's next answer"
    );
    Ok(())
}
