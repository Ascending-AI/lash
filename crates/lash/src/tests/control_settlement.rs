//! A control call settles as its declaration says (FIG-5802): Lash's
//! `control.finish` takes its value under the turn's finish schema as its
//! input, and a control tool's settled result is its control alone, whatever
//! its body or a result transform made of it.

use super::*;

use crate::{TurnEvent, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::{TurnFinish, TurnOutcome};

/// FIG-5802: a send that states a finish schema makes it `control.finish`'s
/// input schema. A value the schema refuses fails the call through ordinary
/// input validation: the cell catches the failure and goes on, the call
/// spent the cell's one control attempt, and a later cell finishes.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_finish_value_fails_the_call_inside_the_cell() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let cells = Arc::new(StdMutex::new(std::collections::VecDeque::from(vec![
        typescript_block(
            "let first = \"none\";\n\
             try {\n  await control.finish(\"wrong\");\n} catch (error) {\n  first = error.name;\n}\n\
             let second = \"none\";\n\
             try {\n  await control.finish(7);\n} catch (error) {\n  second = error.name;\n}\n\
             console.log(`caught ${first} then ${second}`);",
        ),
        typescript_block("await control.finish(7);"),
    ])));
    let provider = {
        let (requests, cells) = (Arc::clone(&requests), Arc::clone(&cells));
        crate::testing::TestProvider::builder()
            .kind("finish-input-contract")
            .complete(move |request| {
                requests.lock_recover().push(
                    serde_json::to_string(&request.messages).expect("serialize request messages"),
                );
                let text = cells.lock_recover().pop_front().expect("scripted cell");
                async move { Ok(text_response(&text)) }
            })
            .build()
            .into_handle()
    };
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse("finish-input-contract").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let report = crate::rlm::RlmSendBuilderExt::finish_schema(
        session.send(TurnInput::text("answer with a whole number")),
        serde_json::json!({ "type": "integer" }),
    )?
    .output()
    .await?;

    assert_eq!(
        report.result.outcome,
        TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "finish".to_string(),
            value: serde_json::json!(7),
        })
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].contains("caught tool_failed then control_refused"),
        "the cell caught the refused value, and its second finish found the attempt spent: {}",
        requests[1]
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

const SUBMIT: &str = "submit";

/// What `submit`'s body answers with its arguments.
type SubmitBody = fn(&serde_json::Value) -> lash_core::ToolOutcome;

/// `submit`: a host tool that declares Finish, whose body is `body`.
struct SubmitTool {
    body: SubmitBody,
}

fn submit_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::control(
        format!("tool:{SUBMIT}"),
        SUBMIT,
        "Submit the answer and end the turn.",
        serde_json::json!({
            "type": "object",
            "properties": { "answer": {} },
            "required": ["answer"],
            "additionalProperties": false
        }),
        lash_core::TurnControls::finish(),
    )
    .expect("valid declared tool schema")
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], SUBMIT))
}

#[async_trait]
impl ToolProvider for SubmitTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![submit_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == SUBMIT).then(|| Arc::new(submit_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (self.body)(call.args).into()
    }
}

/// The settled outputs of a standard turn in which the model calls
/// `submit({answer: 42})` and then answers `done`, with the requests it
/// sent and the turn's outcome.
struct SubmitTurn {
    outcome: TurnOutcome,
    completed: Vec<lash_core::ToolCallOutput>,
    requests: Vec<String>,
}

