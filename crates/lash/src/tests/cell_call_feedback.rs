//! When a cell fails, its next prompt names the host calls it completed
//! before it failed, with their durable ids and bounded results (FIG-6293).
//!
//! Real RLM turns on the core's own node over a SQLite memory store set, in
//! both dialects and on both channels. The model's next request is read as
//! the scripted provider received it.

use super::output_retention::control_cell_response;
use super::*;
use lash_core::llm::types::LlmResponse;

type Requests = Arc<StdMutex<Vec<serde_json::Value>>>;
type Responses = Arc<tokio::sync::Mutex<std::collections::VecDeque<LlmResponse>>>;

/// A core whose sessions are created under `presentation`, answering from
/// `responses` and recording each request's messages in `requests`.
fn core(
    backend: lash_core::Backend,
    dialect: crate::rlm::CellDialect,
    channel: crate::rlm::RlmChannel,
    presentation: crate::rlm::RlmPresentationConfig,
    responses: &Responses,
    requests: &Requests,
) -> Result<LashCore> {
    let (responses, requests) = (Arc::clone(responses), Arc::clone(requests));
    let provider = crate::testing::TestProvider::builder()
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
    let mut config = crate::rlm::RlmProtocolPluginConfig::builder()
        .channel(channel)
        .instruction_limit(crate::rlm::InstructionBound::standard())
        .memory_limit(crate::rlm::MemoryBound::mebibytes(64))
        .build();
    config.presentation = presentation;
    let factory = crate::rlm::RlmProtocolPluginFactory::new(config, dialect)
        .with_worker_service(untimed_fixture_workers());
    explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())
}

/// The text in `messages` that carries a calls block, wherever the channel
/// placed the observation.
fn calls_observation(messages: &serde_json::Value) -> String {
    fn strings<'a>(value: &'a serde_json::Value, found: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::String(text) => found.push(text),
            serde_json::Value::Array(items) => items.iter().for_each(|item| strings(item, found)),
            serde_json::Value::Object(map) => map.values().for_each(|item| strings(item, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    strings(messages, &mut found);
    let observations = found
        .into_iter()
        .filter(|text| text.contains("{\"completed_calls\":"))
        .collect::<Vec<_>>();
    assert_eq!(observations.len(), 1, "one cell lists its calls");
    observations[0].to_string()
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
        crate::rlm::CellDialect::typescript(),
        crate::rlm::CellDialect::python(),
    ] {
        for channel in [
            crate::rlm::RlmChannel::Cell,
            crate::rlm::RlmChannel::NativeTool,
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
                crate::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
            )?;
            let session = running
                .session(session_id.clone())
                .created()
                .await
                .open()
                .await?;
            let output = session
                .send(TurnInput::text("look up, then fail"))
                .output()
                .await?;
            let lookup = output
                .result
                .tool_calls
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
                    crate::transcript::TranscriptItem::Cell(cell) => Some(cell),
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
            let mut live = crate::rlm::RlmPresentationConfig::standard();
            live.prompt_max_tool_calls = 0;
            live.call_feedback =
                crate::rlm::CallFeedbackPolicy::new(0, 0).expect("zero cuts are valid");
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
