//! Only a failed cell's next prompt names its completed host calls with
//! their durable ids and bounded results (FIG-6293, FIG-6338).
//!
//! Real RLM turns on the core's own node over a SQLite memory store set, in
//! both dialects and on both channels. The model's next request is read as
//! the scripted provider received it.

use super::*;
use lash_core::llm::types::LlmResponse;
use lash_facade_tests::control_cell_response;

type Requests = Arc<StdMutex<Vec<serde_json::Value>>>;
type Responses = Arc<tokio::sync::Mutex<std::collections::VecDeque<LlmResponse>>>;

fn message_strings<'a>(value: &'a serde_json::Value, found: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => found.push(text),
        serde_json::Value::Array(items) => {
            items.iter().for_each(|item| message_strings(item, found))
        }
        serde_json::Value::Object(map) => {
            map.values().for_each(|item| message_strings(item, found))
        }
        _ => {}
    }
}

/// A core whose sessions are created under `presentation`, answering from
/// `responses` and recording each request's messages in `requests`.
fn core(
    backend: lash_execution_contracts::backend::Backend,
    dialect: lash::rlm::CellDialect,
    channel: lash::rlm::RlmChannel,
    presentation: lash::rlm::RlmPresentationConfig,
    responses: &Responses,
    requests: &Requests,
) -> Result<LashCore> {
    core_with_feedback(
        backend,
        dialect,
        channel,
        presentation,
        responses,
        requests,
        None,
    )
}

fn core_with_feedback(
    backend: lash_execution_contracts::backend::Backend,
    dialect: lash::rlm::CellDialect,
    channel: lash::rlm::RlmChannel,
    presentation: lash::rlm::RlmPresentationConfig,
    responses: &Responses,
    requests: &Requests,
    feedback: Option<lash::plugins::CellFailureFeedbackHook>,
) -> Result<LashCore> {
    let (responses, requests) = (Arc::clone(responses), Arc::clone(requests));
    let provider = lash::testing::TestProvider::builder()
        .kind("cell-call-feedback")
        .complete(move |request: LlmRequest| {
            let responses = Arc::clone(&responses);
            requests
                .lock_recover()
                .push(serde_json::to_value(&request.messages).expect("serialize request messages"));
            async move {
                Ok(responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("a scripted response"))
            }
        })
        .build()
        .into_handle();
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(channel)
        .instruction_limit(lash::rlm::InstructionBound::standard())
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
    config.presentation = presentation;
    let factory = lash::rlm::RlmProtocolPluginFactory::new(config, dialect)
        .with_worker_service(untimed_fixture_workers());
    let builder = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools));
    let builder = if let Some(feedback) = feedback {
        builder.plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("cell-repairs"),
            lash::plugins::PluginSpec::new()
                .with_cell_failure_feedback(lash::hook_key!("callable"), feedback),
        )))
    } else {
        builder
    };
    builder.build(lash_facade_tests::runtime_lease_owner())
}

/// The text in `messages` that carries a calls block, wherever the channel
/// placed the observation.
fn calls_observation(messages: &serde_json::Value) -> String {
    let mut found = Vec::new();
    message_strings(messages, &mut found);
    let observations = found
        .into_iter()
        .filter(|text| text.contains("{\"completed_calls\":"))
        .collect::<Vec<_>>();
    assert_eq!(observations.len(), 1, "one cell lists its calls");
    observations[0].to_string()
}

