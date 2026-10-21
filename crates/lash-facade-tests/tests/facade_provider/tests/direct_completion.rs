//! A tool's direct model completion runs inside its call on the session
//! actor's turn, answers with its usage and call record, and is traced like
//! any model call.

use super::*;
use lash_facade_tests::fixtures::{DIRECT_PROMPT as PROMPT, DirectProbe};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tools_direct_completion_runs_in_its_call_and_records_usage_and_trace() -> Result<()> {
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("direct-llm")
        .complete({
            let turn_calls = Arc::clone(&turn_calls);
            move |request| {
                let direct = last_user_text(&request).contains(PROMPT);
                let turn_call = if direct {
                    None
                } else {
                    Some(turn_calls.fetch_add(1, Ordering::SeqCst))
                };
                async move {
                    Ok(match turn_call {
                        None => LlmResponse {
                            usage: lash_core::llm::types::LlmUsage {
                                input_tokens: 4,
                                output_tokens: 6,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_output_tokens: 1,
                                unknown_calls: 0,
                            },
                            ..text_response("raw direct answer")
                        },
                        Some(0) => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "direct-call".to_string(),
                                tool_name: "tools__direct_probe".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        },
                        Some(_) => text_response("done"),
                    })
                }
            }
        })
        .build()
        .into_handle();
    let trace = Arc::new(lash::testing::RecordingTraceSink::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(lash::tool_provider_key!("tools"), Arc::new(DirectProbe))
    .trace_sink(trace.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse("direct-llm").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let output = session
        .send(TurnInput::text("ask the model directly"))
        .output()
        .await?;

    assert_eq!(output.assistant_message(), Some("done"));
    let output_tool_calls =
        lash::testing::tool_calls_for_activity(&session.durable(), &output.activities).await;
    let answered = output_tool_calls
        .iter()
        .find(|call| call.tool == "tools.direct_probe")
        .expect("the probe's call is reported")
        .output
        .value_for_projection();
    assert_eq!(answered["text"], "raw direct answer");
    assert_eq!(answered["input_tokens"], 4);
    assert_eq!(answered["output_tokens"], 6);
    assert_eq!(answered["attempts"], 1);

    core.flush_trace_sink()?;
    let records = trace.json_records().expect("trace records serialize");
    assert!(
        records.iter().any(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("llm_call_completed")
                && record.pointer("/usage/output_tokens") == Some(&serde_json::json!(6))
        }),
        "the direct call is traced with its usage: {records:?}"
    );
    Ok(())
}
