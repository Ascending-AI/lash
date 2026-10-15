//! A control call settles as its declaration says (FIG-5802, FIG-5823): a
//! finish value has the type its tool declares, a host's finish tool takes
//! the place of Lash's, and a control tool's settled result is its control
//! alone, whatever its body or a result transform made of it.

use super::*;

use crate::{TurnEvent, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::{TurnFinish, TurnOutcome};

/// `answer.submit`: the host's own finish tool, whose value
/// `value_schema` types.
#[cfg(feature = "rlm")]
fn answer_tool(value_schema: serde_json::Value) -> lash_core::ToolDefinition {
    lash_core::finish_tool(
        "answer",
        lash_core::JsonSchema::admit(value_schema).expect("valid value schema"),
    )
    .with_tool_binding(lash_core::ToolBinding::new(["answer"], "submit"))
}

/// A model that answers each request with the next of `cells`, recording
/// each request's messages in `requests`.
#[cfg(feature = "rlm")]
fn scripted_cells(
    kind: &'static str,
    cells: Vec<String>,
    requests: Arc<StdMutex<Vec<String>>>,
) -> lash_core::facade_support::ProviderHandle {
    let cells = Arc::new(StdMutex::new(std::collections::VecDeque::from(cells)));
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |request| {
            requests.lock_recover().push(
                serde_json::to_string(&request.messages).expect("serialize request messages"),
            );
            let text = cells.lock_recover().pop_front().expect("scripted cell");
            async move { Ok(text_response(&text)) }
        })
        .build()
        .into_handle()
}

/// FIG-5823 law 4: a host that offers its own finish tool replaces Lash's
/// `control.finish`, and the value has the type the host declared: text is
/// refused as the integer, and an integer finishes.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_finish_replaces_the_default_and_types_the_value() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = scripted_cells(
        "host-finish",
        vec![
            typescript_block(
                "let first = \"none\";\n\
                 try {\n  await answer.submit(\"seven\");\n} catch (error) {\n  first = error.name;\n}\n\
                 console.log(`caught ${first}`);",
            ),
            typescript_block("await answer.submit(7);"),
        ],
        Arc::clone(&requests),
    );
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .tools(
                crate::tool_provider_key!("tools"),
                Arc::new(lash_core::FinishToolProvider::new([answer_tool(
                    serde_json::json!({ "type": "integer" }),
                )])),
            )
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse("host-finish").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let typed = session
        .send(TurnInput::text("answer with a number"))
        .output()
        .await?;

    assert_eq!(
        typed.result.outcome,
        TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "answer".to_string(),
            value: serde_json::json!(7),
        })
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].contains("caught tool_failed"),
        "the host's declared type refused text: {}",
        requests[1]
    );
    for request in &requests {
        assert!(
            request.contains("answer.submit") && !request.contains("control.finish"),
            "the host's finish is offered in place of Lash's: {request}"
        );
    }
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// S9: the schema `control.finish` takes its value under is a run option of
/// the RLM namespace, not drift. A run that states a string schema finishes
/// with text; the next run, under the session's integer schema, refuses
/// text and finishes with an integer.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_retypes_the_finish_value_for_itself_alone() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = scripted_cells(
        "run-finish-schema",
        vec![
            typescript_block("await control.finish(\"seven\");"),
            typescript_block(
                "let first = \"none\";\n\
                 try {\n  await control.finish(\"seven\");\n} catch (error) {\n  first = error.name;\n}\n\
                 console.log(`caught ${first}`);",
            ),
            typescript_block("await control.finish(7);"),
        ],
        Arc::clone(&requests),
    );
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .build(crate::testing::runtime_lease_owner())?;
    let id = SessionId::parse("run-finish-schema").expect("nonblank host identity");
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            mock_session_spec()
                .plugin(
                    crate::rlm::RLM_PROTOCOL_PLUGIN_ID,
                    crate::rlm::RlmCreateExtras {
                        render: None,
                        termination: None,
                        finish_value_schema: Some(serde_json::json!({ "type": "integer" })),
                    },
                )
                .expect("the RLM namespace encodes"),
        ))
        .await?;
    let session = core.session(id).open().await?;
    let retyped = session
        .send(TurnInput::text("answer in words"))
        .options(
            crate::rlm::RLM_RUN_OPTIONS,
            crate::rlm::RlmRunOptions(crate::rlm::RlmTurnOptions {
                render: None,
                finish_value_schema: Some(serde_json::json!({ "type": "string" })),
            }),
        )
        .output()
        .await?;
    let typed = session
        .send(TurnInput::text("answer with a number"))
        .output()
        .await?;

    assert_eq!(
        retyped.result.outcome,
        TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "finish".to_string(),
            value: serde_json::json!("seven"),
        })
    );
    assert_eq!(
        typed.result.outcome,
        TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "finish".to_string(),
            value: serde_json::json!(7),
        })
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(
        requests[2].contains("caught tool_failed"),
        "the run under the session's schema refused text: {}",
        requests[2]
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

const SUBMIT: &str = "submit";

/// What `submit`'s body answers with its arguments.
type SubmitBody = fn(&serde_json::Value) -> lash_core::ToolOutcome;

/// `submit`: a host tool that declares Finish with the value schema
/// `value`, whose body is `body`.
struct SubmitTool {
    body: SubmitBody,
    value: lash_core::JsonSchema,
}

fn submit_definition(value: lash_core::JsonSchema) -> lash_core::ToolDefinition {
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
        lash_core::TurnControls::finish(value),
    )
    .expect("valid declared tool schema")
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], SUBMIT))
}