/// FIG-6338: success keeps the concise operation/outcome feedback from
/// before FIG-6293, without exposing durable ids or result summaries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_cell_feedback_omits_completed_call_evidence() -> Result<()> {
    for dialect in [
        lash::rlm::CellDialect::typescript(),
        lash::rlm::CellDialect::python(),
    ] {
        for channel in [
            lash::rlm::RlmChannel::Cell,
            lash::rlm::RlmChannel::NativeTool,
        ] {
            let case = format!("{} {channel:?}", dialect.name());
            let code = if dialect.name() == "python" {
                "found = await tools_app_lookup({})"
            } else {
                "const found = await tools.app_lookup({});"
            };
            let responses: Responses = Arc::new(tokio::sync::Mutex::new(
                [
                    control_cell_response(&dialect, channel, 0, code.to_string()),
                    text_response("The lookup succeeded."),
                ]
                .into(),
            ));
            let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
            let running = core(
                sqlite_memory_store_backend().await,
                dialect.clone(),
                channel,
                lash::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
            )?;
            let session = created_session(
                &running,
                SessionId::fixture("successful-cell-call-feedback"),
            )
            .await
            .open()
            .await?;
            let output = session
                .send(TurnInput::text("look up once"))
                .output()
                .await?;
            let output_tool_calls =
                lash::testing::tool_calls_for_activity(&session.durable(), &output.activities)
                    .await;
            let lookup = output_tool_calls
                .iter()
                .find(|record| record.tool == "app_lookup")
                .unwrap_or_else(|| panic!("{case}: the lookup completed"));
            {
                let requests = requests.lock_recover();
                let mut texts = Vec::new();
                message_strings(&requests[1], &mut texts);
                assert!(
                    texts.iter().all(|text| !text.contains("completed_calls")),
                    "{case}: successful feedback has no completed_calls block: {texts:?}"
                );
                assert!(
                    texts
                        .iter()
                        .all(|text| !text.contains(lookup.call_id.as_str())),
                    "{case}: the durable call id stays out of successful feedback"
                );
                assert!(
                    texts.iter().any(|text| {
                        let operation = if dialect.name() == "python" {
                            "tools_app_lookup"
                        } else {
                            "tools.app_lookup"
                        };
                        text.contains(&format!("Calls:\n- {operation} → ok"))
                    }),
                    "{case}: concise call feedback is preserved: {texts:?}"
                );
            }
            drop(session);
            running.shutdown().await?;
        }
    }
    Ok(())
}