async fn run_submit_turn(
    id: &str,
    body: SubmitBody,
    plugin: Arc<dyn PluginFactory>,
) -> Result<SubmitTurn> {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = {
        let (calls, requests) = (Arc::clone(&calls), Arc::clone(&requests));
        crate::testing::TestProvider::builder()
            .kind("control-settlement")
            .complete(move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                requests
                    .lock_recover()
                    .push(format!("{:?}", request.messages));
                async move {
                    if call == 0 {
                        return Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "call-submit".to_string(),
                                tool_name: SUBMIT.to_string(),
                                input_json: r#"{"answer":42}"#.to_string(),
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
    .tools(Arc::new(SubmitTool { body }))
    .plugin(plugin)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse(id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    let events = RecordingEvents::default();
    let report = session
        .send(TurnInput::text("submit the answer"))
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
    drop(session);
    core.shutdown().await?;
    Ok(SubmitTurn {
        outcome: report.outcome,
        completed,
        requests: requests.lock_recover().clone(),
    })
}

fn hooks_plugin(spec: lash_core::facade_support::PluginSpec) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("control-settlement"),
        spec,
    ))
}

/// FIG-5802: a result transform cannot put an acknowledgement on a control
/// call's result. The transform runs, and the call still settles as its
/// control alone: no value and no view the model could read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_result_transform_cannot_add_an_acknowledgement_to_a_finish() -> Result<()> {
    let acknowledge = hooks_plugin(
        lash_core::facade_support::PluginSpec::new().with_tool_result_transform(
            crate::hook_key!("acknowledge"),
            Arc::new(|input| {
                Box::pin(async move {
                    Ok(lash_core::plugin::ToolResultCandidate {
                        outcome: lash_core::ToolCallOutcome::Success(
                            lash_core::ToolValue::untrusted_json(serde_json::json!({ "ok": true })),
                        ),
                        ..input.current
                    })
                })
            }),
        ),
    );
    let turn = run_submit_turn(
        "control-settlement-transform",
        |args| lash_core::ToolOutcome::finish(args["answer"].clone()),
        acknowledge,
    )
    .await?;

    assert_eq!(
        turn.outcome,
        TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: SUBMIT.to_string(),
            value: serde_json::json!(42),
        })
    );
    let [output] = turn.completed.as_slice() else {
        panic!("one settled call: {:?}", turn.completed);
    };
    assert_eq!(
        output.value_for_projection(),
        serde_json::Value::Null,
        "{output:?}"
    );
    assert_eq!(output.view, None, "{output:?}");
    Ok(())
}

/// FIG-5802: a tool that declares a control and answers a value without
/// emitting one breaks its declaration, whether its body answered the value
/// or a before-check served it from a cache. The call settles as a failure
/// the model reads, and the value never reaches the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_control_tool_that_answers_a_value_is_refused() -> Result<()> {
    let body_answers = run_submit_turn(
        "control-settlement-body-value",
        |_| lash_core::ToolOutcome::ok(serde_json::json!({ "ack": "received" })),
        hooks_plugin(lash_core::facade_support::PluginSpec::new()),
    )
    .await?;
    let cache_answers = run_submit_turn(
        "control-settlement-cached-value",
        |args| lash_core::ToolOutcome::finish(args["answer"].clone()),
        hooks_plugin(
            lash_core::facade_support::PluginSpec::new().with_tool_args_check(
                crate::hook_key!("cache"),
                Arc::new(|_| {
                    Box::pin(async {
                        Ok(lash_core::plugin::BeforeToolDecision::Cached(
                            lash_core::plugin::CachedToolSuccess::new(
                                lash_core::ToolValue::untrusted_json(
                                    serde_json::json!({ "ack": "received" }),
                                ),
                            ),
                        ))
                    })
                }),
            ),
        ),
    )
    .await?;

    for (route, turn) in [("body", body_answers), ("cache", cache_answers)] {
        let [output] = turn.completed.as_slice() else {
            panic!("{route}: one settled call: {:?}", turn.completed);
        };
        let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("{route}: the call fails: {output:?}");
        };
        assert_eq!(
            failure.cause.as_deref(),
            Some(&lash_core::ToolFailureCause::Declaration {
                refusal: lash_core::DeclarationRefusal::UndeclaredOutput,
            }),
            "{route}: {output:?}"
        );
        assert_eq!(
            turn.outcome,
            TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: "done".to_string(),
            }),
            "{route}: the turn goes on to the model's answer"
        );
        let [_, next] = turn.requests.as_slice() else {
            panic!("{route}: two model calls: {:?}", turn.requests);
        };
        assert!(
            !next.contains("received"),
            "{route}: the value never reaches the model: {next}"
        );
    }
    Ok(())
}