#[async_trait]
impl ToolProvider for SubmitTool {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, crate::plugins::PluginError> {
        Ok(vec![submit_definition(self.value.clone()).manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == SUBMIT).then(|| Arc::new(submit_definition(self.value.clone()).contract()))
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
    .tools(
        crate::tool_provider_key!("tools"),
        Arc::new(SubmitTool {
            body,
            value: lash_core::JsonSchema::any(),
        }),
    )
    .plugin(plugin)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse(id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
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

/// A `batch` call of the model's whose one member is `submit` with
/// `answer`.
fn batch_submit(call: usize, answer: serde_json::Value) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: format!("call-batch-{call}"),
            tool_name: "batch".to_string(),
            input_json: serde_json::json!({
                "tool_calls": [{ "tool": SUBMIT, "parameters": { "answer": answer } }]
            })
            .to_string(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

/// FIG-5823 law 5: a standard session that requires a finish, whose model
/// finishes through a `batch` member, has that member's value checked
/// against its tool's declared value schema. Text where the tool declares
/// an integer fails the member, the turn goes on, and the model's next
/// batch finishes it with the integer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_member_finish_is_checked_against_its_value_schema() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = {
        let (calls, requests) = (Arc::clone(&calls), Arc::clone(&requests));
        crate::testing::TestProvider::builder()
            .kind("batch-member-finish")
            .complete(move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                requests
                    .lock_recover()
                    .push(format!("{:?}", request.messages));
                async move {
                    Ok(match call {
                        0 => batch_submit(call, serde_json::json!("forty-two")),
                        _ => batch_submit(call, serde_json::json!(42)),
                    })
                }
            })
            .build()
            .into_handle()
    };
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(
        crate::tool_provider_key!("tools"),
        Arc::new(SubmitTool {
            body: |args| lash_core::ToolOutcome::finish(args["answer"].clone()),
            value: lash_core::JsonSchema::admit(serde_json::json!({ "type": "integer" }))
                .expect("an integer schema is admitted"),
        }),
    )
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse("batch-member-finish").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(
            mock_session_spec()
                .plugin(
                    crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
                    crate::standard::StandardTurnOptions {
                        render: None,
                        termination: Some(lash_core::TerminationMode::TerminalRequired),
                    },
                )
                .expect("the creation options encode"),
        ))
        .await?;
    let report = session
        .send(TurnInput::text("submit the answer"))
        .output()
        .await?;

    assert_eq!(
        report.result.outcome,
        TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: SUBMIT.to_string(),
            value: serde_json::json!(42),
        })
    );
    let requests = requests.lock_recover().clone();
    let [_, refused] = requests.as_slice() else {
        panic!("two model calls: {requests:?}");
    };
    assert!(
        refused.contains("declared value schema"),
        "the member's text value was refused: {refused}"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// A finish after an effecting call ends the turn with its value, without
/// an extra model iteration.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finish_keeps_the_mixed_cell_answer_unchanged() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = scripted_cells(
        "finish-mixed",
        vec![typescript_block(
            "const answer = await tools.app_lookup({}); await control.finish(answer);",
        )],
        Arc::clone(&requests),
    );
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .tools(crate::tool_provider_key!("tools"), Arc::new(LongTextTools))
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse("finish-mixed").expect("session id"))
        .created()
        .await
        .open()
        .await?;
    let report = session
        .send(TurnInput::text("look up then answer"))
        .output()
        .await?;
    assert_eq!(
        serde_json::to_string(&report.result.outcome).expect("outcome encodes"),
        serde_json::to_string(&TurnOutcome::Finished(TurnFinish::Finished {
            tool_name: "finish".to_owned(),
            value: serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789")
        }))
        .expect("original outcome encodes")
    );
    assert_eq!(requests.lock_recover().len(), 1);
    drop(session);
    core.shutdown().await?;
    Ok(())
}