/// FIG-6293: a TypeScript and a Python cell each complete one host call and
/// then fail. The model's next request lists that call by its recorded tool
/// path, the durable id of the turn's ledger record and a bounded summary of
/// its committed result, and says a completed call stays completed. Opened
/// again by another core, configured with other presentation cuts, the
/// session renders the same observation byte for byte, from the cuts it
/// recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_cell_feedback_preserves_completed_call_evidence() -> Result<()> {
    for dialect in [
        lash::rlm::CellDialect::typescript(),
        lash::rlm::CellDialect::python(),
    ] {
        for channel in [
            lash::rlm::RlmChannel::Cell,
            lash::rlm::RlmChannel::NativeTool,
        ] {
            let case = format!("{} {channel:?}", dialect.name());
            let code = if dialect.name() == "python" {
                "found = await tools_app_lookup({})\nraise ValueError('after the lookup')"
            } else {
                "const found = await tools.app_lookup({});\nthrow new Error(\"after the lookup\");"
            };
            let responses: Responses = Arc::new(tokio::sync::Mutex::new(
                [
                    control_cell_response(&dialect, channel, 0, code.to_string()),
                    text_response("The lookup ran; the cell failed after it."),
                ]
                .into(),
            ));
            let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
            let backend = sqlite_memory_store_backend().await;
            let session_id = SessionId::fixture("cell-call-feedback");
            let running = core(
                backend.clone(),
                dialect.clone(),
                channel,
                lash::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
            )?;
            let session = created_session(&running, session_id.clone())
                .await
                .open()
                .await?;
            let output = session
                .send(TurnInput::text("look up, then fail"))
                .output()
                .await?;
            let output_tool_calls =
                lash::testing::tool_calls_for_activity(&session.durable(), &output.activities)
                    .await;
            let lookup = output_tool_calls
                .iter()
                .find(|record| record.tool == "app_lookup")
                .unwrap_or_else(|| panic!("{case}: the turn's ledger holds the lookup"));
            let transcript = committed(&session)
                .await
                .transcript()
                .expect("valid committed history");
            let cell = transcript
                .visible()
                .find_map(|entry| match &entry.item {
                    lash::transcript::TranscriptItem::Cell(cell) => Some(cell),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{case}: the failed cell is committed"));
            assert!(cell.result.is_failed(), "{case}: {:?}", cell.result);
            let [call] = cell.calls.as_slice() else {
                panic!("{case}: one completed call: {:?}", cell.calls);
            };
            assert_eq!(call.call_id.as_ref(), Some(&lookup.call_id), "{case}");

            let observation = calls_observation(&requests.lock_recover()[1]);
            // The row's members in their rendered order.
            let block = format!(
                "{{\"completed_calls\":[{{\"tool\":{},\"call_id\":{},\"outcome\":\"ok\",\"result_summary\":{{\"text\":\"{{\\\"ok\\\":true}}\",\"truncated\":false}}}}],\"omitted\":{{\"ok\":0,\"err\":0}}}}",
                serde_json::to_string(&call.operation)?,
                serde_json::to_string(&lookup.call_id)?,
            );
            assert!(observation.contains(&block), "{case}: {observation}");
            assert!(
                call.operation.ends_with("app_lookup"),
                "{case}: {}",
                call.operation
            );
            assert!(
                observation.contains(
                    "The completed calls above remain completed even though this cell failed."
                ),
                "{case}: {observation}"
            );
            drop(session);
            running.shutdown().await?;

            // Another core, whose live cuts would omit every call.
            let mut live = lash::rlm::RlmPresentationConfig::standard();
            live.prompt_max_tool_calls = 0;
            live.call_feedback =
                lash::rlm::CallFeedbackPolicy::new(0, 0).expect("zero cuts are valid");
            responses
                .lock()
                .await
                .push_back(text_response("Nothing more to do."));
            let reopened = core(
                backend,
                dialect.clone(),
                channel,
                live,
                &responses,
                &requests,
            )?;
            reopened
                .session(session_id)
                .open()
                .await?
                .send(TurnInput::text("anything else?"))
                .output()
                .await?;
            assert_eq!(
                calls_observation(&requests.lock_recover()[2]),
                observation,
                "{case}: the reopened session renders the recorded observation"
            );
            reopened.shutdown().await?;
        }
    }
    Ok(())
}

/// FIG-6342 / host-owned repair policy: only an installed plugin adds hints,
/// and its typed diagnosis reaches the model's very next input in both dialects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_own_cell_failure_repairs_in_the_next_model_input() -> Result<()> {
    for (dialect, code, diagnostic_code, callable) in [
        (
            lash::rlm::CellDialect::typescript(),
            "await app_lookup({});",
            "TS_UNKNOWN_BINDING",
            "tools.app_lookup",
        ),
        (
            lash::rlm::CellDialect::python(),
            "await app_lookup({})",
            "PY_UNKNOWN_NAME",
            "tools_app_lookup",
        ),
    ] {
        let mut baseline = None;
        for installed in [false, true] {
            let hook: lash::plugins::CellFailureFeedbackHook = Arc::new(move |diagnostic| {
                assert_eq!(diagnostic.code, diagnostic_code);
                assert_eq!(diagnostic.unknown_name, Some("app_lookup"));
                let (start, end) = diagnostic.span.expect("unknown name has a span");
                assert_eq!(&code[start..end], "app_lookup");
                assert_eq!(
                    diagnostic.grants.get("app_lookup").map(String::as_str),
                    Some(callable)
                );
                vec![format!("did you mean `{callable}`?")]
            });
            let responses: Responses = Arc::new(tokio::sync::Mutex::new(
                [
                    control_cell_response(
                        &dialect,
                        lash::rlm::RlmChannel::Cell,
                        0,
                        code.to_owned(),
                    ),
                    text_response("done"),
                ]
                .into(),
            ));
            let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
            let running = core_with_feedback(
                sqlite_memory_store_backend().await,
                dialect.clone(),
                lash::rlm::RlmChannel::Cell,
                lash::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
                installed.then_some(hook),
            )?;
            let session = created_session(&running, SessionId::fixture("plugin-cell-repairs"))
                .await
                .open()
                .await?;
            session
                .send(TurnInput::text("call the lookup"))
                .output()
                .await?;
            let feedback = {
                let requests = requests.lock_recover();
                assert_eq!(requests.len(), 2);
                let mut texts = Vec::new();
                message_strings(&requests[1], &mut texts);
                texts
                    .into_iter()
                    .find(|text| text.contains(diagnostic_code))
                    .expect("the next model request carries the diagnosis")
                    .to_owned()
            };
            let repair = format!("\nInstead: did you mean `{callable}`?");
            if installed {
                assert!(feedback.contains(&repair), "{feedback}");
                assert_eq!(
                    feedback.replace(&repair, ""),
                    baseline.as_deref().expect("unextended feedback"),
                    "the plugin adds only its repair"
                );
            } else {
                let message = if dialect.name() == "python" {
                    "name `app_lookup` is not defined"
                } else {
                    "`app_lookup` is not defined"
                };
                assert_eq!(
                    feedback,
                    format!(
                        "{diagnostic_code}: {message} (line 1, column 7)\n  {code}\n\nA line starting with `</{}>` after whitespace terminates the outer cell even inside multiline source text; construct that content without a line-leading closing delimiter. Everything after that closing tag is discarded; wait for real results before continuing.\n\nNext: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected cell.",
                        dialect.name(),
                    ),
                    "no plugin preserves the original feedback bytes"
                );
                baseline = Some(feedback);
            }
            running.shutdown().await?;
        }
    }
    Ok(())
}
